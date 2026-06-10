//! Canonical wire schema for the client ↔ hub WebSocket on `/v1/pty/ws`.
//!
//! This is the single source of truth consumed by both `cloudcode-hub`
//! (via `crate::pty_proto`) and `cloudcode-client` (via `crate::proto`).
//! The agent ↔ hub tunnel protocol and viewer types live elsewhere and
//! are deliberately NOT part of this crate.

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
    /// List every workspace bound to this account across all agents
    /// it's allowed to use. Each item carries its owning agent name +
    /// whether that agent is currently online. The picker uses this
    /// to render one cross-agent list — no more separate "pick agent
    /// then pick workspace" stages.
    ListWorkspaces,
    /// Create workspace `name` bound to `agent`. Both fields are
    /// required: a workspace's owning agent is locked in at creation
    /// time and never changes. Same-named workspaces on different
    /// agents are allowed (UI shows them disambiguated as
    /// `name@agent`); same `(account, agent, name)` triple is a
    /// hard error.
    CreateWorkspace {
        name: String,
        agent: String,
    },
    /// Delete a workspace binding (and ask the owning agent to nuke
    /// the on-disk dir).
    DeleteWorkspace {
        name: String,
        agent: String,
    },
    /// Clear the saved session for a workspace (kill its tmux server,
    /// wipe claude conversation history) without removing the
    /// workspace directory itself. Routed to the bound agent.
    ResetWorkspace {
        name: String,
        agent: String,
    },
    /// Open a PTY session in the given workspace. The owning agent
    /// is part of the workspace identity (the UI carries it forward
    /// from the workspace picker). `claude_args` is forwarded
    /// verbatim to `claude`'s argv when the session is first created
    /// (tmux ignores it on re-attach).
    OpenSession {
        workspace: String,
        agent: String,
        cols: u16,
        rows: u16,
        #[serde(default)]
        claude_args: Vec<String>,
        /// Which tool to launch in the first pane (claude / codex / …).
        /// `None` -> let the agent pick its default. New in v1.10.
        /// `skip_serializing_if` keeps the wire byte-identical to the
        /// pre-extraction CLI client (omits the key rather than `null`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool: Option<String>,
    },
    /// In-session: split an extra tmux pane in the current session
    /// running `tool` (e.g. "codex") with optional extra args.
    /// Requires an active session. New in v1.10.
    SplitPane {
        tool: String,
        /// Where the new pane lands relative to the current one. Defaults
        /// to `Down` so older webterm builds without this field keep tmux's
        /// historical behaviour (split vertically, new pane below).
        #[serde(default)]
        direction: SplitDirection,
        #[serde(default)]
        args: Vec<String>,
    },
    /// In-session: re-arrange every pane in the active session into one
    /// of tmux's preset layouts. No-op if only one pane is alive.
    ChangeLayout {
        layout: PaneLayout,
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
        /// The hub-minted session id for this PTY session. The native
        /// desktop app needs it to open its second ws — the browser-panel
        /// viewer (`/v1/viewer/ws?session=<id>`) — since that endpoint
        /// keys the screencast by session id. The CLI client ignores it.
        /// `#[serde(default)]` keeps older agents/clients that omit it
        /// wire-compatible (it deserializes to the nil uuid).
        #[serde(default)]
        session_id: Uuid,
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
    Ping,
}

/// Where a SplitPane lands relative to the active pane.
///
/// - `Right`: vertical divider, new pane appears to the right (tmux `-h`).
/// - `Down`:  horizontal divider, new pane appears below       (tmux `-v`).
///
/// `Down` is the default to match tmux's own default split behaviour, so
/// older clients that don't send `direction` keep working.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SplitDirection {
    Right,
    #[default]
    Down,
}

