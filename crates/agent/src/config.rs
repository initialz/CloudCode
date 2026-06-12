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
    /// `[remote_mcp]` 段:agent 侧远程-MCP proxy(cc-browser 管道)
    /// 开关与端点(决策 D10)。整段缺省 = 全默认(零配置即用)。
    #[serde(default)]
    pub remote_mcp: RemoteMcpConfig,
    /// `[browser]` 段:agent 本机 web 无头后端(计划②)。整段缺省 =
    /// 全默认(零配置即用)。
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

fn remote_mcp_default_enabled() -> bool {
    true
}

fn remote_mcp_default_port() -> u16 {
    7110
}

/// `[remote_mcp]` 段:远程-MCP proxy 设置。
#[derive(Debug, Clone, Deserialize)]
pub struct RemoteMcpConfig {
    /// 总开关:false 时不启动 localhost 端点、不向 claude 注入任何
    /// MCP 配置。默认 true。
    #[serde(default = "remote_mcp_default_enabled")]
    pub enabled: bool,
    /// proxy 监听端口(仅绑定 127.0.0.1)。
    #[serde(default = "remote_mcp_default_port")]
    pub port: u16,
    /// 静态工具表(JSON **数组**文件)路径:Phase E 在无 client 时用
    /// 它应答 tools/list;缺省 = 空表。dev-browser 的 manifest 内容
    /// 属计划②(决策 D17)。
    #[serde(default)]
    pub tools_manifest: Option<PathBuf>,
}

impl Default for RemoteMcpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: 7110,
            tools_manifest: None,
        }
    }
}

fn browser_default_web_enabled() -> bool {
    true
}

/// `[browser]` 段(计划②):agent 本机 `web` 无头浏览器后端。与
/// `[remote_mcp]` 平行 —— 那边管 cc-browser proxy,这边只管注入配置
/// 里的 web stdio 条目。整段缺省 = 全默认零配置。
#[derive(Debug, Clone, Deserialize)]
pub struct BrowserConfig {
    /// 是否注入 `web` stdio server 条目。false ⇒ 注入配置只含
    /// cc-browser(回到计划①单 server 形态)。默认 true。
    #[serde(default = "browser_default_web_enabled")]
    pub web_enabled: bool,
    /// 整条 web 后端命令覆盖(空白分隔)。缺省 = 内置默认
    /// `npx -y @playwright/mcp@<pin> --headless`
    /// (pin 常量在 mcp_proxy.rs::PLAYWRIGHT_MCP_PKG)。
    #[serde(default)]
    pub web_backend: Option<String>,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            web_enabled: true,
            web_backend: None,
        }
    }
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

#[cfg(test)]
mod remote_mcp_config_tests {
    use super::*;

    #[test]
    fn remote_mcp_defaults() {
        // 段缺省整体:Default 实现。
        let d = RemoteMcpConfig::default();
        assert!(d.enabled);
        assert_eq!(d.port, 7110);
        assert_eq!(d.tools_manifest, None);

        // 段存在但字段缺省:serde 字段默认。
        let c: RemoteMcpConfig = toml::from_str("").unwrap();
        assert!(c.enabled);
        assert_eq!(c.port, 7110);

        // 显式覆盖。
        let c: RemoteMcpConfig =
            toml::from_str("enabled = false\nport = 7200\ntools_manifest = \"/etc/cc/tools.json\"")
                .unwrap();
        assert!(!c.enabled);
        assert_eq!(c.port, 7200);
        assert_eq!(c.tools_manifest, Some(PathBuf::from("/etc/cc/tools.json")));
    }
}

#[cfg(test)]
mod browser_config_tests {
    use super::*;

    #[test]
    fn browser_defaults() {
        // 段缺省整体:Default 实现。
        let d = BrowserConfig::default();
        assert!(d.web_enabled);
        assert_eq!(d.web_backend, None);

        // 段存在但字段缺省:serde 字段默认。
        let c: BrowserConfig = toml::from_str("").unwrap();
        assert!(c.web_enabled);
        assert_eq!(c.web_backend, None);

        // 显式覆盖。
        let c: BrowserConfig = toml::from_str(
            "web_enabled = false\nweb_backend = \"npx -y @playwright/mcp@0.0.76 --headless --browser=chromium\"",
        )
        .unwrap();
        assert!(!c.web_enabled);
        assert_eq!(
            c.web_backend.as_deref(),
            Some("npx -y @playwright/mcp@0.0.76 --headless --browser=chromium")
        );
    }

    #[test]
    fn config_without_browser_section_gets_defaults() {
        // 整段缺省 = 全默认零配置;既有 agent.toml(无 [browser])解析
        // 不变且拿到默认 browser。
        let cfg: Config = toml::from_str(
            "[hub]\nurl = \"wss://h\"\n\n[auth]\nregistration_token = \"t\"\n",
        )
        .unwrap();
        assert!(cfg.browser.web_enabled);
        assert_eq!(cfg.browser.web_backend, None);
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
