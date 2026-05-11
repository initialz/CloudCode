use crate::registry::OutgoingFrame;
use crate::tunnel::{ClientMsg, RejectReason, ServerMsg, PROTOCOL_VERSION};
use crate::AppState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const PING_INTERVAL: Duration = Duration::from_secs(30);
const SEND_QUEUE: usize = 256;

pub async fn upgrade(State(state): State<Arc<AppState>>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>) {
    let (mut sink, mut stream) = socket.split();

    let hello = match tokio::time::timeout(HELLO_TIMEOUT, stream.next()).await {
        Ok(Some(Ok(Message::Text(s)))) => match serde_json::from_str::<ClientMsg>(&s) {
            Ok(ClientMsg::Hello {
                name,
                secret,
                version,
            }) => (name, secret, version),
            _ => {
                let _ = send_rejected(&mut sink, RejectReason::AuthFailed).await;
                return;
            }
        },
        _ => return,
    };

    let (name, secret, version) = hello;

    if version != PROTOCOL_VERSION {
        let _ = send_rejected(&mut sink, RejectReason::VersionMismatch).await;
        return;
    }

    if !crate::auth::verify_token(&secret, &state.config.agents.registration_token_hash) {
        let _ = send_rejected(&mut sink, RejectReason::AuthFailed).await;
        return;
    }

    let (tx, mut rx) = mpsc::channel::<OutgoingFrame>(SEND_QUEUE);
    let Some(conn) = state.registry.try_register(name.clone(), tx) else {
        let _ = send_rejected(&mut sink, RejectReason::NameTaken).await;
        return;
    };

    if send_text(&mut sink, &ServerMsg::Welcome { name: name.clone() })
        .await
        .is_err()
    {
        state.registry.unregister(&conn);
        return;
    }

    tracing::info!(agent = %name, "agent connected");

    let writer = tokio::spawn(async move {
        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.tick().await;
        loop {
            tokio::select! {
                msg = rx.recv() => {
                    let Some(out) = msg else { break; };
                    let r = match out {
                        OutgoingFrame::Text(m) => match serde_json::to_string(&m) {
                            Ok(t) => sink.send(Message::Text(t)).await,
                            Err(e) => {
                                tracing::warn!(error = %e, "encode hub→agent text");
                                continue;
                            }
                        },
                        OutgoingFrame::Binary(b) => sink.send(Message::Binary(b)).await,
                    };
                    if r.is_err() {
                        break;
                    }
                }
                _ = ping.tick() => {
                    if send_text(&mut sink, &ServerMsg::Ping).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = sink.close().await;
    });

    while let Some(item) = stream.next().await {
        let msg = match item {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(agent = %name, error = %e, "ws read error");
                break;
            }
        };
        match msg {
            Message::Text(s) => match serde_json::from_str::<ClientMsg>(&s) {
                Ok(frame) => conn.handle_text_frame(frame).await,
                Err(e) => tracing::warn!(agent = %name, error = %e, "bad frame"),
            },
            Message::Binary(b) => conn.handle_binary_frame(&b).await,
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) => {}
        }
    }

    tracing::info!(agent = %name, "agent disconnected");
    state.registry.unregister(&conn);
    writer.abort();
}

async fn send_text<S>(sink: &mut S, msg: &ServerMsg) -> Result<(), ()>
where
    S: SinkExt<Message, Error = axum::Error> + Unpin,
{
    let text = serde_json::to_string(msg).map_err(|_| ())?;
    sink.send(Message::Text(text)).await.map_err(|_| ())
}

async fn send_rejected<S>(sink: &mut S, reason: RejectReason) -> Result<(), ()>
where
    S: SinkExt<Message, Error = axum::Error> + Unpin,
{
    let _ = send_text(sink, &ServerMsg::Rejected { reason }).await;
    Ok(())
}