/// Whole-session pane arrangement, applied via `tmux select-layout`.
///
/// - `SideBySide` -> `even-horizontal` (panes in a row).
/// - `Stacked`    -> `even-vertical`   (panes in a column).
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PaneLayout {
    SideBySide,
    Stacked,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AgentInfo {
    pub name: String,
    #[serde(default)]
    pub current: bool,
}

/// Workspace status row carried in HubToClient::WorkspaceList.
///
/// - `agent` = the agent that owns this workspace (bound at create
///   time, immutable).
/// - `agent_online` = whether the bound agent is currently
///   registered + reachable on the hub. Opening an offline-agent
///   workspace is rejected client-side without bothering the hub.
/// - `tmux_alive` = agent has a live tmux server for this workspace
///   (so the previous claude state is still recoverable).
/// - `has_client` = some cloudcode client is currently attached to it.
///   Opening it would trigger take-over.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WorkspaceInfo {
    pub name: String,
    /// Bound agent's name. Required as of v1.13 — older hubs that
    /// returned WorkspaceInfo without this field shouldn't talk to
    /// this client, but `#[serde(default)]` keeps us crash-free if
    /// they do.
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
mod tests {
    //! Wire-format round-trip + golden-JSON tests. These lock in the exact
    //! serialized bytes so the hub and client stay interoperable across the
    //! extraction — the whole point of this crate.
    use super::*;
    use serde_json::json;

    /// Helper: assert a value serializes to exactly `expected` JSON and
    /// round-trips back through deserialization (proving wire stability).
    fn assert_json<T>(value: &T, expected: serde_json::Value)
    where
        T: Serialize,
    {
        let got = serde_json::to_value(value).expect("serialize");
        assert_eq!(got, expected);
    }

    #[test]
    fn protocol_version_is_one() {
        assert_eq!(PTY_PROTOCOL_VERSION, "1");
    }

    #[test]
    fn c2h_hello() {
        assert_json(
            &ClientToHub::Hello {
                token: "tok".into(),
                version: "1.28.0".into(),
            },
            json!({"type": "hello", "token": "tok", "version": "1.28.0"}),
        );
    }

    #[test]
    fn c2h_select_agent_none() {
        assert_json(
            &ClientToHub::SelectAgent { agent: None },
            json!({"type": "select_agent", "agent": null}),
        );
    }

    #[test]
    fn c2h_select_agent_some() {
        assert_json(
            &ClientToHub::SelectAgent {
                agent: Some("a1".into()),
            },
            json!({"type": "select_agent", "agent": "a1"}),
        );
    }

    #[test]
    fn c2h_list_agents() {
        assert_json(&ClientToHub::ListAgents, json!({"type": "list_agents"}));
    }

    #[test]
    fn c2h_list_workspaces() {
        assert_json(
            &ClientToHub::ListWorkspaces,
            json!({"type": "list_workspaces"}),
        );
    }

    #[test]
    fn c2h_create_workspace() {
        assert_json(
            &ClientToHub::CreateWorkspace {
                name: "w".into(),
                agent: "a".into(),
            },
            json!({"type": "create_workspace", "name": "w", "agent": "a"}),
        );
    }

    #[test]
    fn c2h_delete_workspace() {
        assert_json(
            &ClientToHub::DeleteWorkspace {
                name: "w".into(),
                agent: "a".into(),
            },
            json!({"type": "delete_workspace", "name": "w", "agent": "a"}),
        );
    }

    #[test]
    fn c2h_reset_workspace() {
        assert_json(
            &ClientToHub::ResetWorkspace {
                name: "w".into(),
                agent: "a".into(),
            },
            json!({"type": "reset_workspace", "name": "w", "agent": "a"}),
        );
    }

    #[test]
    fn c2h_open_session() {
        assert_json(
            &ClientToHub::OpenSession {
                workspace: "w".into(),
                agent: "a".into(),
                cols: 80,
                rows: 24,
                claude_args: vec!["--foo".into()],
                tool: Some("codex".into()),
            },
            json!({
                "type": "open_session",
                "workspace": "w",
                "agent": "a",
                "cols": 80,
                "rows": 24,
                "claude_args": ["--foo"],
                "tool": "codex"
            }),
        );
    }

    #[test]
    fn c2h_split_pane_defaults() {
        // `direction` defaults to `down`; absence on the wire must round-trip.
        assert_json(
            &ClientToHub::SplitPane {
                tool: "codex".into(),
                direction: SplitDirection::Down,
                args: vec![],
            },
            json!({
                "type": "split_pane",
                "tool": "codex",
                "direction": "down",
                "args": []
            }),
        );
        // Older webterm omits `direction` + `args` -> serde defaults.
        let de: ClientToHub =
            serde_json::from_value(json!({"type": "split_pane", "tool": "codex"})).unwrap();
        match de {
            ClientToHub::SplitPane {
                tool,
                direction,
                args,
            } => {
                assert_eq!(tool, "codex");
                assert_eq!(direction, SplitDirection::Down);
                assert!(args.is_empty());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn c2h_change_layout() {
        assert_json(
            &ClientToHub::ChangeLayout {
                layout: PaneLayout::SideBySide,
            },
            json!({"type": "change_layout", "layout": "side_by_side"}),
        );
        assert_json(
            &ClientToHub::ChangeLayout {
                layout: PaneLayout::Stacked,
            },
            json!({"type": "change_layout", "layout": "stacked"}),
        );
    }

    #[test]
    fn c2h_resize() {
        assert_json(
            &ClientToHub::Resize { cols: 100, rows: 40 },
            json!({"type": "resize", "cols": 100, "rows": 40}),
        );
    }

    #[test]
    fn c2h_fs_write_init() {
        let id = Uuid::nil();
        assert_json(
            &ClientToHub::FsWriteInit {
                request_id: id,
                agent: "a".into(),
                workspace: "w".into(),
                path: ".cloudcode/uploads/foo.png".into(),
            },
            json!({
                "type": "fs_write_init",
                "request_id": "00000000-0000-0000-0000-000000000000",
                "agent": "a",
                "workspace": "w",
                "path": ".cloudcode/uploads/foo.png"
            }),
        );
    }

    #[test]
    fn c2h_fs_write_chunk() {
        let id = Uuid::nil();
        assert_json(
            &ClientToHub::FsWriteChunk {
                request_id: id,
                data_b64: "AAAA".into(),
                eof: false,
            },
            json!({
                "type": "fs_write_chunk",
                "request_id": "00000000-0000-0000-0000-000000000000",
                "data_b64": "AAAA",
                "eof": false
            }),
        );
    }

    #[test]
    fn c2h_close_and_pong() {
        assert_json(&ClientToHub::Close, json!({"type": "close"}));
        assert_json(&ClientToHub::Pong, json!({"type": "pong"}));
    }

    #[test]
    fn h2c_welcome() {
        assert_json(
            &HubToClient::Welcome {
                account: "acc".into(),
            },
            json!({"type": "welcome", "account": "acc"}),
        );
    }

    #[test]
    fn h2c_rejected() {
        assert_json(
            &HubToClient::Rejected {
                reason: "nope".into(),
            },
            json!({"type": "rejected", "reason": "nope"}),
        );
    }

    #[test]
    fn h2c_agent_list() {
        assert_json(
            &HubToClient::AgentList {
                items: vec![AgentInfo {
                    name: "a".into(),
                    current: true,
                }],
            },
            json!({"type": "agent_list", "items": [{"name": "a", "current": true}]}),
        );
    }

    #[test]
    fn h2c_workspace_list() {
        assert_json(
            &HubToClient::WorkspaceList {
                items: vec![WorkspaceInfo {
                    name: "w".into(),
                    agent: "a".into(),
                    agent_online: true,
                    tmux_alive: false,
                    has_client: false,
                }],
            },
            json!({
                "type": "workspace_list",
                "items": [{
                    "name": "w",
                    "agent": "a",
                    "agent_online": true,
                    "tmux_alive": false,
                    "has_client": false
                }]
            }),
        );
    }

    #[test]
    fn h2c_session_opened() {
        assert_json(
            &HubToClient::SessionOpened {
                agent: "a".into(),
                workspace: "w".into(),
                cwd: "/home/w".into(),
                session_id: uuid::uuid!("11111111-2222-3333-4444-555555555555"),
            },
            json!({"type": "session_opened", "agent": "a", "workspace": "w", "cwd": "/home/w",
                "session_id": "11111111-2222-3333-4444-555555555555"}),
        );
    }

    #[test]
    fn h2c_session_closed_omits_none_reason() {
        // `skip_serializing_if` must keep `reason` off the wire when None.
        assert_json(
            &HubToClient::SessionClosed { reason: None },
            json!({"type": "session_closed"}),
        );
        assert_json(
            &HubToClient::SessionClosed {
                reason: Some("bye".into()),
            },
            json!({"type": "session_closed", "reason": "bye"}),
        );
    }

    #[test]
    fn h2c_fs_write_result() {
        let id = Uuid::nil();
        assert_json(
            &HubToClient::FsWriteResult {
                request_id: id,
                final_name: Some("foo (1).png".into()),
                error: None,
            },
            json!({
                "type": "fs_write_result",
                "request_id": "00000000-0000-0000-0000-000000000000",
                "final_name": "foo (1).png",
                "error": null
            }),
        );
    }

    #[test]
    fn h2c_ping() {
        assert_json(&HubToClient::Ping, json!({"type": "ping"}));
    }

    #[test]
    fn split_direction_wire_names() {
        assert_eq!(
            serde_json::to_value(SplitDirection::Right).unwrap(),
            json!("right")
        );
        assert_eq!(
            serde_json::to_value(SplitDirection::Down).unwrap(),
            json!("down")
        );
        assert_eq!(SplitDirection::default(), SplitDirection::Down);
    }

    #[test]
    fn pane_layout_wire_names() {
        assert_eq!(
            serde_json::to_value(PaneLayout::SideBySide).unwrap(),
            json!("side_by_side")
        );
        assert_eq!(
            serde_json::to_value(PaneLayout::Stacked).unwrap(),
            json!("stacked")
        );
    }
}
