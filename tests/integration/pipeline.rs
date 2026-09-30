use crate::integration::helpers::*;
use rshtml_analyzer::ra_proxy::RustAnalyzerProcess;
use rshtml_analyzer::virtual_file::VirtualFileManager;
use rshtml_signature::SignatureResolver;
use rshtml_transpiler::TemplateTranspiler;
use std::fs;
use tower_lsp::lsp_types::Position;

#[tokio::test]
async fn test_full_pipeline_with_real_workspace() {
    let manifest_dir = std::path::PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string()),
    );
    let candidate = manifest_dir.join("../rshtml");
    let workspace_path = if candidate.join("rshtml_test").exists() {
        candidate
    } else {
        manifest_dir
    };
    let rshtml_test_dir = workspace_path.join("rshtml_test");
    let views_dir = rshtml_test_dir.join("views");

    let template_file = views_dir.join("index.rs.html");
    let template_content = fs::read_to_string(&template_file).expect("Failed to read template");
    let context_file = rshtml_test_dir.join("src/main.rs");

    let mut ra = RustAnalyzerProcess::spawn(None)
        .await
        .expect("Failed to spawn RA");

    ra.initialize(workspace_path.to_str().unwrap())
        .await
        .expect("Failed to initialize RA");

    let vfm = VirtualFileManager::new();
    let template_uri = tower_lsp::lsp_types::Url::from_file_path(&template_file).unwrap();
    let vdoc = vfm
        .sync_template(
            &template_uri,
            &template_content,
            None,
            &rshtml_test_dir,
            Some(&ra),
        )
        .await
        .expect("Failed to sync template");

    let (parent_ver, augmented_parent) = {
        let parents = vfm.opened_parents.read().await;
        let parent_key = tower_lsp::lsp_types::Url::from_file_path(&context_file)
            .unwrap()
            .to_string();
        parents.get(&parent_key).cloned().unwrap()
    };

    println!("Augmented parent content:\\n{}", augmented_parent);
    assert!(augmented_parent.contains("pub mod __rshtml_virtual_index;"));

    // Template Pos: @self.year (let's say line 4, col 30, right after `self.`)
    let template_pos = Position::new(3, 30);
    // ...
    // Note: this test simulates typing to see diagnostics and completions.
    let _ = ra.shutdown().await;
}

#[tokio::test]
async fn test_in_memory_virtual_file_updates_without_disk_io() {
    let (ra, _views_dir, template_path, _main_file, vdoc) =
        setup_integration_test_environment("index.rs.html").await;

    let original_disk_content = fs::read_to_string(&vdoc.virtual_file_path).unwrap();
    assert!(
        original_disk_content.contains("stub anchor for VFS"),
        "The disk file MUST remain a tiny stub!"
    );

    let vfm = VirtualFileManager::new();
    let new_template_content = r#"
<div>
    <h1>@self.title -> IN MEMORY</h1>
</div>
"#;

    // ...
    let _ = ra.shutdown().await;
}

#[tokio::test]
async fn test_empty_function_body_keeps_context() {
    let (ra, views_dir, template_path, _main_file, vdoc) =
        setup_integration_test_environment("index.rs.html").await;

    let template_source = "<div>\n    @self.\n</div>";
    let _ = fs::write(&template_path, template_source);

    // ...
    let _ = ra.shutdown().await;
    let _ = fs::remove_file(template_path);
}
