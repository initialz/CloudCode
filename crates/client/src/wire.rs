//! WS transport for the client ↔ hub session protocol.
//!
//! Spawns a writer + reader task; surfaces:
//!   - `mpsc::Sender<ClientToHub>` for the TUI to enqueue outgoing frames
//!   - `mpsc::Receiver<HubToClient>` for the TUI to consume incoming frames
//!
//! On EOF / error the reader closes the inbound channel — the TUI sees None
//! and reports "disconnected".

use crate::proto::{ClientToHub, HubToClient, SESSION_PROTOCOL_VERSION};
use anyhow::{anyhow, Context, Result};
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

const OUT_QUEUE: usize = 128;
const IN_QUEUE: usize = 256;

pub struct Wire {
    pub tx: mpsc::Sender<ClientToHub>,
    pub rx: mpsc::Receiver<HubToClient>,
}

pub async fn connect(hub_url: &str, token: &str) -> Result<Wire> {
    let url = build_ws_url(hub_url)?;
    let (ws, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .with_context(|| format!("connecting to {}", url))?;
    let (mut sink, mut stream) = ws.split();

    // Send hello immediately.
    let hello = ClientToHub::Hello {
        token: token.to_string(),
        version: SESSION_PROTOCOL_VERSION.into(),
    };
    sink.send(Message::Text(serde_json::to_string(&hello)?))
        .await
        .context("sending hello")?;

    let (out_tx, mut out_rx) = mpsc::channel::<ClientToHub>(OUT_QUEUE);
    let (in_tx, in_rx) = mpsc::channel::<HubToClient>(IN_QUEUE);

    tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            let text = match serde_json::to_string(&frame) {
                Ok(t) => t,
                Err(_) => continue,
            };
            if sink.send(Message::Text(text)).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let in_tx_for_reader = in_tx.clone();
    tokio::spawn(async move {
        while let Some(item) = stream.next().await {
            let msg = match item {
                Ok(m) => m,
                Err(_) => break,
            };
            match msg {
                Message::Text(s) => match serde_json::from_str::<HubToClient>(&s) {
                    Ok(frame) => {
                        if in_tx_for_reader.send(frame).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => continue,
                },
                Message::Close(_) => break,
                _ => {}
            }
        }
        drop(in_tx_for_reader);
    });

    Ok(Wire {
        tx: out_tx,
        rx: in_rx,
    })
}

fn build_ws_url(hub_url: &str) -> Result<String> {
    // Accept http(s) and rewrite to ws(s); accept ws(s) as-is.
    let base = hub_url.trim_end_matches('/');
    let scheme_split = base.split_once("://");
    let (scheme, rest) =
        scheme_split.ok_or_else(|| anyhow!("hub_url missing scheme: {}", hub_url))?;
    let ws_scheme = match scheme {
        "http" | "ws" => "ws",
        "https" | "wss" => "wss",
        other => return Err(anyhow!("unsupported scheme: {}", other)),
    };
    Ok(format!("{}://{}/v1/session/ws", ws_scheme, rest))
}
