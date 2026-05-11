//! Mirror of the hub's `session_proto.rs` schema. Keep in lockstep.

use serde::{Deserialize, Serialize};

pub const SESSION_PROTOCOL_VERSION: &str = "1";

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientToHub {
    Hello {
        token: String,
        version: String,
    },
    OpenSession {
        #[serde(default)]
        agent: Option<String>,
        workspace: String,
    },
    Input {
        content: String,
    },
    Interrupt,
    SwitchWorkspace {
        workspace: String,
    },
    ListWorkspaces,
    CreateWorkspace {
        name: String,
    },
    DeleteWorkspace {
        name: String,
    },
    Close,
    Pong,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HubToClient {
    Welcome {
        account: String,
    },
    Rejected {
        reason: String,
    },
    SessionOpened {
        agent: String,
        workspace: String,
        cwd: String,
    },
    TurnStarted,
    ClaudeEvent {
        event: String,
    },
    TurnEnded {
        exit_code: i32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    WorkspaceSwitched {
        workspace: String,
        cwd: String,
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
    SessionError {
        message: String,
    },
    SessionClosed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    Ping,
}
