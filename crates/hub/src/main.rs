mod admin;
mod app;
mod audit;
mod auth;
mod config;
mod db;
mod pty_proto;
mod pty_session;
mod registry;
mod supervise;
mod tunnel;
mod update;
mod viewer_session;
mod ws_handler;

use anyhow::{anyhow, Context};
use axum::{
    extract::DefaultBodyLimit,
    middleware,
    routing::{delete, get, post},
    Router,
};
use clap::{Parser, Subcommand};
use dashmap::DashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

use crate::app::UserAuth;
use crate::audit::AuditLog;
use crate::config::Config;
use crate::db::Db;
use crate::registry::AgentRegistry;

pub struct AppState {
    pub config: Config,
    pub audit: AuditLog,
    pub db: Db,
    pub registry: Arc<AgentRegistry>,
    /// (agent_name, account_name, workspace_name) -> session_id, used as a
    /// global mutex so two sessions can't drive `claude` in the same
    /// account+workspace at once. Different accounts on the same agent get
    /// separate namespaces.
    pub workspaces: DashMap<(String, String, String), Uuid>,
    /// User-facing webterm SPA session store. Maps an HttpOnly cookie
    /// session id to the logged-in account name. Backs `/api/*`
    /// and the cookie-auth path through `/v1/pty/ws`.
    pub user_auth: Arc<UserAuth>,
    /// Per-account broadcast channels used by the admin "disconnect"
    /// action. Each live `/v1/pty/ws` connection subscribes; the
    /// admin endpoint signals every subscriber for a given account,
    /// which closes the WS after sending a terminal `Rejected` frame.
    /// Entries are created lazily on first subscribe and stay around
    /// across reconnects (capacity is tiny — one Sender per account).
    pub user_kick: Arc<DashMap<String, tokio::sync::broadcast::Sender<()>>>,
}

impl AppState {
    /// Get-or-create the kick channel for an account. Used both by
    /// the user WS handler (to subscribe) and by the admin endpoint
    /// (to fire). Capacity is 8 because the only thing we ever send
    /// is a single unit; we never expect backlog.
    pub fn user_kick_sender(&self, account: &str) -> tokio::sync::broadcast::Sender<()> {
        if let Some(s) = self.user_kick.get(account) {
            return s.clone();
        }
        self.user_kick
            .entry(account.to_string())
            .or_insert_with(|| tokio::sync::broadcast::channel::<()>(8).0)
            .clone()
    }

    /// Cheap read-only check: does this account have at least one
    /// live `/v1/pty/ws` connection? Used by the admin UI to decide
    /// whether the "Disconnect" button should be active even before
    /// the user has opened a workspace (i.e. WS attached, no PTY
    /// session yet). Doesn't materialise a Sender for unknown
    /// accounts.
    pub fn user_is_connected(&self, account: &str) -> bool {
        self.user_kick
            .get(account)
            .map(|s| s.receiver_count() > 0)
            .unwrap_or(false)
    }
}

#[derive(Parser)]
#[command(name = "cloudcode-hub", version, about = "Cloudcode hub: claude task gateway")]
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
    /// Keep a hub process alive: spawn the hub binary, restart it on
    /// crash with exponential backoff, restart it immediately on a
    /// clean exit (so self-update rolls forward). Forwards SIGTERM /
    /// SIGINT to the child. Normally the child of `daemon start` —
    /// not meant to be run directly.
    Supervise,
    /// 后台管理 hub daemon（start/stop/restart/status）— daemon `start`
    /// 实际 spawn 的是 `cloudcode-hub supervise`，因此 self-update 能在
    /// 后台运行时透明热切换。
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
        Some(Cmd::GenToken { name }) => gen_token(&name, &cli.config),
        Some(Cmd::Supervise) => supervise::run(cli.config),
        // daemon start re-execs us with the `supervise` subcommand so
        // crashes (and self-update exits) are recovered transparently.
        Some(Cmd::Daemon { cmd }) => {
            cloudcode_daemon::run_with_prefix("hub", "hub.toml", cmd, &["supervise"])
        }
    }
}

