macro_rules! log_debug {
    ($cat:expr, $($arg:tt)+) => {
        eprintln!("[DEBUG] [{}] {}", $cat, format!($($arg)+));
    };
}
macro_rules! log_info {
    ($cat:expr, $($arg:tt)+) => {
        eprintln!("[INFO] [{}] {}", $cat, format!($($arg)+));
    };
}
macro_rules! log_error {
    ($cat:expr, $($arg:tt)+) => {
        eprintln!("[ERROR] [{}] {}", $cat, format!($($arg)+));
    };
}

use serde_json::Value;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, RwLock, oneshot};
use tower_lsp::lsp_types::{
    CompletionItem, GotoDefinitionResponse, Hover, Position, ServerCapabilities, Url,
};

/// Manages a background `rust-analyzer` downstream process
/// and provides typed JSON-RPC communication via standard I/O pipes.
pub struct RustAnalyzerProcess {
    stdin: Arc<Mutex<ChildStdin>>,
    pending_requests: Arc<Mutex<std::collections::HashMap<i64, oneshot::Sender<Value>>>>,
    next_request_id: AtomicI64,
    pub is_ready: Arc<AtomicBool>,
    pub server_capabilities: Arc<RwLock<Option<ServerCapabilities>>>,
    _child: Arc<Mutex<Child>>,
}

impl RustAnalyzerProcess {
    /// Spawns the `rust-analyzer` downstream process using the system path or `rustup which rust-analyzer`.
    pub async fn spawn(workspace_root: Option<&str>) -> Result<Self, String> {
        let ra_path = Self::find_rust_analyzer_binary()?;

        let mut cmd = Command::new(ra_path);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn rust-analyzer: {e}"))?;

        let stdin = child
            .stdin
            .take()
            .ok_or("Failed to open stdin for rust-analyzer")?;
        let stdout = child
            .stdout
            .take()
            .ok_or("Failed to open stdout for rust-analyzer")?;

        let pending_requests = Arc::new(Mutex::new(std::collections::HashMap::<
            i64,
            oneshot::Sender<Value>,
        >::new()));
        let pending_clone = Arc::clone(&pending_requests);
        let is_ready = Arc::new(AtomicBool::new(false));
        let ready_clone = Arc::clone(&is_ready);

        // Spawn background task to read JSON-RPC responses from rust-analyzer stdout
        tokio::spawn(async move {
            Self::stdout_reader_loop(stdout, pending_clone, ready_clone).await;
        });

        let process = Self {
            stdin: Arc::new(Mutex::new(stdin)),
            pending_requests,
            next_request_id: AtomicI64::new(1000),
            is_ready,
            server_capabilities: Arc::new(RwLock::new(None)),
            _child: Arc::new(Mutex::new(child)),
        };

        if let Some(root) = workspace_root {
            process.initialize(root).await?;
        }

        Ok(process)
    }

    /// Finds the rust-analyzer binary in system PATH or via rustup.
    fn find_rust_analyzer_binary() -> Result<String, String> {
        if let Ok(output) = std::process::Command::new("rustup")
            .args(["which", "rust-analyzer"])
            .output()
        {
            if output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !path.is_empty() {
                    return Ok(path);
                }
            }
        }

