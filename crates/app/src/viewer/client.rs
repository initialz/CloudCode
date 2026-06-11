//! The app's *second* ws to the hub: the browser-panel viewer client.
//!
//! Where `backend.rs` owns the PTY ws (terminal), this owns the viewer ws
//! (`/v1/viewer/ws`). It is the transport half only — it moves JPEG frames
//! down and `ViewerInputEvent`s up over channels. Decoding frames to
//! textures and capturing egui input into events is Task 3.
//!
//! Threading mirrors `backend::spawn`: the egui UI thread can't run async
//! I/O, so `ViewerHandle::connect` spawns a dedicated std::thread hosting a
//! current-thread tokio runtime. The UI talks to it over two channels:
//!
//!   UI  --ViewerCommand-->  client   (std::sync::mpsc; UI side is sync)
//!   client --ViewerEvent--> UI       (std::sync::mpsc; UI drains per frame)
//!
//! After pushing a `ViewerEvent` the client calls
//! `egui::Context::request_repaint()` so a new frame shows even when the UI
//! is otherwise idle — exactly as the PTY backend does for terminal bytes.
//!
//! Reconnect/teardown: unlike the PTY backend this client does NOT auto-
//! reconnect. The viewer ws is lazily opened by the UI when the browser
//! panel becomes visible (Task 4) and a drop is surfaced as
//! `ViewerEvent::Disconnected` so the UI can show a placeholder and decide
//! whether to reconnect. `ViewerCommand::Close` drops the ws; the hub fires
//! `ViewerDetach` agent-side on disconnect, stopping the screencast.

use crate::viewer::proto::{select_target_json, TargetInfo, ViewerDownlinkText, ViewerInputEvent};
use anyhow::{anyhow, Result};
use futures::{SinkExt, StreamExt};
use std::sync::mpsc::{Receiver, Sender};
use tokio_tungstenite::tungstenite::Message;

/// Commands the UI sends to the viewer client.
#[derive(Debug, Clone)]
pub enum ViewerCommand {
    /// A captured input event to inject into the remote browser. Serialized
    /// to JSON and sent up as a ws Text frame.
    SendInput(ViewerInputEvent),
    /// Switch the screencast to another CDP target (a tab-bar click). Sent
    /// up as the `{"kind":"select_target","target_id":…}` Text frame.
    SelectTarget(String),
    /// Tear the viewer ws down (panel hidden / app quitting). Dropping the
    /// socket makes the hub send `ViewerDetach` to the agent.
    Close,
}

/// Events the viewer client sends to the UI.
#[derive(Debug, Clone)]
pub enum ViewerEvent {
    /// The viewer ws is open and the hub accepted the attach.
    Connected,
    /// One screencast frame (raw JPEG bytes) — Task 3 decodes + uploads it
    /// as an egui texture.
    Frame(Vec<u8>),
    /// The agent's current CDP target list (a downlink `targets` Text
    /// frame) — drives the browser panel's tab bar.
    Targets(Vec<TargetInfo>),
    /// The ws closed (hub closed us, agent stopped the screencast, network
    /// drop, or our own `Close`). The UI shows a placeholder; reconnect is
    /// the UI's call (Task 4 lazy-connect).
    Disconnected,
}

/// Handle the UI holds onto: the command sender + event receiver.
pub struct ViewerHandle {
    pub cmd_tx: Sender<ViewerCommand>,
    pub event_rx: Receiver<ViewerEvent>,
}

impl ViewerHandle {
    /// Spawn the viewer client on its own OS thread and return the channel
    /// handle. `ctx` is the egui context, cloned in so the client can wake
    /// the UI from outside the UI thread on every frame/event.
    ///
    /// `hub_url` is the http(s) hub base (same value `backend` uses);
    /// `token` is the account token from config (same one the PTY ws sends
    /// in its Hello). `session_id` is the PTY session whose browser to
    /// watch. The ws URL is built from these by `build_viewer_ws_url`.
    pub fn connect(
        hub_url: String,
        token: String,
        session_id: String,
        ctx: egui::Context,
    ) -> ViewerHandle {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<ViewerCommand>();
        let (event_tx, event_rx) = std::sync::mpsc::channel::<ViewerEvent>();

        std::thread::Builder::new()
            .name("cc-viewer".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::warn!(error = %e, "viewer: tokio runtime build failed");
                        // Can't connect without a runtime; signal the UI so
                        // it doesn't wait forever on a Connected that never
                        // comes.
                        let _ = event_tx.send(ViewerEvent::Disconnected);
                        ctx.request_repaint();
                        return;
                    }
                };
                rt.block_on(run(hub_url, token, session_id, cmd_rx, event_tx, ctx));
            })
            .expect("spawn viewer thread");

        ViewerHandle { cmd_tx, event_rx }
    }
}

