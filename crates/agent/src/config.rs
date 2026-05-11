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
    #[serde(default)]
    pub tmux: TmuxConfig,
    #[serde(default)]
    pub recording: RecordingConfig,
}

#[derive(Debug, Deserialize)]
pub struct HubConfig {
    pub url: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct AgentSection {
    pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AuthConfig {
    pub registration_token: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ClaudeConfig {
    /// Argv0 passed to tmux as the session's first command. Override if you
    /// want to launch a wrapper (env var injection, mise / direnv shim, ...).
    #[serde(default = "default_claude_executable")]
    pub executable: String,

    /// Root for per-workspace dirs. Defaults to `~/cloudcode-agent/workspaces`.
    #[serde(default = "default_workspace_root")]
    pub workspace_root: PathBuf,

    /// Extra args appended after `claude` when starting the tmux session.
    #[serde(default)]
    pub extra_args: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TmuxConfig {
    /// `tmux` binary to invoke. Defaults to PATH lookup.
    #[serde(default = "default_tmux_executable")]
    pub executable: PathBuf,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RecordingConfig {
    /// Where asciinema `*.cast` files land. Defaults to
    /// `~/.local/state/cloudcode/agent/recordings`. Set to "" or omit to use
    /// the default; pass a per-host path to override.
    #[serde(default = "default_record_dir")]
    pub dir: PathBuf,
    /// Recordings older than this are eligible for GC. 0 (default) keeps
    /// them forever.
    #[serde(default)]
    pub keep_days: u32,
}

fn default_claude_executable() -> String {
    "claude".into()
}

fn default_workspace_root() -> PathBuf {
    if let Some(home) = dirs::home_dir() {
        home.join("cloudcode-agent").join("workspaces")
    } else {
        PathBuf::from("./cloudcode-agent-workspaces")
    }
}

fn default_tmux_executable() -> PathBuf {
    PathBuf::from("tmux")
}

fn default_record_dir() -> PathBuf {
    if let Some(home) = dirs::home_dir() {
        home.join(".local")
            .join("state")
            .join("cloudcode")
            .join("agent")
            .join("recordings")
    } else {
        PathBuf::from("./cloudcode-agent-recordings")
    }
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            executable: default_claude_executable(),
            workspace_root: default_workspace_root(),
            extra_args: Vec::new(),
        }
    }
}

impl Default for TmuxConfig {
    fn default() -> Self {
        Self {
            executable: default_tmux_executable(),
        }
    }
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            dir: default_record_dir(),
            keep_days: 0,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&s)?)
    }
}