        // Fallback to "rust-analyzer" directly from PATH
        Ok("rust-analyzer".to_string())
    }

    /// Background loop reading Content-Length framed JSON-RPC packets from rust-analyzer.
    async fn stdout_reader_loop(
        stdout: ChildStdout,
        pending: Arc<Mutex<std::collections::HashMap<i64, oneshot::Sender<Value>>>>,
        is_ready: Arc<AtomicBool>,
    ) {
        let mut reader = BufReader::new(stdout);
        let mut header_line = String::new();

        loop {
            header_line.clear();
            let mut content_length: Option<usize> = None;

            loop {
                header_line.clear();
                match reader.read_line(&mut header_line).await {
                    Ok(0) => return, // EOF, process exited
                    Ok(_) => {
                        let trimmed = header_line.trim();
                        if trimmed.is_empty() {
                            break; // End of HTTP-style headers
                        }
                        if trimmed.to_lowercase().starts_with("content-length:") {
                            if let Some(val) = trimmed.split(':').nth(1) {
                                content_length = val.trim().parse::<usize>().ok();
                            }
                        }
                    }
                    Err(_) => return,
                }
            }

            if let Some(length) = content_length {
                let mut body_buf = vec![0u8; length];
                if reader.read_exact(&mut body_buf).await.is_ok() {
                    if let Ok(msg) = serde_json::from_slice::<Value>(&body_buf) {
                        // Monitor RA server status and progress
                        if let Some(method) = msg.get("method").and_then(|m| m.as_str()) {
                            let params = msg.get("params");
                            if method == "experimental/serverStatus" {
                                if let Some(quiescent) = params
                                    .and_then(|p| p.get("quiescent"))
                                    .and_then(|q| q.as_bool())
                                {
                                    is_ready.store(quiescent, Ordering::SeqCst);
                                    let status_text = if quiescent {
                                        "rust-analyzer is ready (workspace indexed)"
                                    } else {
                                        "rust-analyzer is loading workspace..."
                                    };
                                    log_info!("RA_STATUS", "{status_text}");
                                }
                            } else if method == "$/progress" {
                                let val = params.and_then(|p| p.get("value"));
                                let title = val
                                    .and_then(|v| v.get("title"))
                                    .and_then(|t| t.as_str())
                                    .unwrap_or("");
                                let message = val
                                    .and_then(|v| v.get("message"))
                                    .and_then(|m| m.as_str())
                                    .unwrap_or("");
                                let fraction = val
                                    .and_then(|v| v.get("percentage"))
                                    .and_then(|p| p.as_u64());

                                let progress_str = match fraction {
                                    Some(pct) => {
                                        format!("[rust-analyzer] {} ({}%) {}", title, pct, message)
                                    }
                                    None => format!("[rust-analyzer] {} {}", title, message),
                                };

                                if !title.is_empty() || !message.is_empty() {
                                    log_debug!("RA_PROGRESS", "{}", progress_str.trim());
                                }
                            }
                        }

                        // Dispatch response to awaiting oneshot channel
                        if let Some(id_val) = msg.get("id").and_then(|i| i.as_i64()) {
                            let mut map = pending.lock().await;
                            if let Some(sender) = map.remove(&id_val) {
                                let _ = sender.send(msg);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Sends a JSON-RPC request to `rust-analyzer` and awaits the matching response.
    pub async fn send_request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let (tx, rx) = oneshot::channel();
        {
            let mut map = self.pending_requests.lock().await;
            map.insert(id, tx);
        }

        self.send_raw_json(&req).await?;

        rx.await
            .map_err(|_| "Response channel canceled".to_string())
    }

    /// Sends a JSON-RPC notification (no response expected).
    pub async fn send_notification(&self, method: &str, params: Value) -> Result<(), String> {
        let notif = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.send_raw_json(&notif).await
    }

    /// Serializes and writes a framed JSON-RPC message into rust-analyzer stdin.
    pub async fn send_raw_json(&self, val: &Value) -> Result<(), String> {
        let body = serde_json::to_string(val).map_err(|e| e.to_string())?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());

        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(header.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        stdin
            .write_all(body.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        stdin.flush().await.map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Performs standard LSP initialize handshake with rust-analyzer.
    pub async fn initialize(&self, workspace_root: &str) -> Result<Value, String> {
        let root_uri = if workspace_root.starts_with("file://") {
            workspace_root.to_string()
        } else {
            format!("file://{}", workspace_root)
        };

        let init_params = serde_json::json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {
                "workspace": {
                    "workspaceFolders": true
                },
                "textDocument": {
                    "completion": {
                        "completionItem": {
                            "snippetSupport": true
                        }
                    },
                    "hover": {
                        "contentFormat": ["markdown", "plaintext"]
                    },
                    "definition": {
                        "linkSupport": true
                    }
                },
                "window": {
                    "workDoneProgress": true
                }
            }
        });

        let init_res = self.send_request("initialize", init_params).await?;
        self.send_notification("initialized", serde_json::json!({}))
            .await?;

        if let Some(caps_val) = init_res.pointer("/result/capabilities") {
            match serde_json::from_value::<ServerCapabilities>(caps_val.clone()) {
                Ok(caps) => {
                    log_info!(
                        "RA_INIT",
                        "Parsed downstream rust-analyzer server capabilities successfully"
                    );
                    let mut lock = self.server_capabilities.write().await;
                    *lock = Some(caps);
                }
                Err(e) => {
                    log_error!(
                        "RA_INIT",
                        "Failed to deserialize RA server capabilities: {}",
                        e
                    );
                }
            }
        }

        Ok(init_res)
    }

    /// High-level typed method to notify RA of an opened document (`textDocument/didOpen`).
    pub async fn did_open(
        &self,
        uri: &Url,
        version: i32,
        text: &str,
        language_id: Option<&str>,
    ) -> Result<(), String> {
        let params = serde_json::json!({
            "textDocument": {
                "uri": uri.to_string(),
                "languageId": language_id.unwrap_or("rust"),
                "version": version,
                "text": text
            }
        });
        self.send_notification("textDocument/didOpen", params).await
    }

    /// High-level typed method to notify RA of changes in a document (`textDocument/didChange`).
    pub async fn did_change(&self, uri: &Url, version: i32, text: &str) -> Result<(), String> {
        let params = serde_json::json!({
            "textDocument": {
                "uri": uri.to_string(),
                "version": version
            },
            "contentChanges": [
                {
                    "text": text
                }
            ]
        });
        self.send_notification("textDocument/didChange", params)
            .await
    }

    /// High-level typed method to query completions from RA (`textDocument/completion`).
    pub async fn completion(
        &self,
        uri: &Url,
        position: Position,
        context: Option<tower_lsp::lsp_types::CompletionContext>,
    ) -> Result<Vec<CompletionItem>, String> {
        let params = serde_json::json!({
            "textDocument": { "uri": uri.to_string() },
            "position": { "line": position.line, "character": position.character },
            "context": context
        });

        let res = self.send_request("textDocument/completion", params).await?;
        if let Some(items_val) = res.pointer("/result/items") {
            if items_val.is_null() {
                return Ok(Vec::new());
            }
            serde_json::from_value::<Vec<CompletionItem>>(items_val.clone())
                .map_err(|e| format!("Failed to deserialize RA completions: {e}"))
        } else if let Some(res_val) = res.pointer("/result") {
            if res_val.is_null() {
                return Ok(Vec::new());
            }
            serde_json::from_value::<Vec<CompletionItem>>(res_val.clone())
                .map_err(|e| format!("Failed to deserialize RA completions: {e}"))
        } else {
            Ok(Vec::new())
        }
    }

    /// High-level typed method to query hover information from RA (`textDocument/hover`).
    pub async fn hover(&self, uri: &Url, position: Position) -> Result<Option<Hover>, String> {
        let params = serde_json::json!({
            "textDocument": { "uri": uri.to_string() },
            "position": { "line": position.line, "character": position.character }
        });

        let res = self.send_request("textDocument/hover", params).await?;
        if let Some(hover_val) = res.get("result") {
            if hover_val.is_null() {
                return Ok(None);
            }
            serde_json::from_value::<Hover>(hover_val.clone())
                .map(Some)
                .map_err(|e| format!("Failed to deserialize RA hover: {e}"))
        } else {
            Ok(None)
        }
    }

    /// High-level typed method to query definition from RA (`textDocument/definition`).
    pub async fn definition(
        &self,
        uri: &Url,
        position: Position,
    ) -> Result<Option<GotoDefinitionResponse>, String> {
        let params = serde_json::json!({
            "textDocument": { "uri": uri.to_string() },
            "position": { "line": position.line, "character": position.character }
        });

        let res = self.send_request("textDocument/definition", params).await?;
        if let Some(def_val) = res.get("result") {
            if def_val.is_null() {
                return Ok(None);
            }
            serde_json::from_value::<GotoDefinitionResponse>(def_val.clone())
                .map(Some)
                .map_err(|e| format!("Failed to deserialize RA definition: {e}"))
        } else {
            Ok(None)
        }
    }

    /// Waits until rust-analyzer is quiescent (workspace fully loaded & indexed).
    pub async fn wait_for_ready(&self, timeout: std::time::Duration) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            if self.is_ready.load(Ordering::SeqCst) {
                return true;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
        false
    }

    /// Returns the cached ServerCapabilities from downstream rust-analyzer, if available.
    pub async fn server_capabilities(&self) -> Option<ServerCapabilities> {
        let lock = self.server_capabilities.read().await;
        lock.clone()
    }

    /// Gracefully shuts down the downstream process.
    pub async fn shutdown(&self) -> Result<(), String> {
        let _ = self.send_request("shutdown", serde_json::Value::Null).await;
        let _ = self
            .send_notification("exit", serde_json::Value::Null)
            .await;
        Ok(())
    }
}

/// Forwards legacy calls to structured log
pub fn append_log(msg: &str) {
    log_info!("LEGACY", "{}", msg);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_solo_inspect_ra_text_edit() {
        let manifest_dir =
            std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string()));
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
        let context_file = rshtml_test_dir.join("src/main.rs");

        let ra = RustAnalyzerProcess::spawn(None).await.unwrap();
        ra.initialize(workspace_path.to_str().unwrap()).await.unwrap();

        let virtual_file_path = views_dir.join(".build/__rshtml_virtual_text_edit_test.rs");
        let virtual_uri = Url::from_file_path(&virtual_file_path).unwrap();

        let orig_parent = std::fs::read_to_string(&context_file).unwrap();
        let mod_link = format!(
            "
#[path = {:?}]
#[allow(dead_code, unused_imports)]
pub mod __rshtml_virtual_text_edit_test;
",
            virtual_file_path.to_string_lossy()
        );
        let parent_uri = Url::from_file_path(&context_file).unwrap();
        ra.did_open(&parent_uri, 1, &format!("{}{}", orig_parent, mod_link), None).await.unwrap();

        let _ = std::fs::create_dir_all(views_dir.join(".build"));
        let _ = std::fs::write(&virtual_file_path, "// stub
");

        let virtual_code = r#"// @generated by rshtml-analyzer
#![allow(unused_imports, dead_code, unused_variables, path_statements)]
use chrono::{DateTime, Datelike, Utc};
use crate::IndexPage;

impl IndexPage {
    pub fn __rshtml_virtual_context(&self) {
        self.
    }
}
"#;

        ra.did_open(&virtual_uri, 1, virtual_code, None).await.unwrap();

        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_secs(20) {
            let items = ra.completion(&virtual_uri, Position::new(7, 13), None).await.unwrap_or_default();
            for it in &items {
                if it.label == "home_time" || it.label == "footer" {
                    println!("INSPECT ITEM: label={}, text_edit={:?}, insert_text={:?}", it.label, it.text_edit, it.insert_text);
                    let _ = ra.shutdown().await;
                    let _ = std::fs::remove_file(virtual_file_path);
                    return;
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        }

        panic!("Timeout waiting for items");
    }


    #[tokio::test]
    async fn test_solo_self_dot_completion() {
        let manifest_dir =
            std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string()));
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
        let context_file = rshtml_test_dir.join("src/main.rs");

        let ra = RustAnalyzerProcess::spawn(None).await.unwrap();
        ra.initialize(workspace_path.to_str().unwrap()).await.unwrap();

        // 1. Link a virtual test module in parent
        let virtual_file_path = views_dir.join(".build/__rshtml_virtual_solo_test.rs");
        let virtual_uri = Url::from_file_path(&virtual_file_path).unwrap();

        let orig_parent = std::fs::read_to_string(&context_file).unwrap();
        let mod_link = format!(
            "
#[path = {:?}]
#[allow(dead_code, unused_imports)]
pub mod __rshtml_virtual_solo_test;
",
            virtual_file_path.to_string_lossy()
        );
        let augmented_parent = format!("{}{}", orig_parent, mod_link);
        let parent_uri = Url::from_file_path(&context_file).unwrap();

        ra.did_open(&parent_uri, 1, &augmented_parent, None).await.unwrap();

        // 2. Anchor stub on disk
        let _ = std::fs::create_dir_all(views_dir.join(".build"));
        let _ = std::fs::write(&virtual_file_path, "// @generated stub
");

        // 3. Virtual code with exact 
        let virtual_code = r#"// @generated by rshtml-analyzer
#![allow(unused_imports, dead_code, unused_variables, path_statements)]
use chrono::{DateTime, Datelike, Utc};
use crate::IndexPage;

impl IndexPage {
    pub fn __rshtml_virtual_context(&self) {
        self.
    }
}
"#;

        ra.did_open(&virtual_uri, 1, virtual_code, None).await.unwrap();

        // Target position right after  (line 7, col 13)
        let target_line = 7;
        let target_col = 13;

        let start = std::time::Instant::now();
        let mut found_fields = Vec::new();

        while start.elapsed() < std::time::Duration::from_secs(20) {
            let items = ra.completion(&virtual_uri, Position::new(target_line, target_col), None).await.unwrap_or_default();
            if !items.is_empty() {
                for it in &items {
                    if it.label == "home_time" || it.label == "footer" {
                        found_fields.push(it.label.clone());
                    }
                }
                if !found_fields.is_empty() {
                    break;
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        }

        println!("SOLO TEST FOUND FIELDS: {:?}", found_fields);
        assert!(!found_fields.is_empty(), "RA must return fields of IndexPage (home_time / footer) when cursor is at self.");

        let _ = ra.shutdown().await;
        let _ = std::fs::remove_file(virtual_file_path);
    }


    #[tokio::test]
    async fn test_spawn_and_handshake() {
        let process = RustAnalyzerProcess::spawn(None).await;
        assert!(
            process.is_ok(),
            "Failed to spawn rust-analyzer: {:?}",
            process.err()
        );

        let ra = process.unwrap();
        let res = ra.initialize("/tmp").await;
        assert!(
            res.is_ok(),
            "Failed to initialize rust-analyzer: {:?}",
            res.err()
        );

        let val = res.unwrap();
        let server_name = val
            .pointer("/result/serverInfo/name")
            .and_then(|v| v.as_str());
        assert_eq!(server_name, Some("rust-analyzer"));

        let caps = ra.server_capabilities().await;
        assert!(
            caps.is_some(),
            "Downstream RA server capabilities should be parsed"
        );

        let _ = ra.shutdown().await;
    }

    #[tokio::test]
    async fn test_did_open_and_completion_routing() {
        let process = RustAnalyzerProcess::spawn(None).await;
        assert!(process.is_ok());
        let ra = process.unwrap();

        let temp_dir = std::env::temp_dir().join(format!("ra_proxy_test_{}", std::process::id()));
        let src_dir = temp_dir.join("src");
        let _ = std::fs::create_dir_all(&src_dir);
        let cargo_toml_content =
            "[package]\nname = \"dummy\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
        let _ = std::fs::write(temp_dir.join("Cargo.toml"), cargo_toml_content);

        let main_file = src_dir.join("main.rs");
        let content = "fn main() { let x = 42; }";
        let _ = std::fs::write(&main_file, content);

        let ws_path = temp_dir.to_str().unwrap();
        let _ = ra.initialize(ws_path).await;

        let main_uri = Url::from_file_path(&main_file).unwrap();
        assert!(ra.did_open(&main_uri, 1, content, None).await.is_ok());

        let comp_items = ra.completion(&main_uri, Position::new(0, 18), None).await;
        assert!(
            comp_items.is_ok(),
            "RA should respond to completion request: {:?}",
            comp_items.err()
        );

        let _ = ra.shutdown().await;
        let _ = std::fs::remove_dir_all(temp_dir);
    }
}
