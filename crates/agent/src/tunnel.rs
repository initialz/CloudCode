use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const PROTOCOL_VERSION: &str = "7";

// ---------------------------------------------------------------------------
// Binary frame layout (Message::Binary on the WS tunnel):
//
//   [0]      1 byte   tag (TAG_PTY_INPUT | TAG_PTY_OUTPUT)
//   [1..17]  16 bytes session_id (uuid raw bytes)
//   [17..]   payload (raw PTY bytes; no further structure)
//
// One agent connection multiplexes multiple sessions over the same WS, so
// every binary frame is keyed by session_id.
// ---------------------------------------------------------------------------

pub const TAG_PTY_INPUT: u8 = 0x01; // hub → agent : keystrokes for PTY master
pub const TAG_PTY_OUTPUT: u8 = 0x02; // agent → hub : output read from PTY master
pub const PTY_FRAME_PREFIX_LEN: usize = 1 + 16;

pub fn pack_pty_frame(tag: u8, session_id: Uuid, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(PTY_FRAME_PREFIX_LEN + payload.len());
    out.push(tag);
    out.extend_from_slice(session_id.as_bytes());
    out.extend_from_slice(payload);
    out
}

/// `(tag, session_id, payload_slice)` or None if too short / unknown tag.
pub fn unpack_pty_frame(buf: &[u8]) -> Option<(u8, Uuid, &[u8])> {
    if buf.len() < PTY_FRAME_PREFIX_LEN {
        return None;
    }
    let tag = buf[0];
    let mut sid = [0u8; 16];
    sid.copy_from_slice(&buf[1..17]);
    Some((tag, Uuid::from_bytes(sid), &buf[PTY_FRAME_PREFIX_LEN..]))
}

