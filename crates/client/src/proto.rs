//! Mirror of the hub's `pty_proto.rs`. Keep in lockstep.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

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
    /// List every workspace bound to this account across all agents.
    /// Each item carries its owning agent name + whether the agent
    /// is currently online.
    ListWorkspaces,
    /// Create workspace `name` bound to `agent`. The owning agent
    /// is locked in at create time and never changes.
    CreateWorkspace {
        name: String,
        agent: String,
    },
    DeleteWorkspace {
        name: String,
        agent: String,
    },
    /// Wipe the saved session for a workspace without touching its
    /// files: kills the per-workspace tmux server (terminating
    /// `claude --continue`'s breadcrumb) and removes claude's
    /// per-project history. The next OpenSession on this workspace
    /// will get a fresh claude with the args the user passes.
    ResetWorkspace {
        name: String,
        agent: String,
    },
    /// Open a PTY session in the given workspace. The owning agent
    /// is carried through from the workspace identity, so the menu
    /// no longer needs an explicit SelectAgent step.
    OpenSession {
        workspace: String,
        agent: String,
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
    /// CLI file-drop (Phase 2): start uploading a local file into the
    /// session's workspace. The hub relays this to the owning agent's
    /// `FsWriteInit`/`FsWriteChunk` write path (conflict-safe naming +
    /// the byte-count integrity check) and replies with
    /// `HubToClient::FsWriteResult`. `path` is the destination dir +
    /// filename (e.g. `.cloudcode/uploads/foo.png`), matching how the
    /// HTTP upload builds its `target_path`.
    FsWriteInit {
        request_id: Uuid,
        agent: String,
        workspace: String,
        path: String,
    },
    /// CLI file-drop: one chunk of the file body. `data_b64` is a
    /// base64-encoded 64 KiB slice; `eof = true` (with empty
    /// `data_b64`) terminates the stream for `request_id`.
    FsWriteChunk {
        request_id: Uuid,
        #[serde(default)]
        data_b64: String,
        #[serde(default)]
        eof: bool,
    },
    /// In-session: one opaque MCP JSON-RPC frame from the client's
    /// browser MCP subprocess back toward claude. Hub forwards it to
    /// the bound agent as `ServerMsg::BrowserRpc` tagged with the
    /// active session_id. Payload is never parsed in transit.
    BrowserRpc {
        /// Opaque MCP JSON-RPC frame as raw text. Never parsed in transit.
        payload: String,
    },
    /// In-session: client tearing down its browser channel.
    BrowserClosed {
        #[serde(default)]
        reason: Option<String>,
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
    /// CLI file-drop result for a `FsWriteInit`/`FsWriteChunk` upload.
    /// On success `final_name` is the name the agent actually wrote
    /// (after any ` (n)` conflict suffix) and `error` is `None`; on
    /// failure `error` carries the reason and `final_name` is `None`.
    FsWriteResult {
        request_id: Uuid,
        #[serde(default)]
        final_name: Option<String>,
        #[serde(default)]
        error: Option<String>,
    },
    /// One opaque MCP JSON-RPC frame from claude (via the agent) toward
    /// the client's browser MCP subprocess. Payload is never parsed.
    BrowserRpc {
        /// Opaque MCP JSON-RPC frame as raw text. Never parsed in transit.
        payload: String,
    },
    /// Hub/agent tore down the browser channel (denied / disconnect /
    /// task ended). Client should stop its MCP subprocess.
    BrowserClosed {
        #[serde(default)]
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

/// Per-workspace status badge data, returned alongside the workspace
/// name in HubToClient::WorkspaceList. The `agent` field is required
/// as of v1.13 (the picker shows it next to the name). Older hubs
/// that omit it get a placeholder via `#[serde(default)]`; the
/// picker treats that as "agent unknown, won't open".
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WorkspaceInfo {
    pub name: String,
    #[serde(default)]
    pub agent: String,
    #[serde(default)]
    pub agent_online: bool,
    #[serde(default)]
    pub tmux_alive: bool,
    #[serde(default)]
    pub has_client: bool,
}

#[cfg(test)]
mod browser_tests {
    use super::*;

    #[test]
    fn browser_rpc_both_directions_byte_exact() {
        let original = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"zebra":1,"alpha":2}}"#;
        let c = ClientToHub::BrowserRpc { payload: original.to_string() };
        let j = serde_json::to_string(&c).unwrap();
        assert!(j.contains("\"type\":\"browser_rpc\""));
        match serde_json::from_str::<ClientToHub>(&j).unwrap() {
            ClientToHub::BrowserRpc { payload } => assert_eq!(payload, original),
            _ => panic!("wrong variant"),
        }

        let h = HubToClient::BrowserRpc { payload: original.to_string() };
        let j2 = serde_json::to_string(&h).unwrap();
        assert!(j2.contains("\"type\":\"browser_rpc\""));
        match serde_json::from_str::<HubToClient>(&j2).unwrap() {
            HubToClient::BrowserRpc { payload } => assert_eq!(payload, original),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn browser_closed_roundtrips_and_defaults() {
        // with reason
        let c = ClientToHub::BrowserClosed { reason: Some("denied".to_string()) };
        let j = serde_json::to_string(&c).unwrap();
        match serde_json::from_str::<ClientToHub>(&j).unwrap() {
            ClientToHub::BrowserClosed { reason } => assert_eq!(reason.as_deref(), Some("denied")),
            _ => panic!("wrong variant"),
        }
        // reason omitted on the wire -> defaults to None
        let from_wire: HubToClient =
            serde_json::from_str(r#"{"type":"browser_closed"}"#).unwrap();
        match from_wire {
            HubToClient::BrowserClosed { reason } => assert_eq!(reason, None),
            _ => panic!("wrong variant"),
        }
    }
}
