mod input;
mod menu;
mod mouse_filter;
mod paste_detect;
mod proto;
mod relay;
mod wire;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use crate::input::ByteRx;
use crate::proto::{ClientToHub, HubToClient};
use crate::relay::RelayOutcome;
use crate::wire::{OutFrame, Wire};

#[derive(Parser)]
#[command(
    name = "cloudcode",
    version,
    about = "Cloudcode client: open an interactive claude session on a remote agent",
    long_about = "Running `cloudcode` with no subcommand opens a workspace \
                  picker for the configured remote agent, then drops into \
                  claude inside that workspace. Everything after `--` is \
                  forwarded verbatim to the remote `claude` process — e.g. \
                  `cloudcode -- --continue` or `cloudcode -- --model opus`. \
                  When claude exits you're back at the picker. Use \
                  `cloudcode config` to inspect or set up the client config."
)]
struct Cli {
    /// Pin to a specific agent. Without this, cloudcode prefers the last
    /// agent you used (kept in $XDG_STATE_HOME) and falls back to whatever
    /// the hub picks if that one is offline.
    #[arg(long)]
    agent: Option<String>,

    /// Path to the client config TOML. Defaults to
    /// `$XDG_CONFIG_HOME/cloudcode/config.toml` (i.e. usually
    /// `~/.config/cloudcode/config.toml`). Handy when you want
    /// multiple hub profiles side by side.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Hub WebSocket URL (overrides `hub_url` in the config file).
    /// Pass this together with --token to skip the config file
    /// entirely — useful for one-liner install + run.
    #[arg(long, value_name = "URL")]
    hub_url: Option<String>,

    /// Account token (overrides `token` in the config file). Goes
    /// into shell history on most shells, so prefer the config
    /// file for long-lived setups; the flag is meant for paste-
    /// and-run commands handed out by the admin UI.
    #[arg(long, value_name = "TOKEN")]
    token: Option<String>,

    /// One-time setup: write a fresh client config.toml template at
    /// the resolved config path (`--config <path>` if given, otherwise
    /// the default). Refuses to overwrite if the file already exists.
    #[arg(long)]
    init: bool,

    /// Which CLI to run inside the workspace on first open: "claude"
    /// (default), "codex", or whatever the agent has configured under
    /// `[tools]` in its agent.toml. Reattach to an existing tmux
    /// session ignores this; use tmux's own split keys (Ctrl+b %) or
    /// the webterm split button to spawn another tool in a side pane.
    #[arg(long, value_name = "NAME")]
    tool: Option<String>,

    /// Everything after `--` is passed through to the remote tool's
    /// argv on session creation. Reattach to an existing workspace
    /// ignores these (tmux only spawns the tool on first open).
    #[arg(last = true, allow_hyphen_values = true)]
    claude_args: Vec<String>,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show the resolved client config (or print init instructions).
    Config,
}

#[derive(serde::Deserialize, Debug)]
struct ClientConfig {
    hub_url: String,
    token: String,
}

fn default_config_path() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("XDG_CONFIG_HOME") {
        if !p.is_empty() {
            return Ok(PathBuf::from(p).join("cloudcode").join("config.toml"));
        }
    }
    let home = dirs::home_dir().context("could not find home dir")?;
    Ok(home.join(".config").join("cloudcode").join("config.toml"))
}

fn resolve_config_path(override_path: Option<&PathBuf>) -> Result<PathBuf> {
    match override_path {
        Some(p) => Ok(p.clone()),
        None => default_config_path(),
    }
}

fn load_config(override_path: Option<&PathBuf>) -> Result<ClientConfig> {
    let path = resolve_config_path(override_path)?;
    let s = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "reading {} (run `cloudcode config` for instructions)",
            path.display()
        )
    })?;
    Ok(toml::from_str(&s)?)
}

/// Merge --hub-url / --token over the config file. If both CLI flags
/// are given, the file is not read at all — paste-and-run installs
/// work without writing config.toml first.
fn resolve_config(
    cli_hub_url: Option<String>,
    cli_token: Option<String>,
    config_override: Option<&PathBuf>,
) -> Result<ClientConfig> {
    let file = if cli_hub_url.is_some() && cli_token.is_some() {
        None
    } else {
        Some(load_config(config_override)?)
    };
    let hub_url = cli_hub_url
        .or_else(|| file.as_ref().map(|c| c.hub_url.clone()))
        .ok_or_else(|| anyhow!("hub_url missing — set in config or pass --hub-url"))?;
    let token = cli_token
        .or_else(|| file.as_ref().map(|c| c.token.clone()))
        .ok_or_else(|| anyhow!("token missing — set in config or pass --token"))?;
    Ok(ClientConfig { hub_url, token })
}

