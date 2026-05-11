use chrono::SecondsFormat;
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct AuditLog {
    file: Arc<Mutex<std::fs::File>>,
}

impl AuditLog {
    pub fn open(path: &str) -> anyhow::Result<Self> {
        let f = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file: Arc::new(Mutex::new(f)),
        })
    }

    pub fn write(&self, event: AuditEvent) {
        let Ok(s) = serde_json::to_string(&event) else {
            return;
        };
        if let Ok(mut f) = self.file.lock() {
            let _ = writeln!(f, "{}", s);
        }
    }
}

#[derive(Debug, Serialize)]
pub struct AuditEvent {
    pub ts: String,
    pub event: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl AuditEvent {
    pub fn now_ts() -> String {
        chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
    }

    pub fn new(event: &'static str) -> Self {
        Self {
            ts: Self::now_ts(),
            event,
            account: None,
            agent: None,
            session_id: None,
            workspace: None,
            status: None,
            exit_code: None,
            reason: None,
        }
    }
}