/// Frames sent from the agent to the hub (text JSON).
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello {
        name: String,
        secret: String,
        version: String,
        /// Self-reported agent build version (`CARGO_PKG_VERSION`). Optional
        /// for compatibility with pre-v1.6.0 agents that don't send it.
        #[serde(default)]
        agent_version: Option<String>,
        /// Rust target triple of the agent binary (e.g. `aarch64-apple-darwin`).
        /// Used by the hub to pick the right release asset on self-update.
        #[serde(default)]
        target_triple: Option<String>,
        /// Names of the tools this agent is configured to spawn
        /// (after auto-detect + `[tools.<name>] disabled` filtering).
        /// Clients use this to hide unavailable choices from the
        /// "Open with X" / Split menus. Empty for pre-v1.13 agents;
        /// hub treats that as "unknown, fall back to client's
        /// hard-coded list".
        #[serde(default)]
        tools: Vec<String>,
    },
    Pong,

    /// PTY established for a session.
    PtyOpened {
        session_id: Uuid,
        workspace: String,
        cwd: String,
    },
    /// Terminal: claude or tmux exited, agent dropped the PTY, etc.
    PtyClosed {
        session_id: Uuid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Open/runtime error that's not a normal close (couldn't spawn tmux,
    /// workspace name rejected, etc).
    PtyError {
        session_id: Uuid,
        message: String,
    },

    /// Reply to a hub-initiated `SplitPane` request. Session-keyed so the
    /// hub can route the result to the same client that asked. `error =
    /// None` means the new pane was successfully spawned; otherwise the
    /// message explains the failure (unknown tool, tmux missing, …).
    /// New in v1.10; pre-1.10 hubs/agents won't emit or expect this.
    SplitPaneResult {
        session_id: Uuid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    /// One JSONL line tailed from claude's per-project history file.
    /// Streamed to the hub so the admin UI can show the conversation
    /// associated with each session.
    Message {
        session_id: Uuid,
        claude_session_id: String,
        ts: i64,
        kind: String,
        body: String,
    },

    /// Workspace management replies (not bound to a PTY session).
    WorkspaceListResult {
        request_id: Uuid,
        items: Vec<WorkspaceItem>,
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
    WorkspaceResetResult {
        request_id: Uuid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    /// Reply to a `WorkspaceListAll` admin query: every (account, workspace)
    /// pair this agent currently has on disk, with tmux-alive state.
    WorkspaceListAllResult {
        request_id: Uuid,
        items: Vec<WorkspaceFullItem>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    /// Reply to a hub-initiated `UpdateAgent` request. On success the agent
    /// exits cleanly so the supervisor relaunches it on the new binary.
    UpdateAgentResult {
        request_id: Uuid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

/// One row in a WorkspaceListResult. `tmux_alive` lets the picker
/// distinguish "session state still on the agent" (saved) from
/// "blank slot" (fresh).
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WorkspaceItem {
    pub name: String,
    pub tmux_alive: bool,
}

/// Row in a `WorkspaceListAllResult`. Carries the account because the
/// admin view aggregates across every account this agent serves.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WorkspaceFullItem {
    pub account: String,
    pub name: String,
    pub tmux_alive: bool,
}

/// Where a SplitPane lands relative to the active pane.
///
/// `Right`: vertical divider, new pane appears to the right (tmux `-h`).
/// `Down`:  horizontal divider, new pane appears below       (tmux `-v`).
#[derive(Debug, Serialize, Deserialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SplitDirection {
    Right,
    #[default]
    Down,
}

/// Whole-session pane arrangement, applied via `tmux select-layout`.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PaneLayout {
    SideBySide,
    Stacked,
}

/// Frames sent from the hub to the agent (text JSON).
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

    /// Allocate a PTY for a session in the given (account, workspace), with
    /// the given initial terminal size. The agent stores workspace state
    /// per-account; the tmux session name is `cloudcode-<account>-<workspace>`
    /// and the cwd is `<workspace_root>/<account>/<workspace>/`.
    PtyOpen {
        session_id: Uuid,
        account: String,
        workspace: String,
        cols: u16,
        rows: u16,
        #[serde(default)]
        claude_args: Vec<String>,
        /// Wrap the spawned tmux+claude in the workspace sandbox.
        /// Decided per-account on the hub. Defaults to false for
        /// back-compat with pre-v1.9 hubs that never sent it.
        #[serde(default)]
        sandbox: bool,
        /// Which tool to launch in the first pane (claude / codex / …).
        /// `None` means "agent default" — the agent falls back to its
        /// `[tools].default`. Optional for back-compat with pre-v1.10
        /// hubs that didn't know about multi-tool.
        #[serde(default)]
        tool: Option<String>,
    },
    PtyResize {
        session_id: Uuid,
        cols: u16,
        rows: u16,
    },
    /// Detach this session. Does not kill the underlying tmux session — the
    /// next PtyOpen on the same (account, workspace) re-attaches.
    PtyClose {
        session_id: Uuid,
    },

    /// Add a new tmux pane to an existing PTY session, running `tool`
    /// (with optional extra args) alongside whatever was already there.
    /// New in v1.10; pre-1.10 agents will fail to deserialize this and
    /// drop the frame — the hub won't send it to them because the
    /// client only emits it when the user explicitly hits split.
    SplitPane {
        session_id: Uuid,
        tool: String,
        /// Where the new pane lands. Defaults to `Down` so frames from
        /// pre-direction hubs/clients keep tmux's default split behaviour.
        #[serde(default)]
        direction: SplitDirection,
        #[serde(default)]
        args: Vec<String>,
    },

    /// Re-arrange panes in an existing session via `tmux select-layout`.
    /// New in v1.10.
    ChangeLayout {
        session_id: Uuid,
        layout: PaneLayout,
    },

    WorkspaceList {
        request_id: Uuid,
        account: String,
    },
    WorkspaceCreate {
        request_id: Uuid,
        account: String,
        name: String,
    },
    WorkspaceDelete {
        request_id: Uuid,
        account: String,
        name: String,
    },
    WorkspaceReset {
        request_id: Uuid,
        account: String,
        name: String,
    },
    /// Admin-only: ask the agent for every (account, workspace) it knows
    /// about, regardless of which account is asking. Used by the admin
    /// UI to render a cross-account workspace inventory.
    WorkspaceListAll {
        request_id: Uuid,
    },

    /// Admin-only: instruct the agent to download a new release tarball,
    /// verify its sha256, and swap the `current` symlink. On success the
    /// agent process exits cleanly and the supervisor relaunches it on
    /// the new binary.
    UpdateAgent {
        request_id: Uuid,
        /// Tag of the form `vX.Y.Z` (matches the release tag on GitHub).
        target_version: String,
        /// `.tar.gz` asset URL for this agent's target triple.
        download_url: String,
        /// `.sha256` manifest URL covering the same asset.
        sha256_url: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    NameTaken,
    AuthFailed,
    VersionMismatch,
}
