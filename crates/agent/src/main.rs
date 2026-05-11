mod auth;
mod claude;
mod config;
mod name;
mod tunnel;
mod ws;

use anyhow::{anyhow, Context};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::claude::SessionManager;
use crate::config::Config;

pub struct AppState {
    pub name: String,
    pub config: Config,
    pub manager: Arc<SessionManager>,
}

#[derive(Parser)]
#[command(
    name = "cloudcode-agent",
    about = "Cloudcode agent: dials a hub via WebSocket and runs claude subprocesses on demand"
)]
struct Cli {
    /// Path to agent config. With no subcommand, agent runs in the foreground
    /// using this config and streams logs to stdout.
    #[arg(short, long, default_value = "agent.toml", global = true)]
    config: PathBuf,

    /// One-time setup: write a fresh agent.toml at `--config` with an
    /// auto-generated shared_secret, and print the matching [[agents]] block
    /// for the hub admin. Refuses to overwrite if the file already exists.
    #[arg(long)]
    init: bool,

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
    // Pick rustls' ring CryptoProvider before any TLS code runs; rustls 0.23
    // requires this when crate features can't disambiguate a default.
    let _ = rustls::crypto::ring::default_provider().install_default();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,cloudcode_agent=debug".into()),
        )
        .init();

    let cli = Cli::parse();
    if cli.init {
        if cli.cmd.is_some() {
            return Err(anyhow!("--init cannot be combined with a subcommand"));
        }
        return init_config(&cli.config);
    }
    match cli.cmd {
        None => serve(cli.config).await,
        Some(Cmd::Daemon { cmd }) => cloudcode_daemon::run("agent", "agent.toml", cmd),
    }
}

async fn serve(config_path: PathBuf) -> anyhow::Result<()> {
    if !config_path.exists() {
        return Err(anyhow!(
            "{} not found; run `cloudcode-agent --init --config {}` to generate one",
            config_path.display(),
            config_path.display()
        ));
    }

    let config =
        Config::load(&config_path).with_context(|| format!("loading {}", config_path.display()))?;

    let name = config
        .agent
        .name
        .clone()
        .unwrap_or_else(name::default_agent_name);
    tracing::info!(agent = %name, "starting cloudcode-agent");

    let manager = Arc::new(SessionManager::new(config.claude.clone()));

    let state = Arc::new(AppState {
        name,
        config,
        manager,
    });

    ws::run(state).await
}

fn init_config(path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        return Err(anyhow!(
            "{} already exists; refusing to overwrite. Delete it first if you really want to re-init.",
            path.display()
        ));
    }

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

# [claude] section is optional; defaults below are usually fine. The agent
# spawns `claude` as a subprocess for every task, so claude must be installed
# and you must have run `claude /login` once as the same OS user.
# [claude]
# executable     = "claude"                            # PATH lookup by default
# workspace_root = "~/cloudcode-agent/workspaces"      # one dir per task
# extra_args     = []                                  # appended to claude args
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
