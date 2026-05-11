use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    /// argon2id hash of the global agent registration token. Any agent that
    /// presents the plaintext token in its `hello` frame is accepted.
    pub agents: AgentsConfig,
    #[serde(default)]
    pub accounts: Vec<Account>,
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

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&s)?)
    }
}
