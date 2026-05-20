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
/// ```
///
/// `default` is the tool the first pane runs when the client doesn't
/// specify one. Empty `resume_command` means the wrapper never tries to
/// resume — the tool is always relaunched fresh on reattach.
///
/// v1.13 collapsed the supported tool surface to just `claude`. Older
/// agent.toml files that still list extra `[tools.<name>]` tables keep
/// parsing — those entries are harmless to carry, but the hub/webterm
/// won't offer them as a choice anymore. Anything that needs to run
/// alongside claude should be invoked from inside claude via the
/// plugins / MCP entry points.
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
    /// Opt out of this tool even if it's discoverable on PATH. Lets
    /// an operator pin the tool list explicitly without uninstalling
    /// the binary. Off (= keep the tool) by default.
    #[serde(default)]
    pub disabled: bool,
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
    // Relative to the agent's cwd, same convention hub now uses.
    // Operators who want workspaces on a different volume set
    // `[claude].workspace_root` in agent.toml.
    PathBuf::from("./agent/workspaces")
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

/// Tools the agent will probe for on PATH at startup when there's no
/// explicit `[tools.<name>]` block configured for them. Hardcoded
/// short-list — extend here when adding a new tool to cloudcode's
/// supported surface, and update webterm's `KNOWN_TOOLS` to match.
///
/// As of v1.13 this is `claude`-only. Tools that need to run next to
/// claude should be invoked from inside it via plugins / MCP.
/// Resolve a possibly-relative path against the config file's
/// directory. Returns absolute paths unchanged. Used to keep
/// `workspace_root = "./agent/workspaces"` stable across daemon
/// restarts that may have a different process cwd than the original
/// start.
fn anchor_to_config_dir(config_path: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        return p.to_path_buf();
    }
    let Some(parent) = config_path.parent() else {
        return p.to_path_buf();
    };
    // Best-effort canonicalize so the resolved path has all symlinks
    // expanded (matches what other parts of the agent — notably the
    // workspace watcher — canonicalize for FSEvents path matching).
    let base = std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
    base.join(p)
}

