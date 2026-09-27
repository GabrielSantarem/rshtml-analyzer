use crate::log_info;
use crate::ra_proxy::RustAnalyzerProcess;
use crate::signature::{ResolvedViewContext, SignatureResolver};
use crate::source_map::SourceMap;
use crate::transpiler::TemplateTranspiler;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_lsp::lsp_types::Url;

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
    opened_parents: Arc<RwLock<HashMap<String, (i32, String)>>>,
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
        _template_text: &str,
        crate_root: &Path,
        ra_process: Option<&RustAnalyzerProcess>,
    ) -> Result<VirtualDocument, String> {
        let file_path = template_uri
            .to_file_path()
            .map_err(|_| "Failed to convert URI to file path".to_string())?;

        let rel_path = file_path.strip_prefix(crate_root).map_err(|e| {
            format!(
                "Path {:?} not inside crate {:?}: {}",
                file_path, crate_root, e
            )
        })?;

        // 1. Resolve struct metadata and location
        let resolved =
            SignatureResolver::resolve(crate_root, rel_path.to_str().unwrap_or_default())
                .ok_or_else(|| format!("Signature resolution failed for {:?}", rel_path))?;

        // 2. Transpile template AST into valid Rust code via rshtml_core
        let views_dir = crate_root.join("views");
        let transpile_res = TemplateTranspiler::transpile_file(&file_path, &views_dir, &resolved)
            .map_err(|e| format!("Transpilation error: {e}"))?;

        let source_map = SourceMap::new(transpile_res.header_lines_count, 0);

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
                    let _ = tokio::fs::write(&gitignore, "*
!.gitignore
").await;
                }
            }
            // Write the virtual file to disk so rust-analyzer's module tree and Cargo graph find it
            let _ = tokio::fs::write(&virtual_file_path, &transpile_res.virtual_code).await;

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
        clean_stem: &str,
    ) {
        if let Ok(parent_uri) = Url::from_file_path(parent_file_path) {
            let parent_uri_str = parent_uri.to_string();
            let mod_name = format!("__rshtml_virtual_{}", clean_stem);

            let mut opened = self.opened_parents.write().await;
            if let Some((version, content)) = opened.get_mut(&parent_uri_str) {
                if !content.contains(&mod_name) {
                    let linkage_decl = format!(
                        "\n#[path = {:?}]\n#[allow(dead_code, unused_imports)]\npub mod {};\n",
                        virtual_file_path.to_string_lossy(),
                        mod_name
                    );
                    content.push_str(&linkage_decl);
                    *version += 1;

                    let change_params = serde_json::json!({
                        "textDocument": {
                            "uri": parent_uri_str,
                            "version": *version
                        },
                        "contentChanges": [
                            { "text": content.clone() }
                        ]
                    });
                    log_info!(
                        "VFS",
                        "Updated parent module linkage via didChange for {}",
                        parent_uri_str
                    );
                    let _ = ra
                        .send_notification("textDocument/didChange", change_params)
                        .await;
                }
            } else if let Ok(orig_parent_content) = std::fs::read_to_string(parent_file_path) {
                let linkage_decl = format!(
                    "\n#[path = {:?}]\n#[allow(dead_code, unused_imports)]\npub mod {};\n",
                    virtual_file_path.to_string_lossy(),
                    mod_name
                );
                let augmented_parent = format!("{}{}", orig_parent_content, linkage_decl);

                let open_params = serde_json::json!({
                    "textDocument": {
                        "uri": parent_uri_str,
                        "languageId": "rust",
                        "version": 1,
                        "text": augmented_parent
                    }
                });
                log_info!(
                    "VFS",
                    "Opened parent module with linkage via didOpen for {}",
                    parent_uri_str
                );
                let _ = ra
                    .send_notification("textDocument/didOpen", open_params)
                    .await;
                opened.insert(parent_uri_str, (1, augmented_parent));
            }
        }
    }

    /// Retrieves an active virtual document by template URI.
    pub async fn get_by_template_uri(&self, template_uri: &Url) -> Option<VirtualDocument> {
        let docs = self.documents.read().await;
        docs.get(&template_uri.to_string()).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_virtual_file_manager_sync() {
        let manifest_dir =
            PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string()));
        let workspace_path = manifest_dir.join("../rshtml");
        let rshtml_test_dir = workspace_path.join("rshtml_test");
        if !rshtml_test_dir.exists() {
            return;
        }

        let template_path = rshtml_test_dir.join("views/index.rs.html");
        let template_uri = Url::from_file_path(&template_path).unwrap();
        let content = std::fs::read_to_string(&template_path).unwrap();

        let vfm = VirtualFileManager::new();
        let doc = vfm
            .sync_template(&template_uri, &content, &rshtml_test_dir, None)
            .await
            .expect("Virtual file synchronization must succeed");

        assert_eq!(doc.resolved_context.struct_name, "IndexPage");
        assert!(doc.virtual_code.contains("impl IndexPage"));
    }
}
