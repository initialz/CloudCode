//! Wire schema for the client ↔ hub WebSocket on `/v1/pty/ws`.
//!
//! The canonical definitions now live in the shared `cloudcode-proto`
//! crate (consumed identically by the CLI client and the desktop app).
//! This module is a thin re-export so existing `crate::pty_proto::X`
//! call sites keep working unchanged.

// `PTY_PROTOCOL_VERSION` isn't named by the hub itself (the client sends it in
// `Hello`); re-exported for completeness without tripping the unused warning.
#[allow(unused_imports)]
pub use cloudcode_proto::{
    AgentInfo, ClientToHub, HubToClient, PaneLayout, SplitDirection, WorkspaceInfo,
    PTY_PROTOCOL_VERSION,
};
