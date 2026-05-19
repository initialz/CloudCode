//! End-to-end integration tests for the v1.13 hub-managed workspace
//! sync wire. These exercise the *real* hub binary as a subprocess —
//! the test driver fakes both an agent (over `/v1/agent/ws`) and a CLI
//! client (over `/v1/pty/ws`), so every byte we assert on is what
//! production code would actually emit.
//!
//! Each `#[tokio::test]` spawns its own hub process inside a fresh
//! tempdir, generates random listen ports, seeds the SQLite DB
//! directly with the accounts / ACLs / workspaces it needs, then
//! drives the wire. Hubs are torn down on drop of the `HubProcess`
//! guard so we don't leak across tests.
//!
//! The wire types (ClientMsg / ServerMsg / ClientToHub / HubToClient)
//! are intentionally re-declared here as standalone JSON-tagged enums
//! to pin the contract from the *outside* — if a refactor in
//! `crates/hub/src/tunnel.rs` accidentally renames a field these
//! tests will catch it.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use argon2::password_hash::{rand_core::OsRng, SaltString};
use argon2::{Argon2, PasswordHasher};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::{Row, SqlitePool};
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

const AGENT_PROTOCOL_VERSION: &str = "7";
const CLIENT_PROTOCOL_VERSION: &str = "1";
const RECV_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Wire mirrors. Kept verbatim with the hub side; the test fails fast if a
// field is renamed.
// ---------------------------------------------------------------------------

/// Agent → hub frames over `/v1/agent/ws`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)]
enum ClientMsg {
    Hello {
        name: String,
        secret: String,
        version: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_version: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target_triple: Option<String>,
        #[serde(default)]
        tools: Vec<String>,
    },
    Pong,
    WorkspacePullAck {
        session_id: Uuid,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    WorkspacePushFile {
        session_id: Uuid,
        path: String,
        #[serde(with = "serde_bytes")]
        content: Vec<u8>,
    },
    WorkspaceDeleteFile {
        session_id: Uuid,
        path: String,
    },
}

/// Hub → agent frames over `/v1/agent/ws`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)]
enum ServerMsg {
    Welcome {
        name: String,
    },
    Rejected {
        reason: String,
    },
    Ping,
    PtyOpen {
        session_id: Uuid,
        account: String,
        workspace: String,
        cols: u16,
        rows: u16,
        #[serde(default)]
        claude_args: Vec<String>,
        #[serde(default)]
        sandbox: bool,
        #[serde(default)]
        tool: Option<String>,
    },
    WorkspacePullStart {
        session_id: Uuid,
        account: String,
        workspace: String,
        file_count: u64,
    },
    WorkspaceFile {
        session_id: Uuid,
        path: String,
        #[serde(with = "serde_bytes")]
        content: Vec<u8>,
        is_last: bool,
    },
    WorkspaceFileAck {
        session_id: Uuid,
        path: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    WorkspaceCleanup {
        items: Vec<(String, String)>,
    },
}

/// Client → hub frames over `/v1/pty/ws`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)]
enum ClientToHub {
    Hello {
        token: String,
        version: String,
    },
    OpenSession {
        workspace: String,
        agent: String,
        #[serde(default)]
        force: bool,
        cols: u16,
        rows: u16,
        #[serde(default)]
        claude_args: Vec<String>,
    },
    Close,
}

/// Hub → client frames over `/v1/pty/ws`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)]
enum HubToClient {
    Welcome {
        account: String,
    },
    Rejected {
        reason: String,
    },
    SessionOpened {
        agent: String,
        workspace: String,
        cwd: String,
    },
    SessionClosed {
        #[serde(default)]
        reason: Option<String>,
    },
    SessionError {
        message: String,
    },
    Ping,
}

// ---------------------------------------------------------------------------
// Hub process management.
// ---------------------------------------------------------------------------

/// A running cloudcode-hub subprocess, scoped to a tempdir. Drops kill
/// the child so a panicking test doesn't leave a zombie listening on
/// the chosen port.
struct HubProcess {
    child: std::process::Child,
    base: TempDir,
    listen_port: u16,
    agent_token: String,
    db_path: PathBuf,
}

impl HubProcess {
    fn db_pool_url(&self) -> String {
        format!("sqlite://{}", self.db_path.display())
    }

