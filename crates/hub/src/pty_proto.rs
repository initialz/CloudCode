//! Wire schema for the client ↔ hub WebSocket on `/v1/pty/ws`.
//! Mirrored verbatim in `crates/client/src/proto.rs`.

use serde::{Deserialize, Serialize};

#[allow(dead_code)]
pub const PTY_PROTOCOL_VERSION: &str = "1";

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientToHub {
    Hello {
        token: String,
        version: String,
    },
    /// Pre-session: bind this client connection to an agent. `None` lets the
    /// hub pick the first online agent (alphabetically). All subsequent
    /// workspace ops + the eventual OpenSession use this agent.
    SelectAgent {
        #[serde(default)]
        agent: Option<String>,
    },
    /// Pre-session: list online agents.
    ListAgents,
    /// Pre-session (or in-session): list workspaces on the selected agent.
    ListWorkspaces,
    CreateWorkspace {
        name: String,
    },
    DeleteWorkspace {
        name: String,
    },
    /// Clear the saved session for a workspace (kill its tmux server,
    /// wipe claude conversation history) without removing the workspace
    /// directory itself.
    ResetWorkspace {
        name: String,
    },
    /// Open a PTY session in the given workspace on the selected agent.
    /// `claude_args` is forwarded verbatim to `claude`'s argv when the
    /// session is first created (tmux ignores it on re-attach).
    OpenSession {
        workspace: String,
        cols: u16,
        rows: u16,
        #[serde(default)]
        claude_args: Vec<String>,
    },
    /// In-session: terminal-size change (SIGWINCH).
    Resize {
        cols: u16,
        rows: u16,
    },
    /// Voluntary client-initiated close (ends the whole connection).
    Close,
    Pong,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HubToClient {
    Welcome {
        account: String,
    },
    /// Connection-level failure (auth, no agent online, …) — terminal.
    Rejected {
        reason: String,
    },
    /// Reply to SelectAgent.
    AgentSelected {
        agent: String,
    },
    /// Reply to ListAgents.
    AgentList {
        items: Vec<AgentInfo>,
    },
    /// Reply to ListWorkspaces. Each item carries enough state for
    /// the picker to render the right badge (active / saved / blank).
    WorkspaceList {
        items: Vec<WorkspaceInfo>,
    },
    WorkspaceCreated {
        name: String,
    },
    WorkspaceDeleted {
        name: String,
    },
    WorkspaceReset {
        name: String,
    },
    /// PTY session is up.
    SessionOpened {
        agent: String,
        workspace: String,
        cwd: String,
    },
    /// PTY session ended; client should drop raw mode and return to menu.
    SessionClosed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Non-fatal error (failed op, busy, ...). Connection stays up.
    SessionError {
        message: String,
    },
    Ping,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AgentInfo {
    pub name: String,
    #[serde(default)]
    pub current: bool,
}

/// Workspace status row carried in HubToClient::WorkspaceList.
///
/// - `tmux_alive` = agent has a live tmux server for this workspace
///   (so the previous claude state is still recoverable).
/// - `has_client` = some cloudcode client is currently attached to it.
///   Opening it would trigger take-over.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WorkspaceInfo {
    pub name: String,
    #[serde(default)]
    pub tmux_alive: bool,
    #[serde(default)]
    pub has_client: bool,
}
