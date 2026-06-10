//! Agent-side viewer manager (Desktop App P2 Task 2).
//!
//! Bridges the hub's `ServerMsg::Viewer*` control frames to the CDP
//! [`ScreencastSession`] machinery: one live screencast per `viewer_session_id`.
//!
//!   * [`ViewerManager::attach`] starts a screencast against the resident
//!     Chrome and spawns a pump that wraps each JPEG frame in a
//!     `TAG_SCREENCAST_FRAME` binary frame (keyed by `viewer_session_id`) and
//!     sends it to the hub over the per-connection `OutFrame` channel.
//!   * [`ViewerManager::input`] forwards a viewer's input event into the
//!     matching screencast (CDP `Input.*`).
//!   * [`ViewerManager::detach`] stops + removes a screencast.
//!
//! The manager is **per hub connection**: it's created in `ws::run_once` with
//! that connection's `OutFrame` sender and dropped when the connection ends,
//! which stops every in-flight screencast (no cross-reconnect leaks).
//!
//! P4 Task 1 wires `session_id` into page selection: `attach` resolves a
//! per-session page hint from the [`EndpointState`] (the mcp endpoint that owns
//! each session's playwright-mcp) and screencasts THAT session's page. When no
//! hint is known — the agent's current default playwright-mcp config shares one
//! browser context across sessions, so there is no distinct per-session target —
//! it falls back to the active page (documented residual risk; see
//! `docs/superpowers/plans/2026-06-10-p4-page-mapping-notes.md`).

use crate::browser::chrome::ChromeManager;
use crate::browser::mcp_endpoint::EndpointState;
use crate::browser::screencast::ScreencastSession;
use crate::pty::OutFrame;
use crate::tunnel::{pack_pty_frame, ClientMsg, ViewerInputEvent, TAG_SCREENCAST_FRAME};
use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use uuid::Uuid;

/// How many JPEG frames we let queue between the screencast read loop and the
/// hub send pump before dropping. Frames are large and latency-sensitive, so a
/// shallow buffer keeps us close to live instead of building backlog.
const FRAME_QUEUE: usize = 8;

/// One hub connection's set of live screencasts, keyed by `viewer_session_id`.
pub struct ViewerManager {
    chrome: Arc<ChromeManager>,
    /// The mcp endpoint state, queried for each session's screencast page hint
    /// (P4 Task 1). Shares Arc internals with the endpoint claude POSTs to, so a
    /// hint recorded against a session is visible here.
    mcp: EndpointState,
    /// The per-connection sender to the ws writer task (agent → hub).
    tx: mpsc::Sender<OutFrame>,
    sessions: DashMap<Uuid, ScreencastSession>,
}

impl ViewerManager {
    pub fn new(chrome: Arc<ChromeManager>, mcp: EndpointState, tx: mpsc::Sender<OutFrame>) -> Self {
        Self {
            chrome,
            mcp,
            tx,
            sessions: DashMap::new(),
        }
    }

