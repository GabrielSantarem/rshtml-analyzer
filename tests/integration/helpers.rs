use rshtml_analyzer::ra_proxy::RustAnalyzerProcess;
use rshtml_analyzer::virtual_file::{VirtualDocument, VirtualFileManager};
use std::env;
use std::fs;
use std::path::PathBuf;

pub async fn setup_integration_test_environment(
    template_name: &str,
) -> (
    RustAnalyzerProcess,
    PathBuf,
    PathBuf,
    PathBuf,
    VirtualDocument,
) {
    let manifest_dir =
        std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string()));
    let candidate = manifest_dir.join("../rshtml");
    let workspace_path = if candidate.join("rshtml_test").exists() {
        candidate
    } else {
        manifest_dir
    };
    let rshtml_test_dir = workspace_path.join("rshtml_test");
    let views_dir = rshtml_test_dir.join("views");

    let template_file = views_dir.join(template_name);
    let template_content = fs::read_to_string(&template_file).unwrap_or_default();
    let main_file = rshtml_test_dir.join("src/main.rs");

    let ra = RustAnalyzerProcess::spawn(None)
        .await
        .unwrap();

    ra.initialize(workspace_path.to_str().unwrap())
        .await
        .unwrap();

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
        .unwrap();

    (ra, views_dir, template_file, main_file, vdoc)
}
