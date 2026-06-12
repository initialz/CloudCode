use crate::config::{ClaudeConfig, RecordingConfig, SandboxConfig, TmuxConfig, ToolConfig};
use crate::tunnel::{
    pack_pty_frame, ClientMsg, PaneLayout, ServerMsg, SplitDirection, WorkspaceFullItem,
    WorkspaceItem, TAG_PTY_OUTPUT,
};
use anyhow::{Context, Result};
use chrono::SecondsFormat;
use dashmap::DashMap;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::mpsc;
use uuid::Uuid;

/// What the WS writer task drains: either a JSON control frame or a binary
/// PTY frame (output direction).
pub enum OutFrame {
    Text(ClientMsg),
    Binary(Vec<u8>),
}

pub struct PtyManager {
    claude: ClaudeConfig,
    tools: HashMap<String, ToolConfig>,
    default_tool: String,
    tmux: TmuxConfig,
    recording: RecordingConfig,
    /// When the workspace sandbox is enabled at startup, this holds the
    /// path to the running cloudcode-agent binary so the PTY spawn path
    /// can re-invoke us with the `sandbox-exec` subcommand. `None` means
    /// "sandbox disabled, exec tmux directly".
    self_exe: Option<PathBuf>,
    /// Path to our agent-owned tmux.conf (one line: `set -g mouse on`).
    /// Passed via `tmux -f` so each per-workspace tmux server inherits
    /// mouse mode on startup. `None` only if we failed to write the
    /// file at boot, in which case spawn falls back to the user's
    /// default tmux config (i.e. mouse off → wheel will misbehave but
    /// the session still works).
    tmux_conf: Option<PathBuf>,
    sessions: Arc<DashMap<Uuid, Arc<PtyHandle>>>,
    write_sessions: crate::fs::WriteSessions,
    /// 远程-MCP proxy(与 AppState.mcp 共享 Arc 内部):open_session
    /// 在此注册工作区 token 路由,HTTP handler 即时可见。
    mcp: crate::mcp_proxy::McpProxy,
    /// `[remote_mcp]` 配置快照(enabled / port / manifest 路径)。
    remote_mcp: crate::config::RemoteMcpConfig,
    /// 每工作区一枚稳定 remote-MCP token,键 (account, workspace)。
    /// 首次注入时铸造、之后每次 open 复用并对新 session_id 重注册
    /// (决策 D12);仅 workspace delete/reset 时移除并注销。
    workspace_tokens: DashMap<(String, String), String>,
}

struct PtyHandle {
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    writer: Mutex<Box<dyn Write + Send>>,
    /// (account, workspace) that this PTY is bound to. Needed by
    /// `split_pane` to derive the tmux label + session name (we already
    /// validated these on open, so they're safe to reuse verbatim).
    account: String,
    workspace: String,
    /// Stops the jsonl watcher task on drop. Held only for its
    /// Drop side-effect.
    _jsonl: crate::jsonl::WatcherHandle,
}

impl PtyManager {
    pub fn new(
        claude: ClaudeConfig,
        tools: HashMap<String, ToolConfig>,
        default_tool: String,
        tmux: TmuxConfig,
        recording: RecordingConfig,
        // Kept in the signature so call sites compile, but the agent
        // no longer makes the sandbox decision: it's per-account on
        // the hub now (see ServerMsg::PtyOpen.sandbox). The agent
        // just figures out whether sandbox is structurally possible
        // (macOS today) and stands the wrapper-path ready.
        _sandbox: SandboxConfig,
        mcp: crate::mcp_proxy::McpProxy,
        remote_mcp: crate::config::RemoteMcpConfig,
    ) -> Result<Self> {
        // Fail fast if tmux is not installed.
        let tmux_path = which::which(&tmux.executable).with_context(|| {
            format!(
                "could not find `{}` on PATH; install tmux (e.g. `brew install tmux` or `apt install tmux`)",
                tmux.executable.display()
            )
        })?;
        tracing::info!(tmux = %tmux_path.display(), "tmux ready");

        // Make sure record dir exists up-front so the first session doesn't
        // race on it.
        if let Err(e) = std::fs::create_dir_all(&recording.dir) {
            tracing::warn!(error = %e, dir = %recording.dir.display(), "could not create recording dir");
        }

        // Locate the wrapper binary if this platform can sandbox at
        // all. `None` -> any PtyOpen that asks for sandbox will be
        // refused with a PtyError, while sandbox=false sessions still
        // run as usual.
        let self_exe = if crate::sandbox::is_supported() {
            let p = std::env::current_exe().context(
                "locating the running cloudcode-agent binary for the sandbox wrapper",
            )?;
            tracing::info!(wrapper = %p.display(), "workspace sandbox capability available");
            Some(p)
        } else {
            tracing::info!("workspace sandbox not supported on this platform");
            None
        };

        // Write our private tmux.conf next to the recordings dir. tmux
        // reads `-f` only when starting a server (per-workspace, with
        // -L), so we just need this file to exist when open_session
        // spawns the first `tmux new-session`. Mouse mode lets webterm
        // wheel events scroll tmux's per-pane scrollback (chat history)
        // instead of being translated to ↑/↓ by xterm.js in alt-screen.
        let tmux_conf = recording
            .dir
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
            .join("tmux.conf");
        let tmux_conf = match std::fs::create_dir_all(tmux_conf.parent().unwrap_or(Path::new(".")))
            .and_then(|_| {
                // mouse off             -> xterm.js owns mouse: native scroll,
                //                         native text selection, native copy.
                // alternate-screen off  -> tmux does NOT propagate alt-screen
                //                         escapes to the outer terminal, so
                //                         xterm.js stays in the main screen
                //                         and its scrollback accumulates all
                //                         output. This is the same trick
                //                         iTerm2's "Save lines to scrollback
                //                         in alternate screen mode" uses.
                //                         Claude's TUI still works inside
                //                         tmux's pane — tmux handles
                //                         alt-screen internally.
                // history-limit 50000  -> tmux's own scrollback (keyboard
                //                         copy-mode via Ctrl-b [ still works).
                // set-clipboard on +   -> keyboard-initiated tmux copies
                // terminal-features       emit OSC 52 → browser clipboard.
                let copy_pipe =
                    "send-keys -X copy-pipe-and-cancel 'base64 | tr -d \"\\n\" | (printf \"\\033]52;c;\"; cat; printf \"\\a\")'";
                let conf = format!(
                    "set -g mouse off\n\
                     set-window-option -g alternate-screen off\n\
                     set -g terminal-overrides '*:smcup@:rmcup@'\n\
                     set -g history-limit 50000\n\
                     set -g set-clipboard on\n\
                     set -as terminal-features ',*:clipboard'\n\
                     bind-key -T copy-mode    Enter {copy_pipe}\n\
                     bind-key -T copy-mode-vi Enter {copy_pipe}\n\
                     bind-key -T copy-mode    y     {copy_pipe}\n\
                     bind-key -T copy-mode-vi y     {copy_pipe}\n\
                     bind-key -T copy-mode    c     {copy_pipe}\n\
                     bind-key -T copy-mode-vi c     {copy_pipe}\n",
                    copy_pipe = copy_pipe
                );
                std::fs::write(&tmux_conf, conf.as_bytes())
            })
        {
            Ok(()) => {
                tracing::info!(path = %tmux_conf.display(), "wrote tmux.conf (mouse on)");
                Some(tmux_conf)
            }
            Err(e) => {
                tracing::warn!(error = %e, path = %tmux_conf.display(), "could not write tmux.conf; mouse-wheel scrollback will be off");
                None
            }
        };

        Ok(Self {
            claude,
            tools,
            default_tool,
            tmux: TmuxConfig {
                executable: tmux_path,
            },
            recording,
            self_exe,
            tmux_conf,
            sessions: Arc::new(DashMap::new()),
            write_sessions: crate::fs::new_write_sessions(),
            mcp,
            remote_mcp,
            workspace_tokens: DashMap::new(),
        })
    }