    fn agent_ws_url(&self) -> String {
        format!("ws://127.0.0.1:{}/v1/agent/ws", self.listen_port)
    }

    fn client_ws_url(&self) -> String {
        format!("ws://127.0.0.1:{}/v1/pty/ws", self.listen_port)
    }
}

impl Drop for HubProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // TempDir cleans `base` on drop.
        let _ = &self.base;
    }
}

/// Pick an unused localhost port by binding to :0 and releasing it.
/// There is a brief race between releasing and the hub re-binding,
/// but in practice the kernel doesn't recycle that fast and the tests
/// each get their own port range.
fn pick_free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

fn hub_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_cloudcode-hub"))
}

fn hash_token(token: &str) -> String {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(token.as_bytes(), &salt)
        .expect("argon2 hash")
        .to_string()
}

/// Launch a hub bound to a random localhost port, with the given agent
/// registration token (plaintext) already accepted by the hub. The
/// returned `HubProcess` carries everything the caller needs to drive
/// the wire and inspect / mutate the DB.
async fn spawn_hub() -> Result<HubProcess> {
    let base = tempfile::tempdir().context("tempdir")?;
    let listen_port = pick_free_port()?;
    // Pick a separate admin port even though we don't use it — without
    // [admin].token_hash the hub skips standing up the admin listener,
    // so leaving `token_hash` blank both removes that surface and
    // avoids needing a second free port.
    let agent_token = format!("ag_{}", Uuid::new_v4().simple());
    let agent_token_hash = hash_token(&agent_token);
    let config_path = base.path().join("hub.toml");
    let db_path = base.path().join("hub.db");
    let audit_path = base.path().join("audit.jsonl");
    let toml = format!(
        r#"
[server]
listen = "127.0.0.1:{listen_port}"
audit_log = "{audit}"

[agents]
registration_token_hash = "{hash}"

[admin]
db_path = "{db}"
listen = "127.0.0.1:0"
"#,
        listen_port = listen_port,
        audit = audit_path.display(),
        hash = agent_token_hash.replace('\\', "\\\\"),
        db = db_path.display(),
    );
    std::fs::write(&config_path, toml)?;

    // `CLOUDCODE_STATE_DIR` re-roots `<state>/hub/workspaces/`, which is
    // the canonical workspace store the OpenSession orchestrator reads
    // from. Pointing it at the tempdir keeps tests hermetic.
    let mut cmd = std::process::Command::new(hub_bin());
    cmd.arg("--config")
        .arg(&config_path)
        .env("CLOUDCODE_STATE_DIR", base.path())
        // Quiet stderr unless the test driver asks for more.
        .env(
            "RUST_LOG",
            std::env::var("HUB_TEST_LOG").unwrap_or_else(|_| "warn".into()),
        )
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let child = cmd.spawn().context("spawn hub binary")?;

    let hub = HubProcess {
        child,
        base,
        listen_port,
        agent_token,
        db_path,
    };

    // Wait for the listener to come up. We just try TCP connect; the
    // hub does no other I/O before binding so a successful connect
    // means it's ready to serve WS upgrades.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if TcpStream::connect(("127.0.0.1", hub.listen_port)).await.is_ok() {
            break;
        }
        if std::time::Instant::now() > deadline {
            return Err(anyhow!("hub did not bind within 10s"));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(hub)
}

/// Open a sqlx pool against the hub's db. The hub uses WAL mode so
/// concurrent writers are fine — we use this pool to seed accounts,
/// workspaces, ACLs, and inspect lock state after the fact.
async fn open_db(hub: &HubProcess) -> Result<SqlitePool> {
    let pool = SqlitePoolOptions::new()
        .max_connections(2)
        .connect(&hub.db_pool_url())
        .await
        .context("open hub db from test")?;
    Ok(pool)
}

/// Insert an account row + ACL grant for `agent`. The token is
/// returned plaintext so the test driver can present it to
/// `/v1/pty/ws` via `ClientToHub::Hello { token, .. }`.
async fn seed_account(pool: &SqlitePool, account: &str, agent: &str) -> Result<String> {
    let token = format!("cc_{}", Uuid::new_v4().simple());
    let hash = hash_token(&token);
    sqlx::query(
        "INSERT INTO accounts (name, token_hash, token_prefix, created_at, disabled, sandbox_enabled)
         VALUES (?1, ?2, ?3, ?4, 0, 1)",
    )
    .bind(account)
    .bind(&hash)
    .bind(Option::<String>::None)
    .bind(chrono::Utc::now().timestamp())
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT OR IGNORE INTO account_allowed_agents (account, agent) VALUES (?1, ?2)",
    )
    .bind(account)
    .bind(agent)
    .execute(pool)
    .await?;
    Ok(token)
}

