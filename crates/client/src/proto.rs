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
    /// Wipe the saved session for a workspace without touching its
    /// files: kills the per-workspace tmux server (terminating
    /// `claude --continue`'s breadcrumb) and removes claude's
    /// per-project history. The next OpenSession on this workspace
    /// will get a fresh claude with the args the user passes.
    ResetWorkspace {
        name: String,
    },
    /// Open a PTY session in the given workspace on the selected agent.
    /// `claude_args` is forwarded verbatim to `claude`'s argv when the
    /// session is first created (tmux ignores it on re-attach, so it
    /// only affects the very first spawn for this workspace).
    OpenSession {
        workspace: String,
        cols: u16,
        rows: u16,
        #[serde(default)]
        claude_args: Vec<String>,
        /// Which tool to run inside the workspace (claude / codex / ...).
        /// None lets the agent fall back to its configured default
        /// (`[tools].default` in agent.toml).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool: Option<String>,
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
    /// Tools the agent will spawn, reported via its Hello frame.
    /// Mirror of `pty_proto::AgentInfo::tools` — see that doc.
    #[serde(default)]
    pub tools: Vec<String>,
}

/// Per-workspace status badge data, returned alongside the workspace
/// name in HubToClient::WorkspaceList.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WorkspaceInfo {
    pub name: String,
    #[serde(default)]
    pub tmux_alive: bool,
    #[serde(default)]
    pub has_client: bool,
}
