use crate::config::{ClaudeConfig, RecordingConfig, SandboxConfig, TmuxConfig};
use crate::tunnel::{
    pack_pty_frame, ClientMsg, ServerMsg, WorkspaceFullItem, WorkspaceItem, TAG_PTY_OUTPUT,
};
use anyhow::{Context, Result};
use chrono::SecondsFormat;
use dashmap::DashMap;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
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
    tmux: TmuxConfig,
    recording: RecordingConfig,
    /// When the workspace sandbox is enabled at startup, this holds the
    /// path to the running cloudcode-agent binary so the PTY spawn path
    /// can re-invoke us with the `sandbox-exec` subcommand. `None` means
    /// "sandbox disabled, exec tmux directly".
    self_exe: Option<PathBuf>,
    sessions: Arc<DashMap<Uuid, Arc<PtyHandle>>>,
}

struct PtyHandle {
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    writer: Mutex<Box<dyn Write + Send>>,
    /// Stops the jsonl watcher task on drop. Held only for its
    /// Drop side-effect.
    _jsonl: crate::jsonl::WatcherHandle,
}

impl PtyManager {
    pub fn new(
        claude: ClaudeConfig,
        tmux: TmuxConfig,
        recording: RecordingConfig,
        sandbox: SandboxConfig,
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

        let self_exe = if sandbox.enabled {
            if !crate::sandbox::is_supported() {
                return Err(anyhow::anyhow!(
                    "[sandbox] enabled = true in agent.toml, but the workspace sandbox \
                     is not implemented for this platform yet (macOS only at the moment). \
                     Set [sandbox] enabled = false to start the agent."
                ));
            }
            let p = std::env::current_exe().context(
                "locating the running cloudcode-agent binary for the sandbox wrapper",
            )?;
            tracing::info!(wrapper = %p.display(), "workspace sandbox enabled");
            Some(p)
        } else {
            None
        };

        Ok(Self {
            claude,
            tmux: TmuxConfig {
                executable: tmux_path,
            },
            recording,
            self_exe,
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
            } => {
                self.open_session(session_id, account, workspace, cols, rows, claude_args, tx)
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
        let mut cmd = if let Some(self_exe) = &self.self_exe {
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
        // Wrap claude in a small shell loop instead of execing it
        // directly. The semantics we want:
        //
        //   1. First boot: run `claude <args>` exactly as the CLI asked.
        //   2. When claude exits (/exit, Ctrl+C, crash): detach every
        //      attached tmux client so the cloudcode user pops straight
        //      back to the menu. The tmux session itself stays alive
        //      (the wrapper is still running), so the picker shows
        //      it as "saved".
        //   3. Sit in a polling sleep until somebody attaches again.
        //   4. On reattach, restart claude with `--continue` so it
        //      resumes the previous conversation from
        //      ~/.claude/projects/<cwd>/.
        //
        // Explicit cleanup (delete workspace) still goes through the
        // menu's `d` action, which kills the per-workspace tmux server
        // and tears the wrapper down with it.
        const WRAPPER: &str = r#"
first=1
sess="$(tmux display-message -p '#S' 2>/dev/null)"
while :; do
    if [ "$first" = "1" ]; then
        "$@"
        first=0
    else
        claude --continue
    fi
    # Disconnect every client attached to THIS session. We pass -s
    # explicitly because `detach-client -a` from a non-attached
    # caller (our wrapper bash is a server-side child, not a client)
    # has ambiguous semantics on some tmux versions.
    if [ -n "$sess" ]; then
        tmux detach-client -s "$sess" 2>/dev/null
    else
        tmux detach-client -a 2>/dev/null
    fi
    # Park until somebody reattaches, then respawn claude.
    while [ "$(tmux list-clients -t "$sess" -F . 2>/dev/null | wc -l)" -eq 0 ]; do
        sleep 1
    done
done
"#;
        cmd.arg("bash");
        cmd.arg("-c");
        cmd.arg(WRAPPER);
        // bash's $0 label, not used by the script.
        cmd.arg("cloudcode-claude");
        cmd.arg(&self.claude.executable);
        for arg in &self.claude.extra_args {
            cmd.arg(arg);
        }
        // Per-session args forwarded from the client (everything after `--`
        // on the cloudcode CLI). Only honoured for the first boot; after
        // that the wrapper switches to `claude --continue`.
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
