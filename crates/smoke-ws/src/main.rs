//! Smoke-test WS client.
//!
//! Drives the v1.13 hub-managed-workspace flow from a shell script.
//! Speaks the same `/v1/pty/ws` protocol the real CLI uses (mirrored
//! in `crates/client/src/proto.rs`) but with a single-shot, JSON-out
//! shape that's friendly to `jq`-style consumption.
//!
//! Each subcommand opens its own WS, sends a Hello + the relevant
//! frame, and prints a single line of JSON to stdout summarising
//! what came back. Non-zero exit code = the hub returned
//! Rejected / SessionError / the frame we expected never arrived.

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;

// ---------------------------------------------------------------------
// Wire shapes (kept as adjacent serde-tagged enums so we don't have to
// import the hub/client crates and pull their full dep trees).
// ---------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientToHub {
    Hello {
        token: String,
        version: String,
    },
    ListWorkspaces,
    CreateWorkspace {
        name: String,
    },
    DeleteWorkspace {
        name: String,
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

/// Single in-binary copy of HubToClient kept loose — we parse via
/// `serde_json::Value` and pluck what each subcommand needs so a
/// future hub-side schema bump doesn't immediately break this helper.
const PROTOCOL_VERSION: &str = "1";
const RECV_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Parser)]
#[command(
    name = "cloudcode-smoke-ws",
    about = "Smoke-test WS client for v1.13 hub-managed workspace flows"
)]
struct Cli {
    /// Hub WS URL — typically ws://127.0.0.1:<port>/v1/pty/ws.
    #[arg(long)]
    url: String,

    /// Per-account token (`cc_...`). Embedded in the Hello frame.
    #[arg(long)]
    token: String,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Send `ListWorkspaces` and print the reply.
    ListWorkspaces,
    /// Send `CreateWorkspace { name }`. Exits non-zero on SessionError.
    CreateWorkspace {
        #[arg(long)]
        name: String,
    },
    /// Send `DeleteWorkspace { name }`. Exits non-zero on SessionError.
    DeleteWorkspace {
        #[arg(long)]
        name: String,
    },
    /// Send `OpenSession`, wait for `SessionOpened`, hold the
    /// connection open for `--hold-secs` (so the agent has time to
    /// realise the workspace + run a sync push), then send `Close`.
    OpenSession {
        #[arg(long)]
        workspace: String,
        #[arg(long)]
        agent: String,
        /// Take the workspace lock even if another agent currently
        /// holds it. Used by the lock-takeover scenarios.
        #[arg(long)]
        force: bool,
        #[arg(long, default_value_t = 80)]
        cols: u16,
        #[arg(long, default_value_t = 24)]
        rows: u16,
        /// How long (in seconds) to keep the WS open after the
        /// session opens. Lets file pushes from the agent land in the
        /// hub canonical copy before we tear the session down.
        #[arg(long, default_value_t = 0)]
        hold_secs: u64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // rustls 0.23 requires a default crypto provider. We never enable TLS
    // in the smoke script (everything is ws:// to localhost) but
    // tokio-tungstenite still imports the rustls feature so we install
    // ring to keep it happy.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cli = Cli::parse();
    let (mut ws, _) = tokio_tungstenite::connect_async(&cli.url)
        .await
        .with_context(|| format!("connect {}", cli.url))?;

    // Hello → wait for Welcome (or Rejected).
    let hello = ClientToHub::Hello {
        token: cli.token.clone(),
        version: PROTOCOL_VERSION.to_string(),
    };
    ws.send(Message::Text(serde_json::to_string(&hello)?))
        .await
        .context("send hello")?;
    let welcome = recv_text(&mut ws).await?;
    match welcome.get("type").and_then(Value::as_str) {
        Some("welcome") => {}
        Some("rejected") => {
            let reason = welcome
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            println!("{}", json!({"ok": false, "stage": "hello", "reason": reason}));
            std::process::exit(2);
        }
        _ => {
            return Err(anyhow!(
                "expected welcome|rejected, got: {}",
                welcome
            ));
        }
    }

    let result = match cli.cmd {
        Cmd::ListWorkspaces => list_workspaces(&mut ws).await,
        Cmd::CreateWorkspace { name } => create_workspace(&mut ws, name).await,
        Cmd::DeleteWorkspace { name } => delete_workspace(&mut ws, name).await,
        Cmd::OpenSession {
            workspace,
            agent,
            force,
            cols,
            rows,
            hold_secs,
        } => {
            open_session(
                &mut ws, workspace, agent, force, cols, rows, hold_secs,
            )
            .await
        }
    };

    // Best-effort Close; ignore errors — connection may already be torn.
    let _ = ws
        .send(Message::Text(serde_json::to_string(&ClientToHub::Close)?))
        .await;

    match result {
        Ok(out) => {
            println!("{}", out);
            Ok(())
        }
        Err(out) => {
            println!("{}", out);
            std::process::exit(3);
        }
    }
}

async fn list_workspaces(ws: &mut WS) -> Result<Value, Value> {
    send_frame(ws, &ClientToHub::ListWorkspaces).await?;
    // The list reply doesn't conflict with any other frame the hub
    // would push during the menu phase, so a single recv is enough.
    let v = recv_text(ws).await.map_err(err_value("recv list"))?;
    if v.get("type").and_then(Value::as_str) != Some("workspace_list") {
        return Err(json!({"ok": false, "stage": "list", "reply": v}));
    }
    Ok(json!({"ok": true, "items": v.get("items").cloned().unwrap_or(Value::Null)}))
}

async fn create_workspace(ws: &mut WS, name: String) -> Result<Value, Value> {
    send_frame(ws, &ClientToHub::CreateWorkspace { name: name.clone() }).await?;
    let v = recv_text(ws).await.map_err(err_value("recv create"))?;
    match v.get("type").and_then(Value::as_str) {
        Some("workspace_created") => Ok(json!({"ok": true, "name": name})),
        Some("session_error") => Err(json!({
            "ok": false,
            "stage": "create",
            "message": v.get("message").cloned().unwrap_or(Value::Null),
        })),
        _ => Err(json!({"ok": false, "stage": "create", "reply": v})),
    }
}

async fn delete_workspace(ws: &mut WS, name: String) -> Result<Value, Value> {
    send_frame(ws, &ClientToHub::DeleteWorkspace { name: name.clone() }).await?;
    let v = recv_text(ws).await.map_err(err_value("recv delete"))?;
    match v.get("type").and_then(Value::as_str) {
        Some("workspace_deleted") => Ok(json!({"ok": true, "name": name})),
        Some("session_error") => Err(json!({
            "ok": false,
            "stage": "delete",
            "message": v.get("message").cloned().unwrap_or(Value::Null),
        })),
        _ => Err(json!({"ok": false, "stage": "delete", "reply": v})),
    }
}

async fn open_session(
    ws: &mut WS,
    workspace: String,
    agent: String,
    force: bool,
    cols: u16,
    rows: u16,
    hold_secs: u64,
) -> Result<Value, Value> {
    send_frame(
        ws,
        &ClientToHub::OpenSession {
            workspace: workspace.clone(),
            agent: agent.clone(),
            force,
            cols,
            rows,
            claude_args: Vec::new(),
        },
    )
    .await?;

    // Wait for the first text frame that decides the open outcome.
    // Ignore PtyOutput binary frames that may pile up.
    let opened = loop {
        let v = recv_either(ws).await.map_err(err_value("recv open"))?;
        match v.get("type").and_then(Value::as_str) {
            Some("session_opened") => break v,
            Some("session_error") => {
                return Err(json!({
                    "ok": false,
                    "stage": "open",
                    "force": force,
                    "message": v.get("message").cloned().unwrap_or(Value::Null),
                }));
            }
            Some("session_closed") => {
                return Err(json!({
                    "ok": false,
                    "stage": "open",
                    "reason": v.get("reason").cloned().unwrap_or(Value::Null),
                }));
            }
            // Any other menu-phase reply (rejected handled above) is
            // unexpected here; abort so the caller doesn't loop forever.
            Some(other) => {
                return Err(
                    json!({"ok": false, "stage": "open", "unexpected": other, "frame": v}),
                );
            }
            None => continue,
        }
    };

    // Hold the WS open so the agent sync engine has time to push the
    // initial workspace state up to the hub before we hang up.
    if hold_secs > 0 {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(hold_secs);
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            match timeout(deadline - now, ws.next()).await {
                Ok(Some(Ok(_))) => continue,
                Ok(Some(Err(e))) => {
                    return Err(json!({
                        "ok": false,
                        "stage": "hold",
                        "error": format!("{}", e),
                    }));
                }
                Ok(None) => {
                    return Err(json!({"ok": false, "stage": "hold", "error": "ws closed"}));
                }
                Err(_) => break, // hold deadline reached
            }
        }
    }

