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
        let cwd = self
            .workspace_root()
            .join(&handle.account)
            .join(&handle.workspace);

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

    fn workspace_root(&self) -> PathBuf {
        expand_path(&self.claude.workspace_root)
    }

    fn account_root(&self, account: &str) -> PathBuf {
        self.workspace_root().join(account)
    }
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
}
