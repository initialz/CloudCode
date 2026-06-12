//! Client ↔ hub WS transport. Mixes JSON control frames (text) and raw PTY
//! byte streams (binary) on a single connection.

use crate::proto::{ClientToHub, HubToClient, PTY_PROTOCOL_VERSION};
use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

const OUT_QUEUE: usize = 256;
const IN_TEXT_QUEUE: usize = 64;
const IN_BIN_QUEUE: usize = 1024;
/// Maximum gap between frames from the hub before we declare the WS
/// dead and let the channels close. The hub pings every 25s on user
/// WSes (see `crates/hub/src/pty_session.rs::USER_PING_INTERVAL`); 45s
/// gives a 2× jitter margin without making the silent-drop UX
/// noticeably sluggish. Without this the OS-level TCP keepalive
/// (default ~2h on macOS) is the only signal, so a network blip or
/// hub restart freezes the relay invisibly — input goes nowhere, no
/// banner appears, the user only notices because typing has stopped
/// echoing through claude's UI.
const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(45);

pub enum OutFrame {
    Text(ClientToHub),
    Binary(Vec<u8>),
}

pub struct Wire {
    pub out_tx: mpsc::Sender<OutFrame>,
    pub in_text_rx: mpsc::Receiver<HubToClient>,
    pub in_bin_rx: mpsc::Receiver<Bytes>,
}

pub async fn connect(hub_url: &str, token: &str) -> Result<Wire> {
    let url = build_ws_url(hub_url)?;
    let (ws, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .with_context(|| format!("connecting to {}", url))?;
    let (mut sink, mut stream) = ws.split();

    let hello = ClientToHub::Hello {
        token: token.to_string(),
        version: PTY_PROTOCOL_VERSION.into(),
        // 配置了后端命令 = 本机能承载远程-MCP 后端(决策 D9)。
        remote_mcp_capable: crate::mcp_host::backend_command().is_some(),
    };
    sink.send(Message::Text(serde_json::to_string(&hello)?))
        .await
        .context("sending hello")?;

    let (out_tx, mut out_rx) = mpsc::channel::<OutFrame>(OUT_QUEUE);
    let (in_text_tx, in_text_rx) = mpsc::channel::<HubToClient>(IN_TEXT_QUEUE);
    let (in_bin_tx, in_bin_rx) = mpsc::channel::<Bytes>(IN_BIN_QUEUE);

    tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            let r = match frame {
                OutFrame::Text(c) => match serde_json::to_string(&c) {
                    Ok(t) => sink.send(Message::Text(t)).await,
                    Err(_) => continue,
                },
                OutFrame::Binary(b) => sink.send(Message::Binary(b)).await,
            };
            if r.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    tokio::spawn(async move {
        loop {
            let next = match tokio::time::timeout(READ_IDLE_TIMEOUT, stream.next()).await {
                Ok(opt) => opt,
                Err(_) => {
                    // No frame from hub for READ_IDLE_TIMEOUT — TCP is
                    // wedged silently. Drop the channels to trigger the
                    // relay's HubLost path so the reconnect banner can
                    // come up. The writer task's next send (or the
                    // outer reconnect's wire::connect) will also see
                    // the dead socket.
                    tracing::debug!("hub silent for {:?}; declaring wire dead", READ_IDLE_TIMEOUT);
                    break;
                }
            };
            let Some(item) = next else { break };
            match item {
                Ok(Message::Text(s)) => match serde_json::from_str::<HubToClient>(&s) {
                    Ok(frame) => {
                        if in_text_tx.send(frame).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => continue,
                },
                Ok(Message::Binary(b)) => {
                    if in_bin_tx.send(Bytes::from(b)).await.is_err() {
                        break;
                    }
                }
                Ok(Message::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
    });

    Ok(Wire {
        out_tx,
        in_text_rx,
        in_bin_rx,
    })
}

fn build_ws_url(hub_url: &str) -> Result<String> {
    let base = hub_url.trim_end_matches('/');
    let (scheme, rest) = base
        .split_once("://")
        .ok_or_else(|| anyhow!("hub_url missing scheme: {}", hub_url))?;
    let ws_scheme = match scheme {
        "http" | "ws" => "ws",
        "https" | "wss" => "wss",
        other => return Err(anyhow!("unsupported scheme: {}", other)),
    };
    Ok(format!("{}://{}/v1/pty/ws", ws_scheme, rest))
}
