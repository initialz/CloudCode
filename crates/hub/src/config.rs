use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    /// Subscription-mode backends running cloudcode-agent.
    #[serde(default)]
    pub agents: Vec<AgentConfig>,
    #[serde(default)]
    pub accounts: Vec<Account>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AgentConfig {
    pub name: String,
    /// argon2id hash of the shared secret. The agent presents the plaintext
    /// secret in its `hello` frame when connecting to /v1/agent/ws.
    pub shared_secret_hash: String,
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

#[derive(Debug, Deserialize, Clone)]
pub struct Account {
    pub name: String,
    pub token_hash: String,
    /// Names of `[[agents]]` this account may route tasks to. First online
    /// agent in this list wins when the client does not specify `agent`.
    #[serde(default)]
    pub allowed_agents: Vec<String>,
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&s)?)
    }
}
