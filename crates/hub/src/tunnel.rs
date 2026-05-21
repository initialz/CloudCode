use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const PROTOCOL_VERSION: &str = "8";

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
        /// Workspaces the agent has on local disk, formatted as
        /// `"<account>/<name>"`. Hub uses this on connect to seed
        /// its `workspaces` table for any name it hasn't seen before
        /// (one-time migration from the pre-v1.13 per-agent model).
        /// New rows are inserted with `INSERT OR IGNORE` so we don't
        /// stomp on existing bindings — first agent to report a
        /// given `(account, name)` wins. Older agents simply omit
        /// this field; hub treats them as if they had no workspaces.
        #[serde(default)]
        workspaces: Vec<String>,
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
    /// hub routes it back through the same client connection. `error =
    /// None` means the new pane was successfully spawned. New in v1.10.
    SplitPaneResult {
        session_id: Uuid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    /// One JSONL line tailed from claude's per-project history file.
    /// The agent streams these to the hub so the admin UI can show the
    /// conversation for each session. `kind` is the outer `type` field
    /// (user / assistant / permission-mode / file-history-snapshot /
    /// …); `body` is the raw line.
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

    /// One user-typed prompt (or bash escape) captured from claude's
    /// per-project jsonl log. Distinct from `Message` (which carries
    /// every event for the conversation view): this is filtered to
    /// actual human inputs and lands in `user_interactions` for the
    /// admin audit surface. New in v1.14 (protocol v8).
    UserInteraction {
        account: String,
        workspace: String,
        /// UUID of the claude session (jsonl filename stem). Independent
        /// of the cloudcode pty `session_id` namespace.
        claude_session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_uuid: Option<String>,
        cwd: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        git_branch: Option<String>,
        /// Wall-clock at which claude wrote this jsonl row.
        ts_ms: i64,
        /// Either `"prompt"` or `"bash_input"`. Tool writebacks are
        /// filtered out by the agent before they reach the wire.
        kind: String,
        content: String,
    },
}

/// One row in a WorkspaceListResult. Same shape on both sides of the
/// tunnel; hub layers on `has_client` separately when forwarding to
/// the cloudcode client.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WorkspaceItem {
    pub name: String,
    pub tmux_alive: bool,
}

/// Row in a `WorkspaceListAllResult`. Carries the account because the
/// admin view aggregates across all accounts the agent serves.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WorkspaceFullItem {
    pub account: String,
    pub name: String,
    pub tmux_alive: bool,
}

/// Where a SplitPane lands relative to the active pane.
///
/// - `Right`: vertical divider, new pane appears to the right (tmux `-h`).
/// - `Down`:  horizontal divider, new pane appears below       (tmux `-v`).
///
/// `Down` is the default to match tmux's own default; older hubs without
/// this field round-trip to the same behaviour.
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
        /// Whether the agent should wrap the spawned tmux+claude in
        /// the workspace sandbox. Decided per-account on the hub
        /// (`accounts.sandbox_enabled`); the agent.toml `[sandbox]`
        /// switch is deprecated. Optional for back-compat with pre-
        /// v1.9 hubs (default false = no sandbox).
        #[serde(default)]
        sandbox: bool,
        /// Which tool to launch in the first pane (claude / codex / …).
        /// `None` -> agent falls back to its `[tools].default`.
        /// Optional for back-compat with pre-v1.10 hubs.
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
    /// New in v1.10.
    SplitPane {
        session_id: Uuid,
        tool: String,
        /// Where the new pane lands. Defaults to `Down` so older agents
        /// that don't know this field keep tmux's default split direction.
        #[serde(default)]
        direction: SplitDirection,
        #[serde(default)]
        args: Vec<String>,
    },

    /// Re-arrange the panes in an existing session via `tmux select-layout`.
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
