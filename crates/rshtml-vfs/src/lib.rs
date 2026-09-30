#[allow(unused_macros)]
macro_rules! log_info {
    ($cat:expr, $($arg:tt)+) => {
        eprintln!("[INFO] [{}] {}", $cat, format!($($arg)+));
    };
}
#[allow(unused_macros)]
macro_rules! log_error {
    ($cat:expr, $($arg:tt)+) => {
        eprintln!("[ERROR] [{}] {}", $cat, format!($($arg)+));
    };
}
use rshtml_ra_proxy::RustAnalyzerProcess;
use rshtml_signature::{ResolvedViewContext, SignatureResolver};
use rshtml_sourcemap::SourceMap;
use rshtml_transpiler::TemplateTranspiler;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_lsp::lsp_types::Url;
use tree_sitter::Tree;

/// Represents an active virtual file synchronized with downstream `rust-analyzer`.
#[derive(Debug, Clone)]
pub struct VirtualDocument {
    /// URI of the original `.rs.html` template.
    pub template_uri: Url,
    /// Path of the virtual `.rs` file inside the crate directory.
    pub virtual_file_path: PathBuf,
    /// URI of the virtual `.rs` file sent to rust-analyzer.
    pub virtual_uri: Url,
    /// Current version for LSP textDocument synchronization.
    pub version: i32,
    /// Bidirectional coordinate source map (with 1:1 column mapping).
    pub source_map: SourceMap,
    /// Generated virtual Rust source code.
    pub virtual_code: String,
    /// Resolved struct metadata backing this template.
    pub resolved_context: ResolvedViewContext,
}

/// Manages virtual documents and module linkage for all open `.rs.html` templates.
#[derive(Debug, Default)]
pub struct VirtualFileManager {
    documents: Arc<RwLock<HashMap<String, VirtualDocument>>>,
    /// Tracks in-memory parent files opened in downstream RA: (version, current_content)
    pub opened_parents: Arc<RwLock<HashMap<String, (i32, String)>>>,
}