    pub async fn handle(self: &Arc<Self>, msg: ServerMsg, tx: mpsc::Sender<OutFrame>) {
        match msg {
            ServerMsg::PtyOpen {
                session_id,
                account,
                workspace,
                cols,
                rows,
                claude_args,
                sandbox,
                sandbox_mode,
                tool,
                env,
                remote_mcp_capable,
            } => {
                self.open_session(
                    session_id,
                    account,
                    workspace,
                    cols,
                    rows,
                    claude_args,
                    sandbox,
                    sandbox_mode,
                    tool,
                    env,
                    remote_mcp_capable,
                    tx,
                )
                .await;
            }
            ServerMsg::PtyResize {
                session_id,
                cols,
                rows,
            } => {
                if let Err(e) = self.resize(session_id, cols, rows) {
                    tracing::debug!(session = %session_id, error = %e, "resize failed");
                }
            }
            ServerMsg::PtyClose { session_id } => {
                self.close(session_id, tx).await;
            }
            ServerMsg::SplitPane {
                session_id,
                tool,
                direction,
                args,
            } => {
                self.split_pane(session_id, tool, direction, args, tx)
                    .await;
            }
            ServerMsg::ChangeLayout { session_id, layout } => {
                self.change_layout(session_id, layout, tx).await;
            }
            ServerMsg::WorkspaceList {
                request_id,
                account,
            } => self.workspace_list(request_id, account, tx).await,
            ServerMsg::WorkspaceCreate {
                request_id,
                account,
                name,
            } => self.workspace_create(request_id, account, name, tx).await,
            ServerMsg::WorkspaceDelete {
                request_id,
                account,
                name,
            } => self.workspace_delete(request_id, account, name, tx).await,
            ServerMsg::WorkspaceReset {
                request_id,
                account,
                name,
            } => self.workspace_reset(request_id, account, name, tx).await,
            ServerMsg::WorkspaceListAll { request_id } => {
                self.workspace_list_all(request_id, tx).await
            }
            // Self-update is intercepted in ws::read_loop before reaching
            // the manager; the arm exists only to keep the match
            // exhaustive. If we somehow see it here, log and drop.
            ServerMsg::UpdateAgent { request_id, .. } => {
                tracing::warn!(%request_id, "UpdateAgent reached PtyManager; should be handled in ws");
            }
            // Filesystem ops: account/workspace names are validated up
            // front so a bogus identifier produces a fast structured
            // error rather than a confusing "workspace not found" from
            // canonicalize(). Path safety itself lives in fs::resolve_safe.
            ServerMsg::FsList {
                request_id,
                account,
                workspace,
                path,
                show_hidden,
            } => {
                let workspace_root = self.workspace_root();
                let (entries, error) = match validate_name(&account, "account")
                    .and_then(|_| validate_name(&workspace, "workspace"))
                {
                    Err(e) => (Vec::new(), Some(e)),
                    Ok(()) => {
                        match crate::fs::list(
                            &workspace_root,
                            &account,
                            &workspace,
                            &path,
                            show_hidden,
                        )
                        .await
                        {
                            Ok(rows) => (rows, None),
                            Err(e) => (Vec::new(), Some(e)),
                        }
                    }
                };
                let _ = tx
                    .send(OutFrame::Text(ClientMsg::FsListResult {
                        request_id,
                        entries,
                        error,
                    }))
                    .await;
            }
            ServerMsg::FsRead {
                request_id,
                account,
                workspace,
                path,
            } => {
                // Run the streaming read in a detached task so a
                // multi-MB download doesn't block subsequent control
                // frames on the same WS. The task owns its tx clone
                // and terminates the stream with an eof chunk in
                // every exit path (see fs::read_stream).
                if let Err(e) = validate_name(&account, "account")
                    .and_then(|_| validate_name(&workspace, "workspace"))
                {
                    let _ = tx
                        .send(OutFrame::Text(ClientMsg::FsReadChunk {
                            request_id,
                            data_b64: String::new(),
                            eof: true,
                            error: Some(e),
                        }))
                        .await;
                } else {
                    let workspace_root = self.workspace_root();
                    let tx2 = tx.clone();
                    tokio::spawn(async move {
                        crate::fs::read_stream(
                            &workspace_root,
                            &account,
                            &workspace,
                            &path,
                            request_id,
                            tx2,
                        )
                        .await;
                    });
                }
            }
            ServerMsg::FsArchive {
                request_id,
                account,
                workspace,
                paths,
            } => {
                if let Err(e) = validate_name(&account, "account")
                    .and_then(|_| validate_name(&workspace, "workspace"))
                {
                    let _ = tx
                        .send(OutFrame::Text(ClientMsg::FsReadChunk {
                            request_id,
                            data_b64: String::new(),
                            eof: true,
                            error: Some(e),
                        }))
                        .await;
                } else {
                    let workspace_root = self.workspace_root();
                    let tx2 = tx.clone();
                    tokio::spawn(async move {
                        crate::fs::archive_stream(
                            &workspace_root,
                            &account,
                            &workspace,
                            &paths,
                            request_id,
                            tx2,
                        )
                        .await;
                    });
                }
            }
            ServerMsg::FsWriteInit {
                request_id,
                account,
                workspace,
                path,
                size: _,
            } => {
                let error = match validate_name(&account, "account")
                    .and_then(|_| validate_name(&workspace, "workspace"))
                {
                    Err(e) => Some(e),
                    Ok(()) => {
                        let workspace_root = self.workspace_root();
                        match crate::fs::write_init(
                            &self.write_sessions,
                            &workspace_root,
                            &account,
                            &workspace,
                            &path,
                            request_id,
                        )
                        .await
                        {
                            Ok(()) => None,
                            Err(e) => Some(e),
                        }
                    }
                };
                if let Some(e) = error {
                    let _ = tx
                        .send(OutFrame::Text(ClientMsg::FsWriteResult {
                            request_id,
                            bytes_written: 0,
                            final_name: None,
                            error: Some(e),
                        }))
                        .await;
                }
            }
            ServerMsg::FsWriteChunk {
                request_id,
                data_b64,
                eof,
            } => {
                match crate::fs::write_chunk(
                    &self.write_sessions,
                    request_id,
                    &data_b64,
                    eof,
                )
                .await
                {
                    Ok((bytes_written, Some(final_name))) => {
                        // EOF: send the final result, reporting the
                        // filename actually written (after any conflict
                        // suffix).
                        let _ = tx
                            .send(OutFrame::Text(ClientMsg::FsWriteResult {
                                request_id,
                                bytes_written,
                                final_name: Some(final_name),
                                error: None,
                            }))
                            .await;
                    }
                    Ok((_bytes_written, None)) => {
                        // Non-eof chunk: no reply needed.
                    }
                    Err(e) => {
                        let _ = tx
                            .send(OutFrame::Text(ClientMsg::FsWriteResult {
                                request_id,
                                bytes_written: 0,
                                final_name: None,
                                error: Some(e),
                            }))
                            .await;
                    }
                }
            }
            ServerMsg::FsDelete {
                request_id,
                account,
                workspace,
                paths,
            } => {
                if let Err(e) = validate_name(&account, "account")
                    .and_then(|_| validate_name(&workspace, "workspace"))
                {
                    let _ = tx
                        .send(OutFrame::Text(ClientMsg::FsDeleteResult {
                            request_id,
                            deleted: Vec::new(),
                            error: Some(e),
                        }))
                        .await;
                } else {
                    let workspace_root = self.workspace_root();
                    let (deleted, error) =
                        crate::fs::delete(&workspace_root, &account, &workspace, &paths).await;
                    let _ = tx
                        .send(OutFrame::Text(ClientMsg::FsDeleteResult {
                            request_id,
                            deleted,
                            error,
                        }))
                        .await;
                }
            }
            // Phase D 起这两类帧在 ws.rs 读循环里被拦截(resolve_response /
            // fail_pending),永远到不了 PtyManager;空臂仅为 match 穷尽性。
            ServerMsg::RemoteMcp { .. } => {}
            ServerMsg::RemoteMcpClosed { .. } => {}
            ServerMsg::Welcome { .. } | ServerMsg::Rejected { .. } | ServerMsg::Ping => {}
        }
    }

    /// Forwarded binary PTY input (keystrokes destined for the master).
    pub fn write_input(&self, session_id: Uuid, data: &[u8]) {
        let Some(h) = self.sessions.get(&session_id) else {
            tracing::debug!(session = %session_id, "input for unknown session");
            return;
        };
        let mut w = h.writer.lock().unwrap();
        if let Err(e) = w.write_all(data) {
            tracing::warn!(session = %session_id, error = %e, "pty write");
        }
        let _ = w.flush();
    }