    Ok(json!({
        "ok": true,
        "agent": opened.get("agent").cloned().unwrap_or(Value::Null),
        "workspace": opened.get("workspace").cloned().unwrap_or(Value::Null),
        "cwd": opened.get("cwd").cloned().unwrap_or(Value::Null),
    }))
}

// ---------------------------------------------------------------------
// WS plumbing
// ---------------------------------------------------------------------

type WS = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

async fn send_frame(ws: &mut WS, frame: &ClientToHub) -> Result<(), Value> {
    let s = serde_json::to_string(frame)
        .map_err(|e| json!({"ok": false, "stage": "encode", "error": format!("{e}")}))?;
    ws.send(Message::Text(s))
        .await
        .map_err(|e| json!({"ok": false, "stage": "send", "error": format!("{e}")}))
}

/// Receive the next text frame, skipping pings and binary PTY output.
async fn recv_text(ws: &mut WS) -> Result<Value> {
    loop {
        let msg = timeout(RECV_TIMEOUT, ws.next())
            .await
            .map_err(|_| anyhow!("recv timeout"))?
            .ok_or_else(|| anyhow!("ws closed before reply"))?
            .context("ws recv")?;
        match msg {
            Message::Text(t) => return Ok(serde_json::from_str(&t)?),
            Message::Binary(_) | Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(_) => return Err(anyhow!("ws closed by peer")),
            Message::Frame(_) => continue,
        }
    }
}

/// Like recv_text, but transparently passes binary PTY-output frames
/// through (we don't care about their contents during open_session).
async fn recv_either(ws: &mut WS) -> Result<Value> {
    recv_text(ws).await
}

fn err_value(stage: &'static str) -> impl Fn(anyhow::Error) -> Value {
    move |e| json!({"ok": false, "stage": stage, "error": format!("{}", e)})
}
