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

// Re-exported for the Session split layout (Task 4) to consume: the App
// holds a `BrowserPanel` + an `Option<ViewerHandle>` and pumps
// `ViewerEvent`s / `ViewerCommand`s between them.
pub use client::{ViewerCommand, ViewerEvent, ViewerHandle};
pub use panel::BrowserPanel;

// Internal-only types (proto::TargetInfo, panel::ViewerUiOutput) ride
// through the re-exported enums/return values; main.rs never names them.
