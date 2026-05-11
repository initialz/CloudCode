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
    about = "Cloudcode client: open an interactive claude session on a remote agent"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open a TUI chat against a remote claude.
    Chat {
        /// Pin the session to a specific agent name.
        #[arg(long)]
        agent: Option<String>,
        /// Open this workspace immediately (default: "default").
        #[arg(long, default_value = "default")]
        workspace: String,
    },
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
    let cli = Cli::parse();
    let result = match cli.cmd {
        Cmd::Chat { agent, workspace } => run_chat(agent, workspace).await,
        Cmd::Config => show_config(),
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