fn state_dir() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("CLOUDCODE_STATE_DIR") {
        return Ok(PathBuf::from(p));
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("state")))
        .context("could not determine state dir")?;
    Ok(base.join("cloudcode"))
}

fn last_agent_path() -> Result<PathBuf> {
    Ok(state_dir()?.join("last_agent"))
}

fn last_workspace_path(agent: &str) -> Result<PathBuf> {
    Ok(state_dir()?
        .join("last_workspace")
        .join(format!("{}.txt", agent)))
}

fn read_text_file(path: &PathBuf) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn write_text_file(path: &PathBuf, contents: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, contents);
}

pub fn read_last_agent() -> Option<String> {
    read_text_file(&last_agent_path().ok()?)
}

pub fn write_last_agent(name: &str) {
    if let Ok(p) = last_agent_path() {
        write_text_file(&p, name);
    }
}

/// Remove the saved `last_agent` so the next launch starts on the
/// top-level agent picker. Called when the user quits the menu from
/// the agent-picker stage (vs. quitting deeper, e.g. workspace
/// picker, where preserving the agent is the user-friendlier
/// default).
pub fn clear_last_agent() {
    if let Ok(p) = last_agent_path() {
        let _ = std::fs::remove_file(&p);
    }
}

/// New single-stage menu's cursor restore. Stored as `<agent>|<name>`
/// in one file so the picker can position the highlight on the
/// previously-picked workspace regardless of which agent it lived on.
pub fn read_last_workspace_global() -> Option<String> {
    read_text_file(&state_dir().ok()?.join("last_workspace_global"))
}

pub fn write_last_workspace_global(agent: &str, name: &str) {
    if let Ok(dir) = state_dir() {
        let path = dir.join("last_workspace_global");
        write_text_file(&path, &format!("{}|{}", agent, name));
    }
}

pub fn read_last_workspace(agent: &str) -> Option<String> {
    read_text_file(&last_workspace_path(agent).ok()?)
}