    fn resize(&self, session_id: Uuid, cols: u16, rows: u16) -> Result<()> {
        let h = self
            .sessions
            .get(&session_id)
            .ok_or_else(|| anyhow::anyhow!("unknown session {}", session_id))?;
        let master = h.master.lock().unwrap();
        master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn open_session(
        self: &Arc<Self>,
        session_id: Uuid,
        account: String,
        workspace: String,
        cols: u16,
        rows: u16,
        mut claude_args: Vec<String>,
        sandbox: bool,
        sandbox_mode: Option<String>,
        tool: Option<String>,
        env: HashMap<String, String>,
        remote_mcp_capable: bool,
        tx: mpsc::Sender<OutFrame>,
    ) {
        if let Err(e) = validate_name(&account, "account") {
            let _ = tx
                .send(OutFrame::Text(ClientMsg::PtyError {
                    session_id,
                    message: e,
                }))
                .await;
            return;
        }
        if let Err(e) = validate_name(&workspace, "workspace") {
            let _ = tx
                .send(OutFrame::Text(ClientMsg::PtyError {
                    session_id,
                    message: e,
                }))
                .await;
            return;
        }
        // Resolve the tool to launch. `None` -> agent's configured
        // default. Unknown tool name -> PtyError before we touch the
        // filesystem.
        let tool_name = tool.unwrap_or_else(|| self.default_tool.clone());
        let Some(tool_cfg) = self.tools.get(&tool_name).cloned() else {
            let _ = tx
                .send(OutFrame::Text(ClientMsg::PtyError {
                    session_id,
                    message: format!("unknown tool '{}' (not in agent.toml [tools])", tool_name),
                }))
                .await;
            return;
        };
        // Same session_id arriving again = "swap workspace in place". Drop the
        // old handle silently (the reader thread will see read==0 and exit
        // without emitting PtyClosed because we set a no-emit marker through
        // the absence of the entry in `sessions`).
        let _ = self.sessions.remove(&session_id);
        let cwd_raw = self.workspace_root().join(&account).join(&workspace);
        if let Err(e) = std::fs::create_dir_all(&cwd_raw) {
            let _ = tx
                .send(OutFrame::Text(ClientMsg::PtyError {
                    session_id,
                    message: format!("create workspace dir: {}", e),
                }))
                .await;
            return;
        }
        // Canonicalize so `claude --continue` actually finds its
        // session history. claude derives its per-project subdir
        // under ~/.claude/projects/ from the *absolute* cwd; if we
        // hand the wrapper a relative cwd (because agent.toml ships
        // `workspace_root = "./agent/workspaces"`) the encoded
        // path mismatches every existing jsonl and the wrapper
        // falls back to a fresh boot — i.e. "every reconnect starts
        // with empty chat history". canonicalize fails if the dir
        // somehow vanished between create_dir_all and now; fall back
        // to the raw path in that pathological case.
        let cwd = std::fs::canonicalize(&cwd_raw).unwrap_or(cwd_raw);

        // capability 在 Phase D 用于 MCP 注入、Phase E 用于 attach 标记;
        // 此处先行记录,排障时可直接看到协商结果。
        tracing::debug!(%session_id, remote_mcp_capable, "open_session: client capability");

        // 注入本会话的远程-MCP 端点配置。token 是工作区稳定 token
        // (决策 D12):hub 每次 OpenSession(含 reattach)都铸新
        // session_id,这里把同一 token 对新 session_id 重注册(覆盖式)
        // —— tmux 里活着的 claude(内存持 token)与重启的 claude(从
        // 字节稳定的 mcp-remote.json 重读)都路由到活会话。
        if should_inject_mcp(self.remote_mcp.enabled, remote_mcp_capable, &tool_name) {
            let token = self
                .workspace_tokens
                .entry((account.clone(), workspace.clone()))
                .or_insert_with(|| {
                    // agent 重启自愈:优先采用本工作区 mcp-remote.json
                    // 已持久化的 token。只接受我们铸造的格式(32 ascii
                    // hex):被篡改/损坏的配置必须铸新,不得把任意
                    // (可猜)token 走私进路由表。
                    std::fs::read_to_string(cwd.join(".cloudcode").join("mcp-remote.json"))
                        .ok()
                        .and_then(|s| crate::mcp_proxy::extract_token_from_config(&s))
                        .filter(|t| crate::mcp_proxy::is_valid_token(t))
                        .unwrap_or_else(|| Uuid::new_v4().simple().to_string())
                })
                .clone();
            let mcp_cfg = crate::mcp_proxy::mcp_config_json(self.remote_mcp.port, &token);
            let mcp_cfg_path = cwd.join(".cloudcode").join("mcp-remote.json");
            if let Some(parent) = mcp_cfg_path.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    tracing::warn!(error = %e, "failed to create .cloudcode dir for remote MCP config");
                }
            }
            // 路由注册 + 注入 --mcp-config 只在配置确实落盘后进行:把
            // claude 指向一个缺失/部分的 --mcp-config(且会随每次 tmux
            // respawn 粘性重现)比「干净地无 cc-browser 启动」更糟。
            match write_remote_mcp_config_0600(&mcp_cfg_path, mcp_cfg.as_bytes(), session_id) {
                Ok(()) => {
                    self.mcp.register(token.clone(), session_id);
                    // 进程级注入:--mcp-config + --strict-mcp-config + 通用
                    // 引导 prompt。绝不写全局 ~/.claude.json,绝不 `claude
                    // mcp add`(D11 铁律)。strict 保证 claude 只看到这份
                    // 配置 —— 同机其他 claude 进程零影响。
                    claude_args.extend(crate::mcp_proxy::claude_mcp_args(&mcp_cfg_path));
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "failed to write remote MCP config; skipping cc-browser injection this open"
                    );
                }
            }
        }

        // Open the PTY.
        let size = PtySize {
            rows: rows.max(1),
            cols: cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = match native_pty_system().openpty(size) {
            Ok(p) => p,
            Err(e) => {
                let _ = tx
                    .send(OutFrame::Text(ClientMsg::PtyError {
                        session_id,
                        message: format!("openpty: {}", e),
                    }))
                    .await;
                return;
            }
        };

        // Build the tmux command. `-A` means "attach to session if it exists,
        // else create"; the workspace becomes a persistent slot. `-L <label>`
        // gives cloudcode its OWN tmux server, distinct from any global tmux
        // the user has running. Without this, our `tmux new-session` would
        // attach as a client to the user's existing server (which is not in
        // our sandbox), so claude would be spawned from a non-sandboxed
        // server and inherit nothing. A per-workspace label also keeps each
        // workspace's tmux server in its own sandbox state.
        // When the workspace sandbox is enabled we don't exec tmux directly:
        // we exec `cloudcode-agent sandbox-exec --workspace=… --home=… --
        // tmux …`, and that thin shim applies the sandbox to itself before
        // execing tmux (so tmux + claude inherit the sandbox state).
        let session_name = format!("cloudcode-{}-{}", account, workspace);
        let tmux_label = format!("cc-{}-{}", account, workspace);

        let home_for_idle = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let claude_proj_dir = crate::jsonl::project_dir(&home_for_idle, &cwd);

        // If the tmux session already exists, check whether claude is
        // idle (finished its turn) or busy (thinking / tool-calling).
        // Idle: kill the tool so the wrapper restarts with --continue,
        // giving the new client full conversation history.
        // Busy: attach to the live session without interrupting.
        //
        // Detection: read the last assistant entry in the most recent
        // jsonl under CLOUDCODE_CLAUDE_PROJECT_DIR. If its stop_reason
        // is "end_turn", claude has finished and is waiting for input.
        let session_exists = std::process::Command::new(&self.tmux.executable)
            .args(["-L", &tmux_label, "has-session", "-t", &session_name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if session_exists {
            let claude_idle = is_claude_idle(&claude_proj_dir);
            if claude_idle {
                tracing::info!(
                    session = %session_name,
                    "claude idle (end_turn) on reattach; killing tool for --continue restart"
                );
                let pane_pid = std::process::Command::new(&self.tmux.executable)
                    .args([
                        "-L", &tmux_label, "list-panes", "-t", &session_name,
                        "-F", "#{pane_pid}",
                    ])
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .and_then(|s| s.lines().next().map(|l| l.trim().to_string()))
                    .and_then(|s| s.parse::<i32>().ok());
                if let Some(pid) = pane_pid {
                    let _ = std::process::Command::new("pkill")
                        .args(["-TERM", "-P", &pid.to_string()])
                        .status();
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            } else {
                tracing::info!(
                    session = %session_name,
                    "claude busy on reattach; attaching to live session"
                );
            }
        }

        // Sandbox mode selection:
        //   sandbox = true  → strict profile  (full per-workspace
        //                     secrets/persistence hardening)
        //   sandbox = false → permissive profile (cross-account
        //                     isolation only; everything else open)
        //
        // We ALWAYS wrap through `cloudcode-agent sandbox-exec` —
        // there is no "no sandbox at all" branch any more, because
        // cross-account isolation must hold regardless of the
        // per-account toggle. If the platform doesn't support
        // Seatbelt at all (Linux today), surface that as a
        // PtyError; we don't want to silently run unconfined.
        // Resolve sandbox mode. Prefer the new `sandbox_mode` field;
        // fall back to legacy bool (strict if true, permissive if false)
        // for pre-v1.23 hubs.
        let mode = sandbox_mode
            .as_deref()
            .and_then(crate::sandbox::SandboxMode::parse)
            .unwrap_or(if sandbox {
                crate::sandbox::SandboxMode::Strict
            } else {
                crate::sandbox::SandboxMode::Permissive
            });

        // When mode == Off, run tmux directly without the sandbox-exec
        // wrapper. Strict/Permissive need the wrapper, which requires
        // self_exe to be set (macOS only today).
        let mut cmd = if mode == crate::sandbox::SandboxMode::Off {
            CommandBuilder::new(&self.tmux.executable)
        } else {
            let Some(self_exe) = self.self_exe.as_ref() else {
                let _ = tx
                    .send(OutFrame::Text(ClientMsg::PtyError {
                        session_id,
                        message: "workspace sandbox is not supported on this agent platform"
                            .to_string(),
                    }))
                    .await;
                return;
            };
            let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
            let ws_root = self.workspace_root();
            let mut c = CommandBuilder::new(self_exe);
            c.arg("sandbox-exec");
            c.arg("--workspace");
            c.arg(&cwd);
            c.arg("--workspace-root");
            c.arg(&ws_root);
            c.arg("--home");
            c.arg(&home);
            c.arg("--mode");
            c.arg(mode.as_str());
            c.arg("--");
            c.arg(&self.tmux.executable);
            c
        };
        // Our private tmux.conf (set -g mouse on). Must come BEFORE
        // the subcommand because tmux only honors -f as a global flag.
        // Only effective when the per-workspace server is starting
        // fresh (subsequent commands hit the existing server and
        // ignore -f), which matches the cases we care about.
        if let Some(conf) = self.tmux_conf.as_ref() {
            cmd.arg("-f");
            cmd.arg(conf);
        }
        cmd.arg("-L");
        cmd.arg(&tmux_label);
        cmd.arg("new-session");
        cmd.arg("-A");
        cmd.arg("-s");
        cmd.arg(&session_name);
        cmd.arg("-x");
        cmd.arg(cols.to_string());
        cmd.arg("-y");
        cmd.arg(rows.to_string());
        cmd.cwd(&cwd);
        // Wrap the tool in a small shell loop instead of execing it
        // directly. The semantics we want:
        //
        //   1. First boot: run `<tool_bin> <args>` exactly as configured.
        //   2. When the tool exits (/exit, Ctrl+C, crash): detach every
        //      attached tmux client so the cloudcode user pops straight
        //      back to the menu. The tmux session itself stays alive
        //      (the wrapper is still running), so the picker shows it
        //      as "saved".
        //   3. Sit in a polling sleep until somebody attaches again.
        //   4. On reattach, run `$CLOUDCODE_RESUME_CMD` if set; for
        //      claude that's `claude --continue`, but only if a saved
        //      jsonl actually exists under
        //      ~/.claude/projects/<encoded-cwd>/. For other tools we
        //      always honor whatever resume_command is configured (or
        //      relaunch fresh if it's empty).
        //
        // Explicit cleanup (delete workspace) still goes through the
        // menu's `d` action, which kills the per-workspace tmux server
        // and tears the wrapper down with it.
        cmd.arg("bash");
        cmd.arg("-c");
        cmd.arg(TOOL_WRAPPER);
        // bash's $0 label, not used by the script. The tool binary itself
        // is passed via $CLOUDCODE_TOOL_BIN (see cmd.env below), NOT as a
        // positional arg — otherwise the wrapper would invoke
        // `"$TOOL_BIN" "$@"` and end up running `claude claude …`, with
        // the duplicated name treated as an initial prompt.
        cmd.arg("cloudcode-tool");
        for arg in &tool_cfg.extra_args {
            cmd.arg(arg);
        }
        // Per-session args forwarded from the client (everything after `--`
        // on the cloudcode CLI). Only honoured for the first boot; on
        // reattach the wrapper falls through to `$CLOUDCODE_RESUME_CMD`.
        for arg in &claude_args {
            cmd.arg(arg);
        }
        // Strip CLAUDECODE* / CLAUDE_CODE_* (matches the multica precedent and
        // our v0.5 behaviour) so the parent's own claude-code session metadata
        // doesn't leak into the child.
        for (k, _) in std::env::vars() {
            if k.starts_with("CLAUDECODE") || k.starts_with("CLAUDE_CODE_") {
                cmd.env_remove(&k);
            }
        }
        // User-configured env (per-account / per-workspace), resolved
        // hub-side and forwarded in PtyOpen. Applied BEFORE the fixed
        // CLOUDCODE_* / TERM vars below so user config can never clobber
        // our internal vars. Set on the outer `cmd` (the sandbox-exec
        // wrapper, or tmux directly when mode == Off): run_sandbox_exec
        // execs via execvp(3), which preserves the environment, so these
        // vars survive into tmux → the bash wrapper → the tool process
        // inside the sandbox. Keys are validated defensively (same rule
        // as webterm/hub); bad keys are skipped, values pass verbatim.
        for (k, v) in &env {
            if is_valid_env_key(k) {
                cmd.env(k, v);
            } else {
                tracing::warn!(key = %k, "skipping env var with invalid key name");
            }
        }
        // Make sure the inner process knows it's interactive.
        cmd.env(
            "TERM",
            std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into()),
        );
        // Tell the wrapper where claude's per-project jsonl history
        // for this workspace lives, so it can decide whether
        // `--continue` is safe to try on reattach. Only meaningful
        // when the tool is claude; harmless for other tools.
        let home_for_proj = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let claude_proj_dir = crate::jsonl::project_dir(&home_for_proj, &cwd);
        cmd.env("CLOUDCODE_CLAUDE_PROJECT_DIR", &claude_proj_dir);
        // Tool-driving env for the generic wrapper.
        cmd.env("CLOUDCODE_TOOL", &tool_name);
        cmd.env("CLOUDCODE_TOOL_BIN", &tool_cfg.executable);
        cmd.env("CLOUDCODE_RESUME_CMD", &tool_cfg.resume_command);

        let child = match pair.slave.spawn_command(cmd) {
            Ok(c) => c,
            Err(e) => {
                let _ = tx
                    .send(OutFrame::Text(ClientMsg::PtyError {
                        session_id,
                        message: format!("spawn tmux: {}", e),
                    }))
                    .await;
                return;
            }
        };
        // Don't keep the slave fd open in the agent process; only the child
        // should hold it. (Required on macOS or read EOF never arrives.)
        drop(pair.slave);

        let writer = match pair.master.take_writer() {
            Ok(w) => w,
            Err(e) => {
                let _ = tx
                    .send(OutFrame::Text(ClientMsg::PtyError {
                        session_id,
                        message: format!("take_writer: {}", e),
                    }))
                    .await;
                return;
            }
        };
        let reader = match pair.master.try_clone_reader() {
            Ok(r) => r,
            Err(e) => {
                let _ = tx
                    .send(OutFrame::Text(ClientMsg::PtyError {
                        session_id,
                        message: format!("clone_reader: {}", e),
                    }))
                    .await;
                return;
            }
        };

        // Start tailing claude's per-project JSONL log for this
        // session. The watcher dies when the PtyHandle is dropped.
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let jsonl = crate::jsonl::spawn(session_id, cwd.clone(), home, tx.clone());

        let master = Arc::new(Mutex::new(pair.master));
        let handle = Arc::new(PtyHandle {
            master,
            writer: Mutex::new(writer),
            account: account.clone(),
            workspace: workspace.clone(),
            _jsonl: jsonl,
        });
        self.sessions.insert(session_id, handle.clone());

        let _ = tx
            .send(OutFrame::Text(ClientMsg::PtyOpened {
                session_id,
                workspace: workspace.clone(),
                cwd: cwd.display().to_string(),
            }))
            .await;

        // Recording: open the cast file (best effort).
        let recorder = match Recorder::open(
            &self.recording.dir,
            &account,
            &workspace,
            session_id,
            cols,
            rows,
        ) {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::warn!(session = %session_id, error = %e, "recorder open failed; continuing without record");
                None
            }
        };

        // PTY reader thread: blocking I/O on the master, push 4 KiB chunks as
        // binary frames; tee into the cast file. `handle` goes into the thread
        // so the reader can ptr_eq itself against `sessions[session_id]` and
        // skip emitting PtyClosed on a workspace-swap (where the entry has
        // already been replaced by a fresh handle).
        let sessions = self.sessions.clone();
        let tx_out = tx.clone();
        let _ = std::thread::Builder::new()
            .name(format!("pty-reader-{}", session_id))
            .spawn(move || {
                pty_reader_loop(
                    handle, reader, session_id, sessions, tx_out, recorder, child,
                )
            });
    }

    async fn close(&self, session_id: Uuid, tx: mpsc::Sender<OutFrame>) {
        // Drop the handle; the reader thread will see read=0 and exit; tmux
        // session stays alive on the OS.
        self.sessions.remove(&session_id);
        let _ = tx
            .send(OutFrame::Text(ClientMsg::PtyClosed {
                session_id,
                reason: Some("closed by hub".into()),
            }))
            .await;
    }

    /// Spawn an extra tmux pane inside an existing PTY session, running
    /// `tool_name` (looked up against `[tools]`). The pane inherits the
    /// session's tmux server (and therefore its sandbox state, if any),
    /// so we don't need to re-wrap it in `sandbox-exec`.
    ///
    /// We invoke tmux out-of-band (`std::process::Command`, not the PTY)
    /// because split-window is fire-and-forget against the tmux server
    /// daemon; the resulting pane's output is already being read by the
    /// reader thread attached to the session's master fd.
    async fn split_pane(
        self: &Arc<Self>,
        session_id: Uuid,
        tool_name: String,
        direction: SplitDirection,
        args: Vec<String>,
        tx: mpsc::Sender<OutFrame>,
    ) {
        let send_err = |error: String| {
            let tx = tx.clone();
            async move {
                let _ = tx
                    .send(OutFrame::Text(ClientMsg::SplitPaneResult {
                        session_id,
                        error: Some(error),
                    }))
                    .await;
            }
        };

        let Some(handle) = self.sessions.get(&session_id).map(|e| e.value().clone()) else {
            send_err(format!("unknown session {}", session_id)).await;
            return;
        };
        if let Err(e) = validate_name(&tool_name, "tool") {
            send_err(e).await;
            return;
        }
        let Some(tool_cfg) = self.tools.get(&tool_name).cloned() else {
            send_err(format!(
                "unknown tool '{}' (not in agent.toml [tools])",
                tool_name
            ))
            .await;
            return;
        };

        let session_name = format!("cloudcode-{}-{}", handle.account, handle.workspace);
        let tmux_label = format!("cc-{}-{}", handle.account, handle.workspace);
        let cwd_raw = self
            .workspace_root()
            .join(&handle.account)
            .join(&handle.workspace);
        // Canonicalize so claude's per-project jsonl lookup hits its
        // real `~/.claude/projects/<abs-path-encoded>/` dir. See the
        // longer note in the `OpenSession` branch above — agent.toml
        // commonly ships a relative `workspace_root`, which leaks
        // through every code path that wants to derive claude's
        // project encoding from cwd.
        let cwd = std::fs::canonicalize(&cwd_raw).unwrap_or(cwd_raw);

        // Pre-compute claude project dir so the wrapper's resume gating
        // works even when tool_name == "claude" in a split pane.
        let home_for_proj = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let claude_proj_dir = crate::jsonl::project_dir(&home_for_proj, &cwd);

        // Build the argv for `tmux split-window`. We use `env` (portable)
        // to set wrapper env vars instead of tmux's own `-e KEY=VAL`,
        // which only landed in tmux 3.2. The split pane inherits the
        // server's sandbox state (a server-side child of tmux), so we
        // don't wrap it in `sandbox-exec` again.
        // tmux's split-window orientation is the opposite of what most
        // people say in conversation: `-h` produces left/right panes
        // (vertical divider), `-v` produces top/bottom (horizontal
        // divider). We map our wire-level `Right` / `Down` directly.
        let split_flag = match direction {
            SplitDirection::Right => "-h",
            SplitDirection::Down => "-v",
        };
        let mut cmd = std::process::Command::new(&self.tmux.executable);
        cmd.arg("-L")
            .arg(&tmux_label)
            .arg("split-window")
            .arg(split_flag)
            .arg("-t")
            .arg(&session_name)
            .arg("-c")
            .arg(&cwd)
            .arg("--")
            .arg("env")
            .arg(format!("CLOUDCODE_TOOL={}", tool_name))
            .arg(format!("CLOUDCODE_TOOL_BIN={}", tool_cfg.executable))
            .arg(format!("CLOUDCODE_RESUME_CMD={}", tool_cfg.resume_command))
            .arg(format!(
                "CLOUDCODE_CLAUDE_PROJECT_DIR={}",
                claude_proj_dir.display()
            ))
            .arg("bash")
            .arg("-c")
            .arg(TOOL_WRAPPER)
            // $0 label only; the tool binary is sourced from
            // $CLOUDCODE_TOOL_BIN, not the positional args (see
            // open_session for the longer explanation).
            .arg("cloudcode-tool");
        for a in &tool_cfg.extra_args {
            cmd.arg(a);
        }
        for a in &args {
            cmd.arg(a);
        }

        // Run synchronously off the tokio runtime so we don't block the
        // WS read loop. tmux split-window returns quickly (sub-second)
        // once the server accepts the command, so spawn_blocking is fine.
        let output = match tokio::task::spawn_blocking(move || cmd.output()).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                send_err(format!("spawn tmux split-window: {}", e)).await;
                return;
            }
            Err(e) => {
                send_err(format!("join tmux split-window task: {}", e)).await;
                return;
            }
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let msg = if stderr.is_empty() {
                format!("tmux split-window exited {}", output.status)
            } else {
                format!("tmux split-window: {}", stderr)
            };
            send_err(msg).await;
            return;
        }

        let _ = tx
            .send(OutFrame::Text(ClientMsg::SplitPaneResult {
                session_id,
                error: None,
            }))
            .await;
    }

    /// Re-arrange the panes in an existing session using `tmux
    /// select-layout`. No-op on a 1-pane session (tmux just keeps it
    /// as-is). Errors come back as a SplitPaneResult so we don't have
    /// to invent a separate result variant for what is effectively the
    /// same fire-and-forget tmux shell-out as split.
    async fn change_layout(
        self: &Arc<Self>,
        session_id: Uuid,
        layout: PaneLayout,
        tx: mpsc::Sender<OutFrame>,
    ) {
        let send_err = |error: String| {
            let tx = tx.clone();
            async move {
                let _ = tx
                    .send(OutFrame::Text(ClientMsg::SplitPaneResult {
                        session_id,
                        error: Some(error),
                    }))
                    .await;
            }
        };

        let Some(handle) = self.sessions.get(&session_id).map(|e| e.value().clone()) else {
            send_err(format!("unknown session {}", session_id)).await;
            return;
        };
        let layout_name = match layout {
            PaneLayout::SideBySide => "even-horizontal",
            PaneLayout::Stacked => "even-vertical",
        };
        let session_name = format!("cloudcode-{}-{}", handle.account, handle.workspace);
        let tmux_label = format!("cc-{}-{}", handle.account, handle.workspace);
        let mut cmd = std::process::Command::new(&self.tmux.executable);
        cmd.arg("-L")
            .arg(&tmux_label)
            .arg("select-layout")
            .arg("-t")
            .arg(&session_name)
            .arg(layout_name);

        let output = match tokio::task::spawn_blocking(move || cmd.output()).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                send_err(format!("spawn tmux select-layout: {}", e)).await;
                return;
            }
            Err(e) => {
                send_err(format!("join tmux select-layout task: {}", e)).await;
                return;
            }
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let msg = if stderr.is_empty() {
                format!("tmux select-layout exited {}", output.status)
            } else {
                format!("tmux select-layout: {}", stderr)
            };
            send_err(msg).await;
        }
    }

    async fn workspace_list(&self, request_id: Uuid, account: String, tx: mpsc::Sender<OutFrame>) {
        let (items, error) = match validate_name(&account, "account") {
            Err(e) => (Vec::new(), Some(e)),
            Ok(()) => {
                let root = self.account_root(&account);
                let _ = std::fs::create_dir_all(&root);
                let mut names: Vec<String> = Vec::new();
                let mut error: Option<String> = None;
                match std::fs::read_dir(&root) {
                    Ok(rd) => {
                        for entry in rd.flatten() {
                            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                                if let Some(n) = entry.file_name().to_str().map(String::from) {
                                    if !n.starts_with('.') {
                                        names.push(n);
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => error = Some(format!("read_dir: {}", e)),
                }
                names.sort();
                let items = names
                    .into_iter()
                    .map(|name| {
                        let tmux_alive = tmux_session_alive(&account, &name);
                        WorkspaceItem { name, tmux_alive }
                    })
                    .collect();
                (items, error)
            }
        };
        let _ = tx
            .send(OutFrame::Text(ClientMsg::WorkspaceListResult {
                request_id,
                items,
                error,
            }))
            .await;
    }

    async fn workspace_create(
        &self,
        request_id: Uuid,
        account: String,
        name: String,
        tx: mpsc::Sender<OutFrame>,
    ) {
        let error = match validate_name(&account, "account")
            .and_then(|_| validate_name(&name, "workspace"))
        {
            Err(e) => Some(e),
            Ok(()) => {
                let dir = self.account_root(&account).join(&name);
                if dir.exists() {
                    Some(format!("workspace '{}' already exists", name))
                } else {
                    std::fs::create_dir_all(&dir)
                        .err()
                        .map(|e| format!("create: {}", e))
                }
            }
        };
        let _ = tx
            .send(OutFrame::Text(ClientMsg::WorkspaceCreateResult {
                request_id,
                error,
            }))
            .await;
    }

    async fn workspace_delete(
        &self,
        request_id: Uuid,
        account: String,
        name: String,
        tx: mpsc::Sender<OutFrame>,
    ) {
        let error = match validate_name(&account, "account")
            .and_then(|_| validate_name(&name, "workspace"))
        {
            Err(e) => Some(e),
            Ok(()) => {
                let dir_raw = self.account_root(&account).join(&name);
                if !dir_raw.exists() {
                    Some(format!("workspace '{}' does not exist", name))
                } else {
                    // Tear down the per-workspace tmux server we spawned
                    // for this slot, if it's still around.
                    let _ = std::process::Command::new(&self.tmux.executable)
                        .args(["-L", &format!("cc-{}-{}", account, name), "kill-server"])
                        .output();
                    // 工作区真死:移除其稳定 token 并注销端点路由。
                    // 同名重建的工作区会铸全新 token。
                    if let Some((_, tok)) = self
                        .workspace_tokens
                        .remove(&(account.clone(), name.clone()))
                    {
                        self.mcp.unregister(&tok);
                    }
                    // Wipe claude's per-project conversation history so
                    // a recreated workspace with the same name doesn't
                    // silently `--continue` into the old chat. The
                    // workspace cwd encodes deterministically into a
                    // dir name under ~/.claude/projects/ — but the
                    // encoding is based on the *absolute* cwd claude
                    // sees, so we canonicalize first (workspace_root
                    // is often a relative path in agent.toml).
                    let dir = std::fs::canonicalize(&dir_raw).unwrap_or_else(|_| dir_raw.clone());
                    if let Some(home) = dirs::home_dir() {
                        let claude_proj =
                            crate::jsonl::project_dir(&home, &dir);
                        let _ = std::fs::remove_dir_all(&claude_proj);
                    }
                    std::fs::remove_dir_all(&dir_raw)
                        .err()
                        .map(|e| format!("remove: {}", e))
                }
            }
        };
        let _ = tx
            .send(OutFrame::Text(ClientMsg::WorkspaceDeleteResult {
                request_id,
                error,
            }))
            .await;
    }

    /// Clear the saved session state for a workspace without removing
    /// its files: kill the per-workspace tmux server (which terminates
    /// the wrapper's `--continue` breadcrumb) and wipe claude's
    /// per-project history. Next OpenSession on this workspace gets a
    /// fresh claude with whatever args the client passes.
    async fn workspace_reset(
        &self,
        request_id: Uuid,
        account: String,
        name: String,
        tx: mpsc::Sender<OutFrame>,
    ) {
        let error = match validate_name(&account, "account")
            .and_then(|_| validate_name(&name, "workspace"))
        {
            Err(e) => Some(e),
            Ok(()) => {
                let dir_raw = self.account_root(&account).join(&name);
                if !dir_raw.exists() {
                    Some(format!("workspace '{}' does not exist", name))
                } else {
                    let _ = std::process::Command::new(&self.tmux.executable)
                        .args(["-L", &format!("cc-{}-{}", account, name), "kill-server"])
                        .output();
                    // reset = 旧 claude(及一切持旧 token 者)永久消失:
                    // 退役稳定 token,下次 open 从全新 token 开始。
                    if let Some((_, tok)) = self
                        .workspace_tokens
                        .remove(&(account.clone(), name.clone()))
                    {
                        self.mcp.unregister(&tok);
                    }
                    // Keep ~/.claude/projects/<encoded-cwd>/ intact so
                    // claude's conversation history and project memory
                    // survive the reset. The next --continue will resume
                    // the most recent session; older jsonl files are
                    // harmless. workspace_delete still wipes the project
                    // dir (a deleted workspace should not resurrect its
                    // chat on recreate).
                    None
                }
            }
        };
        let _ = tx
            .send(OutFrame::Text(ClientMsg::WorkspaceResetResult {
                request_id,
                error,
            }))
            .await;
    }

    /// Admin inventory: enumerate every (account, workspace) directory
    /// under workspace_root and probe tmux liveness for each.
    async fn workspace_list_all(&self, request_id: Uuid, tx: mpsc::Sender<OutFrame>) {
        let root = self.workspace_root();
        let mut items: Vec<WorkspaceFullItem> = Vec::new();
        let mut error: Option<String> = None;
        match std::fs::read_dir(&root) {
            Ok(rd) => {
                let mut accounts: Vec<String> = rd
                    .flatten()
                    .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                    .filter_map(|e| e.file_name().into_string().ok())
                    .filter(|n| !n.starts_with('.') && validate_name(n, "account").is_ok())
                    .collect();
                accounts.sort();
                for account in accounts {
                    let acct_dir = root.join(&account);
                    let Ok(rd2) = std::fs::read_dir(&acct_dir) else {
                        continue;
                    };
                    let mut workspaces: Vec<String> = rd2
                        .flatten()
                        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                        .filter_map(|e| e.file_name().into_string().ok())
                        .filter(|n| !n.starts_with('.') && validate_name(n, "workspace").is_ok())
                        .collect();
                    workspaces.sort();
                    for name in workspaces {
                        let tmux_alive = tmux_session_alive(&account, &name);
                        items.push(WorkspaceFullItem {
                            account: account.clone(),
                            name,
                            tmux_alive,
                        });
                    }
                }
            }
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    error = Some(format!("read_dir: {}", e));
                }
            }
        }
        let _ = tx
            .send(OutFrame::Text(ClientMsg::WorkspaceListAllResult {
                request_id,
                items,
                error,
            }))
            .await;
    }

    /// Walk the workspace root and produce `"<account>/<name>"`
    /// strings for every valid workspace dir we find. Used by the
    /// agent's Hello frame to seed hub's workspace registry on
    /// first connect — see `tunnel.rs::ClientMsg::Hello.workspaces`.
    /// Empty / non-existent / unreadable root yields an empty list
    /// rather than an error, because losing this seed is non-fatal
    /// (the user can recreate via the new flow).
    pub fn list_workspace_paths(&self) -> Vec<String> {
        let root = self.workspace_root();
        let Ok(rd) = std::fs::read_dir(&root) else {
            return Vec::new();
        };
        let mut accounts: Vec<String> = rd
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| !n.starts_with('.') && validate_name(n, "account").is_ok())
            .collect();
        accounts.sort();
        let mut out = Vec::new();
        for account in &accounts {
            let Ok(rd2) = std::fs::read_dir(root.join(account)) else {
                continue;
            };
            let mut workspaces: Vec<String> = rd2
                .flatten()
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| !n.starts_with('.') && validate_name(n, "workspace").is_ok())
                .collect();
            workspaces.sort();
            for name in workspaces {
                out.push(format!("{}/{}", account, name));
            }
        }
        out
    }

    fn workspace_root(&self) -> PathBuf {
        expand_path(&self.claude.workspace_root)
    }

    fn account_root(&self, account: &str) -> PathBuf {
        self.workspace_root().join(account)
    }

    /// Background task: reclaim resources from abandoned workspaces. Every
    /// `REAP_TICK`, kill the per-workspace tmux server (which takes claude down
    /// with it) for any workspace that has had no attached client for
    /// `>= REAP_IDLE_AFTER` AND whose claude is idle (`end_turn`). The
    /// conversation jsonl is left intact, so the next open resumes via
    /// `claude --continue`. Non-claude tools have no idle signal and are never
    /// reaped (conservative). Spawned once from `serve()`.
    pub async fn run_idle_reaper(self: Arc<Self>) {
        use std::collections::HashSet;
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        // (account, workspace) -> first time we observed it detached.
        let mut seen: HashMap<(String, String), Instant> = HashMap::new();
        loop {
            tokio::time::sleep(REAP_TICK).await;

            // Live per-workspace tmux servers.
            let live: Vec<(String, String)> = self
                .list_workspace_paths()
                .iter()
                .filter_map(|p| {
                    p.split_once('/').map(|(a, w)| (a.to_string(), w.to_string()))
                })
                .filter(|(a, w)| tmux_session_alive(a, w))
                .collect();

            // Workspaces with a client attached right now.
            let attached: HashSet<(String, String)> = self
                .sessions
                .iter()
                .map(|e| (e.value().account.clone(), e.value().workspace.clone()))
                .collect();

            // detached = live minus attached.
            let detached: HashSet<(String, String)> =
                live.into_iter().filter(|k| !attached.contains(k)).collect();

            let now = Instant::now();
            for (account, workspace) in due_for_reaping(&detached, &mut seen, now, REAP_IDLE_AFTER)
            {
                // Only reap an idle claude (end_turn). Busy / non-claude → skip
                // this tick but keep the `seen` entry, so we retry once idle.
                let cwd_raw = self.workspace_root().join(&account).join(&workspace);
                let cwd = std::fs::canonicalize(&cwd_raw).unwrap_or(cwd_raw);
                let proj_dir = crate::jsonl::project_dir(&home, &cwd);
                if !is_claude_idle(&proj_dir) {
                    continue;
                }
                // Final guard: nobody attached between the scan above and now.
                let attached_now = self.sessions.iter().any(|e| {
                    e.value().account == account && e.value().workspace == workspace
                });
                if attached_now {
                    continue;
                }
                let label = format!("cc-{}-{}", account, workspace);
                let _ = std::process::Command::new(&self.tmux.executable)
                    .args(["-L", &label, "kill-server"])
                    .output();
                tracing::info!(
                    %account, %workspace,
                    "reaped idle workspace (no client >= 30m + claude idle); --continue will resume"
                );
                seen.remove(&(account, workspace));
            }
        }
    }
}