impl VirtualFileManager {
    pub fn new() -> Self {
        Self {
            documents: Arc::new(RwLock::new(HashMap::new())),
            opened_parents: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Creates or updates a virtual document for a given template file and synchronizes with downstream RA.
    pub async fn sync_template(
        &self,
        template_uri: &Url,
        template_text: &str,
        tree_opt: Option<&Tree>,
        crate_root: &Path,
        ra_process: Option<&RustAnalyzerProcess>,
    ) -> Result<VirtualDocument, String> {
        let file_path = template_uri
            .to_file_path()
            .map_err(|_| "Invalid template URI".to_string())?;

        let rel_path = file_path.strip_prefix(crate_root).map_err(|_| {
            format!(
                "Template path {:?} is not within crate root {:?}",
                file_path, crate_root
            )
        })?;

        // 1. Resolve struct metadata and location
        let resolved =
            SignatureResolver::resolve(crate_root, rel_path.to_str().unwrap_or_default())
                .ok_or_else(|| format!("Signature resolution failed for {:?}", rel_path))?;

        // Ensure tree is parsed if not passed
        let mut parsed_tree = None;
        let tree_ref = if let Some(tree) = tree_opt {
            Some(tree)
        } else {
            let mut parser = tree_sitter::Parser::new();
            let lang: tree_sitter::Language = tree_sitter_rshtml::LANGUAGE.into();
            if parser.set_language(&lang).is_ok() {
                parsed_tree = parser.parse(template_text, None);
            }
            parsed_tree.as_ref()
        };

        // 2. Transpile template AST into valid Rust code via rshtml_core
        let views_dir = crate_root.join("views");
        let transpile_res = TemplateTranspiler::transpile_file_with_tree(
            &file_path,
            &views_dir,
            &resolved,
            tree_ref,
            Some(template_text),
        )
        .map_err(|e| format!("Transpilation error: {e}"))?;

        // Build source map with high-precision spans from tree-sitter AST
        let source_map = if let Some(tree) = tree_ref {
            SourceMap::build_from_tree(tree, template_text, transpile_res.header_lines_count, 0)
        } else {
            SourceMap::new(transpile_res.header_lines_count, 0)
        };

        // 3. Determine virtual file path and URI inside the parent's directory
        let clean_stem = file_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("template")
            .trim_end_matches(".rs");

        let build_dir = views_dir.join(".build");
        let virtual_filename = format!("__rshtml_virtual_{}.rs", clean_stem);
        let virtual_file_path = build_dir.join(&virtual_filename);
        let virtual_uri = Url::from_file_path(&virtual_file_path)
            .map_err(|_| "Failed to build virtual file URI".to_string())?;

        // 4. Update in-memory document state
        let (new_version, is_first_open) = {
            let mut docs = self.documents.write().await;
            let entry = docs.entry(template_uri.to_string());
            match entry {
                std::collections::hash_map::Entry::Occupied(mut occ) => {
                    let next_ver = occ.get().version + 1;
                    let doc = VirtualDocument {
                        template_uri: template_uri.clone(),
                        virtual_file_path: virtual_file_path.clone(),
                        virtual_uri: virtual_uri.clone(),
                        version: next_ver,
                        source_map,
                        virtual_code: transpile_res.virtual_code.clone(),
                        resolved_context: resolved.clone(),
                    };
                    occ.insert(doc);
                    (next_ver, false)
                }
                std::collections::hash_map::Entry::Vacant(vac) => {
                    let doc = VirtualDocument {
                        template_uri: template_uri.clone(),
                        virtual_file_path: virtual_file_path.clone(),
                        virtual_uri: virtual_uri.clone(),
                        version: 1,
                        source_map,
                        virtual_code: transpile_res.virtual_code.clone(),
                        resolved_context: resolved.clone(),
                    };
                    vac.insert(doc);
                    (1, true)
                }
            }
        };

        let doc = self.get_by_template_uri(template_uri).await.unwrap();

        // 5. Notify downstream rust-analyzer via LSP stdio
        if let Some(ra) = ra_process {
            // Ensure views/.build directory exists and has a .gitignore
            if let Some(parent) = virtual_file_path.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
                let gitignore = parent.join(".gitignore");
                if !gitignore.exists() {
                    let _ = tokio::fs::write(&gitignore, "*\n!.gitignore\n").await;
                }
            }
            // Ensure virtual file exists on disk ONCE as a minimal VFS anchor stub.
            // All subsequent updates are sent purely in-memory via textDocument/didChange!
            if !virtual_file_path.exists() {
                let _ = tokio::fs::write(
                    &virtual_file_path,
                    "// @generated by rshtml-analyzer (stub anchor for VFS)\n",
                )
                .await;
            }

            // Ensure parent module file references this virtual submodule in memory
            self.ensure_parent_module_linkage(
                ra,
                &resolved.rust_file_path,
                &virtual_file_path,
                &clean_stem,
            )
            .await;

            if is_first_open {
                let open_params = serde_json::json!({
                    "textDocument": {
                        "uri": virtual_uri.to_string(),
                        "languageId": "rust",
                        "version": new_version,
                        "text": transpile_res.virtual_code
                    }
                });
                let _ = ra
                    .send_notification("textDocument/didOpen", open_params)
                    .await;
            } else {
                let change_params = serde_json::json!({
                    "textDocument": {
                        "uri": virtual_uri.to_string(),
                        "version": new_version
                    },
                    "contentChanges": [
                        { "text": transpile_res.virtual_code }
                    ]
                });
                let _ = ra
                    .send_notification("textDocument/didChange", change_params)
                    .await;
            }
        }

        Ok(doc)
    }

    /// Informs downstream rust-analyzer about the module linkage so the virtual file is part of the CrateGraph.
    async fn ensure_parent_module_linkage(
        &self,
        ra: &RustAnalyzerProcess,
        parent_file_path: &Path,
        virtual_file_path: &Path,
        mod_name: &str,
    ) {
        let parent_uri = match Url::from_file_path(parent_file_path) {
            Ok(u) => u,
            Err(_) => return,
        };

        let linkage = format!(
            "\n#[path = {:?}]\n#[allow(dead_code, unused_imports)]\npub mod __rshtml_virtual_{};\n",
            virtual_file_path.to_string_lossy(),
            mod_name
        );

        let mut parents = self.opened_parents.write().await;
        let parent_key = parent_uri.to_string();

        if let Some((ver, content)) = parents.get_mut(&parent_key) {
            if !content.contains(&format!("pub mod __rshtml_virtual_{};", mod_name)) {
                content.push_str(&linkage);
                *ver += 1;
                let change_params = serde_json::json!({
                    "textDocument": {
                        "uri": parent_uri.to_string(),
                        "version": *ver
                    },
                    "contentChanges": [
                        { "text": content }
                    ]
                });
                let _ = ra
                    .send_notification("textDocument/didChange", change_params)
                    .await;
            }
        } else {
            // First time this parent is encountered: read from disk, append linkage, and send didOpen
            if let Ok(orig_content) = tokio::fs::read_to_string(parent_file_path).await {
                let augmented = format!("{}{}", orig_content, linkage);
                parents.insert(parent_key, (1, augmented.clone()));

                let open_params = serde_json::json!({
                    "textDocument": {
                        "uri": parent_uri.to_string(),
                        "languageId": "rust",
                        "version": 1,
                        "text": augmented
                    }
                });
                let _ = ra
                    .send_notification("textDocument/didOpen", open_params)
                    .await;
            }
        }
    }

    pub async fn get_by_template_uri(&self, template_uri: &Url) -> Option<VirtualDocument> {
        let docs = self.documents.read().await;
        docs.get(template_uri.as_str()).cloned()
    }

    /// Retrieves an active virtual document by its virtual URI or virtual file path.
    pub async fn get_by_virtual_uri(&self, virtual_uri: &Url) -> Option<VirtualDocument> {
        let docs = self.documents.read().await;
        if let Some(doc) = docs.values().find(|doc| &doc.virtual_uri == virtual_uri) {
            return Some(doc.clone());
        }
        if let Ok(target_path) = virtual_uri.to_file_path() {
            if let Some(doc) = docs.values().find(|doc| {
                doc.virtual_file_path == target_path
                    || (doc.virtual_file_path.file_name().is_some()
                        && doc.virtual_file_path.file_name() == target_path.file_name())
                    || (std::fs::canonicalize(&doc.virtual_file_path).is_ok()
                        && std::fs::canonicalize(&doc.virtual_file_path).ok()
                            == std::fs::canonicalize(&target_path).ok())
            }) {
                return Some(doc.clone());
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_virtual_file_manager_creation() {
        let mgr = VirtualFileManager::new();
        assert!(mgr.documents.try_read().is_ok());
    }

    #[tokio::test]
    async fn test_virtual_file_disk_stub_anchor() {
        let temp_dir = std::env::temp_dir().join("rshtml_vfs_test");
        let _ = tokio::fs::create_dir_all(&temp_dir).await;
        let test_file = temp_dir.join("__rshtml_virtual_test.rs");

        if !test_file.exists() {
            let _ = tokio::fs::write(&test_file, "// stub\n").await;
        }

        assert!(test_file.exists());
        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }
}