pub fn write_last_workspace(agent: &str, workspace: &str) {
    if let Ok(p) = last_workspace_path(agent) {
        write_text_file(&p, workspace);
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cli = Cli::parse();

    let result = if cli.init {
        if cli.cmd.is_some() {
            Err(anyhow!("--init cannot be combined with a subcommand"))
        } else {
            init_config(cli.config.as_ref())
        }
    } else {
        match cli.cmd {
            None => {
                run_chat(
                    cli.agent,
                    cli.claude_args,
                    cli.tool,
                    cli.config.as_ref(),
                    cli.hub_url,
                    cli.token,
                )
                .await
            }
            Some(Cmd::Config) => show_config(cli.config.as_ref()),
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("cloudcode: {:#}", e);
            ExitCode::from(1)
        }
    }
}

fn show_config(override_path: Option<&PathBuf>) -> Result<()> {
    let path = resolve_config_path(override_path)?;
    println!("config file: {}", path.display());
    match load_config(override_path) {
        Ok(c) => {
            println!("hub_url: {}", c.hub_url);
            let masked: String = c.token.chars().take(10).collect();
            println!("token:   {}...", masked);
        }
        Err(_) => {
            println!();
            println!("config not found. create with:");
            println!("  cloudcode --init");
        }
    }
    Ok(())
}

fn init_config(override_path: Option<&PathBuf>) -> Result<()> {
    let path = resolve_config_path(override_path)?;
    if path.exists() {
        return Err(anyhow!(
            "{} already exists; refusing to overwrite. Delete it first if you really want to re-init.",
            path.display()
        ));
    }
    let template = r#"# Cloudcode client config.
# - hub_url: where the hub is reachable (http(s)://…).
# - token:   account token printed once by `cloudcode-hub gen-token <name>`
#            on the admin's side; ask them for it.

hub_url = "http://localhost:7100"
token   = "cc_PASTE_TOKEN_HERE"
"#;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, template).with_context(|| format!("writing {}", path.display()))?;
    println!("# Wrote {}", path.display());
    println!();
    println!("# Next: edit hub_url + token, then run `cloudcode`.");
    Ok(())
}

async fn run_chat(
    agent_flag: Option<String>,
    claude_args: Vec<String>,
    tool: Option<String>,
    config_override: Option<&PathBuf>,
    cli_hub_url: Option<String>,
    cli_token: Option<String>,
) -> Result<()> {
    let cfg = resolve_config(cli_hub_url, cli_token, config_override)?;
    let mut wire = wire::connect(&cfg.hub_url, &cfg.token).await?;

    let account_name = match wire.in_text_rx.recv().await {
        Some(HubToClient::Welcome { account }) => account,
        Some(HubToClient::Rejected { reason }) => {
            return Err(anyhow!("hub rejected: {}", reason));
        }
        other => return Err(anyhow!("expected welcome, got {:?}", other.is_some())),
    };

    let mut bytes = input::spawn_byte_reader();
    // v1.13: single-stage menu. The picker always shows every
    // workspace bound to this account (across all agents), the
    // cursor restores from `last_workspace_global`, and Enter
    // routes directly into the bound agent. `--agent` and the
    // legacy `last_agent` files are silently ignored — workspace
    // identity now includes the agent.
    let _ = agent_flag;
    loop {
        let outcome = menu::run(&mut wire, &mut bytes, &account_name).await?;
        match outcome {
            menu::MenuOutcome::OpenWorkspace { agent, workspace } => {
                write_last_workspace_global(&agent, &workspace);
                if let Err(e) = session_with_reconnect(
                    &cfg,
                    &mut wire,
                    &mut bytes,
                    &agent,
                    &workspace,
                    &claude_args,
                    &tool,
                )
                .await
                {
                    // session_with_reconnect always cleans up the
                    // terminal before returning Err. Surface the
                    // reason on a single line so we land back at the
                    // menu cleanly.
                    eprintln!("[cc] {}", e);
                }
            }
            menu::MenuOutcome::Quit => {
                let _ = wire.out_tx.send(OutFrame::Text(ClientToHub::Close)).await;
                return Ok(());
            }
        }
    }
}

/// Open + relay a workspace, transparently reconnecting whenever the
/// hub WS dies mid-session. The caller stays in the menu when this
/// returns; only a genuinely fatal hub rejection (bad token, etc.)
/// surfaces as `Err`.
///
/// Terminal state (alt-screen + raw mode) is acquired here and always
/// released before returning, no matter the exit path — without that
/// guarantee a panic inside the relay would leave the user's shell in
/// an unusable state.
async fn session_with_reconnect(
    cfg: &ClientConfig,
    wire: &mut Wire,
    bytes: &mut ByteRx,
    agent: &str,
    workspace: &str,
    claude_args: &[String],
    tool: &Option<String>,
) -> Result<()> {
    relay::enter_session_mode()?;
    let outcome = session_loop(cfg, wire, bytes, agent, workspace, claude_args, tool).await;
    relay::leave_session_mode();
    outcome
}

async fn session_loop(
    cfg: &ClientConfig,
    wire: &mut Wire,
    bytes: &mut ByteRx,
    agent: &str,
    workspace: &str,
    claude_args: &[String],
    tool: &Option<String>,
) -> Result<()> {
    let mut backoff = Duration::from_millis(500);
    loop {
        // (1) Open or re-open the session on the *current* wire.
        match open_session(wire, agent, workspace, claude_args, tool).await {
            OpenResult::Opened => {
                backoff = Duration::from_millis(500);
                clear_pill();
            }
            OpenResult::SessionError(msg) => {
                // Hub answered but rejected the open. The common case is
                // "agent X is offline" right after a hub restart, while
                // the agent hasn't caught up yet — that's transient.
                // We keep looping; a *real* config error (workspace
                // gone, ACL change) will keep firing the same message
                // and the user sees it on the pill on every attempt,
                // which is good enough discovery in practice.
                show_pill(&format!("Reconnecting: {}", trim_one_line(&msg, 60)));
                tokio::time::sleep(backoff).await;
                backoff = bump(backoff);
                continue;
            }
            OpenResult::WireLost => {
                if !reconnect_wire(cfg, wire, &mut backoff).await? {
                    return Err(anyhow!("hub rejected the reconnect"));
                }
                continue;
            }
        }
        // (2) Run the PTY relay until either claude exits cleanly
        //     (Closed → return to menu) or the wire dies (HubLost →
        //     reconnect + re-open the same workspace).
        match relay::run(wire, bytes, agent, workspace).await? {
            RelayOutcome::Closed => return Ok(()),
            RelayOutcome::HubLost => {
                if !reconnect_wire(cfg, wire, &mut backoff).await? {
                    return Err(anyhow!("hub rejected the reconnect"));
                }
                // Loop back to (1) — fresh open_session on the new wire.
            }
        }
    }
}

enum OpenResult {
    Opened,
    SessionError(String),
    WireLost,
}

async fn open_session(
    wire: &mut Wire,
    agent: &str,
    workspace: &str,
    claude_args: &[String],
    tool: &Option<String>,
) -> OpenResult {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    if wire
        .out_tx
        .send(OutFrame::Text(ClientToHub::OpenSession {
            workspace: workspace.to_string(),
            agent: agent.to_string(),
            cols,
            rows,
            claude_args: claude_args.to_vec(),
            tool: tool.clone(),
        }))
        .await
        .is_err()
    {
        return OpenResult::WireLost;
    }
    loop {
        match wire.in_text_rx.recv().await {
            Some(HubToClient::SessionOpened { .. }) => return OpenResult::Opened,
            Some(HubToClient::SessionError { message }) => return OpenResult::SessionError(message),
            Some(HubToClient::Ping) => {
                let _ = wire.out_tx.send(OutFrame::Text(ClientToHub::Pong)).await;
            }
            Some(_) => continue,
            None => return OpenResult::WireLost,
        }
    }
}

/// Reopen the hub WS, with exponential backoff and a top-right pill
/// painted on every attempt. Returns `Ok(true)` when a fresh wire is
/// installed; `Ok(false)` if the hub responded with a terminal
/// `Rejected` (bad token, etc.) and we should bail out to the menu.
async fn reconnect_wire(
    cfg: &ClientConfig,
    wire: &mut Wire,
    backoff: &mut Duration,
) -> Result<bool> {
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        show_pill(&format!("Reconnecting ({})", attempt));
        tokio::time::sleep(*backoff).await;
        *backoff = bump(*backoff);
        let new_wire = match wire::connect(&cfg.hub_url, &cfg.token).await {
            Ok(w) => w,
            Err(_) => continue,
        };
        let mut new_wire = new_wire;
        match tokio::time::timeout(Duration::from_secs(10), new_wire.in_text_rx.recv()).await {
            Ok(Some(HubToClient::Welcome { .. })) => {
                *wire = new_wire;
                *backoff = Duration::from_millis(500);
                clear_pill();
                return Ok(true);
            }
            Ok(Some(HubToClient::Rejected { .. })) => return Ok(false),
            _ => continue,
        }
    }
}