pub const KNOWN_TOOL_NAMES: &[(&str, &str)] = &[
    // (executable / tool name, default resume command)
    ("claude", "claude --continue"),
];

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        let mut cfg: Config = toml::from_str(&s)?;
        cfg.resolve_tools(|name| which::which(name).is_ok());
        // Anchor a relative `workspace_root` against the config
        // file's directory. Otherwise `daemon restart` from a
        // different cwd would resolve `./agent/workspaces` to a
        // different place than the original start — and the tmux
        // session's cwd would silently drift away from where claude
        // expects its files to live (which manifests as "claude
        // popped up in $HOME" if the resolved path happens to not
        // exist and the kernel rejects the chdir).
        cfg.claude.workspace_root = anchor_to_config_dir(path, &cfg.claude.workspace_root);
        Ok(cfg)
    }

    /// Two-phase tool resolution, split out so unit tests can pass a
    /// fake `which`-style probe in:
    ///
    /// 1. Back-compat: pre-v1.10 agent.toml had only `[claude]` and no
    ///    `[tools]` block. Synthesise a `claude` tool from the legacy
    ///    section so the rest of the agent speaks the new shape
    ///    uniformly.
    /// 2. Auto-detect: for each tool in `KNOWN_TOOL_NAMES` that isn't
    ///    already configured, probe PATH via `probe(name)`. Found =>
    ///    synthesise a sane default entry.
    /// 3. Drop any tool whose config (explicit or auto) has
    ///    `disabled = true` so a single boolean flips a tool off
    ///    without re-running the install.
    /// 4. Resolve the default tool: keep an explicit setting if it
    ///    points at a still-live tool, else prefer "claude", else
    ///    the first remaining tool alphabetically.
    pub fn resolve_tools<F: FnMut(&str) -> bool>(&mut self, mut probe: F) {
        if self.tools.tools.is_empty() {
            self.tools.tools.insert(
                "claude".to_string(),
                ToolConfig {
                    executable: self.claude.executable.clone(),
                    resume_command: "claude --continue".into(),
                    extra_args: self.claude.extra_args.clone(),
                    disabled: false,
                },
            );
        }
        for (name, default_resume) in KNOWN_TOOL_NAMES {
            if self.tools.tools.contains_key(*name) {
                continue;
            }
            if probe(name) {
                self.tools.tools.insert(
                    (*name).to_string(),
                    ToolConfig {
                        executable: (*name).to_string(),
                        resume_command: (*default_resume).to_string(),
                        extra_args: Vec::new(),
                        disabled: false,
                    },
                );
            }
        }
        self.tools.tools.retain(|_, t| !t.disabled);
        let explicit_ok = !self.tools.default.is_empty()
            && self.tools.tools.contains_key(&self.tools.default);
        if !explicit_ok {
            self.tools.default = if self.tools.tools.contains_key("claude") {
                "claude".to_string()
            } else {
                let mut names: Vec<&String> = self.tools.tools.keys().collect();
                names.sort();
                names.first().map(|s| (*s).clone()).unwrap_or_default()
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn cfg_from(toml_str: &str) -> Config {
        toml::from_str(toml_str).expect("parse")
    }

    /// Probe factory: returns a closure that says "yes" for tool names
    /// in `available`, "no" otherwise — mimicking `which::which`.
    fn probe_with(available: &[&str]) -> impl FnMut(&str) -> bool {
        let set: HashSet<String> = available.iter().map(|s| s.to_string()).collect();
        move |name: &str| set.contains(name)
    }

    const MIN_CONFIG: &str = r#"
[hub]
url = "wss://example.com/v1/agent/ws"

[auth]
registration_token = "ag_test"
"#;

    #[test]
    fn auto_detects_claude_when_on_path_and_no_tools_block() {
        // The pre-v1.10 back-compat path synthesises a claude entry
        // from the legacy [claude] section, so this also covers the
        // common "config has no [tools] block at all" case.
        let mut cfg = cfg_from(MIN_CONFIG);
        cfg.resolve_tools(probe_with(&["claude"]));
        let names: Vec<&String> = cfg.tools.tools.keys().collect();
        assert_eq!(names, vec!["claude"]);
        assert_eq!(cfg.tools.tools["claude"].executable, "claude");
        assert_eq!(cfg.tools.tools["claude"].resume_command, "claude --continue");
        assert_eq!(cfg.tools.default, "claude");
    }

    #[test]
    fn disabled_flag_filters_claude_even_when_on_path() {
        let cfg_text = r#"
[hub]
url = "wss://example.com/v1/agent/ws"

[auth]
registration_token = "ag_test"

[tools.claude]
executable = "claude"
disabled = true
"#;
        let mut cfg = cfg_from(cfg_text);
        cfg.resolve_tools(probe_with(&["claude"]));
        assert!(!cfg.tools.tools.contains_key("claude"));
        // No tools left -> default falls back to the first remaining
        // key alphabetically, which is empty here.
        assert!(cfg.tools.default.is_empty());
    }

    #[test]
    fn anchor_resolves_relative_against_config_dir() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("agent.toml");
        let anchored = anchor_to_config_dir(&cfg_path, Path::new("./agent/workspaces"));
        // Canonicalized parent of the config path (resolves /var
        // ↔ /private/var on macOS) joined with the relative tail.
        let expected = std::fs::canonicalize(dir.path())
            .unwrap()
            .join("agent/workspaces");
        assert_eq!(anchored, expected);
    }

    #[test]
    fn anchor_passes_absolute_paths_through() {
        let cfg_path = PathBuf::from("/whatever/agent.toml");
        let abs = PathBuf::from("/srv/cloudcode/workspaces");
        let anchored = anchor_to_config_dir(&cfg_path, &abs);
        assert_eq!(anchored, abs);
    }

    #[test]
    fn config_load_anchors_workspace_root_to_config_dir() {
        // Lay a minimal agent.toml down in a tempdir and load it.
        // The default workspace_root is `./agent/workspaces`, which
        // must end up rooted at the tempdir — NOT at the test
        // process's cwd. This is the regression that the daemon-
        // restart-from-a-different-cwd bug hinged on.
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("agent.toml");
        std::fs::write(
            &cfg_path,
            r#"
[hub]
url = "wss://example.com/v1/agent/ws"

[auth]
registration_token = "ag_test"
"#,
        )
        .unwrap();
        let cfg = Config::load(&cfg_path).unwrap();
        let expected = std::fs::canonicalize(dir.path())
            .unwrap()
            .join("agent/workspaces");
        assert_eq!(cfg.claude.workspace_root, expected);
    }

    #[test]
    fn explicit_default_pointing_at_filtered_tool_falls_back_to_claude() {
        let cfg_text = r#"
[hub]
url = "wss://example.com/v1/agent/ws"

[auth]
registration_token = "ag_test"

[tools]
default = "ghost"

[tools.ghost]
executable = "ghost"
disabled = true
"#;
        let mut cfg = cfg_from(cfg_text);
        cfg.resolve_tools(probe_with(&["claude"]));
        // `ghost` was filtered out; resolver synthesises claude from
        // KNOWN_TOOL_NAMES and pins it as the new default.
        assert_eq!(cfg.tools.default, "claude");
        assert!(cfg.tools.tools.contains_key("claude"));
    }
}
