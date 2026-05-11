use crate::config::{ClaudeConfig, RecordingConfig, TmuxConfig};
use crate::tunnel::{pack_pty_frame, ClientMsg, ServerMsg, TAG_PTY_OUTPUT};
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
    sessions: Arc<DashMap<Uuid, Arc<PtyHandle>>>,
}

struct PtyHandle {
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    writer: Mutex<Box<dyn Write + Send>>,
}

impl PtyManager {
    pub fn new(claude: ClaudeConfig, tmux: TmuxConfig, recording: RecordingConfig) -> Result<Self> {
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

        Ok(Self {
            claude,
            tmux: TmuxConfig {
                executable: tmux_path,
            },
            recording,
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
            } => {
                self.open_session(session_id, account, workspace, cols, rows, tx)
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

    async fn open_session(
        self: &Arc<Self>,
        session_id: Uuid,
        account: String,
        workspace: String,
        cols: u16,
        rows: u16,
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
        // else create"; the workspace becomes a persistent slot.
        let session_name = format!("cloudcode-{}-{}", account, workspace);
        let mut cmd = CommandBuilder::new(&self.tmux.executable);
        cmd.arg("new-session");
        cmd.arg("-A");
        cmd.arg("-s");
        cmd.arg(&session_name);
        cmd.arg("-x");
        cmd.arg(cols.to_string());
        cmd.arg("-y");
        cmd.arg(rows.to_string());
        cmd.cwd(&cwd);
        // The command to run on first create. tmux ignores this when attaching
        // an existing session, so a reconnect just gets back to whatever
        // state claude was in.
        cmd.arg(&self.claude.executable);
        for arg in &self.claude.extra_args {
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

        let master = Arc::new(Mutex::new(pair.master));
        let handle = Arc::new(PtyHandle {
            master,
            writer: Mutex::new(writer),
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
                let mut items = Vec::new();
                let mut error: Option<String> = None;
                match std::fs::read_dir(&root) {
                    Ok(rd) => {
                        for entry in rd.flatten() {
                            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                                if let Some(n) = entry.file_name().to_str().map(String::from) {
                                    if !n.starts_with('.') {
                                        items.push(n);
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => error = Some(format!("read_dir: {}", e)),
                }
                items.sort();
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
                    let _ = std::process::Command::new(&self.tmux.executable)
                        .args([
                            "kill-session",
                            "-t",
                            &format!("cloudcode-{}-{}", account, name),
                        ])
                        .output();
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
