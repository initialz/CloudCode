use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Item, Table};

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

/// One entry in the backfill registry: a documented optional section we know
/// how to materialise with sensible defaults + explanatory comments.
struct BackfillSection {
    /// Top-level table key, e.g. `"browser"`.
    key: &'static str,
    /// Builds the default `toml_edit::Item` (a decorated `[key]` table) to
    /// append when the section is absent.
    build: fn() -> Item,
}

/// Registry of documented optional sections to backfill on startup.
///
/// Intentionally minimal — V1 only seeds `[browser]`. The other optional
/// sections are deliberately NOT here:
///   - `[tmux]` / `[recording]` are auto/host-derived and fine when absent.
///   - `[sandbox]` is deprecated (toggle moved to the hub).
///   - `[claude]` / `[tools]` / `[agent]` already parse fine from serde
///     defaults and carry user-specific wiring we don't want to inject.
/// Adding a future section is a one-line addition here plus its `build_*` fn.
fn backfill_registry() -> &'static [BackfillSection] {
    &[BackfillSection {
        key: "browser",
        build: build_browser_section,
    }]
}

/// Default `[browser]` table: the real key (`enabled = false`) plus the same
/// explanatory comment block init_config writes, attached as the table's decor
/// prefix. The optional fields (chrome_path/cdp_port/mcp_port/mcp_command) are
/// shown as commented hint lines INSIDE that prefix, not as live keys.
fn build_browser_section() -> Item {
    let mut table = Table::new();
    table.insert("enabled", toml_edit::value(false));
    // Leading blank line separates the appended block from prior content.
    table.decor_mut().set_prefix(
        "\n# [browser] enables the agent-side browser channel: claude gets a\n\
         # `cc-browser` MCP tool (Playwright over a resident headless Chrome) and\n\
         # the desktop app / webterm /viewer can mirror its pages via CDP.\n\
         # Disabled by default. Requires Google Chrome (or Chromium) + Node.js on\n\
         # this host. Turn it on by setting enabled = true.\n",
    );
    // The commented optional-field hints trail the `enabled` key. We attach
    // them as the suffix decor of the `enabled` value so they sit directly
    // under it, matching init_config's layout.
    if let Some(enabled) = table.get_mut("enabled") {
        if let Item::Value(v) = enabled {
            v.decor_mut().set_suffix(
                "\n# chrome_path = \"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome\"  # auto-detected if unset\n\
                 # cdp_port = 19222    # resident Chrome's --remote-debugging-port (localhost only)\n\
                 # mcp_port = 7110     # the localhost MCP endpoint claude connects to\n\
                 # mcp_command = \"\"    # override the whole playwright-mcp launch command (advanced/testing)",
            );
        }
    }
    Item::Table(table)
}

/// On startup, append any DOCUMENTED optional config section that's missing
/// from `path` (with its default value + explanatory comments), preserving all
/// existing content / comments / formatting. Idempotent: an already-present
/// section is never touched, and if nothing is added the file is left
/// byte-for-byte unchanged (no mtime churn).
///
/// Returns the list of section keys that were actually appended. Writes are
/// atomic (temp file + rename) and preserve the original file's permissions.
pub fn backfill_defaults(path: &Path) -> anyhow::Result<Vec<&'static str>> {
    let original = std::fs::read_to_string(path)?;
    // Parse first; on a malformed file we bail without writing anything.
    let mut doc: DocumentMut = original
        .parse()
        .map_err(|e| anyhow::anyhow!("parsing {} for backfill: {}", path.display(), e))?;

    let mut added: Vec<&'static str> = Vec::new();
    for section in backfill_registry() {
        if doc.get(section.key).is_none() {
            doc.insert(section.key, (section.build)());
            added.push(section.key);
        }
    }

    if added.is_empty() {
        // Nothing to do — don't rewrite the file (idempotent, no mtime churn).
        return Ok(added);
    }

    let new_contents = doc.to_string();

    // Atomic write: serialise to a temp file in the SAME dir (so the rename is
    // a same-filesystem move), then rename over the original. A crash
    // mid-write leaves the temp file, never a half-written config.
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    let tmp_path = match dir {
        Some(d) => d.join(format!(
            ".{}.backfill.{}.tmp",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("config"),
            std::process::id()
        )),
        None => PathBuf::from(format!(".config.backfill.{}.tmp", std::process::id())),
    };

    // Write + flush the temp file, then ensure it's gone on any later failure.
    let write_result = (|| -> std::io::Result<()> {
        use std::io::Write as _;
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(new_contents.as_bytes())?;
        f.flush()?;
        // Preserve the original file's permissions so we don't loosen the mode
        // on a file that holds the plaintext registration token.
        if let Ok(meta) = std::fs::metadata(path) {
            std::fs::set_permissions(&tmp_path, meta.permissions())?;
        }
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(anyhow::anyhow!(
            "writing backfill temp for {}: {}",
            path.display(),
            e
        ));
    }

    if let Err(e) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(anyhow::anyhow!(
            "renaming backfilled config into {}: {}",
            path.display(),
            e
        ));
    }

    Ok(added)
}

