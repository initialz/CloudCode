mod audit;
mod auth;
mod config;
mod registry;
mod session;
mod session_proto;
mod tunnel;
mod ws_handler;

use anyhow::{anyhow, Context};
use axum::{routing::get, Router};
use clap::{Parser, Subcommand};
use dashmap::DashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

use crate::audit::AuditLog;
use crate::config::Config;
use crate::registry::AgentRegistry;

pub struct AppState {
    pub config: Config,
    pub audit: AuditLog,
    pub registry: Arc<AgentRegistry>,
    /// (agent_name, workspace_name) -> session_id, used as a global mutex
    /// so two sessions can't drive `claude` in the same workspace at once.
    pub workspaces: DashMap<(String, String), Uuid>,
}

#[derive(Parser)]
#[command(name = "cloudcode-hub", about = "Cloudcode hub: claude task gateway")]
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
    let listen = config.server.listen.clone();

    let state = Arc::new(AppState {
        config,
        audit,
        registry: Arc::new(AgentRegistry::new()),
        workspaces: DashMap::new(),
    });

    let app = Router::new()
        .route("/v1/session/ws", get(session::upgrade))
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

    let agent_token = auth::generate_agent_token();
    let agent_token_hash = auth::hash_token(&agent_token)?;

    let template = format!(
        r#"# Cloudcode Hub config. Task gateway for `claude` subprocesses
# running on remote agents.

[server]
# Listen address. Bind behind a TLS-terminating reverse proxy (nginx /
# caddy) in production. Agents dial wss://<your-host>/v1/agent/ws.
listen = "0.0.0.0:7000"
audit_log = "./audit.jsonl"

[agents]
# argon2id hash of the global agent registration token. Any agent that
# presents the matching plaintext token in its hello frame is accepted;
# agent names are first-come, first-served at runtime (no pre-registration).
# To rotate: re-run `cloudcode-hub --init` against a fresh hub.toml.
registration_token_hash = "{agent_token_hash}"

# Accounts. Generate per-user tokens with:
#   cloudcode-hub gen-token alice
# Any account may use any online agent — the client chooses via /agent use.
# [[accounts]]
# name = "alice"
# token_hash = "$argon2id$v=19$..."
"#
    );

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
    }
    std::fs::write(path, &template).with_context(|| format!("writing {}", path.display()))?;

    println!("# Wrote {}", path.display());
    println!();
    println!("# Agent registration token (give to every agent operator;");
    println!("# they paste it into agent.toml [auth].registration_token):");
    println!("{}", agent_token);
    println!();
    println!("# Next steps:");
    println!("#   1) Generate per-user tokens:");
    println!("#        cloudcode-hub gen-token alice");
    println!(
        "#      Paste the printed [[accounts]] block into {}.",
        path.display()
    );
    println!("#   2) Distribute the agent registration token (above) to each");
    println!("#      agent operator. They run `cloudcode-agent --init` then");
    println!("#      paste it into agent.toml.");
    println!(
        "#   3) Start the hub: cloudcode-hub --config {}",
        path.display()
    );
    Ok(())
}
