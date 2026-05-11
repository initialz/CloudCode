mod audit;
mod auth;
mod config;
mod proxy;
mod registry;
mod tunnel;
mod ws_handler;

use anyhow::{anyhow, Context};
use axum::{routing::get, routing::post, Router};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::audit::AuditLog;
use crate::config::Config;
use crate::registry::AgentRegistry;

pub struct AppState {
    pub config: Config,
    pub http: reqwest::Client,
    pub audit: AuditLog,
    pub registry: Arc<AgentRegistry>,
}

#[derive(Parser)]
#[command(name = "cloudcode-hub", about = "Cloudcode hub: LLM API gateway")]
struct Cli {
    /// Path to hub config. With no subcommand, hub runs in the foreground
    /// using this config and streams logs to stdout.
    #[arg(short, long, default_value = "hub.toml", global = true)]
    config: PathBuf,

    /// One-time setup: write a fresh hub.toml at `--config` (defaults to
    /// ./hub.toml). Refuses to overwrite if the file already exists.
    #[arg(long)]
    init: bool,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// 为一个账号生成新 token，输出明文（仅此一次）和 hash（写入 hub.toml）
    GenToken {
        /// 账号名称
        name: String,
    },
    /// 后台管理 hub daemon（start/stop/restart/status）
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
                .unwrap_or_else(|_| "info,cloudcode_hub=debug".into()),
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
        Some(Cmd::GenToken { name }) => gen_token(&name),
        Some(Cmd::Daemon { cmd }) => cloudcode_daemon::run("hub", "hub.toml", cmd),
    }
}

async fn serve(config_path: PathBuf) -> anyhow::Result<()> {
    let config =
        Config::load(&config_path).with_context(|| format!("loading {}", config_path.display()))?;
    let audit = AuditLog::open(&config.server.audit_log)?;
    let http = reqwest::Client::builder().build()?;
    let listen = config.server.listen.clone();

    let state = Arc::new(AppState {
        config,
        http,
        audit,
        registry: Arc::new(AgentRegistry::new()),
    });

    let app = Router::new()
        .route("/anthropic/v1/messages", post(proxy::anthropic_messages))
        .route("/v1/agent/ws", get(ws_handler::upgrade))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("binding {}", listen))?;
    tracing::info!("cloudcode hub listening on {}", listen);

    axum::serve(listener, app).await?;
    Ok(())
}

fn gen_token(name: &str) -> anyhow::Result<()> {
    let token = auth::generate_token();
    let hash = auth::hash_token(&token)?;
    println!("# Account: {}", name);
    println!("# Token (give to user, will not be shown again):");
    println!("{}", token);
    println!();
    println!("# Add to hub.toml:");
    println!("[[accounts]]");
    println!("name = \"{}\"", name);
    println!("token_hash = \"{}\"", hash);
    println!("allowed_providers = [\"anthropic\"]");
    Ok(())
}

/// Write a fresh hub.toml. Refuses to overwrite an existing file.
fn init_config(path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        return Err(anyhow!(
            "{} already exists; refusing to overwrite. Delete it first if you really want to re-init.",
            path.display()
        ));
    }

    let template = r#"# Cloudcode Hub config. Reverse-WS gateway for Claude.

[server]
# Listen address. Bind behind a TLS-terminating reverse proxy (nginx /
# caddy) in production. The agent dials wss://<your-host>/v1/agent/ws.
listen = "0.0.0.0:7000"
audit_log = "./audit.jsonl"

# Optional: direct API-key fallback. Uncomment and fill if you want
# accounts with allowed_providers = ["anthropic"] to fall through here
# when no allowed agent is online.
# [anthropic]
# upstream = "https://api.anthropic.com"
# api_key  = "sk-ant-..."

# Subscription-mode agent slots. Each [[agents]] entry authorises one
# agent to connect; the agent presents (name, plaintext secret) in its
# hello frame, hub argon2-verifies against shared_secret_hash.
# Add entries with the block printed by `cloudcode-agent --init`.
# [[agents]]
# name = "peter-mbp"
# shared_secret_hash = "$argon2id$v=19$..."

# Accounts. Generate token + hash with:
#   cloudcode-hub gen-token alice
# - allowed_providers: ["anthropic"] or ["*"] to allow the API-key fallback.
# - allowed_agents:    names of [[agents]] this account may route to
#                      (first online wins).
# [[accounts]]
# name = "alice"
# token_hash = "$argon2id$v=19$..."
# allowed_providers = ["anthropic"]
# allowed_agents = []
"#;

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
    }
    std::fs::write(path, template).with_context(|| format!("writing {}", path.display()))?;

    println!("# Wrote {}", path.display());
    println!();
    println!("# Next steps:");
    println!("#   1) Generate per-user tokens:");
    println!("#        cloudcode-hub gen-token alice");
    println!(
        "#      Paste the printed [[accounts]] block into {}.",
        path.display()
    );
    println!("#   2) Have each agent run `cloudcode-agent --init` and paste");
    println!(
        "#      its printed [[agents]] block into {}.",
        path.display()
    );
    println!("#   3) Optionally configure [anthropic] for API-key fallback.");
    println!(
        "#   4) Start the hub: cloudcode-hub --config {}",
        path.display()
    );
    Ok(())
}
