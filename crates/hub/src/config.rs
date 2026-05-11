use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub anthropic: AnthropicConfig,
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
pub struct AnthropicConfig {
    #[serde(default = "default_upstream")]
    pub upstream: String,
    pub api_key: String,
}

fn default_upstream() -> String {
    "https://api.anthropic.com".into()
}

#[derive(Debug, Deserialize)]
pub struct Account {
    pub name: String,
    pub token_hash: String,
    #[serde(default)]
    pub allowed_providers: Vec<String>,
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&s)?)
    }
}
