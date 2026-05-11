mod audit;
mod auth;
mod config;
mod proxy;

use anyhow::Context;
use axum::{routing::get, routing::post, Router};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;

use crate::audit::AuditLog;
use crate::config::Config;

pub struct AppState {
    pub config: Config,
    pub http: reqwest::Client,
    pub audit: AuditLog,
}

#[derive(Parser)]
#[command(name = "cloudcode-hub", about = "Cloudcode hub: LLM API gateway")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 启动 hub 服务
    Serve {
        #[arg(short, long, default_value = "hub.toml")]
        config: PathBuf,
    },
    /// 为一个账号生成新 token，输出明文（仅此一次）和 hash（写入 hub.toml）
    GenToken {
        /// 账号名称
        name: String,
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

    match Cli::parse().cmd {
        Cmd::Serve { config } => serve(config).await,
        Cmd::GenToken { name } => gen_token(&name),
    }
}

async fn serve(config_path: PathBuf) -> anyhow::Result<()> {
    let config = Config::load(&config_path)
        .with_context(|| format!("loading {}", config_path.display()))?;
    let audit = AuditLog::open(&config.server.audit_log)?;
    let http = reqwest::Client::builder().build()?;
    let listen = config.server.listen.clone();

    let state = Arc::new(AppState {
        config,
        http,
        audit,
    });

    let app = Router::new()
        .route("/anthropic/v1/messages", post(proxy::anthropic_messages))
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
