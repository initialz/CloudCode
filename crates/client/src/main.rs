mod menu;
mod proto;
mod relay;
mod wire;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;
use tokio::sync::mpsc;

use crate::proto::{ClientToHub, HubToClient};
use crate::wire::OutFrame;

#[derive(Parser)]
#[command(
    name = "cloudcode",
    about = "Cloudcode client: open an interactive claude session on a remote agent",
    long_about = "Running `cloudcode` with no subcommand opens a workspace \
                  picker for the configured remote agent, then drops into \
                  claude inside that workspace. When claude exits you're \
                  back at the picker. Use `cloudcode config` to inspect or \
                  set up the client config."
)]
struct Cli {
    /// Pin to a specific agent. Without this, cloudcode prefers the last
    /// agent you used (kept in $XDG_STATE_HOME) and falls back to whatever
    /// the hub picks if that one is offline.
    #[arg(long)]
    agent: Option<String>,

    /// One-time setup: write a fresh client config.toml template in the user
    /// config dir. Refuses to overwrite if the file already exists.
    #[arg(long)]
    init: bool,

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

fn config_path() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("XDG_CONFIG_HOME") {
        if !p.is_empty() {
            return Ok(PathBuf::from(p).join("cloudcode").join("config.toml"));
        }
    }
    let home = dirs::home_dir().context("could not find home dir")?;
    Ok(home.join(".config").join("cloudcode").join("config.toml"))
}

fn load_config() -> Result<ClientConfig> {
    let path = config_path()?;
    let s = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "reading {} (run `cloudcode config` for instructions)",
            path.display()
        )
    })?;
    Ok(toml::from_str(&s)?)
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

fn read_last_agent() -> Option<String> {
    read_text_file(&last_agent_path().ok()?)
}

fn write_last_agent(name: &str) {
    if let Ok(p) = last_agent_path() {
        write_text_file(&p, name);
    }
}

fn read_last_workspace(agent: &str) -> Option<String> {
    read_text_file(&last_workspace_path(agent).ok()?)
}

fn write_last_workspace(agent: &str, workspace: &str) {
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
            init_config()
        }
    } else {
        match cli.cmd {
            None => run_chat(cli.agent).await,
            Some(Cmd::Config) => show_config(),
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

fn show_config() -> Result<()> {
    let path = config_path()?;
    println!("config file: {}", path.display());
    match load_config() {
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

fn init_config() -> Result<()> {
    let path = config_path()?;
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

async fn run_chat(agent_flag: Option<String>) -> Result<()> {
    let cfg = load_config()?;
    let mut wire = wire::connect(&cfg.hub_url, &cfg.token).await?;

    match wire.in_text_rx.recv().await {
        Some(HubToClient::Welcome { .. }) => {}
        Some(HubToClient::Rejected { reason }) => {
            return Err(anyhow!("hub rejected: {}", reason));
        }
        other => return Err(anyhow!("expected welcome, got {:?}", other.is_some())),
    }

    let (stdin_tx, mut stdin_rx) = mpsc::channel::<Vec<u8>>(64);
    std::thread::Builder::new()
        .name("stdin-pump".into())
        .spawn(move || stdin_pump_loop(stdin_tx))
        .ok();

    let mut chosen_agent: Option<String> = agent_flag.or_else(read_last_agent);

    loop {
        wire.out_tx
            .send(OutFrame::Text(ClientToHub::SelectAgent {
                agent: chosen_agent.clone(),
            }))
            .await
            .map_err(|_| anyhow!("hub disconnected"))?;
        let selected = loop {
            match wire.in_text_rx.recv().await {
                Some(HubToClient::AgentSelected { agent }) => break agent,
                Some(HubToClient::SessionError { message }) => {
                    return Err(anyhow!("select agent: {}", message));
                }
                Some(HubToClient::Ping) => {
                    let _ = wire.out_tx.send(OutFrame::Text(ClientToHub::Pong)).await;
                }
                Some(_) => continue,
                None => return Err(anyhow!("hub closed connection")),
            }
        };
        chosen_agent = Some(selected.clone());
        write_last_agent(&selected);

        let last_ws = read_last_workspace(&selected);
        let action = menu::run(&mut wire, &mut stdin_rx, &selected, last_ws.as_deref()).await?;

        match action {
            menu::MenuAction::Open(workspace) => {
                let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                wire.out_tx
                    .send(OutFrame::Text(ClientToHub::OpenSession {
                        workspace: workspace.clone(),
                        cols,
                        rows,
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

                write_last_workspace(&selected, &workspace);
                relay::run(&mut wire, &mut stdin_rx).await.ok();
                // Back to the menu on the next iteration.
            }
            menu::MenuAction::SwitchAgent(name) => {
                chosen_agent = name;
            }
            menu::MenuAction::Quit => {
                let _ = wire.out_tx.send(OutFrame::Text(ClientToHub::Close)).await;
                return Ok(());
            }
        }
    }
}

fn stdin_pump_loop(tx: mpsc::Sender<Vec<u8>>) {
    let mut stdin = std::io::stdin().lock();
    let mut buf = [0u8; 4096];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if tx.blocking_send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}
