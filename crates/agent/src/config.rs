use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
pub struct Config {
    pub hub: HubConfig,
    #[serde(default)]
    pub agent: AgentSection,
    pub auth: AuthConfig,
    #[serde(default)]
    pub claude: ClaudeConfig,
}

#[derive(Debug, Deserialize)]
pub struct HubConfig {
    /// WebSocket URL of the hub, e.g. `wss://hub.example.com/v1/agent/ws`.
    pub url: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct AgentSection {
    /// Override the auto-generated agent name (`<hostname>-<user>`).
    pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AuthConfig {
    /// Plaintext registration token issued by the hub on first init. Every
    /// agent in the fleet uses the same token; the hub argon2-verifies it
    /// against [agents].registration_token_hash in hub.toml.
    pub registration_token: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ClaudeConfig {
    /// Path to the `claude` executable. Defaults to looking up `"claude"` on
    /// PATH.
    #[serde(default = "default_executable")]
    pub executable: PathBuf,

    /// Root directory under which per-task workspaces are created. Defaults
    /// to `~/cloudcode-agent/workspaces`.
    #[serde(default = "default_workspace_root")]
    pub workspace_root: PathBuf,

    /// Extra arguments appended to every `claude` invocation. Use sparingly;
    /// protocol-critical flags (-p, --output-format, --input-format,
    /// --permission-mode, --verbose) are managed by the agent.
    #[serde(default)]
    pub extra_args: Vec<String>,
}

fn default_executable() -> PathBuf {
    PathBuf::from("claude")
}

fn default_workspace_root() -> PathBuf {
    if let Some(home) = dirs::home_dir() {
        home.join("cloudcode-agent").join("workspaces")
    } else {
        PathBuf::from("./cloudcode-agent-workspaces")
    }
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            executable: default_executable(),
            workspace_root: default_workspace_root(),
            extra_args: Vec::new(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&s)?)
    }
}