/// Emit one event to the UI and wake it. Returns false if the UI side is
/// gone (window closed / handle dropped) so the caller can stop.
fn emit(event_tx: &Sender<ViewerEvent>, ctx: &egui::Context, ev: ViewerEvent) -> bool {
    let ok = event_tx.send(ev).is_ok();
    ctx.request_repaint();
    ok
}

/// Connect the viewer ws and pump it until it closes or the UI asks to
/// stop. On any exit we emit `Disconnected` (best-effort) so the UI never
/// hangs on a half-open panel.
async fn run(
    hub_url: String,
    token: String,
    session_id: String,
    cmd_rx: Receiver<ViewerCommand>,
    event_tx: Sender<ViewerEvent>,
    ctx: egui::Context,
) {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let url = match build_viewer_ws_url(&hub_url, &session_id, &token) {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "viewer: bad ws url");
            emit(&event_tx, &ctx, ViewerEvent::Disconnected);
            return;
        }
    };

    let ws = match tokio_tungstenite::connect_async(&url).await {
        Ok((ws, _resp)) => ws,
        Err(e) => {
            tracing::warn!(error = %e, "viewer: connect failed");
            emit(&event_tx, &ctx, ViewerEvent::Disconnected);
            return;
        }
    };

    // The hub authenticates + runs its ownership guard during the upgrade;
    // a successful ws handshake means we're attached. Tell the UI.
    if !emit(&event_tx, &ctx, ViewerEvent::Connected) {
        return; // UI gone already
    }

    event_loop(ws, &cmd_rx, &event_tx, &ctx).await;

    // Whatever ended the loop (ws close, UI Close, send error), surface a
    // single Disconnected. Dropping `ws` here closes the socket, which the
    // hub turns into a ViewerDetach for the agent.
    emit(&event_tx, &ctx, ViewerEvent::Disconnected);
}

/// Steady-state pump: viewer-ws binary frames → `ViewerEvent::Frame`; UI
/// `ViewerCommand`s → ws (input as Text, Close drops the socket).
///
/// `std::sync::mpsc::Receiver` is blocking and not tokio-pollable, so we
/// drain it with `try_recv` on a short interval inside the `select!`,
/// exactly like `backend::event_loop`. 8ms ≈ 120Hz keeps input latency
/// well under what a human notices.
async fn event_loop<S>(
    mut ws: S,
    cmd_rx: &Receiver<ViewerCommand>,
    event_tx: &Sender<ViewerEvent>,
    ctx: &egui::Context,
) where
    S: futures::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error>
        + Unpin,
{
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(8));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            incoming = ws.next() => {
                match incoming {
                    // A JPEG frame from the hub → up to the UI.
                    Some(Ok(Message::Binary(jpeg))) => {
                        if !emit(event_tx, ctx, ViewerEvent::Frame(jpeg)) {
                            return; // UI gone
                        }
                    }
                    // A downlink Text frame — currently only the `targets`
                    // envelope (the agent's tab list). Unparseable text is
                    // logged and skipped, never a teardown.
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ViewerDownlinkText>(&text) {
                            Ok(ViewerDownlinkText::Targets { targets }) => {
                                if !emit(event_tx, ctx, ViewerEvent::Targets(targets)) {
                                    return; // UI gone
                                }
                            }
                            Err(e) => {
                                tracing::debug!(
                                    error = %e,
                                    "viewer: unparseable downlink text; skipping"
                                );
                            }
                        }
                    }
                    // tungstenite answers pings itself.
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Err(e)) => {
                        tracing::debug!(error = %e, "viewer: ws read error");
                        return;
                    }
                    // Ping/Pong/Frame: nothing for the viewer to do.
                    Some(Ok(_)) => {}
                }
            }
            _ = tick.tick() => {
                loop {
                    match cmd_rx.try_recv() {
                        Ok(ViewerCommand::SendInput(ev)) => {
                            // Serialize to the hub's JSON shape and send as
                            // Text. A send error means the socket is dead;
                            // end the loop so the UI sees Disconnected.
                            let json = match serde_json::to_string(&ev) {
                                Ok(j) => j,
                                Err(_) => continue, // unserializable; drop it
                            };
                            if ws.send(Message::Text(json)).await.is_err() {
                                return;
                            }
                        }
                        Ok(ViewerCommand::SelectTarget(id)) => {
                            // Tab switch: the exact uplink shape the hub's
                            // `parse_viewer_uplink` accepts.
                            if ws.send(Message::Text(select_target_json(&id))).await.is_err() {
                                return;
                            }
                        }
                        Ok(ViewerCommand::Close) => {
                            let _ = ws.close().await;
                            return;
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        // UI dropped the handle (window closed) — stop.
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                    }
                }
            }
        }
    }
}

