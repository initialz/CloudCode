//! Mirror of the hub's `pty_proto.rs`. Keep in lockstep.

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
    /// Pre-session: bind this client connection to an agent. `None` lets
    /// the hub pick the first online agent (alphabetically). Becomes
    /// semi-redundant in v1.13 — `OpenSession` carries an explicit
    /// `agent` — but the CLI menu uses AgentList/SelectAgent to render
    /// the picker, so we keep them.
    SelectAgent {
        #[serde(default)]
        agent: Option<String>,
    },
    /// Pre-session: list online agents.
    ListAgents,
    /// List workspaces visible to this account. Hub-canonical and
    /// per-account in v1.13 — no agent in scope.
    ListWorkspaces,
    CreateWorkspace {
        name: String,
    },
    DeleteWorkspace {
        name: String,
    },
    /// Wipe the saved session for a workspace without touching its
    /// files: kills the per-workspace tmux server (terminating
    /// `claude --continue`'s breadcrumb) and removes claude's
    /// per-project history. The next OpenSession on this workspace
    /// will get a fresh claude with the args the user passes.
    ResetWorkspace {
        name: String,
    },
    /// Open a PTY session in the given workspace on the given agent.
    /// v1.13: hub streams canonical bytes to the agent then issues
    /// PtyOpen. `force=true` wrests the workspace lock from whichever
    /// agent currently holds it (the previous holder's local copy is
    /// queued for cleanup).
    OpenSession {
        workspace: String,
        agent: String,
        #[serde(default)]
        force: bool,
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
    /// Reply to ListWorkspaces. Each item carries its current state
    /// (tmux_alive + has_client) so the picker can render badges.
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
    /// `code = "workspace_locked"` + `workspace_lock_holder = "<agent>"`
    /// signal a lock conflict the CLI can offer to force-take. Both
    /// stay None for everything else; an older hub omits them and the
    /// client falls back to a plain message-only error path.
    SessionError {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_lock_holder: Option<String>,
    },
    Ping,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AgentInfo {
    pub name: String,
    #[serde(default)]
    pub current: bool,
    /// Tools the agent will spawn, reported via its Hello frame.
    /// Mirror of `pty_proto::AgentInfo::tools` — see that doc.
    #[serde(default)]
    pub tools: Vec<String>,
}

/// Per-workspace state row carried in HubToClient::WorkspaceList.
/// v1.13: hub-canonical, per-account. Mirror of `pty_proto::WorkspaceInfo`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WorkspaceInfo {
    pub name: String,
    #[serde(default)]
    pub locked_by_agent: Option<String>,
    #[serde(default)]
    pub last_sync_at: Option<i64>,
    #[serde(default)]
    pub size_bytes: u64,
}
