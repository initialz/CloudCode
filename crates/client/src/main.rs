mod proto;
mod tui;
mod wire;

use anyhow::{Context, Result};
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
    /// Pin the session to a specific agent name.
    #[arg(long)]
    agent: Option<String>,

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
    let dir = dirs::config_dir().context("could not find user config dir")?;
    Ok(dir.join("cloudcode").join("config.toml"))
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

#[tokio::main]
async fn main() -> ExitCode {
    // Pick rustls' ring CryptoProvider before any TLS code runs; rustls 0.23
    // requires this when crate features can't disambiguate a default.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cli = Cli::parse();
    let result = match cli.cmd {
        None => run_chat(cli.agent, cli.workspace).await,
        Some(Cmd::Config) => show_config(),
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
            println!("  mkdir -p {}", path.parent().unwrap().display());
            println!("  cat > {} <<EOF", path.display());
            println!("  hub_url = \"http://localhost:7000\"");
            println!("  token = \"cc_xxx\"");
            println!("  EOF");
        }
    }
    Ok(())
}

async fn run_chat(agent: Option<String>, workspace: String) -> Result<()> {
    let cfg = load_config()?;
    let wire = wire::connect(&cfg.hub_url, &cfg.token).await?;

    // Immediately send OpenSession; the TUI will reflect SessionOpened.
    wire.tx
        .send(proto::ClientToHub::OpenSession { agent, workspace })
        .await
        .context("hub send")?;

    let app = tui::App {
        tx: wire.tx,
        rx: wire.rx,
    };
    tui::run(app).await
}