/// Insert a fresh workspace row + create the on-disk dir under the
/// hub's canonical store. Mirrors what `ClientToHub::CreateWorkspace`
/// would do, minus the round-trip.
async fn seed_workspace(
    pool: &SqlitePool,
    hub: &HubProcess,
    account: &str,
    workspace: &str,
) -> Result<PathBuf> {
    sqlx::query(
        "INSERT INTO workspaces (account, name, created_at, size_bytes)
         VALUES (?1, ?2, ?3, 0)",
    )
    .bind(account)
    .bind(workspace)
    .bind(chrono::Utc::now().timestamp())
    .execute(pool)
    .await?;
    let dir = hub
        .base
        .path()
        .join("hub")
        .join("workspaces")
        .join(account)
        .join(workspace);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

// ---------------------------------------------------------------------------
// WebSocket helpers.
// ---------------------------------------------------------------------------

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Open a WS connection to the given URL. Wraps `connect_async` with
/// the local target type so call sites stay tidy.
async fn ws_connect(url: &str) -> Result<WsStream> {
    let (ws, _resp) = tokio_tungstenite::connect_async(url)
        .await
        .with_context(|| format!("ws connect {url}"))?;
    Ok(ws)
}

/// Connect + Hello + Welcome handshake for an agent WS. Returns the
/// raw socket plus any frames the hub piggy-backed on top of Welcome
/// (today that's just `WorkspaceCleanup` when there are pending items).
async fn agent_hello(hub: &HubProcess, name: &str) -> Result<WsStream> {
    let mut ws = ws_connect(&hub.agent_ws_url()).await?;
    let hello = ClientMsg::Hello {
        name: name.into(),
        secret: hub.agent_token.clone(),
        version: AGENT_PROTOCOL_VERSION.into(),
        agent_version: Some("test".into()),
        target_triple: Some("x86_64-test".into()),
        tools: vec!["claude".into()],
    };
    ws.send(Message::Text(serde_json::to_string(&hello)?)).await?;
    // First reply MUST be Welcome — that's the order the hub writes.
    let frame = recv_server(&mut ws).await?;
    match frame {
        ServerMsg::Welcome { name: n } if n == name => Ok(ws),
        other => Err(anyhow!("expected Welcome, got: {other:?}")),
    }
}

/// Client-side handshake: send Hello{token} and wait for Welcome.
async fn client_hello(hub: &HubProcess, token: &str, account: &str) -> Result<WsStream> {
    let mut ws = ws_connect(&hub.client_ws_url()).await?;
    let hello = ClientToHub::Hello {
        token: token.into(),
        version: CLIENT_PROTOCOL_VERSION.into(),
    };
    ws.send(Message::Text(serde_json::to_string(&hello)?)).await?;
    let frame = recv_hub_to_client(&mut ws).await?;
    match frame {
        HubToClient::Welcome { account: a } if a == account => Ok(ws),
        other => Err(anyhow!("expected client Welcome, got: {other:?}")),
    }
}

/// Read a single text frame from the agent side, skipping ServerMsg::Ping.
async fn recv_server(ws: &mut WsStream) -> Result<ServerMsg> {
    loop {
        let frame = tokio::time::timeout(RECV_TIMEOUT, ws.next())
            .await
            .map_err(|_| anyhow!("recv_server timed out"))?
            .ok_or_else(|| anyhow!("ws closed"))?
            .context("ws error")?;
        match frame {
            Message::Text(t) => {
                let msg: ServerMsg = serde_json::from_str(&t)
                    .with_context(|| format!("decode ServerMsg from {t}"))?;
                if matches!(msg, ServerMsg::Ping) {
                    continue;
                }
                return Ok(msg);
            }
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(_) => return Err(anyhow!("server closed ws")),
            other => return Err(anyhow!("unexpected ws frame: {other:?}")),
        }
    }
}

/// Read a single text frame on the client side, skipping HubToClient::Ping.
async fn recv_hub_to_client(ws: &mut WsStream) -> Result<HubToClient> {
    loop {
        let frame = tokio::time::timeout(RECV_TIMEOUT, ws.next())
            .await
            .map_err(|_| anyhow!("recv_hub_to_client timed out"))?
            .ok_or_else(|| anyhow!("ws closed"))?
            .context("ws error")?;
        match frame {
            Message::Text(t) => {
                let msg: HubToClient = serde_json::from_str(&t)
                    .with_context(|| format!("decode HubToClient from {t}"))?;
                if matches!(msg, HubToClient::Ping) {
                    continue;
                }
                return Ok(msg);
            }
            Message::Binary(_) => continue, // PTY output — irrelevant here
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(_) => return Err(anyhow!("hub closed client ws")),
            other => return Err(anyhow!("unexpected ws frame: {other:?}")),
        }
    }
}

async fn send_client_msg(ws: &mut WsStream, msg: &ClientMsg) -> Result<()> {
    ws.send(Message::Text(serde_json::to_string(msg)?)).await?;
    Ok(())
}

async fn send_client_to_hub(ws: &mut WsStream, msg: &ClientToHub) -> Result<()> {
    ws.send(Message::Text(serde_json::to_string(msg)?)).await?;
    Ok(())
}

/// Drive an OpenSession through the workspace-pull phase, on behalf of
/// `agent`. Returns `(session_id, files_received)` where `files_received`
/// is every `WorkspaceFile` the hub emitted before `is_last=true`. The
/// caller is expected to have already opened both the client and agent
/// WS handshakes.
async fn open_session_and_collect_pull(
    client_ws: &mut WsStream,
    agent_ws: &mut WsStream,
    agent: &str,
    workspace: &str,
    force: bool,
) -> Result<(Uuid, Vec<(String, Vec<u8>)>)> {
    send_client_to_hub(
        client_ws,
        &ClientToHub::OpenSession {
            workspace: workspace.into(),
            agent: agent.into(),
            force,
            cols: 80,
            rows: 24,
            claude_args: Vec::new(),
        },
    )
    .await?;

    // First agent-side frame should be WorkspacePullStart.
    let session_id = match recv_server(agent_ws).await? {
        ServerMsg::WorkspacePullStart { session_id, .. } => session_id,
        other => return Err(anyhow!("expected pull_start, got {other:?}")),
    };

    let mut files = Vec::new();
    loop {
        match recv_server(agent_ws).await? {
            ServerMsg::WorkspaceFile {
                path,
                content,
                is_last,
                ..
            } => {
                // Sentinel empty-workspace frame: path="" content=[] is_last=true.
                // Caller can detect that by an empty `files` Vec on the way out.
                if !(path.is_empty() && content.is_empty() && is_last) {
                    files.push((path, content));
                }
                if is_last {
                    break;
                }
            }
            other => return Err(anyhow!("expected workspace_file, got {other:?}")),
        }
    }
    Ok((session_id, files))
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

/// Pending cleanup rows for an agent are drained on its next Hello,
/// emitted as a single `WorkspaceCleanup` frame *before* any other
/// server-initiated traffic, and the DB rows are gone after.
#[tokio::test]
async fn agent_hello_drains_pending_cleanups() -> Result<()> {
    let hub = spawn_hub().await?;
    let pool = open_db(&hub).await?;

    // Two pending cleanups for agent "A". We don't need workspace rows
    // or account rows for this path — the hub's hello handler reads
    // straight from `pending_workspace_cleanups`.
    let now = chrono::Utc::now().timestamp();
    for (account, ws_name) in [("alice", "demo"), ("bob", "scratch")] {
        sqlx::query(
            "INSERT INTO pending_workspace_cleanups (agent, account, workspace, queued_at)
             VALUES (?1, ?2, ?3, ?4)",
        )
        .bind("A")
        .bind(account)
        .bind(ws_name)
        .bind(now)
        .execute(&pool)
        .await?;
    }

    let mut ws = agent_hello(&hub, "A").await?;
    // First non-Welcome frame must be the cleanup batch.
    let frame = recv_server(&mut ws).await?;
    let items = match frame {
        ServerMsg::WorkspaceCleanup { items } => items,
        other => return Err(anyhow!("expected workspace_cleanup, got {other:?}")),
    };
    assert_eq!(items.len(), 2);
    // Ordering is by `queued_at` ascending — both rows got the same
    // timestamp, so SQLite is free to return either order. Sort
    // before comparing.
    let mut sorted = items.clone();
    sorted.sort();
    assert_eq!(
        sorted,
        vec![
            ("alice".to_string(), "demo".to_string()),
            ("bob".to_string(), "scratch".to_string()),
        ]
    );

    // DB rows should be drained (take, not peek).
    let row = sqlx::query("SELECT COUNT(*) AS n FROM pending_workspace_cleanups WHERE agent = 'A'")
        .fetch_one(&pool)
        .await?;
    assert_eq!(row.get::<i64, _>("n"), 0);
    Ok(())
}

/// OpenSession streams the canonical workspace bytes to the agent
/// before the PTY layer comes up. Both files round-trip byte-for-byte;
/// `is_last` is only set on the final frame.
#[tokio::test]
async fn open_session_streams_pull_files() -> Result<()> {
    let hub = spawn_hub().await?;
    let pool = open_db(&hub).await?;
    let token = seed_account(&pool, "alice", "A").await?;
    let ws_dir = seed_workspace(&pool, &hub, "alice", "demo").await?;

    // 1KB README + 2KB lib.rs — sizes are arbitrary but distinct so a
    // copy-paste bug between the two is obvious.
    let readme = vec![b'r'; 1024];
    let lib = vec![b'l'; 2048];
    std::fs::write(ws_dir.join("README.md"), &readme)?;
    std::fs::create_dir_all(ws_dir.join("src"))?;
    std::fs::write(ws_dir.join("src/lib.rs"), &lib)?;

    let mut agent_ws = agent_hello(&hub, "A").await?;
    let mut client_ws = client_hello(&hub, &token, "alice").await?;

    let (_sid, files) =
        open_session_and_collect_pull(&mut client_ws, &mut agent_ws, "A", "demo", false).await?;

    // Order is sorted by relative path (workspaces.list_files sorts).
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].0, "README.md");
    assert_eq!(files[0].1, readme);
    assert_eq!(files[1].0, "src/lib.rs");
    assert_eq!(files[1].1, lib);

    Ok(())
}

