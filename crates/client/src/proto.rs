//! Mirror of the hub's `pty_proto.rs`. Keep in lockstep.

use serde::{Deserialize, Serialize};

pub const PTY_PROTOCOL_VERSION: &str = "1";

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientToHub {
    Hello {
        token: String,
        version: String,
    },
    /// Open a PTY session. The hub picks an agent (preferring `agent` if
    /// given), claims the workspace mutex, allocates a session_id, and tells
    /// the agent to spawn tmux+claude with the given initial size.
    OpenSession {
        #[serde(default)]
        agent: Option<String>,
        workspace: String,
        cols: u16,
        rows: u16,
    },
    /// Terminal-size change (SIGWINCH).
    Resize {
        cols: u16,
        rows: u16,
    },
    /// Move the session to another workspace on the same agent (releases the
    /// old workspace mutex, claims the new one). The agent re-attaches the
    /// PTY to `cloudcode-<new>` tmux session, creating it if missing.
    SwitchWorkspace {
        workspace: String,
    },
    /// Workspace metadata ops — forwarded to the bound agent.
    ListWorkspaces,
    CreateWorkspace {
        name: String,
    },
    DeleteWorkspace {
        name: String,
    },
    /// Snapshot of currently-online agents (with `current=true` on the one
    /// this session is bound to).
    ListAgents,
    /// Voluntary client-initiated close.
    Close,
    Pong,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HubToClient {
    Welcome {
        account: String,
    },
    /// Pre-session failure (auth, no agent online, workspace busy, …).
    Rejected {
        reason: String,
    },
    SessionOpened {
        agent: String,
        workspace: String,
        cwd: String,
    },
    WorkspaceSwitched {
        workspace: String,
        cwd: String,
    },
    AgentList {
        items: Vec<AgentInfo>,
    },
    WorkspaceList {
        items: Vec<String>,
    },
    WorkspaceCreated {
        name: String,
    },
    WorkspaceDeleted {
        name: String,
    },
    /// Recoverable error during an active session.
    SessionError {
        message: String,
    },
    /// Terminal: server side has shut the session down.
    SessionClosed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    Ping,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AgentInfo {
    pub name: String,
    #[serde(default)]
    pub current: bool,
}
