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
//! P2 ignores `session_id` for page selection (single active-page screencast,
//! per the plan); it's carried through for the P4 per-session page mapping.

use crate::browser::chrome::ChromeManager;
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
    /// The per-connection sender to the ws writer task (agent → hub).
    tx: mpsc::Sender<OutFrame>,
    sessions: DashMap<Uuid, ScreencastSession>,
}

impl ViewerManager {
    pub fn new(chrome: Arc<ChromeManager>, tx: mpsc::Sender<OutFrame>) -> Self {
        Self {
            chrome,
            tx,
            sessions: DashMap::new(),
        }
    }

    /// Start a screencast for `viewer_session_id` and pump its frames to the
    /// hub. On start failure, reports `ClientMsg::ViewerClosed` to the hub so
    /// the viewer ws loop tears down promptly.
    ///
    /// `session_id` identifies the PTY/browser session the viewer asked to
    /// watch; P2 screencasts the single active page and ignores it.
    pub async fn attach(&self, viewer_session_id: Uuid, session_id: Uuid) {
        // Replacing an existing attach for the same viewer: stop the old one.
        if let Some((_, old)) = self.sessions.remove(&viewer_session_id) {
            old.stop().await;
        }

        let cdp_http_url = self.chrome.cdp_http_url();
        let (frame_tx, mut frame_rx) = mpsc::channel::<Vec<u8>>(FRAME_QUEUE);

        let session = match ScreencastSession::start(&cdp_http_url, frame_tx).await {
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
