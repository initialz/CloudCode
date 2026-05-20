use crate::config::{ClaudeConfig, RecordingConfig, SandboxConfig, TmuxConfig, ToolConfig};
use crate::sync::{
    run_push_worker, AckMsg, IgnoreFilter, PushQueue, PushWorker, WorkspaceWatcher,
};
use crate::tunnel::{
    pack_pty_frame, ClientMsg, ServerMsg, WorkspaceFullItem, WorkspaceItem, TAG_PTY_OUTPUT,
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
use tokio::sync::{mpsc, OnceCell};
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
    /// Active workspace pulls keyed by session_id. An entry exists for
    /// the window between `WorkspacePullStart` and the final
    /// `WorkspaceFile { is_last: true }`. We hold it here rather than
    /// in `PtyHandle` because the pull arrives BEFORE PtyOpen — there
    /// is no PtyHandle yet at that point.
    pulls: Arc<DashMap<Uuid, PullState>>,
    /// Shared push queue across every session this agent is running.
    /// Opened lazily on first use so the agent can boot without a
    /// state dir (e.g. in tests that never trigger sync).
    push_queue: Arc<OnceCell<Arc<PushQueue>>>,
    /// Routes inbound `WorkspaceFileAck` frames to the right session's
    /// push worker. Cleared when the session closes.
    ack_routes: Arc<DashMap<Uuid, mpsc::Sender<AckMsg>>>,
    /// Live outbound WS sender, swapped in by `ws.rs::run_once` on each
    /// successful connect and cleared when the WS goes away. Long-
    /// lived consumers (PTY reader thread, push worker, JSONL
    /// watcher) snapshot this on every send instead of holding the
    /// sender by value — otherwise a transient WS reset would tear
    /// down every active session as soon as the tied-off sender's
    /// receiver gets dropped.
    ws_tx: Arc<std::sync::RwLock<Option<mpsc::Sender<OutFrame>>>>,
}

struct PtyHandle {
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    writer: Mutex<Box<dyn Write + Send>>,
    /// Stops the jsonl watcher task on drop. Held only for its
    /// Drop side-effect.
    _jsonl: crate::jsonl::WatcherHandle,
    /// Workspace sync state for this session. Optional because not
    /// every PtyOpen is preceded by a pull (e.g. legacy hubs).
    _sync: Option<SessionSync>,
}

/// Owns the watcher + push-worker handles. Dropping it tears both
/// down (the worker via shutdown_tx, the watcher via its own Drop).
struct SessionSync {
    /// Held only for its Drop side-effect — the watcher stops when
    /// this goes out of scope.
    _watcher: WorkspaceWatcher,
    /// Fires the push worker's shutdown branch.
    shutdown_tx: mpsc::Sender<()>,
}

impl Drop for SessionSync {
    fn drop(&mut self) {
        // Best-effort; if the worker already exited the send fails
        // harmlessly. We never await this — we're in a Drop.
        let _ = self.shutdown_tx.try_send(());
    }
}

