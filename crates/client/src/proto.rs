//! Mirror of the hub's `pty_proto.rs`. Keep in lockstep.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 文档性镜像版本(本常量两侧均 #[allow(dead_code)],hub 不校验;
/// 跨版本安全靠 Hello.remote_mcp_capable 缺省 false + 读循环对未知帧
/// 容忍跳过)。与 crates/hub/src/pty_proto.rs 同步改动。
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
