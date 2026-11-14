use std::sync::Mutex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub timestamp: String,
    pub level: String,    // "INFO", "WARN", "ERROR", "ALSA"
    pub message: String,
}

static LOG_BUFFER: Mutex<Vec<LogEntry>> = Mutex::new(Vec::new());

pub fn log_msg(level: &str, message: impl Into<String>) {
    let msg = message.into();
    let now = chrono_now();

    eprintln!("[{}] [{}] {}", now, level, msg);

    if let Ok(mut buf) = LOG_BUFFER.lock() {
        if buf.len() > 500 {
            buf.remove(0);
        }
        buf.push(LogEntry {
            timestamp: now,
            level: level.to_string(),
            message: msg,
        });
    }
}

pub fn get_logs() -> Vec<LogEntry> {
    if let Ok(buf) = LOG_BUFFER.lock() {
        buf.clone()
    } else {
        Vec::new()
    }
}

pub fn clear_logs() {
    if let Ok(mut buf) = LOG_BUFFER.lock() {
        buf.clear();
    }
}

fn chrono_now() -> String {
    // Generate simple HH:MM:SS without requiring chrono crate
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let sec = now % 60;
    let min = (now / 60) % 60;
    let hour = (now / 3600 + 7) % 24; // +7 for user's timezone or simple clock

    format!("{:02}:{:02}:{:02}", hour, min, sec)
}
