mod proto;
mod relay;
mod wire;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "cloudcode",
    about = "Cloudcode client: open an interactive claude session on a remote agent",
    long_about = "Running `cloudcode` with no subcommand opens a TUI chat against \
                  the remote agent configured in ~/.config/cloudcode/config.toml. \
                  Use `cloudcode config` to inspect or set up that file."
)]
struct Cli {
    /// Open this workspace immediately.
    #[arg(long, default_value = "default")]
    workspace: String,
    /// Pin the session to a specific agent name. Without this, cloudcode
    /// prefers the last agent you used (kept in $XDG_STATE_HOME) and falls
    /// back to whatever the hub picks if that one is offline.
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
    // Cross-platform: use $XDG_CONFIG_HOME if set, else ~/.config/. We
    // deliberately don't follow `dirs::config_dir()`, which would put
    // the file under ~/Library/Application Support on macOS — most CLI
    // tools (rustup, gh, docker…) use ~/.config there too.
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

fn read_last_agent() -> Option<String> {
    let path = last_agent_path().ok()?;
    let s = std::fs::read_to_string(&path).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn write_last_agent(name: &str) {
    let Ok(path) = last_agent_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, name);
}

#[tokio::main]
async fn main() -> ExitCode {
    // Pick rustls' ring CryptoProvider before any TLS code runs; rustls 0.23
    // requires this when crate features can't disambiguate a default.
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
            None => run_chat(cli.agent, cli.workspace).await,
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
# - hub_url: where the hub is reachable (http(s)://… — `cloudcode` rewrites
#   scheme to ws(s):// internally to dial /v1/session/ws).
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
    println!("# Next steps:");
    println!("#   1) Set hub_url to your hub's URL (default 7100 on localhost).");
    println!("#   2) Ask your hub admin for an account token");
    println!("#      (`cloudcode-hub gen-token <name>` prints it once).");
    println!("#   3) Paste it into token = \"...\" then run `cloudcode`.");
    Ok(())
}

async fn run_chat(agent_flag: Option<String>, workspace: String) -> Result<()> {
    let cfg = load_config()?;

    let mut chosen_agent = agent_flag.or_else(read_last_agent);
    let chosen_workspace = workspace;

    loop {
        let wire = wire::connect(&cfg.hub_url, &cfg.token).await?;
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));

        wire.out_tx
            .send(wire::OutFrame::Text(proto::ClientToHub::OpenSession {
                agent: chosen_agent.clone(),
                workspace: chosen_workspace.clone(),
                cols,
                rows,
            }))
            .await
            .context("hub send")?;

        let app = relay::App { wire };
        let outcome = relay::run(app).await?;
        if let Some(name) = &outcome.last_agent {
            write_last_agent(name);
        }
        match outcome.next {
            relay::NextAction::Quit => return Ok(()),
            relay::NextAction::Reconnect { agent } => {
                chosen_agent = agent;
            }
        }
    }
}
