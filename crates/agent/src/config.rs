use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub auth: AuthConfig,
    pub claude: ClaudeConfig,
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    pub listen: String,
}

#[derive(Debug, Deserialize)]
pub struct AuthConfig {
    /// argon2id hash of the shared secret that hubs present in
    /// `Authorization: Bearer <secret>` when calling this agent.
    pub shared_secret_hash: String,
}

#[derive(Debug, Deserialize)]
pub struct ClaudeConfig {
    /// Path to claude's credentials.json. Defaults to ~/.claude/.credentials.json.
    #[serde(default = "default_credentials_path")]
    pub credentials_path: PathBuf,

    /// Upstream Anthropic API base URL.
    #[serde(default = "default_upstream")]
    pub upstream: String,

    /// Anthropic-beta header values to send (joined with ',').
    /// Claude Code itself sends `oauth-2025-04-20` plus a handful of
    /// claude-code-specific feature flags. Override here if you need extra ones.
    #[serde(default = "default_anthropic_beta")]
    pub anthropic_beta: Vec<String>,
}

fn default_credentials_path() -> PathBuf {
    if let Some(home) = dirs::home_dir() {
        home.join(".claude").join(".credentials.json")
    } else {
        PathBuf::from(".credentials.json")
    }
}

fn default_upstream() -> String {
    "https://api.anthropic.com".into()
}

fn default_anthropic_beta() -> Vec<String> {
    vec!["oauth-2025-04-20".into()]
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&s)?)
    }
}