/// How often the idle reaper scans for abandoned workspaces.
const REAP_TICK: std::time::Duration = std::time::Duration::from_secs(60);
/// A detached workspace is reaped after this long with no attached client
/// (and only when claude is idle). Fixed, not configurable.
const REAP_IDLE_AFTER: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Given the currently-detached workspaces, update `seen` (first-observed-
/// detached timestamps) and return those detached for at least `threshold`.
/// Pure decision logic, unit-tested: workspaces no longer detached (re-attached
/// or whose server is gone) are dropped from `seen` so their timer resets.
fn due_for_reaping(
    detached: &std::collections::HashSet<(String, String)>,
    seen: &mut HashMap<(String, String), Instant>,
    now: Instant,
    threshold: std::time::Duration,
) -> Vec<(String, String)> {
    seen.retain(|k, _| detached.contains(k));
    let mut due = Vec::new();
    for k in detached {
        let since = *seen.entry(k.clone()).or_insert(now);
        if now.duration_since(since) >= threshold {
            due.push(k.clone());
        }
    }
    due
}

/// Generic shell wrapper used for every pane (first or split). The
/// wrapper is identical for all tools; behaviour is steered by env
/// vars set by the spawn path:
///
/// - `CLOUDCODE_TOOL`        — tool key (e.g. `claude`, `codex`). The
///   wrapper only special-cases `claude` (to avoid `--continue` on an
///   empty conversation slot).
/// - `CLOUDCODE_TOOL_BIN`    — absolute path / argv0 of the tool binary.
///   Required; the wrapper exits immediately if it's unset.
/// - `CLOUDCODE_RESUME_CMD`  — shell snippet evaluated on reattach. Empty
///   = always relaunch fresh.
/// - `CLOUDCODE_CLAUDE_PROJECT_DIR` — claude-only: path to the per-cwd
///   jsonl history dir; resume is suppressed when this is empty or has
///   no `*.jsonl` yet.
///
/// Detach logic only kicks the user back to the menu when *this* is the
/// last pane in the session — otherwise other panes (codex etc.) are
/// still doing useful work and shouldn't be torn down behind the user.
const TOOL_WRAPPER: &str = r#"
TOOL="${CLOUDCODE_TOOL:-claude}"
TOOL_BIN="${CLOUDCODE_TOOL_BIN:-}"
RESUME_CMD="${CLOUDCODE_RESUME_CMD:-}"

if [ -z "$TOOL_BIN" ]; then
    echo "cloudcode-tool wrapper: CLOUDCODE_TOOL_BIN is required" >&2
    exit 1
fi

first=1
sess="$(tmux display-message -p '#S' 2>/dev/null)"
while :; do
    if [ "$first" = "1" ]; then
        "$TOOL_BIN" "$@"
        first=0
    else
        do_resume=false
        if [ -n "$RESUME_CMD" ]; then
            if [ "$TOOL" = "claude" ]; then
                # claude's `--continue` doesn't reliably non-zero exit
                # when there's no saved session, so we still gate on a
                # jsonl file actually existing under
                # ~/.claude/projects/<encoded-cwd>/.
                if [ -n "$CLOUDCODE_CLAUDE_PROJECT_DIR" ] \
                    && ls "$CLOUDCODE_CLAUDE_PROJECT_DIR"/*.jsonl >/dev/null 2>&1; then
                    do_resume=true
                fi
            else
                do_resume=true
            fi
        fi
        if [ "$do_resume" = "true" ]; then
            eval "$RESUME_CMD" '"$@"' || "$TOOL_BIN" "$@"
        else
            "$TOOL_BIN" "$@"
        fi
    fi
    # Tool has exited. Clear the pane BEFORE detaching so that when a
    # future client attaches, tmux's initial paint shows a blank pane
    # rather than briefly flashing the previous tool's exit dump
    # (claude on Ctrl-C dumps its chat UI back to main-screen, which
    # otherwise stays in the pane buffer until the wrapper finally
    # gets around to re-launching claude --continue).
    printf '\033[H\033[2J\033[3J'
    # Only detach the tmux client when we're the last pane in the
    # session. Other panes (e.g. codex running next to claude) are
    # still in use and the user shouldn't be kicked back to the menu
    # while they're alive.
    panes=$(tmux list-panes 2>/dev/null | wc -l)
    if [ "${panes:-0}" -le 1 ]; then
        if [ -n "$sess" ]; then
            tmux detach-client -s "$sess" 2>/dev/null
        else
            tmux detach-client -a 2>/dev/null
        fi
        # Park until somebody reattaches, then respawn the tool.
        while [ "$(tmux list-clients -t "$sess" -F . 2>/dev/null | wc -l)" -eq 0 ]; do
            sleep 1
        done
    else
        # Not the last pane: just kill this pane so the user is left
        # with whatever else was running. tmux will clean up on its
        # own once all panes exit.
        exit 0
    fi
done
"#;

#[allow(clippy::too_many_arguments)]
fn pty_reader_loop(
    handle: Arc<PtyHandle>,
    mut reader: Box<dyn Read + Send>,
    session_id: Uuid,
    sessions: Arc<DashMap<Uuid, Arc<PtyHandle>>>,
    tx_out: mpsc::Sender<OutFrame>,
    mut recorder: Option<Recorder>,
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
) {
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let chunk = &buf[..n];
                let frame = pack_pty_frame(TAG_PTY_OUTPUT, session_id, chunk);
                if tx_out.blocking_send(OutFrame::Binary(frame)).is_err() {
                    break;
                }
                if let Some(r) = recorder.as_mut() {
                    r.write_chunk(chunk);
                }
            }
            Err(e) => {
                tracing::debug!(session = %session_id, error = %e, "pty read error");
                break;
            }
        }
    }
    // Only emit PtyClosed if the session map still points at *us* — a
    // workspace swap replaces the entry with a fresh handle, and we don't
    // want the old reader to tell the hub the session ended.
    let still_us = sessions
        .get(&session_id)
        .map(|e| Arc::ptr_eq(e.value(), &handle))
        .unwrap_or(false);
    if still_us {
        sessions.remove(&session_id);
        let _ = child.try_wait();
        let _ = tx_out.blocking_send(OutFrame::Text(ClientMsg::PtyClosed {
            session_id,
            reason: Some("pty closed".into()),
        }));
    } else {
        let _ = child.try_wait();
    }
}

// ---------------------------------------------------------------------------
// Recording (asciinema cast v2; output-only, no input)
// ---------------------------------------------------------------------------

struct Recorder {
    file: std::fs::File,
    start: Instant,
}

impl Recorder {
    fn open(
        dir: &Path,
        account: &str,
        workspace: &str,
        session_id: Uuid,
        cols: u16,
        rows: u16,
    ) -> Result<Self> {
        let dir = dir.join(account).join(workspace);
        std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
        let path = dir.join(format!("{}.cast", session_id));
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        let header = serde_json::json!({
            "version": 2,
            "width": cols,
            "height": rows,
            "timestamp": chrono::Utc::now().timestamp(),
            "title": format!("cloudcode {}/{}", account, workspace),
            "env": { "TERM": std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into()) },
        });
        writeln!(file, "{}", header).context("write cast header")?;
        let _ = file.sync_all();
        let now = chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        tracing::info!(path = %path.display(), at = %now, "recording started");
        Ok(Self {
            file,
            start: Instant::now(),
        })
    }

    fn write_chunk(&mut self, chunk: &[u8]) {
        let dt = self.start.elapsed().as_secs_f64();
        let s = String::from_utf8_lossy(chunk);
        let line = serde_json::json!([dt, "o", s]);
        if let Err(e) = writeln!(self.file, "{}", line) {
            tracing::debug!(error = %e, "cast write failed");
        }
    }
}

// ---------------------------------------------------------------------------

/// Common name rules for accounts and workspaces (must be safe to drop into
/// a path component and a tmux session name).
/// 注入决策(纯函数,单测):Phase D = enabled && capable && claude。
/// Phase E(Task 14)翻转为 enabled && claude(始终广告,决策 D7)。
/// tool 门是硬条件:--mcp-config/--strict-mcp-config/
/// --append-system-prompt 是 claude 专属 flag,喂给 codex 等其他工具
/// 会直接启动失败(决策 D11;M1-M3 未做此门控,本计划修正)。
fn should_inject_mcp(enabled: bool, remote_mcp_capable: bool, tool_name: &str) -> bool {
    enabled && remote_mcp_capable && tool_name == "claude"
}

/// 原子写一份含 bearer token 的 0600 配置:写进同目录临时文件
/// (创建即 0600,不留「先写后 chmod」的 0644 窗口)再 rename 覆盖
/// 目标 —— 目标永远是「要么旧内容、要么完整的新 0600 文件」,绝无
/// 部分写入或短暂可读窗口。临时名带 session_id,避免同一工作区并发
/// open 撞名。
fn write_remote_mcp_config_0600(
    path: &Path,
    contents: &[u8],
    session_id: Uuid,
) -> std::io::Result<()> {
    let tmp = path.with_file_name(format!("mcp-remote.json.{}.tmp", session_id.simple()));
    let res = {
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .and_then(|mut f| {
                    f.write_all(contents)?;
                    f.flush()
                })
        }
        #[cfg(not(unix))]
        {
            std::fs::write(&tmp, contents)
        }
    };
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

fn validate_name(name: &str, kind: &str) -> std::result::Result<(), String> {
    if name.is_empty() || name.len() > 63 {
        return Err(format!("{} name must be 1..=63 chars", kind));
    }
    if name.starts_with('-') || name.starts_with('.') {
        return Err(format!("{} name cannot start with '-' or '.'", kind));
    }
    for c in name.chars() {
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_') {
            return Err(format!(
                "invalid char '{}' in {} name; allowed: lowercase a-z, 0-9, '-', '_'",
                c, kind
            ));
        }
    }
    Ok(())
}

/// Defensive validation for user-supplied env var names. Matches the
/// shared contract `^[A-Za-z_][A-Za-z0-9_]*$` used by webterm and the hub.
/// Values are never validated — they pass through verbatim.
fn is_valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn expand_path(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    let expanded = if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            home.join(rest)
        } else {
            p.to_path_buf()
        }
    } else {
        p.to_path_buf()
    };
    // The sandbox profile passes WORKSPACE_ROOT to SBPL's `subpath`,
    // which only matches against the absolute paths the kernel
    // actually reports. Relative paths like `./agent/workspaces` would
    // never match, silently disabling the cross-account deny. Always
    // anchor to an absolute path so every downstream consumer (sandbox
    // params, claude project-dir encoding, etc.) sees the same canonical
    // form. canonicalize() requires the dir to exist; fall back to
    // current_dir+join when it hasn't been created yet.
    if expanded.is_absolute() {
        expanded
    } else if let Ok(abs) = std::fs::canonicalize(&expanded) {
        abs
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(&expanded)
    } else {
        expanded
    }
}

/// Quick liveness probe for the per-workspace tmux server we spawn
/// with `-L cc-<account>-<workspace>`. We avoid running tmux itself
/// (that would *create* a fresh server if one isn't around). Instead
/// we just try to connect to the unix socket; the socket only exists
/// while the server is alive, and connect() returns ECONNREFUSED if
/// it died and left a stale socket behind.
fn tmux_session_alive(account: &str, workspace: &str) -> bool {
    let label = format!("cc-{}-{}", account, workspace);
    for path in tmux_socket_candidates(&label) {
        if std::os::unix::net::UnixStream::connect(&path).is_ok() {
            return true;
        }
    }
    false
}

fn tmux_socket_candidates(label: &str) -> Vec<PathBuf> {
    // tmux uses $TMUX_TMPDIR if set, else /tmp — it deliberately does
    // not look at $TMPDIR (which on macOS is the per-process private
    // /var/folders/ path, no tmux server lives there). We probe the
    // realpath /private/tmp too because /tmp is a symlink on macOS.
    // SAFETY: getuid is always safe.
    let uid = unsafe { libc::getuid() };
    let mut out = Vec::new();
    if let Some(td) = std::env::var_os("TMUX_TMPDIR") {
        out.push(PathBuf::from(td).join(format!("tmux-{}", uid)).join(label));
    }
    out.push(
        PathBuf::from("/tmp")
            .join(format!("tmux-{}", uid))
            .join(label),
    );
    out.push(
        PathBuf::from("/private/tmp")
            .join(format!("tmux-{}", uid))
            .join(label),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reaper_due_logic() {
        use std::collections::HashSet;
        use std::time::Duration;
        let k = |a: &str, w: &str| (a.to_string(), w.to_string());
        let mut seen: HashMap<(String, String), Instant> = HashMap::new();
        let now = Instant::now();
        let threshold = Duration::from_secs(30 * 60);

        // Newly detached: timestamp recorded, not yet due.
        let det: HashSet<(String, String)> = [k("acct", "ws1")].into_iter().collect();
        assert!(due_for_reaping(&det, &mut seen, now, threshold).is_empty());
        assert!(seen.contains_key(&k("acct", "ws1")));

        // Still detached, observed 31 min after first-seen → due.
        let later = now + Duration::from_secs(31 * 60);
        assert_eq!(
            due_for_reaping(&det, &mut seen, later, threshold),
            vec![k("acct", "ws1")]
        );

        // No longer detached (re-attached): dropped from `seen`, timer resets.
        let empty: HashSet<(String, String)> = HashSet::new();
        assert!(due_for_reaping(&empty, &mut seen, later, threshold).is_empty());
        assert!(!seen.contains_key(&k("acct", "ws1")));

        // A fresh detach starts the clock over: not immediately due.
        assert!(due_for_reaping(&det, &mut seen, later, threshold).is_empty());
    }

    #[test]
    fn names_accept_safe_chars() {
        assert!(validate_name("alice", "account").is_ok());
        assert!(validate_name("test1", "workspace").is_ok());
        assert!(validate_name("a_b-c-123", "workspace").is_ok());
        // Single char is fine.
        assert!(validate_name("a", "workspace").is_ok());
        // 63 chars is the documented upper bound.
        assert!(validate_name(&"a".repeat(63), "workspace").is_ok());
    }

    #[test]
    fn names_reject_unsafe_chars() {
        assert!(validate_name("", "account").is_err(), "empty");
        assert!(validate_name(&"a".repeat(64), "workspace").is_err(), "too long");
        assert!(validate_name("-leading-dash", "workspace").is_err());
        assert!(validate_name(".hidden", "workspace").is_err());
        assert!(validate_name("Has-Caps", "workspace").is_err(), "uppercase");
        assert!(validate_name("../escape", "workspace").is_err(), "path traversal");
        assert!(validate_name("with space", "workspace").is_err());
        assert!(validate_name("with/slash", "workspace").is_err());
        assert!(validate_name("with\0nul", "workspace").is_err());
        // Reject every char outside [a-z0-9_-]; cover a sample.
        for bad in ['/', '\\', '*', '$', '`', ';', '|', '&', '"', '\''] {
            let n = format!("ws{}bad", bad);
            assert!(validate_name(&n, "workspace").is_err(), "expected reject: {:?}", n);
        }
    }

    #[test]
    fn env_keys_match_posix_identifier_rule() {
        // ^[A-Za-z_][A-Za-z0-9_]*$
        assert!(is_valid_env_key("FOO"));
        assert!(is_valid_env_key("_foo"));
        assert!(is_valid_env_key("ANTHROPIC_BASE_URL"));
        assert!(is_valid_env_key("a1_B2"));
        assert!(is_valid_env_key("x"));
        // Rejections.
        assert!(!is_valid_env_key(""), "empty");
        assert!(!is_valid_env_key("1FOO"), "leading digit");
        assert!(!is_valid_env_key("FOO-BAR"), "dash");
        assert!(!is_valid_env_key("FOO BAR"), "space");
        assert!(!is_valid_env_key("FOO.BAR"), "dot");
        assert!(!is_valid_env_key("FOO="), "equals");
        assert!(!is_valid_env_key("PATH$"), "dollar");
    }
}

/// Check whether claude is idle by reading the most recent jsonl file
/// in the project dir. Returns true if the last `assistant` entry has
/// `stop_reason: "end_turn"` — meaning claude finished its response and
/// is waiting for user input. Returns false (assume busy) on any error
/// or if the last assistant message has a different stop_reason
/// (e.g. "tool_use" means claude is mid-chain).
fn is_claude_idle(claude_proj_dir: &Path) -> bool {
    use std::io::{BufRead, Seek, SeekFrom};

    let newest = match std::fs::read_dir(claude_proj_dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .and_then(|x| x.to_str())
                    == Some("jsonl")
            })
            .max_by_key(|e| e.metadata().ok().and_then(|m| m.modified().ok())),
        Err(_) => return false,
    };
    let Some(entry) = newest else { return false };

    let file = match std::fs::File::open(entry.path()) {
        Ok(f) => f,
        Err(_) => return false,
    };

    // Read the tail of the file (last 64KB is enough for several entries).
    let meta = match file.metadata() {
        Ok(m) => m,
        Err(_) => return false,
    };
    let tail_start = if meta.len() > 65536 { meta.len() - 65536 } else { 0 };
    let mut reader = std::io::BufReader::new(file);
    if reader.seek(SeekFrom::Start(tail_start)).is_err() {
        return false;
    }
    if tail_start > 0 {
        // Skip partial first line after seeking.
        let mut discard = String::new();
        let _ = reader.read_line(&mut discard);
    }

    let mut last_stop_reason: Option<String> = None;
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        if line.is_empty() { continue; }
        // Quick pre-filter: only parse lines that look like assistant entries.
        if !line.contains("\"assistant\"") { continue; }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
            if v.get("type").and_then(|t| t.as_str()) == Some("assistant") {
                if let Some(stop) = v
                    .get("message")
                    .and_then(|m| m.get("stop_reason"))
                    .and_then(|s| s.as_str())
                {
                    last_stop_reason = Some(stop.to_string());
                }
            }
        }
    }

    last_stop_reason.as_deref() == Some("end_turn")
}

#[cfg(test)]
mod remote_mcp_inject_tests {
    use super::*;

    #[test]
    fn inject_gates_on_enabled_capability_and_claude() {
        // Phase D 语义:三个条件齐才注入。Phase E(Task 14)翻转为
        // 始终广告(去掉 capability 条件),届时本测试同步更新。
        assert!(should_inject_mcp(true, true, "claude"));
        assert!(!should_inject_mcp(false, true, "claude"), "disabled kills injection");
        assert!(!should_inject_mcp(true, false, "claude"), "incapable client: no injection");
        assert!(
            !should_inject_mcp(true, true, "codex"),
            "claude-only flags must never reach other tools"
        );
    }
}
