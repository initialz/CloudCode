//! Wire schema for the client ↔ hub WebSocket on `/v1/pty/ws`.
//! Mirrored verbatim in `crates/client/src/proto.rs`.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 文档性镜像版本(本常量两侧均 #[allow(dead_code)],hub 不校验;
/// 跨版本安全靠 Hello.remote_mcp_capable 缺省 false + 读循环对未知帧
/// 容忍跳过)。与 crates/client/src/proto.rs 同步改动。
#[allow(dead_code)]
pub const PTY_PROTOCOL_VERSION: &str = "2";

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientToHub {
    Hello {
        token: String,
        version: String,
        /// 本 client 能否承载远程-MCP 后端子进程(配置了后端命令)。
        /// 缺省 false:旧 client / webterm SPA 不发该字段,hub→agent
        /// 链路便绝不向其转发 RemoteMcp 帧(决策 D5/D6)。
        #[serde(default)]
        remote_mcp_capable: bool,
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
        #[serde(default)]
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
    /// In-session:client 侧后端 MCP 子进程回向 claude 的一帧不透明
    /// JSON-RPC。hub 打上当前活动会话的 session_id 转发给绑定 agent
    ///(ServerMsg::RemoteMcp)。负载中途零解析。
    RemoteMcp {
        server: String,
        payload: String,
    },
    /// In-session:client 拆除其远程-MCP 通道(后端不可用 / 子进程
    /// 死亡 / 收摊)。agent 据此立刻 fail 该会话在飞请求。
    RemoteMcpClosed {
        server: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
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
    /// claude(经 agent proxy)指向 client 侧后端 MCP 子进程的一帧
    /// 不透明 JSON-RPC。负载中途零解析。
    RemoteMcp {
        server: String,
        payload: String,
    },
    /// hub/agent 侧拆除远程-MCP 通道;client 应停掉后端子进程
    ///(保留握手缓存,见 Phase C)。
    RemoteMcpClosed {
        server: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
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
mod remote_mcp_tests {
    use super::*;

    #[test]
    fn remote_mcp_both_directions_roundtrip_byte_exact() {
        let original = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"zebra":1,"alpha":2}}"#;
        let c = ClientToHub::RemoteMcp {
            server: "cc-browser".to_string(),
            payload: original.to_string(),
        };
        let j = serde_json::to_string(&c).unwrap();
        assert!(j.contains("\"type\":\"remote_mcp\""), "tag mismatch: {j}");
        match serde_json::from_str::<ClientToHub>(&j).unwrap() {
            ClientToHub::RemoteMcp { server, payload } => {
                assert_eq!(server, "cc-browser");
                assert_eq!(payload, original);
            }
            _ => panic!("wrong variant"),
        }

        let h = HubToClient::RemoteMcp {
            server: "cc-browser".to_string(),
            payload: original.to_string(),
        };
        let j2 = serde_json::to_string(&h).unwrap();
        assert!(j2.contains("\"type\":\"remote_mcp\""), "tag mismatch: {j2}");
        match serde_json::from_str::<HubToClient>(&j2).unwrap() {
            HubToClient::RemoteMcp { server, payload } => {
                assert_eq!(server, "cc-browser");
                assert_eq!(payload, original);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn remote_mcp_closed_roundtrips_and_defaults() {
        let c = ClientToHub::RemoteMcpClosed {
            server: "cc-browser".to_string(),
            reason: Some("backend unavailable".to_string()),
        };
        let j = serde_json::to_string(&c).unwrap();
        assert!(j.contains("\"type\":\"remote_mcp_closed\""), "tag mismatch: {j}");
        match serde_json::from_str::<ClientToHub>(&j).unwrap() {
            ClientToHub::RemoteMcpClosed { server, reason } => {
                assert_eq!(server, "cc-browser");
                assert_eq!(reason.as_deref(), Some("backend unavailable"));
            }
            _ => panic!("wrong variant"),
        }
        // 线上省略 reason → None
        let from_wire: HubToClient =
            serde_json::from_str(r#"{"type":"remote_mcp_closed","server":"cc-browser"}"#).unwrap();
        match from_wire {
            HubToClient::RemoteMcpClosed { server, reason } => {
                assert_eq!(server, "cc-browser");
                assert_eq!(reason, None);
            }
            _ => panic!("wrong variant"),
        }
        let from_wire_c: ClientToHub =
            serde_json::from_str(r#"{"type":"remote_mcp_closed","server":"cc-browser"}"#).unwrap();
        match from_wire_c {
            ClientToHub::RemoteMcpClosed { server, reason } => {
                assert_eq!(server, "cc-browser");
                assert_eq!(reason, None);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn hello_without_capability_defaults_false() {
        // 旧 client / webterm SPA 的 Hello 没有该字段 → 必须解析成功且为 false。
        let j = r#"{"type":"hello","token":"t","version":"1"}"#;
        match serde_json::from_str::<ClientToHub>(j).unwrap() {
            ClientToHub::Hello { remote_mcp_capable, .. } => assert!(!remote_mcp_capable),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn pty_protocol_version_is_2() {
        assert_eq!(PTY_PROTOCOL_VERSION, "2");
    }
}
