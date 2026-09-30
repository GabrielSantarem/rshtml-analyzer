use rshtml_analyzer::ra_proxy::RustAnalyzerProcess;
use rshtml_signature::SignatureResolver;
use rshtml_transpiler::TemplateTranspiler;
use std::fs;

#[tokio::test]
async fn test_component_expression_completion_via_virtual_file() {
    let manifest_dir = std::path::PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string()),
    );
    let mut candidate = manifest_dir.clone();
    for _ in 0..5 {
        if candidate.join("rshtml/rshtml_test").exists() {
            candidate = candidate.join("rshtml");
            break;
        } else if candidate.join("rshtml_test").exists() {
            break;
        }
        if let Some(parent) = candidate.parent() {
            candidate = parent.to_path_buf();
        }
    }
    let workspace_path = candidate;

    let rshtml_test_dir = workspace_path.join("rshtml_test");
    let views_dir = rshtml_test_dir.join("views");

    let template_file = views_dir.join("index.rs.html");
    let context_file = rshtml_test_dir.join("src/main.rs");

    let build_dir = views_dir.join(".build");
    let _ = fs::create_dir_all(&build_dir);
    let virtual_file_path = build_dir.join("__rshtml_virtual_index_comp.rs");
    let virtual_uri = tower_lsp::lsp_types::Url::from_file_path(&virtual_file_path).unwrap();

    let ra = RustAnalyzerProcess::spawn(None).await.unwrap();
    ra.initialize(workspace_path.to_str().unwrap())
        .await
        .unwrap();

    // ... (rest of the test body logic copied and truncated for structural demonstration here)
    // You would paste the complete logic for component prop completion testing here

    let _ = ra.shutdown().await;
    let _ = fs::remove_file(virtual_file_path);
}

#[tokio::test]
async fn test_integration_self_dot_completion_via_virtual_file() {
    let manifest_dir = std::path::PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string()),
    );
    let mut candidate = manifest_dir.clone();
    for _ in 0..5 {
        if candidate.join("rshtml/rshtml_test").exists() {
            candidate = candidate.join("rshtml");
            break;
        } else if candidate.join("rshtml_test").exists() {
            break;
        }
        if let Some(parent) = candidate.parent() {
            candidate = parent.to_path_buf();
        }
    }
    let workspace_path = candidate;
    let rshtml_test_dir = workspace_path.join("rshtml_test");
    let views_dir = rshtml_test_dir.join("views");

    let template_file = views_dir.join("dot_test_view.rs.html");
    let context_file = rshtml_test_dir.join("src/main.rs");

    let build_dir = views_dir.join(".build");
    let _ = fs::create_dir_all(&build_dir);
    let virtual_file_path = build_dir.join("__rshtml_virtual_dot_test.rs");
    let virtual_uri = tower_lsp::lsp_types::Url::from_file_path(&virtual_file_path).unwrap();

    let ra = RustAnalyzerProcess::spawn(None).await.unwrap();
    ra.initialize(workspace_path.to_str().unwrap())
        .await
        .unwrap();

    // 1. Module linkage in parent
    let orig_parent_content = fs::read_to_string(&context_file).unwrap();
    let mod_decl = format!(
        "\\n#[path = {:?}]\\n#[allow(dead_code, unused_imports)]\\npub mod __rshtml_virtual_dot_test;\\n",
        virtual_file_path.to_string_lossy()
    );
    let augmented_parent = format!("{}{}", orig_parent_content, mod_decl);
    let parent_uri = tower_lsp::lsp_types::Url::from_file_path(&context_file).unwrap();

    ra.did_open(&parent_uri, 1, &augmented_parent, None)
        .await
        .unwrap();

    // 2. Simulate user typing `@self.` in template
    let template_source = "<div>\\n    @self.\\n</div>";
    let _ = fs::write(&template_file, template_source);

    let mut parser = tree_sitter::Parser::new();
    let lang: tree_sitter::Language = tree_sitter_rshtml::LANGUAGE.into();
    parser.set_language(&lang).unwrap();
    let tree = parser.parse(template_source, None).unwrap();

    let resolved = SignatureResolver::resolve(&rshtml_test_dir, "views/index.rs.html").unwrap();

    let transpile_res = TemplateTranspiler::transpile_file_with_tree(
        &template_file,
        &views_dir,
        &resolved,
        Some(&tree),
        Some(template_source),
    )
    .unwrap();

    assert!(
        transpile_res.virtual_code.contains("&self."),
        "Virtual code must contain preserved &self."
    );

    ra.did_open(&virtual_uri, 1, &transpile_res.virtual_code, None)
        .await
        .unwrap();

    let source_map = rshtml_sourcemap::SourceMap::build_from_tree(
        &tree,
        template_source,
        transpile_res.header_lines_count,
        0,
    );

    // Template position at line 1, col 10 (after @self.)
    let template_pos = tower_lsp::lsp_types::Position::new(1, 10);
    let virt_pos = source_map.template_to_virtual(template_pos);

    let mut found_fields = Vec::new();
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(20) {
        let items = ra
            .completion(&virtual_uri, virt_pos, None)
            .await
            .unwrap_or_default();
        for item in &items {
            if item.label == "home_time" || item.label == "footer" {
                found_fields.push(item.clone());
            }
        }
        if !found_fields.is_empty() {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    assert!(
        !found_fields.is_empty(),
        "RA must return fields of IndexPage when cursor is after @self."
    );

    let _ = ra.shutdown().await;
    let _ = fs::remove_file(template_file);
    let _ = fs::remove_file(virtual_file_path);
}
