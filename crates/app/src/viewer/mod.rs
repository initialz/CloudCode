//! The desktop app's browser-panel viewer client (P4 Task 2).
//!
//! This is the *second* ws the app opens to the hub — `backend.rs` owns
//! the PTY ws (terminal), this owns the viewer ws (`/v1/viewer/ws`) that
//! carries the headless-Chrome screencast and human input for the right-
//! hand browser panel.
//!
//! - [`proto`]  — app-local mirror of the hub's `ViewerInputEvent` JSON
//!   shape (hub `viewer_session.rs` is the source of truth).
//! - [`client`] — the transport: a backend task that relays JPEG frames
//!   down (`ViewerEvent::Frame`) and `ViewerInputEvent`s up, bridged to the
//!   egui UI over channels via [`client::ViewerHandle`].
//!
//! - [`panel`]  — the browser panel: decodes JPEG frames to an egui
//!   texture, draws them letterboxed, and captures mouse/keyboard/IME into
//!   `ViewerInputEvent`s (Task 3). Wired into the Session split in Task 4.

pub mod client;
pub mod panel;
pub mod proto;

// Re-exported for the browser panel + Session screen to consume. Unused
// until Task 4 wires the panel into the Session split layout.
#[allow(unused_imports)]
pub use client::{ViewerCommand, ViewerEvent, ViewerHandle};
#[allow(unused_imports)]
pub use panel::BrowserPanel;
#[allow(unused_imports)]
pub use proto::ViewerInputEvent;