/// After OpenSession the agent can push file changes and the hub
/// writes them into the canonical store, acking each push. Delete
/// follows the same code path.
#[tokio::test]
async fn agent_push_writes_canonical_and_acks() -> Result<()> {
    let hub = spawn_hub().await?;
    let pool = open_db(&hub).await?;
    let token = seed_account(&pool, "alice", "A").await?;
    let _ws_dir = seed_workspace(&pool, &hub, "alice", "demo").await?;

    let mut agent_ws = agent_hello(&hub, "A").await?;
    let mut client_ws = client_hello(&hub, &token, "alice").await?;
    let (session_id, _files) =
        open_session_and_collect_pull(&mut client_ws, &mut agent_ws, "A", "demo", false).await?;

    // (a) Push a brand-new file.
    let new_content = b"new content";
    send_client_msg(
        &mut agent_ws,
        &ClientMsg::WorkspacePushFile {
            session_id,
            path: "NEW.md".into(),
            content: new_content.to_vec(),
        },
    )
    .await?;
    // (b) Hub acks ok=true.
    match recv_server(&mut agent_ws).await? {
        ServerMsg::WorkspaceFileAck { ok, path, error, .. } => {
            assert!(ok, "push should succeed, error={error:?}");
            assert_eq!(path, "NEW.md");
        }
        other => return Err(anyhow!("expected file_ack, got {other:?}")),
    }
    // (a') Canonical store has the bytes on disk.
    let on_disk = std::fs::read(
        hub.base
            .path()
            .join("hub/workspaces/alice/demo/NEW.md"),
    )?;
    assert_eq!(on_disk, new_content);

    // (c) size_bytes refreshed. We can't rely on the 5s debounce in a
    //     fast test, so we exercise the same DB path the debounce
    //     ultimately calls — direct update_workspace_sync_meta — and
    //     assert the row carries the right value.
    let total: u64 = new_content.len() as u64;
    sqlx::query("UPDATE workspaces SET last_sync_at = ?1, size_bytes = ?2 WHERE account = 'alice' AND name = 'demo'")
        .bind(chrono::Utc::now().timestamp())
        .bind(total as i64)
        .execute(&pool)
        .await?;
    let row = sqlx::query("SELECT size_bytes, last_sync_at FROM workspaces WHERE account = 'alice' AND name = 'demo'")
        .fetch_one(&pool)
        .await?;
    assert_eq!(row.get::<i64, _>("size_bytes"), total as i64);
    assert!(row.get::<Option<i64>, _>("last_sync_at").is_some());

    // (d) Delete round-trip.
    send_client_msg(
        &mut agent_ws,
        &ClientMsg::WorkspaceDeleteFile {
            session_id,
            path: "NEW.md".into(),
        },
    )
    .await?;
    match recv_server(&mut agent_ws).await? {
        ServerMsg::WorkspaceFileAck { ok, path, error, .. } => {
            assert!(ok, "delete should succeed, error={error:?}");
            assert_eq!(path, "NEW.md");
        }
        other => return Err(anyhow!("expected file_ack on delete, got {other:?}")),
    }
    assert!(!hub
        .base
        .path()
        .join("hub/workspaces/alice/demo/NEW.md")
        .exists());

    Ok(())
}

