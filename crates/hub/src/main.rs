mod audit;
mod auth;
mod config;
mod proxy;
mod registry;
mod tunnel;
mod ws_handler;

use anyhow::Context;
use axum::{routing::get, routing::post, Router};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
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
