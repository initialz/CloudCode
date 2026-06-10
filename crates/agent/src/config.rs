use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
pub struct Config {
    pub hub: HubConfig,
    #[serde(default)]
    pub agent: AgentSection,
    pub auth: AuthConfig,
    /// Legacy single-tool section. Still parsed for back-compat with
    /// pre-v1.10 agent.toml files; once `[tools]` is populated this
    /// is only consulted for `workspace_root` (which is tool-agnostic).
    #[serde(default)]
    pub claude: ClaudeConfig,
    /// New in v1.10: per-tool runtime config. If empty, `Config::load`
    /// synthesises a single `claude` entry from `[claude]` so existing
    /// installs keep working.
    #[serde(default)]
    pub tools: ToolsSection,
    #[serde(default)]
    pub tmux: TmuxConfig,
    #[serde(default)]
    pub recording: RecordingConfig,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub browser: BrowserConfig,
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

/// Legacy single-`claude` config. Kept so pre-v1.10 agent.toml files
/// continue to parse; new fields should go on [`ToolConfig`] instead.
/// `workspace_root` lives here because it's tool-agnostic (fs layout)
/// and moving it would force every existing agent.toml to be rewritten.
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

/// New-style multi-tool config block.
///
/// ```toml
/// [tools]
/// default = "claude"
///
/// [tools.claude]
/// executable     = "claude"
/// resume_command = "claude --continue"
/// extra_args     = []
///
/// [tools.codex]
/// executable     = "codex"
/// resume_command = ""        # empty -> always fresh, no resume
/// extra_args     = []
/// ```
///
/// `default` is the tool the first pane runs when the client doesn't
/// specify one. Empty `resume_command` means the wrapper never tries to
/// resume — the tool is always relaunched fresh on reattach.
#[derive(Debug, Deserialize, Clone)]
pub struct ToolsSection {
    #[serde(default = "default_tool")]
    pub default: String,
    /// Map of tool name -> config. Populated by serde's `flatten`, so
    /// the section is written as `[tools.<name>]` inline.
    #[serde(flatten, default)]
    pub tools: HashMap<String, ToolConfig>,
}

impl Default for ToolsSection {
    fn default() -> Self {
        Self {
            default: default_tool(),
            tools: HashMap::new(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct ToolConfig {
    /// Executable name or absolute path. Looked up via PATH if not absolute.
    pub executable: String,
    /// Command to run on reattach (instead of `executable <extra_args>`).
    /// Empty string = no resume; always relaunch fresh. The wrapper
    /// `eval`s this string, so quoting follows shell rules.
    #[serde(default)]
    pub resume_command: String,
    /// Extra args appended after `executable` on every spawn.
    #[serde(default)]
    pub extra_args: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TmuxConfig {
    /// `tmux` binary to invoke. Defaults to PATH lookup.
    #[serde(default = "default_tmux_executable")]
    pub executable: PathBuf,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct SandboxConfig {
    /// Wrap each spawned `claude` (and the tmux session it lives in) in a
    /// per-workspace OS-level sandbox. macOS only at the moment — Linux
    /// support is coming. Off by default; opt in once you trust the
    /// profile fits your tooling.
    #[serde(default)]
    pub enabled: bool,
}

/// Resident headless-Chrome config (P1 desktop app). Off by default: the
/// browser only stands up when explicitly opted in. See `browser::chrome`.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct BrowserConfig {
    /// Master switch. Default false — browser off unless opted in.
    #[serde(default)]
    pub enabled: bool,
    /// Explicit Chrome/Chromium binary path. If unset (or the path doesn't
    /// exist) we auto-detect well-known install locations / PATH names.
    #[serde(default)]
    pub chrome_path: Option<String>,
    /// CDP remote-debugging port Chrome listens on. Default 19222.
    #[serde(default = "default_cdp_port")]
    pub cdp_port: u16,
    /// Localhost port the resident MCP HTTP endpoint binds. This is SEPARATE
    /// from `cdp_port` (Chrome's debugging port): claude connects here, the
    /// endpoint drives a per-session playwright-mcp subprocess that in turn
    /// attaches to Chrome on `cdp_port`. Default 7110.
    #[serde(default = "default_mcp_port")]
    pub mcp_port: u16,
    /// Override the playwright-mcp launch command (whitespace-split). Test /
    /// escape hatch; normally left unset so the built-in launcher is used.
    #[serde(default)]
    pub mcp_command: Option<String>,
}

fn default_cdp_port() -> u16 {
    19222
}

fn default_mcp_port() -> u16 {
    7110
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

fn default_tool() -> String {
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
        let mut cfg: Config = toml::from_str(&s)?;
        // Back-compat: pre-v1.10 agent.toml had only [claude] and no
        // [tools] block. Synthesise a default `claude` tool from the
        // legacy section so the rest of the agent can speak the new
        // shape uniformly.
        if cfg.tools.tools.is_empty() {
            cfg.tools.tools.insert(
                "claude".to_string(),
                ToolConfig {
                    executable: cfg.claude.executable.clone(),
                    // Match the previous hard-coded wrapper behaviour
                    // (which always ran `claude --continue` when a saved
                    // jsonl existed).
                    resume_command: "claude --continue".into(),
                    extra_args: cfg.claude.extra_args.clone(),
                },
            );
            // If `[tools].default` wasn't set we already defaulted to
            // "claude" via default_tool, so nothing to do here.
        }
        Ok(cfg)
    }
}
