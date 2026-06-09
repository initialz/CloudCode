use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const PROTOCOL_VERSION: &str = "12";

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
        /// Workspaces that exist on the agent's local disk, formatted
        /// as `"<account>/<name>"`. Hub seeds its workspaces table
        /// with these on first sighting (one-time migration so users
        /// don't lose access to pre-v1.13 dirs).
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

    /// One user-typed prompt (or bash escape) captured from claude's
    /// per-project jsonl log. Distinct from `Message` (which streams
    /// every event for the conversation view): this is filtered to
    /// actual human inputs and lands in `user_interactions` for the
    /// admin audit surface. New in v1.14 (protocol v8).
    UserInteraction {
        account: String,
        workspace: String,
        /// UUID of the claude session (jsonl filename stem). Independent
        /// of the cloudcode pty `session_id` namespace — the same
        /// claude session can outlive several pty sessions.
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
        /// Either `"prompt"` (normal chat) or `"bash_input"` (the user
        /// hit `!` and typed a shell line). Tool writebacks
        /// (bash-stdout / bash-stderr / system-reminder) are filtered
        /// out by the agent before they reach the wire.
        kind: String,
        content: String,
    },

    /// Reply to a `FsList` request — one shot per request_id.
    /// New in v1.15 (protocol v9). `error` is set when the workspace
    /// dir doesn't exist, the path escapes the workspace root, etc.
    /// `entries` is populated for ANY successful list, even if empty.
    FsListResult {
        request_id: Uuid,
        #[serde(default)]
        entries: Vec<FsEntry>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    /// One chunk of a streaming download started by `FsRead`. The agent
    /// emits these in order, each carrying up to ~64 KB of file
    /// contents base64-encoded. `eof = true` marks the final chunk
    /// (`data_b64` may still hold the tail). On any read failure the
    /// agent emits a single chunk with `error` set and `eof = true`
    /// to terminate the stream; the hub then short-reads its HTTP
    /// response.
    FsReadChunk {
        request_id: Uuid,
        #[serde(default)]
        data_b64: String,
        #[serde(default)]
        eof: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    FsWriteResult {
        request_id: Uuid,
        #[serde(default)]
        bytes_written: u64,
        #[serde(default)]
        final_name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    FsDeleteResult {
        request_id: Uuid,
        #[serde(default)]
        deleted: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    /// One opaque MCP JSON-RPC frame from the agent's resident MCP
    /// endpoint (claude is the MCP client) toward the bound client's
    /// browser subprocess. `session_id` is the routing key; payload is
    /// raw text, never parsed in transit.
    BrowserRpc {
        session_id: Uuid,
        payload: String,
    },
}

/// One entry returned in a `FsListResult`. Directory entries have
/// `size = 0`; symlinks report the link's own size (which is
/// usually meaningless, but the UI can show them distinctly via
/// `kind`).
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FsEntry {
    pub name: String,
    pub kind: FsKind,
    pub size: u64,
    /// Last-modified wall-clock in milliseconds since the Unix epoch.
    pub mtime_ms: i64,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FsKind {
    File,
    Dir,
    Symlink,
    /// Anything else we don't recognise (device files, FIFOs, …).
    /// Listed for completeness but the UI usually hides these.
    Other,
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
        /// Legacy bool kept for back-compat with pre-v1.23 agents.
        /// New code path is `sandbox_mode` below.
        #[serde(default)]
        sandbox: bool,
        /// Per-account sandbox mode: "strict" / "permissive" / "off".
        /// When None, agent derives from the legacy `sandbox` bool.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sandbox_mode: Option<String>,
        /// Which tool to launch in the first pane (claude / codex / …).
        /// `None` means "agent default" — the agent falls back to its
        /// `[tools].default`. Optional for back-compat with pre-v1.10
        /// hubs that didn't know about multi-tool.
        #[serde(default)]
        tool: Option<String>,
        /// Extra environment variables to inject into the tool process.
        /// Resolved per-account / per-workspace from the stored
        /// `user_preferences` blob. `#[serde(default)]` so an older peer
        /// degrades to "no extra env" instead of failing to parse.
        #[serde(default)]
        env: std::collections::HashMap<String, String>,
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

    /// List a workspace directory. `path` is relative to the workspace
    /// root (leading `/` allowed but ignored). `show_hidden` controls
    /// whether dotfiles appear in the result. New in v1.15 (proto v9).
    FsList {
        request_id: Uuid,
        account: String,
        workspace: String,
        #[serde(default)]
        path: String,
        #[serde(default)]
        show_hidden: bool,
    },
    /// Stream a workspace file back to the hub as a series of
    /// `FsReadChunk` frames. `path` is relative to the workspace root.
    /// New in v1.15 (proto v9).
    FsRead {
        request_id: Uuid,
        account: String,
        workspace: String,
        path: String,
    },

    /// Bundle one or more workspace paths (files and/or directories,
    /// any mix) into a single in-memory zip and stream it back as a
    /// series of `FsReadChunk` frames — same wire shape as `FsRead`
    /// so the hub's existing fs_read_streams routing handles it
    /// without changes. New in v1.16 (proto v10).
    FsArchive {
        request_id: Uuid,
        account: String,
        workspace: String,
        paths: Vec<String>,
    },

    FsWriteInit {
        request_id: Uuid,
        account: String,
        workspace: String,
        path: String,
        #[serde(default)]
        size: u64,
    },
    FsWriteChunk {
        request_id: Uuid,
        #[serde(default)]
        data_b64: String,
        #[serde(default)]
        eof: bool,
    },

    FsDelete {
        request_id: Uuid,
        account: String,
        workspace: String,
        paths: Vec<String>,
    },

    /// One opaque MCP JSON-RPC frame from the client's browser
    /// subprocess back toward claude, routed by `session_id` to the
    /// matching MCP endpoint stream. Payload is raw text, never parsed.
    BrowserRpc {
        session_id: Uuid,
        payload: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    NameTaken,
    AuthFailed,
    VersionMismatch,
}