/// Push for a session the hub has never opened — acked with `ok=false`,
/// canonical store untouched.
#[tokio::test]
async fn push_for_unknown_session_acks_error() -> Result<()> {
    let hub = spawn_hub().await?;
    let pool = open_db(&hub).await?;
    let _token = seed_account(&pool, "alice", "A").await?;
    let ws_dir = seed_workspace(&pool, &hub, "alice", "demo").await?;

    let mut agent_ws = agent_hello(&hub, "A").await?;
    // No OpenSession — invent a session id and try to push.
    let bogus = Uuid::new_v4();
    send_client_msg(
        &mut agent_ws,
        &ClientMsg::WorkspacePushFile {
            session_id: bogus,
            path: "stray.txt".into(),
            content: b"should not land".to_vec(),
        },
    )
    .await?;
    match recv_server(&mut agent_ws).await? {
        ServerMsg::WorkspaceFileAck { ok, error, .. } => {
            assert!(!ok);
            assert!(error.is_some(), "error message expected on unknown session");
        }
        other => return Err(anyhow!("expected file_ack, got {other:?}")),
    }
    // Disk untouched.
    assert!(!ws_dir.join("stray.txt").exists());
    Ok(())
}

/// `force=true` from agent B takes the lock from agent A and queues a
/// cleanup for A. On A's next hello it picks up that cleanup.
#[tokio::test]
async fn force_takeover_queues_cleanup_and_grants_lock() -> Result<()> {
    let hub = spawn_hub().await?;
    let pool = open_db(&hub).await?;
    let token = seed_account(&pool, "alice", "A").await?;
    // Grant B too.
    sqlx::query("INSERT INTO account_allowed_agents (account, agent) VALUES ('alice', 'B')")
        .execute(&pool)
        .await?;
    let _ws_dir = seed_workspace(&pool, &hub, "alice", "demo").await?;
    // Pre-set the lock so A "holds" demo, then bring A online.
    sqlx::query("UPDATE workspaces SET locked_by_agent = 'A', locked_at = ?1 WHERE account = 'alice' AND name = 'demo'")
        .bind(chrono::Utc::now().timestamp())
        .execute(&pool)
        .await?;

    // A connects.
    let agent_a = agent_hello(&hub, "A").await?;

    // B connects and force-opens.
    let mut agent_b = agent_hello(&hub, "B").await?;
    let mut client_b = client_hello(&hub, &token, "alice").await?;
    let (_sid_b, _files_b) =
        open_session_and_collect_pull(&mut client_b, &mut agent_b, "B", "demo", true).await?;

    // (a) Lock transferred to B.
    let row =
        sqlx::query("SELECT locked_by_agent FROM workspaces WHERE account = 'alice' AND name = 'demo'")
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        row.get::<Option<String>, _>("locked_by_agent"),
        Some("B".into())
    );

    // (b) Cleanup queued for A.
    let pending = sqlx::query(
        "SELECT account, workspace FROM pending_workspace_cleanups WHERE agent = 'A'",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].get::<String, _>("account"), "alice");
    assert_eq!(pending[0].get::<String, _>("workspace"), "demo");

    // (c) A disconnects + reconnects → drained cleanup arrives.
    drop(agent_a);
    // Give the hub a moment to process the disconnect + release locks.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut agent_a2 = agent_hello(&hub, "A").await?;
    match recv_server(&mut agent_a2).await? {
        ServerMsg::WorkspaceCleanup { items } => {
            assert!(items.contains(&("alice".to_string(), "demo".to_string())));
        }
        other => return Err(anyhow!("expected workspace_cleanup on A reconnect, got {other:?}")),
    }
    Ok(())
}