/// Build the viewer ws URL from the http(s) hub base, mirroring
/// `wire::build_ws_url` but for `/v1/viewer/ws` and with the
/// `session` + `token` query params the hub's `ViewerQuery` reads.
///
/// `http`/`ws` → `ws`, `https`/`wss` → `wss`; a trailing slash on the base
/// is trimmed. `session_id` and `token` are percent-encoded so unusual
/// characters can't break the query (tokens are hex so this is belt-and-
/// braces, but session ids and future token formats stay safe).
pub fn build_viewer_ws_url(hub_url: &str, session_id: &str, token: &str) -> Result<String> {
    let base = hub_url.trim_end_matches('/');
    let (scheme, rest) = base
        .split_once("://")
        .ok_or_else(|| anyhow!("hub_url missing scheme: {}", hub_url))?;
    let ws_scheme = match scheme {
        "http" | "ws" => "ws",
        "https" | "wss" => "wss",
        other => return Err(anyhow!("unsupported scheme: {}", other)),
    };
    let session = urlencode(session_id);
    let token = urlencode(token);
    Ok(format!(
        "{}://{}/v1/viewer/ws?session={}&token={}",
        ws_scheme, rest, session, token
    ))
}

/// Minimal percent-encoding for query-component values. Encodes everything
/// outside the RFC 3986 "unreserved" set (`A-Z a-z 0-9 - _ . ~`). Kept
/// dependency-free — `url`/`percent-encoding` aren't in the app's tree and
/// the inputs (uuids, hex tokens) are tiny.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_session_token_becomes_ws() {
        assert_eq!(
            build_viewer_ws_url("http://h:7100", "sess-1", "cc_abc").unwrap(),
            "ws://h:7100/v1/viewer/ws?session=sess-1&token=cc_abc"
        );
    }

    #[test]
    fn https_becomes_wss_and_trailing_slash_trimmed() {
        assert_eq!(
            build_viewer_ws_url("https://hub.example.com/", "s", "t").unwrap(),
            "wss://hub.example.com/v1/viewer/ws?session=s&token=t"
        );
    }

    #[test]
    fn ws_and_wss_schemes_pass_through() {
        assert_eq!(
            build_viewer_ws_url("ws://h:1/", "s", "t").unwrap(),
            "ws://h:1/v1/viewer/ws?session=s&token=t"
        );
        assert_eq!(
            build_viewer_ws_url("wss://h/", "s", "t").unwrap(),
            "wss://h/v1/viewer/ws?session=s&token=t"
        );
    }

    #[test]
    fn query_values_are_percent_encoded() {
        // A uuid session id with a token containing a reserved char must
        // not break the query string.
        let url = build_viewer_ws_url(
            "http://h:7100",
            "11111111-2222-3333-4444-555555555555",
            "tok/with+amp&eq=x",
        )
        .unwrap();
        assert_eq!(
            url,
            "ws://h:7100/v1/viewer/ws?session=11111111-2222-3333-4444-555555555555\
             &token=tok%2Fwith%2Bamp%26eq%3Dx"
        );
    }

    #[test]
    fn missing_scheme_errors() {
        assert!(build_viewer_ws_url("h:7100", "s", "t").is_err());
    }

    #[test]
    fn unsupported_scheme_errors() {
        assert!(build_viewer_ws_url("ftp://h", "s", "t").is_err());
    }
}
