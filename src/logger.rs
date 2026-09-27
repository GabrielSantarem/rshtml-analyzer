use std::sync::RwLock;
use tower_lsp::Client;
use tower_lsp::lsp_types::MessageType;

/// Structured log levels for rshtml-analyzer
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
        }
    }

    pub fn to_lsp_message_type(&self) -> MessageType {
        match self {
            LogLevel::Debug => MessageType::LOG,
            LogLevel::Info => MessageType::INFO,
            LogLevel::Warn => MessageType::WARNING,
            LogLevel::Error => MessageType::ERROR,
        }
    }
}

/// Global client holder so that background components (like RA proxy reader) can stream logs to Zed
static CLIENT_HOLDER: RwLock<Option<Client>> = RwLock::new(None);

pub fn set_lsp_client(client: Client) {
    let mut holder = CLIENT_HOLDER.write().unwrap();
    *holder = Some(client);
}

/// Dispatches a structured log:
/// 1. Sends to editor via `window/logMessage` (visible in Zed's "View Log" / LSP logs)
/// 2. Prints to stderr formatted (so CLI runs show clean traces)
/// 3. Writes timestamped entry to `/tmp/rshtml-analyzer.log` (for offline inspection)
pub fn log(level: LogLevel, category: &str, message: &str) {
    let formatted_msg = format!("[{}] [{}] {}", level.as_str(), category, message);

    // 1. Send to Zed / LSP client via window/logMessage
    if let Ok(guard) = CLIENT_HOLDER.read() {
        if let Some(client) = guard.as_ref() {
            let cl = client.clone();
            let msg_type = level.to_lsp_message_type();
            let text = formatted_msg.clone();
            tokio::spawn(async move {
                cl.log_message(msg_type, text).await;
            });
        }
    }

    // 2. Output to stderr (avoid stdio collision because JSON-RPC runs on stdout)
    eprintln!("{formatted_msg}");

    // 3. Structured file append
    append_to_file(&formatted_msg);
}

#[macro_export]
macro_rules! log_debug {
    ($cat:expr, $($arg:tt)+) => {
        $crate::logger::log($crate::logger::LogLevel::Debug, $cat, &format!($($arg)+))
    };
}

#[macro_export]
macro_rules! log_info {
    ($cat:expr, $($arg:tt)+) => {
        $crate::logger::log($crate::logger::LogLevel::Info, $cat, &format!($($arg)+))
    };
}

#[macro_export]
macro_rules! log_warn {
    ($cat:expr, $($arg:tt)+) => {
        $crate::logger::log($crate::logger::LogLevel::Warn, $cat, &format!($($arg)+))
    };
}

#[macro_export]
macro_rules! log_error {
    ($cat:expr, $($arg:tt)+) => {
        $crate::logger::log($crate::logger::LogLevel::Error, $cat, &format!($($arg)+))
    };
}

fn append_to_file(formatted_msg: &str) {
    use std::io::Write;
    let log_path = std::env::temp_dir().join("rshtml-analyzer.log");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
    {
        let time_str = chrono_or_fallback_timestamp();
        let _ = writeln!(file, "[{}] {}", time_str, formatted_msg);
    }
}

fn chrono_or_fallback_timestamp() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now();
    if let Ok(dur) = now.duration_since(SystemTime::UNIX_EPOCH) {
        let secs = dur.as_secs();
        let millis = dur.subsec_millis();
        format!("{}.{:03}", secs, millis)
    } else {
        "0.000".to_string()
    }
}
