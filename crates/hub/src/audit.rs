use crate::db::{AuditRow, Db};
use chrono::SecondsFormat;
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct AuditLog {
    file: Arc<Mutex<std::fs::File>>,
    db: Db,
}

impl AuditLog {
    pub fn open(path: &str, db: Db) -> anyhow::Result<Self> {
        let f = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file: Arc::new(Mutex::new(f)),
            db,
        })
    }

    /// Append the event to the JSONL file synchronously, then fire-and-forget
    /// the insert into the admin db. The JSONL file remains the durability
    /// floor — if the db write fails for any reason, the event is still on
    /// disk and the admin UI can be reconstructed from it.
    pub fn write(&self, event: AuditEvent) {
        if let Ok(s) = serde_json::to_string(&event) {
            if let Ok(mut f) = self.file.lock() {
                let _ = writeln!(f, "{}", s);
            }
        }

        let row = to_db_row(&event);
        let db = self.db.clone();
        tokio::spawn(async move {
            db.insert_audit(&row).await;
        });
    }
}

fn to_db_row(e: &AuditEvent) -> AuditRow {
    // detail captures any extra fields not promoted to first-class columns.
    let mut detail = serde_json::Map::new();
    if let Some(s) = e.status {
        detail.insert("status".into(), serde_json::Value::from(s));
    }
    if let Some(c) = e.exit_code {
        detail.insert("exit_code".into(), serde_json::Value::from(c));
    }
    if let Some(r) = &e.reason {
        detail.insert("reason".into(), serde_json::Value::from(r.clone()));
    }
    let detail = if detail.is_empty() {
        None
    } else {
        serde_json::to_string(&detail).ok()
    };
    let ts = chrono::DateTime::parse_from_rfc3339(&e.ts)
        .map(|d| d.timestamp())
        .unwrap_or_else(|_| chrono::Utc::now().timestamp());
    AuditRow {
        ts,
        kind: e.event.to_string(),
        account: e.account.clone(),
        agent: e.agent.clone(),
        session_id: e.session_id.clone(),
        workspace: e.workspace.clone(),
        detail,
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
