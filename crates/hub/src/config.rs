use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    /// Optional. When present and an account has no usable agent,
    /// hub falls back to forwarding directly to Anthropic with this API key.
    #[serde(default)]
    pub anthropic: Option<AnthropicConfig>,
    /// Subscription-mode backends running cloudcode-agent.
    #[serde(default)]
    pub agents: Vec<AgentConfig>,
    #[serde(default)]
    pub accounts: Vec<Account>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AgentConfig {
    pub name: String,
    pub url: String,
    /// Plain shared secret sent as `Authorization: Bearer <secret>` to the agent.
    pub shared_secret: String,
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
    /// Legacy: which providers this account may use via the direct API-key
    /// path. Keep "anthropic" (or "*") to allow falling back to the
    /// `[anthropic]` API key when no agent is allowed.
    #[serde(default)]
    pub allowed_providers: Vec<String>,
    /// Names of `[[agents]]` this account may route to. First match wins.
    /// Empty means "no subscription-mode access; fall back to API key path".
    #[serde(default)]
    pub allowed_agents: Vec<String>,
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&s)?)
    }
}