async fn serve(config_path: PathBuf) -> anyhow::Result<()> {
    let config =
        Config::load(&config_path).with_context(|| format!("loading {}", config_path.display()))?;
    let db = Db::open(&config.admin.db_path)
        .await
        .with_context(|| format!("opening admin db at {}", config.admin.db_path.display()))?;
    migrate_accounts_from_toml(&db, &config).await?;
    match db.close_orphan_sessions("hub restart").await {
        Ok(n) if n > 0 => {
            tracing::info!(n, "closed orphan sessions left over from previous hub run");
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "orphan session cleanup failed"),
    }
    // Sweep stale admin / webterm cookies whose TTL has lapsed. Keeps
    // the new persistent-session tables from growing unboundedly
    // across long uptimes + frequent re-logins.
    match db.cleanup_expired_sessions().await {
        Ok(n) if n > 0 => {
            tracing::info!(n, "swept expired admin/webterm sessions");
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "expired-session cleanup failed"),
    }
    let audit = AuditLog::open(&config.server.audit_log, db.clone())?;
    let listen = config.server.listen.clone();

    let user_auth = Arc::new(UserAuth::new(db.clone()));
    let state = Arc::new(AppState {
        config,
        audit,
        db,
        registry: Arc::new(AgentRegistry::new()),
        workspaces: DashMap::new(),
        user_auth,
        user_kick: Arc::new(DashMap::new()),
    });

    // Root — user-facing webterm SPA + its JSON API. Lives on the
    // same listener as /v1/pty/ws so cookie auth carries through to
    // the WebSocket upgrade. Explicit routes (/v1/*, /healthz, /api/*,
    // /assets/*) resolve before the `/*spa` wildcard fallback.
    let user_gate = middleware::from_fn_with_state(state.clone(), app::require_user);
    let app_router = Router::new()
        .route("/api/login", post(app::api::login))
        .route("/api/logout", post(app::api::logout))
        // Public invite endpoints — deliberately NOT under user_gate
        // since invitees don't have an account yet. The token in the
        // path is the auth: anyone with the URL can read the info /
        // accept the invite, that's the whole point of a shareable
        // link.
        .route("/api/invite/:token/info", get(app::api::invite_info))
        .route("/api/invite/:token/accept", post(app::api::invite_accept))
        .route(
            "/api/me",
            get(app::api::me)
                .put(app::api::update_me)
                .route_layer(user_gate.clone()),
        )
        .route(
            "/api/preferences",
            get(app::api::get_preferences)
                .put(app::api::put_preferences)
                .route_layer(user_gate.clone()),
        )
        .route(
            "/api/files/list",
            get(app::api::files_list).route_layer(user_gate.clone()),
        )
        .route(
            "/api/files/download",
            get(app::api::files_download).route_layer(user_gate.clone()),
        )
        .route(
            "/api/files/archive",
            get(app::api::files_archive).route_layer(user_gate.clone()),
        )
        .route(
            "/api/files/upload",
            post(app::api::files_upload)
                .route_layer(user_gate.clone())
                .layer(DefaultBodyLimit::max(1024 * 1024 * 1024)), // 1 GiB
        )
        .route(
            "/api/files/delete",
            delete(app::api::files_delete).route_layer(user_gate.clone()),
        )
        // Standalone screencast verify page. Explicit route so the `/*spa`
        // wildcard below can't swallow it.
        .route("/viewer", get(app::viewer::serve_viewer_html))
        .route("/", get(app::assets::serve_index))
        .route("/assets/*path", get(app::assets::serve_asset))
        .route("/*spa", get(app::assets::serve_spa));

    let app = Router::new()
        .route("/v1/pty/ws", get(pty_session::upgrade))
        .route("/v1/viewer/ws", get(viewer_session::upgrade))
        .route("/v1/agent/ws", get(ws_handler::upgrade))
        .route("/healthz", get(|| async { "ok" }))
        .merge(app_router)
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("binding {}", listen))?;
    tracing::info!("cloudcode hub listening on {}", listen);

    // Optional admin server on a separate listener. Runs only if
    // [admin].token_hash is set in hub.toml (i.e. you've run --init at
    // least once on a v0.7+ hub).
    if let Some(token_hash) = state.config.admin.token_hash.clone() {
        let admin_listen = state.config.admin.listen.clone();
        let admin_state = admin::AdminState {
            app: state.clone(),
            auth: Arc::new(admin::AdminAuth::new(state.db.clone(), token_hash)),
            releases: Arc::new(admin::ReleasesCache::new()),
        };
        let admin_app = admin::router(admin_state);
        let admin_listener = tokio::net::TcpListener::bind(&admin_listen)
            .await
            .with_context(|| format!("binding admin listener on {}", admin_listen))?;
        tracing::info!(admin = %admin_listen, "admin UI ready (login at /admin/login)");
        tokio::spawn(async move {
            if let Err(e) = axum::serve(admin_listener, admin_app).await {
                tracing::error!(error = %e, "admin server stopped");
            }
        });
    } else {
        tracing::info!("admin UI disabled — set [admin].token_hash in hub.toml");
    }

    axum::serve(listener, app).await?;
    Ok(())
}

/// One-shot import of accounts inline in hub.toml into the SQLite db.
/// Only runs when the db has zero accounts; subsequent hub starts skip
/// this (the db is the source of truth). The token_hash in toml is
/// already argon2id, so we copy it as-is. token_prefix is unknown for
/// imported rows — admin UI will display "(legacy)" for those.
async fn migrate_accounts_from_toml(db: &Db, config: &Config) -> anyhow::Result<()> {
    if config.accounts.is_empty() {
        return Ok(());
    }
    let existing = db.account_count().await?;
    if existing > 0 {
        return Ok(());
    }
    let mut imported = 0;
    for a in &config.accounts {
        if let Err(e) = db.insert_account(&a.name, &a.token_hash, None, None).await {
            tracing::warn!(account = %a.name, error = %e, "import account skipped");
            continue;
        }
        imported += 1;
    }
    if imported > 0 {
        tracing::info!(
            count = imported,
            "imported accounts from hub.toml into admin db (further changes happen in db only)"
        );
    }
    Ok(())
}

