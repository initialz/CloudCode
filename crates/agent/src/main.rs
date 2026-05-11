mod auth;
mod config;
mod credentials;
mod proxy;
mod refresh;

use anyhow::Context;
use axum::{routing::get, routing::post, Router};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;

use crate::config::Config;
use crate::credentials::CredentialsStore;

pub struct AppState {
    pub config: Config,
    pub http: reqwest::Client,
    pub credentials: Arc<CredentialsStore>,
}

#[derive(Parser)]
#[command(
    name = "cloudcode-agent",
    about = "Cloudcode agent: forwards hub requests to Anthropic using locally-stored claude OAuth credentials"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start the agent.
    Serve {
        #[arg(short, long, default_value = "agent.toml")]
        config: PathBuf,
    },
    /// Generate a new shared secret + hash for hub<->agent auth.
    GenSecret,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,cloudcode_agent=debug".into()),
        )
        .init();

    match Cli::parse().cmd {
        Cmd::Serve { config } => serve(config).await,
        Cmd::GenSecret => gen_secret(),
    }
}

async fn serve(config_path: PathBuf) -> anyhow::Result<()> {
    let config = Config::load(&config_path)
        .with_context(|| format!("loading {}", config_path.display()))?;
    let credentials = Arc::new(
        CredentialsStore::load(config.claude.credentials_path.clone()).with_context(|| {
            format!(
                "loading credentials from {}",
                config.claude.credentials_path.display()
            )
        })?,
    );
    let http = reqwest::Client::builder().build()?;
    let listen = config.server.listen.clone();

    refresh::spawn(credentials.clone(), http.clone());

    let state = Arc::new(AppState {
        config,
        http,
        credentials,
    });

    let app = Router::new()
        .route("/v1/messages", post(proxy::messages))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("binding {}", listen))?;
    tracing::info!("cloudcode-agent listening on {}", listen);

    axum::serve(listener, app).await?;
    Ok(())
}

fn gen_secret() -> anyhow::Result<()> {
    let secret = auth::generate_secret();
    let hash = auth::hash_secret(&secret)?;
    println!("# Shared secret (give to hub admin, will not be shown again):");
    println!("{}", secret);
    println!();
    println!("# Add to agent.toml:");
    println!("[auth]");
    println!("shared_secret_hash = \"{}\"", hash);
    Ok(())
}