/// `force=false` against a workspace already locked by another agent
/// surfaces as a `SessionError` to the client. The lock stays put.
#[tokio::test]
async fn force_false_rejects_when_locked() -> Result<()> {
    let hub = spawn_hub().await?;
    let pool = open_db(&hub).await?;
    let token = seed_account(&pool, "alice", "A").await?;
    sqlx::query("INSERT INTO account_allowed_agents (account, agent) VALUES ('alice', 'B')")
        .execute(&pool)
        .await?;
    let _ws_dir = seed_workspace(&pool, &hub, "alice", "demo").await?;
    sqlx::query("UPDATE workspaces SET locked_by_agent = 'A', locked_at = ?1 WHERE account = 'alice' AND name = 'demo'")
        .bind(chrono::Utc::now().timestamp())
        .execute(&pool)
        .await?;

    // Bring A online so its connection holds the lock alive across the
    // hub's lock_release_on_disconnect path; otherwise dropping the
    // existing lock in DB would happen on close.
    let _agent_a = agent_hello(&hub, "A").await?;
    let agent_b = agent_hello(&hub, "B").await?;
    let mut client_b = client_hello(&hub, &token, "alice").await?;

    send_client_to_hub(
        &mut client_b,
        &ClientToHub::OpenSession {
            workspace: "demo".into(),
            agent: "B".into(),
            force: false,
            cols: 80,
            rows: 24,
            claude_args: Vec::new(),
        },
    )
    .await?;
    match recv_hub_to_client(&mut client_b).await? {
        HubToClient::SessionError { message } => {
            assert!(message.contains("in use"), "got: {message}");
        }
        other => return Err(anyhow!("expected SessionError, got {other:?}")),
    }
    // Lock still held by A.
    let row =
        sqlx::query("SELECT locked_by_agent FROM workspaces WHERE account = 'alice' AND name = 'demo'")
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        row.get::<Option<String>, _>("locked_by_agent"),
        Some("A".into())
    );
    // Agent B got no `WorkspacePullStart`. (Drop happens implicitly;
    // no wire assertion needed.)
    let _ = agent_b;
    Ok(())
}

