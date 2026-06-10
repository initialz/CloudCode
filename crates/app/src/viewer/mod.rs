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
//! Rendering frames to textures and capturing egui input is Task 3; this
//! module is wiring + transport only.

pub mod client;
pub mod proto;

// Re-exported for Task 3 (the browser panel) to consume. Unused until then.
#[allow(unused_imports)]
pub use client::{ViewerCommand, ViewerEvent, ViewerHandle};
#[allow(unused_imports)]
pub use proto::ViewerInputEvent;