fn bump(d: Duration) -> Duration {
    (d * 2).min(Duration::from_secs(30))
}

/// Trim a hub-supplied error message to a single line `<= max` chars so
/// the top-right pill can show it without wrapping. Anything past the
/// first newline is dropped first.
fn trim_one_line(msg: &str, max: usize) -> String {
    let one_line = msg.lines().next().unwrap_or("").trim();
    if one_line.chars().count() <= max {
        one_line.to_string()
    } else {
        let trimmed: String = one_line.chars().take(max.saturating_sub(1)).collect();
        format!("{}…", trimmed)
    }
}

/// Repaint the entire alt-screen with a centered "hub disconnected"
/// banner. Called on every reconnect attempt — clearing claude's UI
/// is intentional, otherwise users freeze the wrong way: typing into
/// a still-visible UI that ignores them with no feedback. The tmux
/// reattach push will fill alt-screen back in once a fresh session
/// opens, so we don't lose anything permanent.
fn show_pill(text: &str) {
    use std::io::Write;
    let mut stdout = std::io::stdout();
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));

    // Two-line block: a fixed title + the per-attempt status line.
    let title = "Hub disconnected";
    let hint = "session resumes automatically once the hub is back";
    let lines = [title, text, "", hint];
    let widest = lines.iter().map(|s| s.chars().count()).max().unwrap_or(0);
    let inner = widest + 4; // 2-cell padding on each side
    let box_height = lines.len() as u16;

    let cols = cols as usize;
    let pad_left = cols.saturating_sub(inner) / 2;
    let pad_str = " ".repeat(pad_left);
    let start_row = rows.saturating_sub(box_height) / 2 + 1;

    // Clear, hide cursor (so it doesn't blink in some random cell
    // mid-banner), paint each line in yellow.
    let _ = stdout.write_all(b"\x1b[H\x1b[2J\x1b[?25l");
    for (i, line) in lines.iter().enumerate() {
        let row = start_row + i as u16;
        let inner_pad = inner.saturating_sub(line.chars().count());
        let left = inner_pad / 2;
        let right = inner_pad - left;
        // Bold title + plain rest; whole pill is yellow-on-black.
        let attr = if i == 0 { "\x1b[1;33m" } else { "\x1b[33m" };
        let _ = write!(
            stdout,
            "\x1b[{};1H{}{}{}{}{}\x1b[0m",
            row,
            pad_str,
            attr,
            " ".repeat(left),
            line,
            " ".repeat(right),
        );
    }
    let _ = stdout.flush();
}

/// Tear the banner down. After a successful reconnect the tmux
/// reattach paints the full alt-screen state, so a plain clear is all
/// we need — leaves an empty buffer for tmux's push to land in.
fn clear_pill() {
    use std::io::Write;
    let mut stdout = std::io::stdout();
    // Show cursor again + clear.
    let _ = stdout.write_all(b"\x1b[?25h\x1b[H\x1b[2J");
    let _ = stdout.flush();
}