fn gen_token(name: &str, config_path: &Path) -> anyhow::Result<()> {
    if !config_path.exists() {
        return Err(anyhow!(
            "{} not found; run `cloudcode-hub --init --config {}` first",
            config_path.display(),
            config_path.display()
        ));
    }

    let original = std::fs::read_to_string(config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    let mut doc: toml_edit::DocumentMut = original
        .parse()
        .with_context(|| format!("parsing {}", config_path.display()))?;

    let account_exists = doc
        .get("accounts")
        .and_then(|v| v.as_array_of_tables())
        .map(|arr| {
            arr.iter()
                .any(|t| t.get("name").and_then(|v| v.as_str()) == Some(name))
        })
        .unwrap_or(false);

    let action = if account_exists {
        if !confirm_overwrite(name)? {
            println!("aborted; existing token for '{}' kept.", name);
            return Ok(());
        }
        "rotated"
    } else {
        "added"
    };

    let token = auth::generate_token();
    let hash = auth::hash_token(&token)?;

    if account_exists {
        // Rotate in place — keep the user's surrounding comments / order.
        let arr = doc
            .get_mut("accounts")
            .and_then(|v| v.as_array_of_tables_mut())
            .expect("checked above");
        for table in arr.iter_mut() {
            if table.get("name").and_then(|v| v.as_str()) == Some(name) {
                table["token_hash"] = toml_edit::value(hash.clone());
                break;
            }
        }
    } else {
        // Append a new [[accounts]] entry.
        let arr = doc
            .entry("accounts")
            .or_insert_with(|| toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()))
            .as_array_of_tables_mut()
            .ok_or_else(|| anyhow!("`accounts` exists but is not an array of tables"))?;
        let mut table = toml_edit::Table::new();
        table["name"] = toml_edit::value(name);
        table["token_hash"] = toml_edit::value(hash.clone());
        arr.push(table);
    }

    let new_contents = doc.to_string();
    std::fs::write(config_path, new_contents)
        .with_context(|| format!("writing {}", config_path.display()))?;

    println!("# Account: {}", name);
    println!("# Token (give to user, will not be shown again):");
    println!("{}", token);
    println!();
    println!("# {} account in {}.", action, config_path.display());
    println!("# Restart the hub for the change to take effect.");
    Ok(())
}

fn confirm_overwrite(name: &str) -> anyhow::Result<bool> {
    use std::io::{BufRead, IsTerminal, Write};
    eprint!("account '{}' already exists. Overwrite token? [y/N] ", name);
    std::io::stderr().flush().ok();
    if !std::io::stdin().is_terminal() {
        return Err(anyhow!(
            "stdin is not a tty; refusing to clobber. Re-run from an interactive shell, \
             or delete the existing [[accounts]] block manually."
        ));
    }
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim().to_lowercase().as_str(), "y" | "yes"))
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
    let admin_token = auth::generate_admin_token();
    let admin_token_hash = auth::hash_token(&admin_token)?;

    let template = format!(
        r#"# Cloudcode Hub config. Task gateway for `claude` subprocesses
# running on remote agents.

[server]
# Listen address. Bind behind a TLS-terminating reverse proxy (nginx /
# caddy) in production. Agents dial wss://<your-host>/v1/agent/ws.
listen = "0.0.0.0:7100"
audit_log = "./audit.jsonl"

[agents]
# argon2id hash of the global agent registration token. Any agent that
# presents the matching plaintext token in its hello frame is accepted;
# agent names are first-come, first-served at runtime (no pre-registration).
# To rotate: re-run `cloudcode-hub --init` against a fresh hub.toml.
registration_token_hash = "{agent_token_hash}"

# Accounts. Once the hub starts these are imported into the admin db
# and managed from the admin UI / `cloudcode-hub gen-token`.
# [[accounts]]
# name = "alice"
# token_hash = "$argon2id$v=19$..."

[admin]
# Listen address for the admin UI. 0.0.0.0 by default so a fresh
# install is reachable. The login token is the only auth, so in
# production put a TLS-terminating reverse proxy in front (caddy /
# nginx) and use a firewall / cloud security group to limit who can
# hit this port. Switch to 127.0.0.1 if you only want SSH-tunnel
# access.
listen = "0.0.0.0:7101"
# Path to the SQLite database (accounts, audit events, session records).
db_path = "./cloudcode-hub.db"
# argon2id hash of the admin UI login token. The plaintext was printed
# once by --init; if you lose it, re-run --init against a fresh hub.toml.
token_hash = "{admin_token_hash}"
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
    println!("# Admin UI login token (open the admin URL and paste this");
    println!("# once; lost == re-run --init against a fresh hub.toml):");
    println!("{}", admin_token);
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
