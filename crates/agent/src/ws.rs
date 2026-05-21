use crate::pty::OutFrame;
use crate::tunnel::{
    unpack_pty_frame, ClientMsg, RejectReason, ServerMsg, PROTOCOL_VERSION, TAG_PTY_INPUT,
};
use crate::AppState;
use anyhow::anyhow;
use futures::{SinkExt, StreamExt};
use rand::Rng;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const SEND_QUEUE: usize = 256;

pub async fn run(state: Arc<AppState>) -> anyhow::Result<()> {
    let mut backoff = Backoff::new();
    loop {
        match run_once(state.clone()).await {
            Ok(()) => {
                tracing::info!("hub session closed; reconnecting shortly");
                backoff.reset();
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(RunError::Fatal(reason)) => {
                return Err(anyhow!(
                    "hub rejected agent ({reason}); fix config and restart"
                ));
            }
            Err(RunError::Transient(e)) => {
                let delay = backoff.next();
                tracing::warn!(error = %e, delay_ms = delay.as_millis(), "hub connection failed");
                tokio::time::sleep(delay).await;
            }
        }
    }
}

#[derive(Debug)]
enum RunError {
    Fatal(String),
    Transient(String),
}

async fn run_once(state: Arc<AppState>) -> Result<(), RunError> {
    let url = state.config.hub.url.clone();
    tracing::info!(url = %url, name = %state.name, "connecting to hub");

    let (ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .map_err(|e| RunError::Transient(format!("connect: {}", e)))?;
    let (mut sink, mut stream) = ws.split();

    let hello = ClientMsg::Hello {
        name: state.name.clone(),
        secret: state.config.auth.registration_token.clone(),
        version: PROTOCOL_VERSION.into(),
        agent_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        target_triple: Some(crate::update::target_triple().to_string()),
        // Seed hub's workspaces table with whatever we already have
        // on disk. Hub upserts each `(account, this-agent, name)`;
        // already-known bindings are a no-op.
        workspaces: state.manager.list_workspace_paths(),
    };
    let hello_json = serde_json::to_string(&hello)
        .map_err(|e| RunError::Transient(format!("encode hello: {}", e)))?;
    sink.send(Message::Text(hello_json))
        .await
        .map_err(|e| RunError::Transient(format!("send hello: {}", e)))?;

    let first = tokio::time::timeout(HANDSHAKE_TIMEOUT, stream.next())
        .await
        .map_err(|_| RunError::Transient("welcome timeout".into()))?;
    match first {
        Some(Ok(Message::Text(s))) => {
            let msg: ServerMsg = serde_json::from_str(&s)
                .map_err(|e| RunError::Transient(format!("parse welcome: {}", e)))?;
            match msg {
                ServerMsg::Welcome { name } => {
                    tracing::info!(agent = %name, "connected to hub");
                }
                ServerMsg::Rejected { reason } => {
                    return Err(RunError::Fatal(reject_label(reason).into()));
                }
                _ => return Err(RunError::Transient("unexpected handshake frame".into())),
            }
        }
        Some(Ok(_)) => return Err(RunError::Transient("non-text handshake frame".into())),
        Some(Err(e)) => return Err(RunError::Transient(format!("ws: {}", e))),
        None => return Err(RunError::Transient("eof before welcome".into())),
    }

    let (tx, mut rx) = mpsc::channel::<OutFrame>(SEND_QUEUE);

    // Hand the live sender to the agent-level audit task. Cleared on
    // any return from this function so audit pauses (rather than
    // silently dropping events) until reconnect re-arms the slot.
    state.audit_slot.set(tx.clone());
    let _slot_guard = SlotGuard {
        slot: state.audit_slot.clone(),
    };

    let writer = tokio::spawn(async move {
        while let Some(out) = rx.recv().await {
            let msg = match out {
                OutFrame::Text(m) => match serde_json::to_string(&m) {
                    Ok(t) => Message::Text(t),
                    Err(e) => {
                        tracing::warn!(error = %e, "encode text frame");
                        continue;
                    }
                },
                OutFrame::Binary(b) => Message::Binary(b),
            };
            if sink.send(msg).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let read_result = read_loop(state.clone(), tx.clone(), &mut stream).await;
    drop(tx);
    let _ = writer.await;
    read_result
}

async fn read_loop<S>(
    state: Arc<AppState>,
    tx: mpsc::Sender<OutFrame>,
    stream: &mut S,
) -> Result<(), RunError>
where
    S: futures::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    while let Some(item) = stream.next().await {
        let msg = item.map_err(|e| RunError::Transient(format!("ws: {}", e)))?;
        match msg {
            Message::Text(s) => match serde_json::from_str::<ServerMsg>(&s) {
                Ok(ServerMsg::Ping) => {
                    let _ = tx.send(OutFrame::Text(ClientMsg::Pong)).await;
                }
                Ok(ServerMsg::Welcome { .. }) => {
                    tracing::warn!("duplicate welcome from hub; ignoring");
                }
                Ok(ServerMsg::Rejected { reason }) => {
                    return Err(RunError::Fatal(reject_label(reason).into()));
                }
                Ok(ServerMsg::UpdateAgent {
                    request_id,
                    target_version,
                    download_url,
                    sha256_url,
                }) => {
                    // Self-update is a top-level concern, not a PTY/workspace
                    // operation, so we handle it here rather than in
                    // PtyManager::handle. On success the agent process
                    // exits cleanly and the supervisor relaunches us on
                    // the new binary.
                    let tx_reply = tx.clone();
                    tokio::spawn(async move {
                        let req = crate::update::UpdateRequest {
                            request_id,
                            target_version: target_version.clone(),
                            download_url,
                            sha256_url,
                        };
                        match crate::update::perform_update(req).await {
                            Ok(()) => {
                                let _ = tx_reply
                                    .send(OutFrame::Text(ClientMsg::UpdateAgentResult {
                                        request_id,
                                        error: None,
                                    }))
                                    .await;
                                // Give the writer task a beat to flush the
                                // ack frame onto the wire before we exit.
                                tokio::time::sleep(Duration::from_millis(500)).await;
                                tracing::info!(
                                    %target_version,
                                    "self-update applied; exiting for supervisor to relaunch"
                                );
                                std::process::exit(0);
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "self-update failed");
                                let _ = tx_reply
                                    .send(OutFrame::Text(ClientMsg::UpdateAgentResult {
                                        request_id,
                                        error: Some(e),
                                    }))
                                    .await;
                            }
                        }
                    });
                }
                Ok(frame) => {
                    let mgr = state.manager.clone();
                    let send = tx.clone();
                    tokio::spawn(async move {
                        mgr.handle(frame, send).await;
                    });
                }
                Err(e) => tracing::warn!(error = %e, "bad text frame from hub"),
            },
            Message::Binary(b) => {
                let Some((tag, session_id, payload)) = unpack_pty_frame(&b) else {
                    tracing::warn!("malformed binary frame");
                    continue;
                };
                if tag != TAG_PTY_INPUT {
                    tracing::warn!(tag, "unexpected binary tag from hub");
                    continue;
                }
                state.manager.write_input(session_id, payload);
            }
            Message::Ping(_) | Message::Pong(_) => {}
            Message::Close(_) => return Ok(()),
            Message::Frame(_) => {}
        }
    }
    Ok(())
}

fn reject_label(r: RejectReason) -> &'static str {
    match r {
        RejectReason::NameTaken => "name_taken (another agent with this name is already connected)",
        RejectReason::AuthFailed => "auth_failed (registration_token does not match)",
        RejectReason::VersionMismatch => "version_mismatch (upgrade agent or hub)",
    }
}

/// Clears the audit sender slot whenever we leave `run_once` — covers
/// normal close, transient error, and fatal reject paths in a single
/// place so a future code path can't accidentally leave a stale
/// sender wired into the audit task.
struct SlotGuard {
    slot: crate::audit::SenderSlot,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.slot.clear();
    }
}

struct Backoff {
    next_ms: u64,
}

impl Backoff {
    fn new() -> Self {
        Self { next_ms: 1000 }
    }
    fn reset(&mut self) {
        self.next_ms = 1000;
    }
    fn next(&mut self) -> Duration {
        let cur = self.next_ms;
        self.next_ms = (cur * 2).min(30_000);
        let jitter = rand::thread_rng().gen_range(0..500);
        Duration::from_millis(cur + jitter)
    }
}