#[cfg(test)]
mod backfill_tests {
    use super::*;
    use std::io::Write as _;

    /// Minimal valid agent.toml with only the required sections.
    const MINIMAL: &str = "[hub]\nurl = \"wss://hub.example.com/v1/agent/ws\"\n\n[auth]\nregistration_token = \"ag_TEST\"\n";

    fn write_tmp(name: &str, contents: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cc-backfill-test-{}-{}",
            std::process::id(),
            name
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    #[test]
    fn backfill_adds_browser_when_absent() {
        let path = write_tmp("adds-browser", MINIMAL);
        let added = backfill_defaults(&path).unwrap();
        assert_eq!(added, vec!["browser"]);

        let out = std::fs::read_to_string(&path).unwrap();
        eprintln!("--- backfilled file ---\n{out}\n--- end ---");
        // New section present with real key + comment text.
        assert!(out.contains("[browser]"));
        assert!(out.contains("enabled = false"));
        assert!(out.contains("enables the agent-side browser channel"));
        assert!(out.contains("Turn it on by setting enabled = true"));
        assert!(out.contains("# cdp_port = 19222"));
        // Required sections preserved verbatim.
        assert!(out.contains("url = \"wss://hub.example.com/v1/agent/ws\""));
        assert!(out.contains("registration_token = \"ag_TEST\""));
    }

    #[test]
    fn backfill_is_idempotent() {
        let path = write_tmp("idempotent", MINIMAL);
        let first = backfill_defaults(&path).unwrap();
        assert_eq!(first, vec!["browser"]);
        let after_first = std::fs::read_to_string(&path).unwrap();

        let second = backfill_defaults(&path).unwrap();
        assert!(second.is_empty(), "second run should add nothing");
        let after_second = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after_first, after_second, "file must be byte-identical");
    }

    #[test]
    fn backfill_preserves_user_browser() {
        let contents = format!("{MINIMAL}\n[browser]\nenabled = true\n");
        let path = write_tmp("preserve-browser", &contents);
        let added = backfill_defaults(&path).unwrap();
        assert!(added.is_empty());
        let out = std::fs::read_to_string(&path).unwrap();
        assert_eq!(out, contents, "user [browser] must be untouched");
        assert!(out.contains("enabled = true"));
        assert!(!out.contains("enabled = false"));
    }

    #[test]
    fn backfill_preserves_comments_and_other_sections() {
        let contents = format!(
            "# my hand-written note\n{MINIMAL}\n[tmux]\nexecutable = \"/usr/local/bin/tmux\"\n"
        );
        let path = write_tmp("preserve-comments", &contents);
        let added = backfill_defaults(&path).unwrap();
        assert_eq!(added, vec!["browser"]);
        let out = std::fs::read_to_string(&path).unwrap();
        assert!(out.contains("# my hand-written note"));
        assert!(out.contains("[tmux]"));
        assert!(out.contains("executable = \"/usr/local/bin/tmux\""));
        assert!(out.contains("[browser]"));
        // Comment + tmux survive ahead of the appended block.
        let note_idx = out.find("# my hand-written note").unwrap();
        let tmux_idx = out.find("[tmux]").unwrap();
        let browser_idx = out.find("[browser]").unwrap();
        assert!(note_idx < browser_idx);
        assert!(tmux_idx < browser_idx);
    }

    #[test]
    fn backfilled_file_parses_as_config() {
        let path = write_tmp("parses", MINIMAL);
        backfill_defaults(&path).unwrap();
        let cfg = Config::load(&path).expect("backfilled file must load");
        assert!(!cfg.browser.enabled);
    }

    #[test]
    fn backfill_leaves_no_temp_files() {
        let path = write_tmp("no-temp", MINIMAL);
        backfill_defaults(&path).unwrap();
        let dir = path.parent().unwrap();
        let strays: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(strays.is_empty(), "stray temp files left: {strays:?}");
    }
}
