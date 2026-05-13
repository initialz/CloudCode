mod input;
mod menu;
mod proto;
mod relay;
mod wire;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

use crate::proto::{ClientToHub, HubToClient};
use crate::wire::OutFrame;

#[derive(Parser)]
#[command(
    name = "cloudcode",
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

    /// Everything after `--` is passed through to the remote `claude`
    /// binary's argv on session creation. Reattach to an existing
    /// workspace ignores these (tmux only spawns claude on first open).
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
    let preferred_agent: Option<String> = agent_flag.or_else(read_last_agent);

    loop {
        let outcome = menu::run(
            &mut wire,
            &mut bytes,
            &account_name,
            preferred_agent.as_deref(),
        )
        .await?;
        match outcome {
            menu::MenuOutcome::OpenWorkspace { agent, workspace } => {
                let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                wire.out_tx
                    .send(OutFrame::Text(ClientToHub::OpenSession {
                        workspace: workspace.clone(),
                        cols,
                        rows,
                        claude_args: claude_args.clone(),
                    }))
                    .await
                    .map_err(|_| anyhow!("hub disconnected"))?;
                let mut opened = false;
                loop {
                    match wire.in_text_rx.recv().await {
                        Some(HubToClient::SessionOpened { .. }) => {
                            opened = true;
                            break;
                        }
                        Some(HubToClient::SessionError { message }) => {
                            eprintln!("[cc] {}", message);
                            break;
                        }
                        Some(HubToClient::Ping) => {
                            let _ = wire.out_tx.send(OutFrame::Text(ClientToHub::Pong)).await;
                        }
                        Some(_) => continue,
                        None => return Err(anyhow!("hub closed connection")),
                    }
                }
                if !opened {
                    continue;
                }
                write_last_workspace(&agent, &workspace);
                relay::run(&mut wire, &mut bytes).await.ok();
                // back to menu
            }
            menu::MenuOutcome::Quit => {
                let _ = wire.out_tx.send(OutFrame::Text(ClientToHub::Close)).await;
                return Ok(());
            }
        }
    }
}
