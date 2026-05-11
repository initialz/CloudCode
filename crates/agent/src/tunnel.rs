use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const PROTOCOL_VERSION: &str = "3";

/// Frames sent from the agent to the hub.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello {
        name: String,
        secret: String,
        version: String,
    },
    Pong,

    /// Session lifecycle (agent confirms creation / restart / close).
    SessionOpened {
        session_id: Uuid,
        workspace: String,
        cwd: String,
    },
    /// Emitted once per turn after claude prints its first `system/init`
    /// frame; carries the claude-side session id for `--resume`.
    SessionTurnStarted {
        session_id: Uuid,
        claude_session_id: String,
    },
    /// One stream-json line from claude (verbatim).
    SessionEvent {
        session_id: Uuid,
        event: String,
    },
    SessionTurnEnded {
        session_id: Uuid,
        exit_code: i32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    SessionWorkspaceSwitched {
        session_id: Uuid,
        workspace: String,
        cwd: String,
    },
    SessionClosed {
        session_id: Uuid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    SessionError {
        session_id: Uuid,
        message: String,
    },

    /// Workspace management replies (not bound to a session).
    WorkspaceListResult {
        request_id: Uuid,
        items: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    WorkspaceCreateResult {
        request_id: Uuid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    WorkspaceDeleteResult {
        request_id: Uuid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

/// Frames sent from the hub to the agent.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Welcome {
        name: String,
    },
    Rejected {
        reason: RejectReason,
    },
    Ping,

    /// Open a new session in the given workspace (mkdir if missing).
    SessionStart {
        session_id: Uuid,
        workspace: String,
    },
    /// Run one user turn. `resume` is None on the first turn; Some thereafter
    /// (carries the claude_session_id from a previous SessionTurnStarted).
    SessionInput {
        session_id: Uuid,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume: Option<String>,
    },
    SessionInterrupt {
        session_id: Uuid,
    },
    SessionSwitchWorkspace {
        session_id: Uuid,
        workspace: String,
    },
    SessionStop {
        session_id: Uuid,
    },

    WorkspaceList {
        request_id: Uuid,
    },
    WorkspaceCreate {
        request_id: Uuid,
        name: String,
    },
    WorkspaceDelete {
        request_id: Uuid,
        name: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    NameInvalid,
    NameTaken,
    AuthFailed,
    VersionMismatch,
}