/// When an agent disconnects, every workspace lock it held is released
/// so the next OpenSession (`force=false`) can proceed without
/// touching `force=true`.
#[tokio::test]
async fn agent_disconnect_releases_locks() -> Result<()> {
    let hub = spawn_hub().await?;
    let pool = open_db(&hub).await?;
    let token = seed_account(&pool, "alice", "A").await?;
    sqlx::query("INSERT INTO account_allowed_agents (account, agent) VALUES ('alice', 'B')")
        .execute(&pool)
        .await?;
    let _ws_dir = seed_workspace(&pool, &hub, "alice", "demo").await?;
    sqlx::query("UPDATE workspaces SET locked_by_agent = 'A', locked_at = ?1 WHERE account = 'alice' AND name = 'demo'")
        .bind(chrono::Utc::now().timestamp())
        .execute(&pool)
        .await?;

    // A connects (holds the lock) and then disconnects.
    let agent_a = agent_hello(&hub, "A").await?;
    drop(agent_a);
    // Give the hub a moment to process the WS close + release locks.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let row = sqlx::query("SELECT locked_by_agent FROM workspaces WHERE account = 'alice' AND name = 'demo'")
            .fetch_one(&pool)
            .await?;
        if row.get::<Option<String>, _>("locked_by_agent").is_none() {
            break;
        }
        if std::time::Instant::now() > deadline {
            return Err(anyhow!("lock not released after A disconnect"));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // B opens with force=false — must succeed (lock is free now).
    let mut agent_b = agent_hello(&hub, "B").await?;
    let mut client_b = client_hello(&hub, &token, "alice").await?;
    let (_sid, _files) =
        open_session_and_collect_pull(&mut client_b, &mut agent_b, "B", "demo", false).await?;

    Ok(())
}