/// State the agent tracks for one in-flight `WorkspacePullStart` ->
/// `WorkspaceFile*` -> `WorkspacePullAck` round trip.
///
/// `expected` / `received` are tracked for debug logging — we don't
/// gate the final ack on them because the hub explicitly flags the
/// last frame via `is_last`. If `received` ends up != `expected` we
/// still log a warning so operators can spot a leaky stream.
struct PullState {
    account: String,
    workspace: String,
    expected: u64,
    received: u64,
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
                // mouse on             -> wheel scrolls per-pane scrollback;
                //                         drag-select enters tmux copy mode
                //                         and highlights the selection.
                // history-limit 50000  -> default 2000 lines is way too
                //                         small for AI chat transcripts.
                // set-clipboard on +   -> on copy, tmux emits an OSC 52
                // terminal-features       escape carrying the selected text
                // *:clipboard             upstream. webterm registers an
                //                         OSC 52 handler that drops it
                //                         straight into the browser
                //                         clipboard, so drag → release =
                //                         system-clipboard copy without
                //                         needing a modifier key.
                // UX we want, matching standard desktop selection:
                //   drag        -> tmux enters copy-mode, selection visible
                //   release     -> selection STAYS visible (don't auto-copy)
                //   click       -> exit copy-mode, drop selection
                //   y/Enter/c   -> copy + OSC 52 + exit; webterm
                //                  intercepts Cmd+C and sends 'y' to
                //                  bridge from desktop-style copy
                //
                // The piped shell command stays on one line — tmux's
                // conf parser doesn't honour backslash continuations
                // inside bind-key argv. Note the leading comma on
                // terminal-features: `set -a` is raw string append, so
                // without the separator tmux silently mangles the
                // value.
                let copy_pipe =
                    "send-keys -X copy-pipe-and-cancel 'base64 | tr -d \"\\n\" | (printf \"\\033]52;c;\"; cat; printf \"\\a\")'";
                let bindings = format!(
                    // Drag-end: stop the selection but stay in copy
                    // mode so it remains visible and the user can
                    // decide what to do next.
                    "bind-key -T copy-mode    MouseDragEnd1Pane send-keys -X stop-selection\n\
                     bind-key -T copy-mode-vi MouseDragEnd1Pane send-keys -X stop-selection\n\
                     bind-key -T copy-mode    MouseDown1Pane    send-keys -X cancel\n\
                     bind-key -T copy-mode-vi MouseDown1Pane    send-keys -X cancel\n\
                     bind-key -T copy-mode    Enter {copy_pipe}\n\
                     bind-key -T copy-mode-vi Enter {copy_pipe}\n\
                     bind-key -T copy-mode    y     {copy_pipe}\n\
                     bind-key -T copy-mode-vi y     {copy_pipe}\n\
                     bind-key -T copy-mode    c     {copy_pipe}\n\
                     bind-key -T copy-mode-vi c     {copy_pipe}\n",
                    copy_pipe = copy_pipe
                );
                let conf = format!(
                    "set -g mouse on\n\
                     set -g history-limit 50000\n\
                     set -g set-clipboard on\n\
                     set -as terminal-features ',*:clipboard'\n\
                     {bindings}"
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
            pulls: Arc::new(DashMap::new()),
            push_queue: Arc::new(OnceCell::new()),
            ack_routes: Arc::new(DashMap::new()),
            ws_tx: Arc::new(std::sync::RwLock::new(None)),
        })
    }

    /// Called by `ws.rs::run_once` right after a successful connect
    /// (the writer task's `Sender<OutFrame>` is still alive). Existing
    /// reader threads / push workers / jsonl watchers will pick this
    /// up on their next send.
    pub fn bind_ws_tx(&self, tx: mpsc::Sender<OutFrame>) {
        if let Ok(mut g) = self.ws_tx.write() {
            *g = Some(tx);
        }
    }

    /// Called when the WS connection has died. Subsequent sends from
    /// long-lived consumers see `None`, drop the frame, and wait for
    /// the next `bind_ws_tx` from a reconnect.
    pub fn clear_ws_tx(&self) {
        if let Ok(mut g) = self.ws_tx.write() {
            *g = None;
        }
    }

    /// Snapshot accessor so spawn paths can hand the slot to threads
    /// / tasks without pulling in the full `PtyManager` Arc.
    fn ws_tx_slot(&self) -> Arc<std::sync::RwLock<Option<mpsc::Sender<OutFrame>>>> {
        self.ws_tx.clone()
    }


    /// Test-only constructor that skips the tmux probe and FS dir
    /// setup. The pull / cleanup / ack paths only consult
    /// `claude.workspace_root`, so the other fields can be defaulted.
    #[cfg(test)]
    pub(crate) fn for_test(workspace_root: PathBuf) -> Self {
        Self {
            claude: ClaudeConfig {
                workspace_root,
                ..Default::default()
            },
            tools: HashMap::new(),
            default_tool: String::new(),
            tmux: TmuxConfig {
                executable: PathBuf::from("tmux"),
            },
            recording: RecordingConfig {
                dir: PathBuf::from("."),
                keep_days: 0,
            },
            self_exe: None,
            tmux_conf: None,
            sessions: Arc::new(DashMap::new()),
            pulls: Arc::new(DashMap::new()),
            push_queue: Arc::new(OnceCell::new()),
            ack_routes: Arc::new(DashMap::new()),
            ws_tx: Arc::new(std::sync::RwLock::new(None)),
        }
    }

    /// Lazily open (or return) the shared push queue. Returns `Err` if
    /// no state dir can be determined — the caller surfaces this as a
    /// PullAck failure to the hub so the open is aborted cleanly.
    async fn push_queue(&self) -> Result<Arc<PushQueue>> {
        if let Some(q) = self.push_queue.get() {
            return Ok(q.clone());
        }
        let path = crate::sync::push_queue::default_db_path()
            .ok_or_else(|| anyhow::anyhow!("no state dir for push queue"))?;
        // Race is safe: OnceCell::get_or_try_init serialises initialisation.
        let q = self
            .push_queue
            .get_or_try_init(|| async {
                let q = PushQueue::open(&path)
                    .await
                    .with_context(|| format!("open push queue at {}", path.display()))?;
                Ok::<_, anyhow::Error>(Arc::new(q))
            })
            .await?;
        Ok(q.clone())
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
                tool,
            } => {
                self.open_session(
                    session_id,
                    account,
                    workspace,
                    cols,
                    rows,
                    claude_args,
                    sandbox,
                    tool,
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
            ServerMsg::WorkspacePullStart {
                session_id,
                account,
                workspace,
                file_count,
            } => {
                self.workspace_pull_start(session_id, account, workspace, file_count, tx)
                    .await;
            }
            ServerMsg::WorkspaceFile {
                session_id,
                path,
                content,
                is_last,
            } => {
                self.workspace_file(session_id, path, content, is_last, tx)
                    .await;
            }
            ServerMsg::WorkspaceFileAck {
                session_id,
                path,
                ok,
                error,
            } => {
                self.workspace_file_ack(session_id, path, ok, error).await;
            }
            ServerMsg::WorkspaceCleanup { items } => {
                self.workspace_cleanup(items).await;
            }
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
        claude_args: Vec<String>,
        sandbox: bool,
        tool: Option<String>,
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
        let cwd = self.workspace_root().join(&account).join(&workspace);
        if let Err(e) = std::fs::create_dir_all(&cwd) {
            let _ = tx
                .send(OutFrame::Text(ClientMsg::PtyError {
                    session_id,
                    message: format!("create workspace dir: {}", e),
                }))
                .await;
            return;
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
        // Sandbox is now a per-session decision driven by the hub
        // (account.sandbox_enabled). If the hub asked for sandbox but
        // this platform can't deliver it (Linux), surface that as a
        // PtyError rather than silently spawning unsandboxed.
        if sandbox && self.self_exe.is_none() {
            let _ = tx
                .send(OutFrame::Text(ClientMsg::PtyError {
                    session_id,
                    message: "sandbox requested but not supported on this agent platform"
                        .to_string(),
                }))
                .await;
            return;
        }
        let mut cmd = if sandbox {
            // self_exe is Some here because we just checked above.
            let self_exe = self.self_exe.as_ref().unwrap();
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
            c.arg("--");
            c.arg(&self.tmux.executable);
            c
        } else {
            CommandBuilder::new(&self.tmux.executable)
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

        // Spin up the workspace sync engine for this session. Failure
        // is non-fatal: the PTY still works, just without bidirectional
        // sync (any local edits won't reach the hub). We log a warning
        // and skip — better than refusing the open entirely.
        let sync = self
            .start_session_sync(session_id, &account, &workspace)
            .await;
        if let Err(e) = sync.as_ref() {
            tracing::warn!(
                session = %session_id,
                error = %e,
                "workspace sync engine failed to start; session running without sync"
            );
        }

        let master = Arc::new(Mutex::new(pair.master));
        let handle = Arc::new(PtyHandle {
            master,
            writer: Mutex::new(writer),
            _jsonl: jsonl,
            _sync: sync.ok(),
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
        // already been replaced by a fresh handle). The thread holds the
        // shared `ws_tx` slot — NOT the per-WS sender by value — so a
        // transient WS reconnect doesn't kill the session: failed sends
        // just drop the frame and the next byte tries the freshly-bound
        // sender.
        let sessions = self.sessions.clone();
        let ws_tx_slot = self.ws_tx_slot();
        let _ = std::thread::Builder::new()
            .name(format!("pty-reader-{}", session_id))
            .spawn(move || {
                pty_reader_loop(
                    handle, reader, session_id, sessions, ws_tx_slot, recorder, child,
                )
            });
    }

    async fn close(&self, session_id: Uuid, tx: mpsc::Sender<OutFrame>) {
        // Drop the handle; the reader thread will see read=0 and exit; tmux
        // session stays alive on the OS. Dropping the handle also drops
        // the SessionSync which fires the push worker's shutdown branch.
        self.sessions.remove(&session_id);
        // Stop routing acks for a session that's going away. Any
        // in-flight rows stay in the durable queue for the next open.
        self.ack_routes.remove(&session_id);
        // Same with any half-finished pull. Hub treats a missing ack
        // as a failure and won't reuse the session_id.
        self.pulls.remove(&session_id);
        let _ = tx
            .send(OutFrame::Text(ClientMsg::PtyClosed {
                session_id,
                reason: Some("closed by hub".into()),
            }))
            .await;
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
                let dir = self.account_root(&account).join(&name);
                if !dir.exists() {
                    Some(format!("workspace '{}' does not exist", name))
                } else {
                    // Tear down the per-workspace tmux server we spawned
                    // for this slot, if it's still around.
                    let _ = std::process::Command::new(&self.tmux.executable)
                        .args(["-L", &format!("cc-{}-{}", account, name), "kill-server"])
                        .output();
                    // Wipe claude's per-project conversation history so
                    // a recreated workspace with the same name doesn't
                    // silently `--continue` into the old chat. The
                    // workspace cwd encodes deterministically into a
                    // dir name under ~/.claude/projects/.
                    if let Some(home) = dirs::home_dir() {
                        let claude_proj =
                            crate::jsonl::project_dir(&home, &dir);
                        let _ = std::fs::remove_dir_all(&claude_proj);
                    }
                    std::fs::remove_dir_all(&dir)
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
                let dir = self.account_root(&account).join(&name);
                if !dir.exists() {
                    Some(format!("workspace '{}' does not exist", name))
                } else {
                    let _ = std::process::Command::new(&self.tmux.executable)
                        .args(["-L", &format!("cc-{}-{}", account, name), "kill-server"])
                        .output();
                    if let Some(home) = dirs::home_dir() {
                        let claude_proj = crate::jsonl::project_dir(&home, &dir);
                        let _ = std::fs::remove_dir_all(&claude_proj);
                    }
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

    // -----------------------------------------------------------------
    // v1.13 hub-managed workspace sync
    // -----------------------------------------------------------------

    /// Hub announces a pull. Wipe any stale workspace dir for
    /// `(account, workspace)` and start tracking the inbound stream.
    /// If `file_count == 0`, the hub still sends one sentinel
    /// `WorkspaceFile { is_last: true, path: "" }` so the agent can
    /// ack — see `workspace_file`.
    pub async fn workspace_pull_start(
        self: &Arc<Self>,
        session_id: Uuid,
        account: String,
        workspace: String,
        file_count: u64,
        tx: mpsc::Sender<OutFrame>,
    ) {
        if let Err(e) =
            validate_name(&account, "account").and_then(|_| validate_name(&workspace, "workspace"))
        {
            let _ = tx
                .send(OutFrame::Text(ClientMsg::WorkspacePullAck {
                    session_id,
                    ok: false,
                    error: Some(e),
                }))
                .await;
            return;
        }
        let dir = self.workspace_root().join(&account).join(&workspace);
        // Wipe the dir's contents but preserve the directory inode
        // itself. The "rm -rf + mkdir" approach we used to do here
        // breaks the wrapper's resume-on-reattach flow: when a user
        // exits claude cleanly the wrapper parks and the tmux server
        // stays alive holding `cwd = <old inode>`. A subsequent pull
        // would unlink that inode under it, so the next claude
        // relaunch (triggered by tmux re-attach) inherits a dangling
        // cwd — `getcwd()` returns ENOENT and claude exits before it
        // can even paint, and from hub's POV the session just closes
        // again. Wiping children instead keeps the inode and the new
        // canonical bytes still land where the wrapper expects them.
        let dir_for_io = dir.clone();
        let wipe = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            if !dir_for_io.exists() {
                std::fs::create_dir_all(&dir_for_io)?;
                return Ok(());
            }
            for entry in std::fs::read_dir(&dir_for_io)? {
                let entry = entry?;
                let path = entry.path();
                if entry.file_type()?.is_dir() {
                    std::fs::remove_dir_all(&path)?;
                } else {
                    std::fs::remove_file(&path)?;
                }
            }
            Ok(())
        })
        .await;
        if let Err(e) = wipe.unwrap_or_else(|e| Err(std::io::Error::other(e))) {
            let _ = tx
                .send(OutFrame::Text(ClientMsg::WorkspacePullAck {
                    session_id,
                    ok: false,
                    error: Some(format!("wipe workspace dir: {}", e)),
                }))
                .await;
            return;
        }

        self.pulls.insert(
            session_id,
            PullState {
                account,
                workspace,
                expected: file_count,
                received: 0,
            },
        );

        // Empty workspace: ack right away. The hub will still send the
        // sentinel WorkspaceFile (is_last=true, empty path) but
        // `workspace_file` handles the double-ack idempotently because
        // the pull entry is already gone.
        if file_count == 0 {
            self.pulls.remove(&session_id);
            let _ = tx
                .send(OutFrame::Text(ClientMsg::WorkspacePullAck {
                    session_id,
                    ok: true,
                    error: None,
                }))
                .await;
        }
    }

    /// One file from a pull stream. The agent buffers nothing — every
    /// frame writes straight to disk so a huge workspace doesn't
    /// balloon agent RAM.
    pub async fn workspace_file(
        self: &Arc<Self>,
        session_id: Uuid,
        path: String,
        content: Vec<u8>,
        is_last: bool,
        tx: mpsc::Sender<OutFrame>,
    ) {
        // Snapshot the destination dir before mutating pull state. If
        // the entry is gone we silently drop the frame — pull was
        // already acked (file_count == 0 case) or the agent restarted.
        let dest_root = {
            let Some(p) = self.pulls.get(&session_id) else {
                if is_last {
                    tracing::debug!(%session_id, "WorkspaceFile after pull acked; ignoring");
                }
                return;
            };
            self.workspace_root().join(&p.account).join(&p.workspace)
        };

        // Sentinel "empty stream" frame on an empty workspace: path is
        // blank, is_last is true, nothing to write. Just ack.
        let nonempty_path = !path.is_empty();
        if nonempty_path {
            if let Err(e) = write_pull_file(&dest_root, &path, &content).await {
                tracing::warn!(%session_id, %path, error = %e, "workspace pull: write failed");
                self.pulls.remove(&session_id);
                let _ = tx
                    .send(OutFrame::Text(ClientMsg::WorkspacePullAck {
                        session_id,
                        ok: false,
                        error: Some(format!("write {}: {}", path, e)),
                    }))
                    .await;
                return;
            }
        }

        // Bump the counter. We do this even for empty-path frames so
        // file_count == 0 ack-once semantics work uniformly.
        if let Some(mut p) = self.pulls.get_mut(&session_id) {
            if nonempty_path {
                p.received += 1;
            }
        }

        if is_last {
            // Drop pull state and ack — order matters so a fast follow
            // up PtyOpen doesn't double-ack.
            if let Some((_, p)) = self.pulls.remove(&session_id) {
                if p.received != p.expected {
                    tracing::warn!(
                        %session_id,
                        account = %p.account,
                        workspace = %p.workspace,
                        expected = p.expected,
                        received = p.received,
                        "workspace pull: file count mismatch (still acking as ok=true since hub flagged is_last)"
                    );
                } else {
                    tracing::debug!(
                        %session_id,
                        files = p.received,
                        "workspace pull: complete"
                    );
                }
            }
            let _ = tx
                .send(OutFrame::Text(ClientMsg::WorkspacePullAck {
                    session_id,
                    ok: true,
                    error: None,
                }))
                .await;
        }
    }

    /// Hub's reply to a `WorkspacePushFile` / `WorkspaceDeleteFile`.
    /// Look up the session's ack channel and forward — the push worker
    /// dequeues on `ok = true` or backs off on `ok = false`.
    pub async fn workspace_file_ack(
        &self,
        session_id: Uuid,
        path: String,
        ok: bool,
        error: Option<String>,
    ) {
        let Some(route) = self.ack_routes.get(&session_id) else {
            tracing::debug!(%session_id, %path, "WorkspaceFileAck for unknown session");
            return;
        };
        // Best-effort: worker may have just shut down.
        let _ = route.send(AckMsg { path, ok, error }).await;
    }

    /// Hub-driven garbage collection of stale local workspaces. Each
    /// item is `(account, workspace)`; we `rm -rf` the matching dir
    /// under `workspace_root`.
    pub async fn workspace_cleanup(&self, items: Vec<(String, String)>) {
        let root = self.workspace_root();
        let mut cleared = 0usize;
        let mut failed = 0usize;
        for (account, workspace) in items {
            if validate_name(&account, "account").is_err()
                || validate_name(&workspace, "workspace").is_err()
            {
                tracing::warn!(%account, %workspace, "workspace cleanup: invalid name; skipping");
                failed += 1;
                continue;
            }
            let dir = root.join(&account).join(&workspace);
            if !dir.exists() {
                continue;
            }
            // Kill the per-workspace tmux server first, otherwise the
            // wrapper process keeps the cwd open and the rm becomes a
            // race against ongoing writes from the resumed tool.
            let _ = std::process::Command::new(&self.tmux.executable)
                .args([
                    "-L",
                    &format!("cc-{}-{}", account, workspace),
                    "kill-server",
                ])
                .output();
            match tokio::fs::remove_dir_all(&dir).await {
                Ok(()) => {
                    cleared += 1;
                    tracing::info!(%account, %workspace, "workspace cleanup: removed");
                }
                Err(e) => {
                    failed += 1;
                    tracing::warn!(%account, %workspace, error = %e, "workspace cleanup: remove failed");
                }
            }
        }
        if cleared > 0 || failed > 0 {
            tracing::info!(cleared, failed, "workspace cleanup: done");
        }
    }

    /// Internal: bring the watcher + push worker online for a session
    /// that just opened. Returns the [`SessionSync`] handle the
    /// PtyHandle holds for the lifetime of the session.
    async fn start_session_sync(
        self: &Arc<Self>,
        session_id: Uuid,
        account: &str,
        workspace: &str,
    ) -> Result<SessionSync> {
        let queue = self.push_queue().await?;
        let cwd = self.workspace_root().join(account).join(workspace);
        // Build the ignore filter once and share it between the watcher
        // (which filters per-event) and the push worker (which uses the
        // same filter inside `enqueue_subtree` for directory fan-outs).
        let filter = Arc::new(IgnoreFilter::new(&cwd).context("build ignore filter")?);
        tracing::info!(
            session = %session_id,
            account, workspace,
            watch_dir = %cwd.display(),
            "starting workspace sync engine"
        );

        // Channels: watcher -> worker, ws read loop -> worker (acks),
        // PtyHandle::Drop -> worker (shutdown).
        let (watch_tx, watch_rx) = mpsc::channel(256);
        let (ack_tx, ack_rx) = mpsc::channel(64);
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        let watcher = WorkspaceWatcher::start(cwd.clone(), filter.clone(), watch_tx)
            .context("start workspace watcher")?;
        tracing::info!(session = %session_id, "workspace watcher running");

        self.ack_routes.insert(session_id, ack_tx);

        let worker = PushWorker {
            session_id,
            account: account.to_string(),
            workspace: workspace.to_string(),
            workspace_root: self.workspace_root(),
            ignore_filter: filter,
            queue,
            watch_rx,
            ack_rx,
            shutdown_rx,
            ws_tx_slot: self.ws_tx_slot(),
        };
        tokio::spawn(async move {
            run_push_worker(worker).await;
        });

        Ok(SessionSync {
            _watcher: watcher,
            shutdown_tx,
        })
    }

    fn workspace_root(&self) -> PathBuf {
        expand_path(&self.claude.workspace_root)
    }

    fn account_root(&self, account: &str) -> PathBuf {
        self.workspace_root().join(account)
    }
}

/// Generic shell wrapper used for the workspace's single pane. The
/// wrapper is identical for all tools; behaviour is steered by env
/// vars set by the spawn path:
///
/// - `CLOUDCODE_TOOL`        — tool key (effectively `claude` in v1.13;
///   the wrapper still special-cases it explicitly to avoid
///   `--continue` on an empty conversation slot).
/// - `CLOUDCODE_TOOL_BIN`    — absolute path / argv0 of the tool binary.
///   Required; the wrapper exits immediately if it's unset.
/// - `CLOUDCODE_RESUME_CMD`  — shell snippet evaluated on reattach. Empty
///   = always relaunch fresh.
/// - `CLOUDCODE_CLAUDE_PROJECT_DIR` — claude-only: path to the per-cwd
///   jsonl history dir; resume is suppressed when this is empty or has
///   no `*.jsonl` yet.
///
/// The detach loop tolerates the user creating extra tmux panes by
/// hand (Ctrl-b %, Ctrl-b "); when those panes are still around the
/// wrapper just exits its own pane instead of detaching the whole
/// session. UI-driven split was removed in v1.13.
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
            eval "$RESUME_CMD" || "$TOOL_BIN" "$@"
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
    # session. If the user manually opened other panes (tmux's own
    # Ctrl-b % / ") they're still in use and the user shouldn't be
    # kicked back to the menu while they're alive.
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
    ws_tx_slot: Arc<std::sync::RwLock<Option<mpsc::Sender<OutFrame>>>>,
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
                // Snapshot the current ws tx each loop iteration.
                // None / send-failure just drops the frame and keeps
                // reading — the session must survive across a WS
                // reconnect (which arrives as: writer task closes →
                // tx_out's receiver dropped → blocking_send Err).
                // Always record locally regardless of WS state so the
                // asciicast captures everything.
                let snapshot = ws_tx_slot.read().ok().and_then(|g| g.clone());
                if let Some(tx) = snapshot {
                    let _ = tx.blocking_send(OutFrame::Binary(frame));
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
        // Best-effort PtyClosed via whichever ws is bound right now;
        // if the WS is mid-reconnect we just drop the frame — the
        // hub will infer "session gone" from the agent disconnect
        // anyway on the next handshake.
        let snapshot = ws_tx_slot.read().ok().and_then(|g| g.clone());
        if let Some(tx) = snapshot {
            let _ = tx.blocking_send(OutFrame::Text(ClientMsg::PtyClosed {
                session_id,
                reason: Some("pty closed".into()),
            }));
        }
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

fn expand_path(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    p.to_path_buf()
}

/// Write one workspace file under `root`, refusing any path that
/// would escape the workspace. The hub validates this too, but the
/// agent is the one with FS write access — defence in depth.
async fn write_pull_file(root: &Path, rel: &str, content: &[u8]) -> std::io::Result<()> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "absolute path",
        ));
    }
    for c in rel_path.components() {
        match c {
            std::path::Component::Normal(_) => {}
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "non-normal path component",
                ));
            }
        }
    }
    let abs = root.join(rel_path);
    if let Some(parent) = abs.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&abs, content).await
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

    // -----------------------------------------------------------------
    // v1.13 workspace pull / cleanup / push worker
    //
    // These tests mock the wire as channels and verify the agent's
    // pull bookkeeping + cleanup side-effects + push worker matching
    // hub acks to queue dequeues. They never touch tmux.
    // -----------------------------------------------------------------

    use tokio::time::{timeout, Duration};

    /// Helper: collect every outbound text frame matching the predicate
    /// until either (a) we get one, or (b) the wait window elapses.
    async fn wait_for<F>(
        rx: &mut mpsc::Receiver<OutFrame>,
        mut pred: F,
    ) -> Option<ClientMsg>
    where
        F: FnMut(&ClientMsg) -> bool,
    {
        let dl = tokio::time::sleep(Duration::from_secs(2));
        tokio::pin!(dl);
        loop {
            tokio::select! {
                _ = &mut dl => return None,
                frame = rx.recv() => match frame {
                    Some(OutFrame::Text(m)) if pred(&m) => return Some(m),
                    Some(_) => continue,
                    None => return None,
                }
            }
        }
    }

    #[tokio::test]
    async fn pull_zero_files_acks_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = Arc::new(PtyManager::for_test(dir.path().to_path_buf()));
        let (tx, mut rx) = mpsc::channel(64);
        let sid = Uuid::new_v4();

        mgr.workspace_pull_start(
            sid,
            "alice".into(),
            "ws1".into(),
            0,
            tx.clone(),
        )
        .await;

        // First reply must be PullAck ok=true.
        let ack = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("PullAck within 1s")
            .expect("channel open");
        match ack {
            OutFrame::Text(ClientMsg::WorkspacePullAck { session_id, ok, error }) => {
                assert_eq!(session_id, sid);
                assert!(ok);
                assert!(error.is_none());
            }
            other => panic!("expected PullAck, got {:?}", text_kind(&other)),
        }
        // Workspace dir exists and is empty.
        let ws_dir = dir.path().join("alice").join("ws1");
        assert!(ws_dir.exists() && ws_dir.is_dir());
        assert!(std::fs::read_dir(&ws_dir).unwrap().next().is_none());
    }

    #[tokio::test]
    async fn pull_n_files_writes_all_and_acks() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = Arc::new(PtyManager::for_test(dir.path().to_path_buf()));
        let (tx, mut rx) = mpsc::channel(64);
        let sid = Uuid::new_v4();

        mgr.workspace_pull_start(
            sid,
            "alice".into(),
            "ws1".into(),
            3,
            tx.clone(),
        )
        .await;
        // No ack yet — file_count > 0.
        assert!(timeout(Duration::from_millis(50), rx.recv()).await.is_err());

        for (i, (path, body)) in [
            ("README.md", &b"# hi"[..]),
            ("src/main.rs", &b"fn main() {}"[..]),
            ("sub/dir/file.txt", &b"deep"[..]),
        ]
        .iter()
        .enumerate()
        {
            mgr.workspace_file(
                sid,
                path.to_string(),
                body.to_vec(),
                i == 2,
                tx.clone(),
            )
            .await;
        }

        let ack = wait_for(&mut rx, |m| matches!(m, ClientMsg::WorkspacePullAck { .. }))
            .await
            .expect("PullAck");
        match ack {
            ClientMsg::WorkspacePullAck { ok, .. } => assert!(ok),
            _ => unreachable!(),
        }
        let ws_dir = dir.path().join("alice").join("ws1");
        assert_eq!(std::fs::read(ws_dir.join("README.md")).unwrap(), b"# hi");
        assert_eq!(
            std::fs::read(ws_dir.join("src/main.rs")).unwrap(),
            b"fn main() {}"
        );
        assert_eq!(
            std::fs::read(ws_dir.join("sub/dir/file.txt")).unwrap(),
            b"deep"
        );
    }

    /// Pinning the inode-preservation fix: a second pull on the same
    /// workspace must wipe the contents but keep the directory inode
    /// alive, otherwise a still-running tmux server (whose cwd is that
    /// inode) loses its cwd and any tool relaunch on reattach fails
    /// with ENOENT. See the comment in `workspace_pull_start`.
    #[cfg(unix)]
    #[tokio::test]
    async fn second_pull_preserves_workspace_dir_inode() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let mgr = Arc::new(PtyManager::for_test(dir.path().to_path_buf()));
        let (tx, mut rx) = mpsc::channel(64);
        let ws_dir = dir.path().join("alice").join("ws1");

        // First pull: 1 file.
        let sid1 = Uuid::new_v4();
        mgr.workspace_pull_start(sid1, "alice".into(), "ws1".into(), 1, tx.clone())
            .await;
        mgr.workspace_file(
            sid1,
            "first.txt".into(),
            b"first".to_vec(),
            true,
            tx.clone(),
        )
        .await;
        wait_for(&mut rx, |m| matches!(m, ClientMsg::WorkspacePullAck { .. }))
            .await
            .expect("first PullAck");
        let inode_before = std::fs::metadata(&ws_dir).unwrap().ino();
        assert!(ws_dir.join("first.txt").exists());

        // Second pull: a different file. The dir contents change but
        // the dir's own inode must survive.
        let sid2 = Uuid::new_v4();
        mgr.workspace_pull_start(sid2, "alice".into(), "ws1".into(), 1, tx.clone())
            .await;
        mgr.workspace_file(
            sid2,
            "second.txt".into(),
            b"second".to_vec(),
            true,
            tx.clone(),
        )
        .await;
        wait_for(&mut rx, |m| matches!(m, ClientMsg::WorkspacePullAck { .. }))
            .await
            .expect("second PullAck");
        let inode_after = std::fs::metadata(&ws_dir).unwrap().ino();
        assert_eq!(
            inode_before, inode_after,
            "workspace dir inode must be stable across pulls"
        );
        assert!(!ws_dir.join("first.txt").exists());
        assert!(ws_dir.join("second.txt").exists());
    }

    #[tokio::test]
    async fn pull_rejects_absolute_and_traversal_paths() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = Arc::new(PtyManager::for_test(dir.path().to_path_buf()));
        let (tx, mut rx) = mpsc::channel(64);
        let sid = Uuid::new_v4();

        mgr.workspace_pull_start(sid, "alice".into(), "ws1".into(), 1, tx.clone())
            .await;
        // Try to escape with "../etc/passwd". The agent must fail the ack.
        mgr.workspace_file(
            sid,
            "../leak".into(),
            b"oops".to_vec(),
            true,
            tx.clone(),
        )
        .await;
        let ack = wait_for(&mut rx, |m| matches!(m, ClientMsg::WorkspacePullAck { .. }))
            .await
            .expect("PullAck");
        match ack {
            ClientMsg::WorkspacePullAck { ok, error, .. } => {
                assert!(!ok);
                assert!(error.is_some());
            }
            _ => unreachable!(),
        }
        // Nothing escaped to the parent.
        assert!(!dir.path().join("leak").exists());
    }

    #[tokio::test]
    async fn workspace_cleanup_removes_listed_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = Arc::new(PtyManager::for_test(dir.path().to_path_buf()));

        // Lay down three workspaces.
        for (acct, ws) in [
            ("alice", "ws1"),
            ("alice", "ws2"),
            ("bob", "ws1"),
        ] {
            let p = dir.path().join(acct).join(ws);
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join("f.txt"), b"x").unwrap();
        }

        mgr.workspace_cleanup(vec![
            ("alice".into(), "ws1".into()),
            ("bob".into(), "ws1".into()),
            // Non-existent: must not error.
            ("alice".into(), "missing".into()),
        ])
        .await;

        assert!(!dir.path().join("alice/ws1").exists());
        assert!(dir.path().join("alice/ws2").exists());
        assert!(!dir.path().join("bob/ws1").exists());
    }

    #[tokio::test]
    async fn workspace_cleanup_rejects_invalid_names() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = Arc::new(PtyManager::for_test(dir.path().to_path_buf()));
        std::fs::create_dir_all(dir.path().join("alice/ws1")).unwrap();
        // Even if the dir happens to exist on disk, an invalid name
        // must be ignored — the FS-layer's already shielded by name
        // validation, but this is the only second-chance check.
        mgr.workspace_cleanup(vec![("../escape".into(), "ws".into())])
            .await;
        assert!(dir.path().join("alice/ws1").exists());
    }

    #[tokio::test]
    async fn push_worker_drains_queue_on_ack() {
        use crate::sync::push_queue::PushQueue;
        use crate::sync::runtime::{run_push_worker, AckMsg, PushWorker};
        use crate::sync::QueueOp;

        let qdir = tempfile::tempdir().unwrap();
        let wsdir = tempfile::tempdir().unwrap();
        let queue = Arc::new(PushQueue::open(&qdir.path().join("q.db")).await.unwrap());
        queue
            .enqueue(QueueOp::PushFile {
                account: "alice".into(),
                workspace: "ws1".into(),
                path: "a.rs".into(),
                content: b"v1".to_vec(),
            })
            .await
            .unwrap();
        queue
            .enqueue(QueueOp::DeleteFile {
                account: "alice".into(),
                workspace: "ws1".into(),
                path: "b.rs".into(),
            })
            .await
            .unwrap();
        // Another session's row — must NOT be touched.
        queue
            .enqueue(QueueOp::PushFile {
                account: "bob".into(),
                workspace: "ws1".into(),
                path: "c.rs".into(),
                content: b"keep".to_vec(),
            })
            .await
            .unwrap();
        assert_eq!(queue.len().await.unwrap(), 3);

        let (tx, mut rx) = mpsc::channel(64);
        let (_watch_tx, watch_rx) = mpsc::channel(16);
        let (ack_tx, ack_rx) = mpsc::channel(16);
        let (_shutdown_tx, shutdown_rx) = mpsc::channel(1);
        let sid = Uuid::new_v4();

        let ws_slot = Arc::new(std::sync::RwLock::new(Some(tx.clone())));
        let worker = PushWorker {
            session_id: sid,
            account: "alice".into(),
            workspace: "ws1".into(),
            workspace_root: wsdir.path().to_path_buf(),
            ignore_filter: Arc::new(IgnoreFilter::new(wsdir.path()).unwrap()),
            queue: queue.clone(),
            watch_rx,
            ack_rx,
            shutdown_rx,
            ws_tx_slot: ws_slot,
        };
        let handle = tokio::spawn(async move { run_push_worker(worker).await });

        // Two pushes should hit the wire (PushFile + DeleteFile).
        let mut seen_paths: Vec<String> = Vec::new();
        for _ in 0..2 {
            let frame = timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("push frame")
                .expect("channel open");
            match frame {
                OutFrame::Text(ClientMsg::WorkspacePushFile { path, content, .. }) => {
                    assert_eq!(path, "a.rs");
                    assert_eq!(content, b"v1");
                    seen_paths.push(path);
                }
                OutFrame::Text(ClientMsg::WorkspaceDeleteFile { path, .. }) => {
                    assert_eq!(path, "b.rs");
                    seen_paths.push(path);
                }
                other => panic!("unexpected frame: {:?}", text_kind(&other)),
            }
        }
        assert_eq!(seen_paths.len(), 2);

        // Ack both. The worker should dequeue each.
        ack_tx
            .send(AckMsg {
                path: "a.rs".into(),
                ok: true,
                error: None,
            })
            .await
            .unwrap();
        ack_tx
            .send(AckMsg {
                path: "b.rs".into(),
                ok: true,
                error: None,
            })
            .await
            .unwrap();

        // Wait until the queue shrinks to just bob's row.
        let mut depth = queue.len().await.unwrap();
        for _ in 0..40 {
            if depth == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            depth = queue.len().await.unwrap();
        }
        assert_eq!(depth, 1, "expected only bob's row to remain");

        drop(_shutdown_tx);
        let _ = timeout(Duration::from_secs(2), handle).await;
    }

    fn text_kind(f: &OutFrame) -> &'static str {
        match f {
            OutFrame::Text(_) => "text",
            OutFrame::Binary(_) => "binary",
        }
    }
}
