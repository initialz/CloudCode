use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    /// argon2id hash of the global agent registration token. Any agent that
    /// presents the plaintext token in its `hello` frame is accepted.
    pub agents: AgentsConfig,
    /// Legacy accounts inline in hub.toml. On first run with an empty db
    /// the hub imports these into SQLite; afterwards accounts live in the
    /// db and this list is informational only. Keep / remove as you like.
    #[serde(default)]
    pub accounts: Vec<Account>,
    #[serde(default)]
    pub admin: AdminConfig,
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    pub listen: String,
    #[serde(default = "default_audit_log")]
    pub audit_log: String,
}

fn default_audit_log() -> String {
    "./audit.jsonl".into()
}

#[derive(Debug, Deserialize)]
pub struct AgentsConfig {
    /// argon2id hash of the registration token printed by `cloudcode-hub
    /// --init` (give the plaintext token to agent operators; it is the
    /// same token for every agent and never expires until you re-init).
    pub registration_token_hash: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Account {
    pub name: String,
    pub token_hash: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AdminConfig {
    /// SQLite database file. Holds accounts, audit events, and session
    /// records used by the admin UI.
    #[serde(default = "default_db_path")]
    pub db_path: PathBuf,
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            db_path: default_db_path(),
        }
    }
}

fn default_db_path() -> PathBuf {
    PathBuf::from("./cloudcode-hub.db")
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&s)?)
    }
}
