use rshtml_analyzer::ra_proxy::RustAnalyzerProcess;
use rshtml_analyzer::signature::SignatureResolver;
use rshtml_analyzer::transpiler::TemplateTranspiler;
use std::fs;
use std::path::PathBuf;

/// Tests the end-to-end integration pipeline:
/// 1. Initialize downstream rust-analyzer on the discovered workspace.
/// 2. Transpile views/index.rs.html to virtual Rust code.
/// 3. Feed parent file and virtual file into rust-analyzer via in-memory textDocument/didOpen.
/// 4. Query symbols and completions on the virtual Rust expression.
#[tokio::test]
async fn test_full_pipeline_with_real_workspace() {
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string()));
    let candidate = manifest_dir.join("../rshtml");
    let workspace_path = if candidate.join("rshtml_test").exists() {
        candidate
    } else {
        manifest_dir.clone()
    };

    let rshtml_test_dir = workspace_path.join("rshtml_test");
    if !rshtml_test_dir.exists() {
        return;
    }

    let test_template = rshtml_test_dir.join("views/__test_pipeline.rs.html");
    let valid_template_content = r#"<div>
    @if self.footer {
        <p>@self.home_time.year()</p>
    }
</div>"#;
    fs::write(&test_template, valid_template_content).unwrap();
    let template_file = test_template.clone();
    let views_dir = rshtml_test_dir.join("views");

    // 1. Resolve Signature & Metadata
    let resolved_ctx = SignatureResolver::resolve(&rshtml_test_dir, "views/index.rs.html")
        .expect("Signature resolution for index.rs.html must succeed");

    assert_eq!(resolved_ctx.struct_name, "IndexPage");
    assert!(!resolved_ctx.fields.is_empty());

    // 2. Transpile via official rshtml_core
    let transpile_res =
        TemplateTranspiler::transpile_file(&template_file, &views_dir, &resolved_ctx)
            .expect("Transpilation via rshtml_core must succeed");

    assert!(transpile_res.virtual_code.contains("impl IndexPage {"));

    // 3. Start RA on discovered workspace
    let ra = RustAnalyzerProcess::spawn(None)
        .await
        .expect("RA must spawn");
    let init_res = ra.initialize(workspace_path.to_str().unwrap()).await;
    assert!(init_res.is_ok(), "RA must initialize on workspace");

    // 4. Link parent module in-memory via didOpen
    let parent_file = &resolved_ctx.rust_file_path;
    let parent_uri = format!("file://{}", parent_file.to_str().unwrap());
    let parent_content = fs::read_to_string(parent_file).unwrap();

    let build_dir = views_dir.join(".build");
    fs::create_dir_all(&build_dir).unwrap();
    let virtual_file_path = build_dir.join("__rshtml_virtual_index.rs");
    let virtual_uri = format!("file://{}", virtual_file_path.to_str().unwrap());

    let linkage_decl = format!(
        "\n#[path = {:?}]\n#[allow(dead_code, unused_imports)]\npub mod __rshtml_virtual_index;\n",
        virtual_file_path.to_string_lossy()
    );
    let augmented_parent = format!("{}{}", parent_content, linkage_decl);

    ra.send_notification(
        "textDocument/didOpen",
        serde_json::json!({
            "textDocument": {
                "uri": parent_uri,
                "languageId": "rust",
                "version": 1,
                "text": augmented_parent
            }
        }),
    )
    .await
    .unwrap();

    // 5. Write virtual file to disk and send didOpen
    fs::write(&virtual_file_path, &transpile_res.virtual_code).unwrap();

    ra.send_notification(
        "textDocument/didOpen",
        serde_json::json!({
            "textDocument": {
                "uri": virtual_uri,
                "languageId": "rust",
                "version": 1,
                "text": transpile_res.virtual_code
            }
        }),
    )
    .await
    .unwrap();

    // 6. Test Query to RA: documentSymbol
    let query_res = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        ra.send_request(
            "textDocument/documentSymbol",
            serde_json::json!({
                "textDocument": { "uri": virtual_uri }
            }),
        ),
    )
    .await;

    match query_res {
        Ok(Ok(val)) => {
            let symbols = val.pointer("/result");
            assert!(symbols.is_some(), "Document symbols must be returned by RA");
        }
        Ok(Err(e)) => panic!("RA returned error: {}", e),
        Err(_) => panic!("Timeout waiting for RA response"),
    }

    // 7. Find target position for self.home_time.
    let (target_line, target_col) = transpile_res
        .virtual_code
        .lines()
        .enumerate()
        .find_map(|(line_idx, line)| {
            if let Some(col_idx) = line.find("self.home_time.") {
                Some((line_idx as u32, (col_idx + "self.home_time.".len()) as u32))
            } else {
                None
            }
        })
        .expect("Should find self.home_time. in virtual code");

    // Retry completion until RA indexing finishes (up to 15s)
    let start = std::time::Instant::now();
    let mut completion_items = None;

    while start.elapsed() < std::time::Duration::from_secs(15) {
        let comp_res = ra
            .send_request(
                "textDocument/completion",
                serde_json::json!({
                    "textDocument": { "uri": virtual_uri },
                    "position": { "line": target_line, "character": target_col }
                }),
            )
            .await;

        if let Ok(val) = comp_res {
            if let Some(items) = val
                .pointer("/result/items")
                .or_else(|| val.pointer("/result"))
            {
                if let Some(arr) = items.as_array() {
                    if !arr.is_empty() {
                        completion_items = Some(arr.clone());
                        break;
                    }
                }
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    let items =
        completion_items.expect("RA must return non-empty completion items for DateTime<Utc>");
    println!("SUCCESS: RA returned {} completion items!", items.len());
    let labels: Vec<_> = items
        .iter()
        .filter_map(|it| it.get("label").and_then(|l| l.as_str()))
        .take(10)
        .collect();
    println!("Sample completion items: {:?}", labels);
    println!(
        "FIRST ITEM JSON: {}",
        serde_json::to_string_pretty(&items[0]).unwrap()
    );

    assert!(
        items.len() >= 10,
        "Expected methods on DateTime<Utc> like year(), timestamp(), etc."
    );

    // 8. Test goto_definition for self.home_time
    let (def_line, def_col) = transpile_res
        .virtual_code
        .lines()
        .enumerate()
        .find_map(|(line_idx, line)| {
            if let Some(col_idx) = line.find("home_time") {
                Some((line_idx as u32, (col_idx + 2) as u32))
            } else {
                None
            }
        })
        .expect("Should find home_time in virtual code");

    let def_res = ra
        .send_request(
            "textDocument/definition",
            serde_json::json!({
                "textDocument": { "uri": virtual_uri },
                "position": { "line": def_line, "character": def_col }
            }),
        )
        .await;

    assert!(def_res.is_ok(), "RA should respond to definition request");
    let def_val = def_res.unwrap();
    let def_result = def_val.get("result").filter(|v| !v.is_null());
    assert!(
        def_result.is_some(),
        "Definition must resolve to backing struct field"
    );

    // 9. Verify server capabilities were retrieved
    let caps = ra.server_capabilities().await;
    assert!(caps.is_some(), "RA server capabilities must be populated");

    let _ = ra.shutdown().await;
    let _ = fs::remove_file(virtual_file_path);
    let _ = fs::remove_file(test_template);
}

/// Tests that once a virtual file is opened in rust-analyzer,
/// subsequent code changes can be sent EXCLUSIVELY in-memory via `textDocument/didChange`
/// WITHOUT writing anything to disk, and rust-analyzer correctly responds to completions and hover!
#[tokio::test]
async fn test_in_memory_virtual_file_updates_without_disk_io() {
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string()));
    let candidate = manifest_dir.join("../rshtml");
    let workspace_path = if candidate.join("rshtml_test").exists() {
        candidate
    } else {
        manifest_dir.clone()
    };

    let rshtml_test_dir = workspace_path.join("rshtml_test");
    if !rshtml_test_dir.exists() {
        return;
    }

    let views_dir = rshtml_test_dir.join("views");
    let resolved_ctx = SignatureResolver::resolve(&rshtml_test_dir, "views/index.rs.html")
        .expect("Signature resolution for index.rs.html must succeed");

    // 1. Start RA on workspace
    let ra = RustAnalyzerProcess::spawn(None)
        .await
        .expect("RA must spawn");
    let init_res = ra.initialize(workspace_path.to_str().unwrap()).await;
    assert!(init_res.is_ok(), "RA must initialize on workspace");

    // 2. Link parent module in-memory via didOpen
    let parent_file = &resolved_ctx.rust_file_path;
    let parent_uri = format!("file://{}", parent_file.to_str().unwrap());
    let parent_content = fs::read_to_string(parent_file).unwrap();

    let build_dir = views_dir.join(".build");
    fs::create_dir_all(&build_dir).unwrap();
    let virtual_file_path = build_dir.join("__rshtml_virtual_mem_test.rs");
    let virtual_uri = format!("file://{}", virtual_file_path.to_str().unwrap());

    let linkage_decl = format!(
        "\n#[path = {:?}]\n#[allow(dead_code, unused_imports)]\npub mod __rshtml_virtual_mem_test;\n",
        virtual_file_path.to_string_lossy()
    );
    let augmented_parent = format!("{}{}", parent_content, linkage_decl);

    ra.send_notification(
        "textDocument/didOpen",
        serde_json::json!({
            "textDocument": {
                "uri": parent_uri,
                "languageId": "rust",
                "version": 1,
                "text": augmented_parent
            }
        }),
    )
    .await
    .unwrap();

    // 3. Write INITIAL EMPTY SCAFFOLDING to disk ONCE so RA discovers the module
    let initial_disk_content = r#"// @generated by rshtml-analyzer
#![allow(unused_imports, dead_code, unused_variables, path_statements)]
use chrono::{DateTime, Datelike, Utc};
use crate::IndexPage;

impl IndexPage {
    pub fn __rshtml_virtual_context(&self) {
        // EMPTY ON DISK
    }
}
"#;
    fs::write(&virtual_file_path, initial_disk_content).unwrap();

    // Send didOpen with this initial version (v1)
    ra.send_notification(
        "textDocument/didOpen",
        serde_json::json!({
            "textDocument": {
                "uri": virtual_uri,
                "languageId": "rust",
                "version": 1,
                "text": initial_disk_content
            }
        }),
    )
    .await
    .unwrap();

    // 4. NOW: MODIFY THE BUFFER IN MEMORY ONLY via didChange (version 2)
    // Notice: we DO NOT write to virtual_file_path on disk!
    let in_memory_modified_code = r#"// @generated by rshtml-analyzer
#![allow(unused_imports, dead_code, unused_variables, path_statements)]
use chrono::{DateTime, Datelike, Utc};
use crate::IndexPage;

impl IndexPage {
    pub fn __rshtml_virtual_context(&self) {
        let _test_val = self.home_time.;
    }
}
"#;

    ra.send_notification(
        "textDocument/didChange",
        serde_json::json!({
            "textDocument": {
                "uri": virtual_uri,
                "version": 2
            },
            "contentChanges": [
                {
                    "text": in_memory_modified_code
                }
            ]
        }),
    )
    .await
    .unwrap();

    // Verify disk content is STILL the empty version
    let current_disk_content = fs::read_to_string(&virtual_file_path).unwrap();
    assert!(
        current_disk_content.contains("// EMPTY ON DISK"),
        "Disk file MUST remain unchanged!"
    );

    // 5. Query RA for completion at `self.home_time.` on the IN-MEMORY buffer
    let (target_line, target_col) = in_memory_modified_code
        .lines()
        .enumerate()
        .find_map(|(line_idx, line)| {
            if let Some(col_idx) = line.find("self.home_time.") {
                Some((line_idx as u32, (col_idx + "self.home_time.".len()) as u32))
            } else {
                None
            }
        })
        .expect("Should find self.home_time. in in-memory code");

    let start = std::time::Instant::now();
    let mut completion_items = None;

    while start.elapsed() < std::time::Duration::from_secs(15) {
        let comp_res = ra
            .send_request(
                "textDocument/completion",
                serde_json::json!({
                    "textDocument": { "uri": virtual_uri },
                    "position": { "line": target_line, "character": target_col }
                }),
            )
            .await;

        if let Ok(val) = comp_res {
            if let Some(items) = val
                .pointer("/result/items")
                .or_else(|| val.pointer("/result"))
            {
                if let Some(arr) = items.as_array() {
                    if !arr.is_empty() {
                        completion_items = Some(arr.clone());
                        break;
                    }
                }
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    let items =
        completion_items.expect("RA must return completions for the IN-MEMORY modified buffer!");
    println!(
        "SUCCESS: RA returned {} completion items from pure in-memory buffer without disk write!",
        items.len()
    );

    assert!(
        items.len() >= 10,
        "Expected methods on DateTime<Utc> from in-memory didChange update"
    );

    let _ = ra.shutdown().await;
    let _ = fs::remove_file(virtual_file_path);
}

/// Tests that rust-analyzer keeps the struct context even when the function body is completely empty!
#[tokio::test]
async fn test_empty_function_body_keeps_context() {
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string()));
    let candidate = manifest_dir.join("../rshtml");
    let workspace_path = if candidate.join("rshtml_test").exists() {
        candidate
    } else {
        manifest_dir.clone()
    };

    let rshtml_test_dir = workspace_path.join("rshtml_test");
    if !rshtml_test_dir.exists() {
        return;
    }

    let views_dir = rshtml_test_dir.join("views");
    let resolved_ctx = SignatureResolver::resolve(&rshtml_test_dir, "views/index.rs.html")
        .expect("Signature resolution for index.rs.html must succeed");

    // 1. Generate virtual code with EMPTY function body
    let empty_output = TemplateTranspiler::wrap_empty_context(&resolved_ctx);
    assert!(empty_output.virtual_code.contains("impl IndexPage {"));
    assert!(
        empty_output
            .virtual_code
            .contains("pub fn __rshtml_virtual_context(&self) {")
    );

    // 2. Start RA on workspace
    let ra = RustAnalyzerProcess::spawn(None)
        .await
        .expect("RA must spawn");
    let init_res = ra.initialize(workspace_path.to_str().unwrap()).await;
    assert!(init_res.is_ok(), "RA must initialize on workspace");

    // 3. Link parent module in-memory via didOpen
    let parent_file = &resolved_ctx.rust_file_path;
    let parent_uri = format!("file://{}", parent_file.to_str().unwrap());
    let parent_content = fs::read_to_string(parent_file).unwrap();

    let build_dir = views_dir.join(".build");
    fs::create_dir_all(&build_dir).unwrap();
    let virtual_file_path = build_dir.join("__rshtml_virtual_empty_test.rs");
    let virtual_uri = format!("file://{}", virtual_file_path.to_str().unwrap());

    let linkage_decl = format!(
        "
#[path = {:?}]
#[allow(dead_code, unused_imports)]
pub mod __rshtml_virtual_empty_test;
",
        virtual_file_path.to_string_lossy()
    );
    let augmented_parent = format!("{}{}", parent_content, linkage_decl);

    ra.send_notification(
        "textDocument/didOpen",
        serde_json::json!({
            "textDocument": {
                "uri": parent_uri,
                "languageId": "rust",
                "version": 1,
                "text": augmented_parent
            }
        }),
    )
    .await
    .unwrap();

    // 4. Write empty body code to disk & didOpen
    fs::write(&virtual_file_path, &empty_output.virtual_code).unwrap();
    ra.send_notification(
        "textDocument/didOpen",
        serde_json::json!({
            "textDocument": {
                "uri": virtual_uri,
                "languageId": "rust",
                "version": 1,
                "text": empty_output.virtual_code
            }
        }),
    )
    .await
    .unwrap();

    // 5. Query RA for hover/symbols on self
    let (target_line, target_col) = empty_output
        .virtual_code
        .lines()
        .enumerate()
        .find_map(|(line_idx, line)| {
            if let Some(col_idx) = line.find("&self") {
                Some((line_idx as u32, (col_idx + 1) as u32))
            } else {
                None
            }
        })
        .expect("Should find &self in virtual code");

    let _ = ra
        .send_request(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": virtual_uri },
                "position": { "line": target_line, "character": target_col }
            }),
        )
        .await;

    let start = std::time::Instant::now();
    let mut resolved_self = false;
    while start.elapsed() < std::time::Duration::from_secs(15) {
        let hover_res = ra
            .send_request(
                "textDocument/hover",
                serde_json::json!({
                    "textDocument": { "uri": virtual_uri },
                    "position": { "line": target_line, "character": target_col }
                }),
            )
            .await;
        if let Ok(val) = hover_res {
            if let Some(res) = val.get("result").filter(|v| !v.is_null()) {
                println!("SUCCESS: RA returned hover for &self in empty body: {:?}", res);
                resolved_self = true;
                break;
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }
    assert!(resolved_self, "RA must resolve &self in empty body after indexing");

    let _ = ra.shutdown().await;
    let _ = fs::remove_file(virtual_file_path);
}
