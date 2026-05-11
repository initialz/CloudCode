mod auth;
mod config;
mod credentials;
mod name;
mod proxy;
mod refresh;
mod tunnel;
mod ws;

use anyhow::Context;
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::Config;
use crate::credentials::CredentialsStore;

pub struct AppState {
    pub name: String,
    pub config: Config,
    pub http: reqwest::Client,
    pub credentials: Arc<CredentialsStore>,
}

#[derive(Parser)]
#[command(
    name = "cloudcode-agent",
    about = "Cloudcode agent: dials out to a hub via WebSocket and serves its claude OAuth credentials"
)]
struct Cli {
    /// Path to agent config. With no subcommand, agent runs in the foreground
    /// using this config and streams logs to stdout. On first run, if the
    /// file is missing, a template (with an auto-generated shared_secret)
    /// is written here and the agent exits so you can edit it.
    #[arg(short, long, default_value = "agent.toml", global = true)]
    config: PathBuf,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// 后台管理 agent daemon（start/stop/restart/status）
    Daemon {
        #[command(subcommand)]
        cmd: cloudcode_daemon::DaemonCmd,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,cloudcode_agent=debug".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        None => serve(cli.config).await,
        Some(Cmd::Daemon { cmd }) => cloudcode_daemon::run("agent", "agent.toml", cmd),
    }
}

async fn serve(config_path: PathBuf) -> anyhow::Result<()> {
    if !config_path.exists() {
        init_config(&config_path)?;
        return Ok(());
    }

    let config =
        Config::load(&config_path).with_context(|| format!("loading {}", config_path.display()))?;
    let credentials = Arc::new(
        CredentialsStore::load(config.claude.credentials_path.clone()).with_context(|| {
            format!(
                "loading credentials from {}",
                config.claude.credentials_path.display()
            )
        })?,
    );
    let http = reqwest::Client::builder().build()?;

    let name = config
        .agent
        .name
        .clone()
        .unwrap_or_else(name::default_agent_name);
    tracing::info!(agent = %name, "starting cloudcode-agent");

    refresh::spawn(credentials.clone(), http.clone());

    let state = Arc::new(AppState {
        name,
        config,
        http,
        credentials,
    });

    ws::run(state).await
}

/// Write a fresh agent.toml with an auto-generated shared_secret, and print
/// the matching [[agents]] block (containing the argon2id hash) so the user
/// can hand it to the hub admin.
fn init_config(path: &Path) -> anyhow::Result<()> {
    let secret = auth::generate_secret();
    let hash = auth::hash_secret(&secret)?;
    let agent_name = name::default_agent_name();

    let template = format!(
        r#"# Auto-generated on first run. Edit [hub].url before re-running.

[hub]
url = "wss://hub.example.com/v1/agent/ws"

[agent]
# Auto-detected from hostname-user; override if hub reports name_taken.
# name = "{agent_name}"

[auth]
shared_secret = "{secret}"

# [claude] section is optional; defaults read ~/.claude/.credentials.json.
# [claude]
# credentials_path = "/custom/path/credentials.json"
# upstream         = "https://api.anthropic.com"
# anthropic_beta   = ["oauth-2025-04-20"]
"#
    );

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
    }
    std::fs::write(path, template).with_context(|| format!("writing {}", path.display()))?;

    println!("# Wrote {}", path.display());
    println!("# Auto-detected agent name: {}", agent_name);
    println!();
    println!("# Give the following block to your hub admin to paste into hub.toml:");
    println!("[[agents]]");
    println!("name = \"{}\"", agent_name);
    println!("shared_secret_hash = \"{}\"", hash);
    println!();
    println!("# Then edit [hub].url in {} and re-run.", path.display());
    Ok(())
}
