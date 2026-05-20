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
    /// hub pick the first online agent (alphabetically). Becomes
    /// semi-redundant in v1.13 — `OpenSession` now carries an explicit
    /// `agent` field — but is kept for AgentList/SelectAgent flows the
    /// CLI menu relies on. No state on the hub end beyond a cached
    /// pointer for `AgentList { current }` rendering.
    SelectAgent {
        #[serde(default)]
        agent: Option<String>,
    },
    /// Pre-session: list online agents.
    ListAgents,
    /// List workspaces visible to this account. In v1.13 workspaces are
    /// hub-canonical and per-account; they are NOT routed to any agent.
    /// The reply's `WorkspaceInfo.locked_by_agent` field tells the
    /// caller which agent (if any) currently owns each workspace's lock.
    ListWorkspaces,
    /// Create a hub-canonical workspace under the calling account.
    /// Errors if `(account, name)` already exists.
    CreateWorkspace {
        name: String,
    },
    /// Delete a hub-canonical workspace. Errors if it is currently
    /// locked by an agent — the caller must wait for the lock to
    /// release (or remove the agent first).
    DeleteWorkspace {
        name: String,
    },
    /// Clear the saved session for a workspace (kill its tmux server,
    /// wipe claude conversation history) without removing the workspace
    /// directory itself.
    ResetWorkspace {
        name: String,
    },
    /// Open a PTY session in a workspace on a specific agent. The hub
    /// streams the canonical workspace bytes down to the agent, takes
    /// the workspace lock, and only then issues `PtyOpen`. `claude_args`
    /// is forwarded verbatim to `claude`'s argv on first spawn.
    ///
    /// `force = true` lets the caller wrest the lock away from another
    /// agent that currently holds it (typically because that agent went
    /// offline). The old holder's local copy is queued for cleanup via
    /// `pending_workspace_cleanups` so when it reconnects it deletes
    /// its stale working copy before doing anything else.
    OpenSession {
        workspace: String,
        /// Explicit target agent. v1.13 wire shape — older clients that
        /// relied on `SelectAgent`'s implicit binding still work because
        /// the CLI client always sets this field now.
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
    /// `code` and `workspace_lock_holder` are populated for the
    /// specific "workspace is in use" case so the CLI can offer a
    /// force-take confirmation instead of bouncing back to the menu.
    /// Older clients ignore the extra fields and just show `message`.
    /// Use `HubToClient::session_error(...)` /
    /// `HubToClient::workspace_locked(...)` to construct.
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
    /// Tools this agent can actually launch (auto-detected from PATH
    /// on the agent host, minus anything `agent.toml [tools.<name>]
    /// disabled = true` opted out of). Empty if the agent is
    /// pre-v1.13 — clients should treat that as "unknown" and fall
    /// back to their built-in tool list.
    #[serde(default)]
    pub tools: Vec<String>,
}

/// Workspace status row carried in HubToClient::WorkspaceList.
///
/// v1.13 redefined this around the hub-canonical model: workspaces no
/// longer live on a specific agent, so we surface the lock holder
/// (`locked_by_agent`), the timestamp of the last successful sync
/// (`last_sync_at`), and the size of the canonical copy on disk
/// (`size_bytes`). All three come straight from the hub's `workspaces`
/// table.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WorkspaceInfo {
    pub name: String,
    /// Agent currently holding the write lock, if any. `None` means the
    /// workspace is free for any agent to pick up.
    #[serde(default)]
    pub locked_by_agent: Option<String>,
    /// Unix seconds of the last successful agent → hub sync. `None`
    /// for brand-new workspaces that have never been synced.
    #[serde(default)]
    pub last_sync_at: Option<i64>,
    /// Aggregate size in bytes of the canonical copy on disk. `0` for
    /// empty workspaces.
    #[serde(default)]
    pub size_bytes: u64,
}

impl HubToClient {
    /// Plain SessionError with just a message. Use for the common
    /// "something went wrong" case.
    pub fn session_error(message: impl Into<String>) -> Self {
        Self::SessionError {
            message: message.into(),
            code: None,
            workspace_lock_holder: None,
        }
    }

    /// SessionError specifically for the workspace-lock conflict.
    /// The CLI uses `code == "workspace_locked"` to recognize this
    /// path and offer a force-take confirmation prompt.
    pub fn workspace_locked(message: impl Into<String>, holder: impl Into<String>) -> Self {
        Self::SessionError {
            message: message.into(),
            code: Some("workspace_locked".into()),
            workspace_lock_holder: Some(holder.into()),
        }
    }
}
