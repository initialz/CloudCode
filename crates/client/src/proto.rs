//! Client ↔ hub wire schema.
//!
//! The canonical definitions now live in the shared `cloudcode-proto`
//! crate (the same types the hub consumes). This module is a thin
//! re-export so existing `crate::proto::X` call sites keep working
//! unchanged. The client never constructs the webterm-only variants
//! (`SplitPane`/`ChangeLayout`); they're carried for wire compatibility.

// The CLI client only constructs/matches a subset of the protocol; the
// webterm-only types (SplitDirection/PaneLayout) and AgentInfo are re-exported
// for a complete, drop-in module surface even though the CLI doesn't name them.
#[allow(unused_imports)]
pub use cloudcode_proto::{
    AgentInfo, ClientToHub, HubToClient, PaneLayout, SplitDirection, WorkspaceInfo,
    PTY_PROTOCOL_VERSION,
};
