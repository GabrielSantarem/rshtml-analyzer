use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{oneshot, Mutex};
use serde_json::Value;

/// Manages a background `rust-analyzer` downstream process
/// and provides JSON-RPC communication via standard I/O pipes.
pub struct RustAnalyzerProcess {
    stdin: Arc<Mutex<ChildStdin>>,
    pending_requests: Arc<Mutex<std::collections::HashMap<i64, oneshot::Sender<Value>>>>,
    next_request_id: AtomicI64,
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

        let mut child = cmd.spawn().map_err(|e| format!("Failed to spawn rust-analyzer: {e}"))?;

        let stdin = child.stdin.take().ok_or("Failed to open stdin for rust-analyzer")?;
        let stdout = child.stdout.take().ok_or("Failed to open stdout for rust-analyzer")?;

        let pending_requests = Arc::new(Mutex::new(std::collections::HashMap::<i64, oneshot::Sender<Value>>::new()));
        let pending_clone = Arc::clone(&pending_requests);

        // Spawn background task to read JSON-RPC responses from rust-analyzer stdout
        tokio::spawn(async move {
            Self::stdout_reader_loop(stdout, pending_clone).await;
        });

        let process = Self {
            stdin: Arc::new(Mutex::new(stdin)),
            pending_requests,
            next_request_id: AtomicI64::new(1000),
            _child: Arc::new(Mutex::new(child)),
        };

        if let Some(root) = workspace_root {
            process.initialize(root).await?;
        }

        Ok(process)
    }

    /// Finds the rust-analyzer binary in system PATH or via rustup.
    fn find_rust_analyzer_binary() -> Result<String, String> {
        if let Ok(output) = std::process::Command::new("rustup").args(["which", "rust-analyzer"]).output() {
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

        rx.await.map_err(|_| "Response channel canceled".to_string())
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
        stdin.write_all(header.as_bytes()).await.map_err(|e| e.to_string())?;
        stdin.write_all(body.as_bytes()).await.map_err(|e| e.to_string())?;
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
                "textDocument": {
                    "completion": {
                        "completionItem": {
                            "snippetSupport": true
                        }
                    },
                    "hover": {
                        "contentFormat": ["markdown", "plaintext"]
                    }
                }
            }
        });

        let init_res = self.send_request("initialize", init_params).await?;
        self.send_notification("initialized", serde_json::json!({})).await?;
        Ok(init_res)
    }

    /// Gracefully shuts down the downstream process.
    pub async fn shutdown(&self) -> Result<(), String> {
        let _ = self.send_request("shutdown", serde_json::Value::Null).await;
        let _ = self.send_notification("exit", serde_json::Value::Null).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_spawn_and_handshake() {
        let process = RustAnalyzerProcess::spawn(None).await;
        assert!(process.is_ok(), "Failed to spawn rust-analyzer: {:?}", process.err());

        let ra = process.unwrap();
        let res = ra.initialize("/tmp").await;
        assert!(res.is_ok(), "Failed to initialize rust-analyzer: {:?}", res.err());

        let val = res.unwrap();
        let server_name = val.pointer("/result/serverInfo/name").and_then(|v| v.as_str());
        assert_eq!(server_name, Some("rust-analyzer"));

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
        let cargo_toml_content = "[package]\nname = \"dummy\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
        let _ = std::fs::write(temp_dir.join("Cargo.toml"), cargo_toml_content);

        let main_file = src_dir.join("main.rs");
        let content = "fn main() { let x = 42; }";
        let _ = std::fs::write(&main_file, content);

        let ws_path = temp_dir.to_str().unwrap();
        let _ = ra.initialize(ws_path).await;

        let main_uri = format!("file://{}", main_file.to_str().unwrap());
        let open_params = serde_json::json!({
            "textDocument": {
                "uri": main_uri,
                "languageId": "rust",
                "version": 1,
                "text": content
            }
        });
        assert!(ra.send_notification("textDocument/didOpen", open_params).await.is_ok());

        let comp_params = serde_json::json!({
            "textDocument": {
                "uri": main_uri
            },
            "position": {
                "line": 0,
                "character": 18
            }
        });

        let comp_res = ra.send_request("textDocument/completion", comp_params).await;
        assert!(comp_res.is_ok(), "RA should respond to completion request: {:?}", comp_res.err());

        let _ = ra.shutdown().await;
        let _ = std::fs::remove_dir_all(temp_dir);
    }
}