    /// Start a screencast for `viewer_session_id` and pump its frames to the
    /// hub. On start failure, reports `ClientMsg::ViewerClosed` to the hub so
    /// the viewer ws loop tears down promptly.
    ///
    /// `session_id` identifies the PTY/browser session the viewer asked to
    /// watch. We resolve that session's page target hint from the mcp endpoint
    /// and screencast THAT page; if no hint is known (default config — sessions
    /// share one browser context, so there is no distinct per-session target),
    /// we fall back to the active page.
    pub async fn attach(&self, viewer_session_id: Uuid, session_id: Uuid) {
        // Replacing an existing attach for the same viewer: stop the old one.
        if let Some((_, old)) = self.sessions.remove(&viewer_session_id) {
            old.stop().await;
        }

        let cdp_http_url = self.chrome.cdp_http_url();
        // Per-session page hint (P4 Task 1). `None` → active-page fallback.
        let hint = self.mcp.page_hint_for(session_id);
        let (frame_tx, mut frame_rx) = mpsc::channel::<Vec<u8>>(FRAME_QUEUE);

        let session = match ScreencastSession::start(&cdp_http_url, hint.as_deref(), frame_tx).await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    viewer = %viewer_session_id,
                    session = %session_id,
                    error = %e,
                    "screencast start failed"
                );
                let _ = self
                    .tx
                    .send(OutFrame::Text(ClientMsg::ViewerClosed {
                        viewer_session_id,
                        reason: Some(format!("screencast start failed: {e}")),
                    }))
                    .await;
                return;
            }
        };

        // Pump: wrap each JPEG as a TAG_SCREENCAST_FRAME (keyed by
        // viewer_session_id) and ship it to the hub. Ends when the screencast
        // drops `frame_tx` (page closed / session stopped) or the hub sender
        // closes (connection gone). On a normal frame-channel close we tell the
        // hub the screencast ended.
        let tx = self.tx.clone();
        tokio::spawn(async move {
            while let Some(jpeg) = frame_rx.recv().await {
                let frame = pack_pty_frame(TAG_SCREENCAST_FRAME, viewer_session_id, &jpeg);
                if tx.send(OutFrame::Binary(frame)).await.is_err() {
                    // Hub connection gone; nothing more to do.
                    return;
                }
            }
            let _ = tx
                .send(OutFrame::Text(ClientMsg::ViewerClosed {
                    viewer_session_id,
                    reason: Some("screencast ended".into()),
                }))
                .await;
        });

        self.sessions.insert(viewer_session_id, session);
    }

    /// Inject a viewer input event into the matching screencast (no-op if the
    /// viewer has no live session). Synchronous — the CDP write is queued
    /// inside the screencast task.
    pub fn input(&self, viewer_session_id: Uuid, event: &ViewerInputEvent) {
        if let Some(session) = self.sessions.get(&viewer_session_id) {
            session.input(event);
        } else {
            tracing::debug!(viewer = %viewer_session_id, "input for unknown viewer; dropping");
        }
    }

    /// Stop and remove the screencast for `viewer_session_id`. Always calls
    /// `stop()` on the removed session (per T1's leak note) so the CDP ws and
    /// its task are torn down even if the viewer detaches mid-stream.
    pub async fn detach(&self, viewer_session_id: Uuid) {
        if let Some((_, session)) = self.sessions.remove(&viewer_session_id) {
            session.stop().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BrowserConfig;
    use crate::tunnel::unpack_pty_frame;
    use std::time::Duration;

    /// P2 end-to-end (T1+T2) integration: drive the real agent-internal viewer
    /// path against a real headless Chrome — `ViewerManager::attach` →
    /// `ScreencastSession` → a `TAG_SCREENCAST_FRAME`-tagged binary frame lands
    /// on the per-connection `OutFrame` channel keyed by the viewer_session_id —
    /// then inject a viewer input (mouse + IME InsertText) and detach. This is
    /// the agent half of the screencast pipe with NO hub/browser-UI: it proves
    /// the same wiring the hub's viewer ws relay drives in production.
    ///
    /// Run manually:
    /// `cargo test -p cloudcode-agent viewer_attach_streams -- --ignored --nocapture`
    /// Prereqs: a real Chrome/Chromium install on PATH (no internet needed; the
    /// page is a self-contained `data:` URL).
    #[tokio::test]
    #[ignore = "requires a real Chrome install; run manually"]
    async fn viewer_attach_streams_tagged_jpeg_frame_and_accepts_input() {
        let cfg = BrowserConfig {
            enabled: true,
            chrome_path: None,
            cdp_port: 19245,
            mcp_port: 7111,
            mcp_command: None,
        };
        let tmp = tempfile::tempdir().unwrap();
        let chrome = Arc::new(ChromeManager::new(cfg.clone(), tmp.path()));
        chrome
            .start()
            .await
            .expect("Chrome should start and become ready");
        let cdp = chrome.cdp_http_url();

        // Open a non-blank page so the screencast has real pixels. Newer Chrome
        // wants PUT on /json/new; older accepts GET. Try PUT first, GET fallback.
        let data_url = "data:text/html,<h1 style=font-size:80px>P2-VIEWER</h1>";
        let new_url = format!("{cdp}/json/new?{data_url}");
        let http = reqwest::Client::new();
        let opened = match http.put(&new_url).send().await {
            Ok(r) if r.status().is_success() => true,
            _ => matches!(http.get(&new_url).send().await, Ok(r) if r.status().is_success()),
        };
        assert!(opened, "failed to open a data: page target via /json/new");
        // Give Chrome a moment to register + render the new target.
        tokio::time::sleep(Duration::from_millis(800)).await;

        // Per-connection OutFrame channel — exactly what ws::run_once hands the
        // ViewerManager. The hub's viewer ws relay drains this same channel.
        let (out_tx, mut out_rx) = mpsc::channel::<OutFrame>(32);
        let mcp = EndpointState::new(Arc::clone(&chrome), cfg.clone());
        let manager = ViewerManager::new(Arc::clone(&chrome), mcp, out_tx);

        let viewer_session_id = Uuid::new_v4();
        // No page hint recorded → active-page fallback (default config).
        let session_id = Uuid::new_v4();
        manager.attach(viewer_session_id, session_id).await;

        // Drain OutFrames until the first Binary screencast frame (skip any
        // interleaved Text). Must arrive within 5s. A ViewerClosed Text means
        // the screencast failed to start — surface it loudly.
        let frame = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match out_rx.recv().await {
                    Some(OutFrame::Binary(b)) => break b,
                    Some(OutFrame::Text(ClientMsg::ViewerClosed { reason, .. })) => {
                        panic!("screencast closed before any frame: {reason:?}");
                    }
                    Some(OutFrame::Text(_)) => continue,
                    None => panic!("OutFrame channel closed before any frame"),
                }
            }
        })
        .await
        .expect("a screencast frame should arrive within 5s");

        // Unpack the binary frame: TAG_SCREENCAST_FRAME, keyed by the
        // viewer_session_id, payload is a raw JPEG (magic bytes FF D8).
        let (tag, id, payload) =
            unpack_pty_frame(&frame).expect("frame must unpack as [tag][16B id][payload]");
        assert_eq!(
            tag, TAG_SCREENCAST_FRAME,
            "binary frame must carry the screencast tag 0x03"
        );
        assert_eq!(
            id, viewer_session_id,
            "frame must be keyed by the viewer_session_id"
        );
        assert!(payload.len() >= 2, "JPEG payload too short");
        assert_eq!(
            &payload[0..2],
            &[0xFF, 0xD8],
            "payload must start with JPEG magic bytes FF D8; got {:02X} {:02X}",
            payload[0],
            payload[1]
        );
        eprintln!(
            "got TAG_SCREENCAST_FRAME (0x{tag:02X}) viewer={id} JPEG {} bytes, magic {:02X} {:02X}",
            payload.len(),
            payload[0],
            payload[1]
        );

        // Inject viewer input down the same path the hub relay uses: a mouse
        // move and an IME InsertText (Chinese). Must not panic; must NOT trip a
        // ViewerClosed error on the channel.
        manager.input(viewer_session_id, &ViewerInputEvent::MouseMove { x: 20.0, y: 20.0 });
        manager.input(
            viewer_session_id,
            &ViewerInputEvent::InsertText { text: "你好".into() },
        );
        // Input to an unknown viewer is a harmless no-op (no panic).
        manager.input(Uuid::new_v4(), &ViewerInputEvent::MouseMove { x: 0.0, y: 0.0 });

        // Drain briefly: confirm the channel is still alive and hasn't surfaced
        // an error ViewerClosed off the input path.
        match tokio::time::timeout(Duration::from_millis(300), out_rx.recv()).await {
            Ok(Some(OutFrame::Text(ClientMsg::ViewerClosed { reason, .. }))) => {
                panic!("unexpected ViewerClosed after input: {reason:?}");
            }
            // More frames or a quiet channel are both fine.
            _ => {}
        }

        // Detach tears down the CDP ws + read task (no leak).
        manager.detach(viewer_session_id).await;
        drop(chrome);
    }
}
