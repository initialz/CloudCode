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
/// Maximum gap we tolerate between frames from the hub before
/// declaring the connection dead and reconnecting. Hub pings every
/// 5s, so 6s gives us a 1s safety margin against jitter while
/// surfacing a dead hub (mid-upgrade, crashed, or stale TCP) within
/// ~6s instead of waiting on the OS keepalive timer.
const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(6);

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
                    return Err(classify_reject(reason));
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
    // Abort the writer instead of awaiting it. read_loop spawns
    // detached tasks (PtyManager::handle) that each hold their own
    // tx clone — those tasks survive the connection and only drop
    // their tx when their PTY session ends. If we awaited the
    // writer, we'd wait on rx.recv() returning None, which only
    // happens once *every* tx clone is dropped — practically
    // "forever" if a workspace is open. That made the second hub
    // restart look like the agent stopped reconnecting: run_once
    // never returned, so the outer reconnect loop never advanced.
    // Aborting the writer is fine here — the WS is already torn
    // down and we're about to throw away the sink anyway.
    drop(tx);
    writer.abort();
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
    loop {
        let next = match tokio::time::timeout(READ_IDLE_TIMEOUT, stream.next()).await {
            Ok(opt) => opt,
            Err(_) => {
                // No frame from hub for READ_IDLE_TIMEOUT. Hub pings
                // every 5s so this means the connection is wedged
                // (hub mid-upgrade / crashed / network dropped without
                // a TCP FIN). Break out so the outer reconnect loop
                // can re-establish.
                return Err(RunError::Transient(format!(
                    "no hub frame for {}s; assuming dead connection",
                    READ_IDLE_TIMEOUT.as_secs()
                )));
            }
        };
        let Some(item) = next else { break };
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
                    return Err(classify_reject(reason));
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
                // Stateful, ordered frames (the FsWriteInit → FsWriteChunk…
                // → eof upload sequence) MUST run in arrival order. Spawning
                // a task per frame (below) races them: a chunk handled before
                // its init — or after the eof that drops the session — finds
                // no write session and silently discards the data, leaving a
                // 0-byte / truncated file. Handle these inline. They're cheap
                // (create file / decode+append a chunk), so this doesn't
                // reintroduce the head-of-line blocking the spawn was added to
                // fix (v1.15.3) for the genuinely slow ops, which self-spawn.
                Ok(frame) if is_ordered_frame(&frame) => {
                    state.manager.clone().handle(frame, tx.clone()).await;
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

/// Decide whether a hub-side rejection should kill the agent process
/// (Fatal) or just trip a reconnect (Transient).
///
/// - `VersionMismatch`: transient. Rolling upgrades are the normal
///   case here — e.g. the operator upgrades the agent first and the
///   hub a minute later, or the supervisor restarts the hub on a
///   newer protocol. Treating this as Fatal forces a manual restart
///   *every* time the two binaries drift, which is the opposite of
///   the "agent self-heals when hub catches up" UX we want. Backoff
///   keeps the reconnect storm bounded (cap 30s).
/// - `NameTaken`: transient. The most common reason this fires is
///   *not* a real config conflict — it's the hub still holding our
///   own prior AgentConn after a silent TCP drop, until its
///   read-idle timeout finally fires. Treating it as Fatal in that
///   case kicks us out of the agent process entirely and forces a
///   supervisor restart on exponential backoff, which can drag the
///   reconnect out to tens of seconds. Transient + the same
///   `run_once` backoff (cap 5s) closes the gap much faster, and a
///   genuine name conflict (operator misconfig — two agents sharing
///   `[agent].name`) still shows up loudly in the log on every
///   retry.
/// - `AuthFailed`: fatal. The token is a config item; auto-retry just
///   pummels the hub with bad credentials without any chance of
///   succeeding until the operator fixes `registration_token` and
///   restarts the agent.
fn classify_reject(r: RejectReason) -> RunError {
    match r {
        RejectReason::VersionMismatch | RejectReason::NameTaken => {
            RunError::Transient(reject_label(r).to_string())
        }
        _ => RunError::Fatal(reject_label(r).to_string()),
    }
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

/// Exponential reconnect backoff. The cap is intentionally short
/// (5s) so the agent reattaches quickly after a hub restart — the
/// previous 30s cap meant operators perceived hub upgrades as
/// "agent died, must restart manually" because the next retry was
/// almost a full minute away. The whole purpose of backoff is to
/// avoid pummelling a hub that's genuinely down; 5s is plenty for
/// that while staying responsive in the common rolling-upgrade case.
struct Backoff {
    next_ms: u64,
}

/// Frames belonging to a stateful, ordered sequence that must be handled
/// in arrival order rather than each in its own task. Today that's the
/// upload protocol: `FsWriteInit` opens a write session keyed by
/// `request_id`, successive `FsWriteChunk` frames append to it, and the
/// final `eof` chunk closes it. Spawning a task per frame races them, so a
/// chunk can land before its init (no session yet) or after eof (session
/// already gone) and the data is silently dropped — producing 0-byte or
/// truncated uploads. All other frames keep the per-frame spawn.
fn is_ordered_frame(msg: &ServerMsg) -> bool {
    matches!(
        msg,
        ServerMsg::FsWriteInit { .. } | ServerMsg::FsWriteChunk { .. }
    )
}

impl Backoff {
    fn new() -> Self {
        Self { next_ms: 500 }
    }
    fn reset(&mut self) {
        self.next_ms = 500;
    }
    fn next(&mut self) -> Duration {
        let cur = self.next_ms;
        self.next_ms = (cur * 2).min(5_000);
        let jitter = rand::thread_rng().gen_range(0..200);
        Duration::from_millis(cur + jitter)
    }
}

#[cfg(test)]
mod tests {
    use super::is_ordered_frame;
    use crate::tunnel::ServerMsg;
    use uuid::Uuid;

    // Upload frames must be order-preserving: handling them out of order
    // (the per-frame spawn) drops data and yields 0-byte/truncated files.
    #[test]
    fn upload_write_frames_are_ordered() {
        let init = ServerMsg::FsWriteInit {
            request_id: Uuid::nil(),
            account: "a".into(),
            workspace: "w".into(),
            path: "f.txt".into(),
            size: 0,
        };
        let chunk = ServerMsg::FsWriteChunk {
            request_id: Uuid::nil(),
            data_b64: "aGVsbG8=".into(),
            eof: false,
        };
        let eof = ServerMsg::FsWriteChunk {
            request_id: Uuid::nil(),
            data_b64: String::new(),
            eof: true,
        };
        assert!(is_ordered_frame(&init));
        assert!(is_ordered_frame(&chunk));
        assert!(is_ordered_frame(&eof));
    }

    // Everything else keeps the concurrent per-frame spawn (HOL fix).
    #[test]
    fn non_write_frames_are_not_ordered() {
        assert!(!is_ordered_frame(&ServerMsg::Ping));
        assert!(!is_ordered_frame(&ServerMsg::PtyClose {
            session_id: Uuid::nil()
        }));
    }
}
