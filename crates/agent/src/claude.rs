use crate::config::ClaudeConfig;
use crate::tunnel::{ClientMsg, ServerMsg};
use dashmap::DashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use uuid::Uuid;

const STDOUT_LINE_LIMIT: usize = 10 * 1024 * 1024;

pub struct SessionManager {
    pub config: ClaudeConfig,
    sessions: Arc<DashMap<Uuid, Arc<Mutex<SessionState>>>>,
}

#[derive(Debug)]
struct SessionState {
    workspace: String,
    cwd: PathBuf,
    claude_session_id: Option<String>,
    active_pid: Option<u32>,
}

impl SessionManager {
    pub fn new(config: ClaudeConfig) -> Self {
        Self {
            config,
            sessions: Arc::new(DashMap::new()),
        }
    }

    /// Dispatch one frame coming off the WS read loop.
    pub async fn handle(self: &Arc<Self>, msg: ServerMsg, tx: mpsc::Sender<ClientMsg>) {
        match msg {
            ServerMsg::SessionStart {
                session_id,
                workspace,
            } => self.start_session(session_id, workspace, tx).await,
            ServerMsg::SessionInput {
                session_id,
                content,
                resume,
            } => {
                let mgr = self.clone();
                tokio::spawn(async move {
                    mgr.run_turn(session_id, content, resume, tx).await;
                });
            }
            ServerMsg::SessionInterrupt { session_id } => {
                self.interrupt(session_id, tx).await;
            }
            ServerMsg::SessionSwitchWorkspace {
                session_id,
                workspace,
            } => {
                let mgr = self.clone();
                tokio::spawn(async move {
                    mgr.switch_workspace(session_id, workspace, tx).await;
                });
            }
            ServerMsg::SessionStop { session_id } => {
                self.stop_session(session_id, tx).await;
            }
            ServerMsg::WorkspaceList { request_id } => {
                self.workspace_list(request_id, tx).await;
            }
            ServerMsg::WorkspaceCreate { request_id, name } => {
                self.workspace_create(request_id, name, tx).await;
            }
            ServerMsg::WorkspaceDelete { request_id, name } => {
                self.workspace_delete(request_id, name, tx).await;
            }
            ServerMsg::Welcome { .. } | ServerMsg::Rejected { .. } | ServerMsg::Ping => {
                // Handshake + heartbeat frames are dealt with in ws.rs.
            }
        }
    }

    async fn start_session(
        self: &Arc<Self>,
        session_id: Uuid,
        workspace: String,
        tx: mpsc::Sender<ClientMsg>,
    ) {
        if let Err(e) = validate_workspace_name(&workspace) {
            let _ = tx
                .send(ClientMsg::SessionError {
                    session_id,
                    message: e,
                })
                .await;
            return;
        }
        let cwd = self.workspace_root().join(&workspace);
        if let Err(e) = tokio::fs::create_dir_all(&cwd).await {
            let _ = tx
                .send(ClientMsg::SessionError {
                    session_id,
                    message: format!("create workspace: {}", e),
                })
                .await;
            return;
        }
        self.sessions.insert(
            session_id,
            Arc::new(Mutex::new(SessionState {
                workspace: workspace.clone(),
                cwd: cwd.clone(),
                claude_session_id: None,
                active_pid: None,
            })),
        );
        let _ = tx
            .send(ClientMsg::SessionOpened {
                session_id,
                workspace,
                cwd: cwd.display().to_string(),
            })
            .await;
    }

    async fn run_turn(
        self: Arc<Self>,
        session_id: Uuid,
        content: String,
        resume_from_client: Option<String>,
        tx: mpsc::Sender<ClientMsg>,
    ) {
        // Resolve session.
        let Some(state_arc) = self.sessions.get(&session_id).map(|e| e.value().clone()) else {
            let _ = tx
                .send(ClientMsg::SessionError {
                    session_id,
                    message: "unknown session".into(),
                })
                .await;
            return;
        };

        // Reject if a turn is already running.
        if state_arc.lock().unwrap().active_pid.is_some() {
            let _ = tx
                .send(ClientMsg::SessionError {
                    session_id,
                    message: "turn already in progress".into(),
                })
                .await;
            return;
        }

        let (cwd, claude_session_id_local) = {
            let s = state_arc.lock().unwrap();
            (s.cwd.clone(), s.claude_session_id.clone())
        };
        // Honour client-supplied resume if it agrees with our cached id, else
        // use our cached id (defensive — protects against client confusion).
        let resume = claude_session_id_local.or(resume_from_client);

        let mut cmd = Command::new(&self.config.executable);
        cmd.args([
            "-p",
            "--output-format",
            "stream-json",
            "--input-format",
            "stream-json",
            "--permission-mode",
            "bypassPermissions",
            "--verbose",
        ]);
        if let Some(id) = &resume {
            cmd.args(["--resume", id]);
        }
        for arg in &self.config.extra_args {
            cmd.arg(arg);
        }
        for (k, _) in std::env::vars() {
            if k.starts_with("CLAUDECODE") || k.starts_with("CLAUDE_CODE_") {
                cmd.env_remove(&k);
            }
        }
        cmd.current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = tx
                    .send(ClientMsg::SessionTurnEnded {
                        session_id,
                        exit_code: -1,
                        error: Some(format!("spawn {}: {}", self.config.executable.display(), e)),
                    })
                    .await;
                return;
            }
        };

        let pid = child.id();
        state_arc.lock().unwrap().active_pid = pid;

        // stdin: one user frame, then EOF.
        let payload = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{ "type": "text", "text": content }],
            },
        });
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin
                .write_all(serde_json::to_string(&payload).unwrap().as_bytes())
                .await;
            let _ = stdin.write_all(b"\n").await;
            let _ = stdin.flush().await;
            // Drop stdin so claude knows no more input is coming.
        }

        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");

        let tx_out = tx.clone();
        let state_for_init = state_arc.clone();
        let stdout_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut buf = Vec::with_capacity(8 * 1024);
            let mut init_done = false;
            loop {
                buf.clear();
                let n = match read_line_limited(&mut reader, &mut buf, STDOUT_LINE_LIMIT).await {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) => {
                        tracing::warn!(error = %e, session = %session_id, "stdout read error");
                        break;
                    }
                };
                let mut line = String::from_utf8_lossy(&buf[..n]).into_owned();
                if line.ends_with('\n') {
                    line.pop();
                    if line.ends_with('\r') {
                        line.pop();
                    }
                }
                if line.is_empty() {
                    continue;
                }
                if !init_done {
                    if let Some(claude_session_id) = extract_init_session_id(&line) {
                        state_for_init.lock().unwrap().claude_session_id =
                            Some(claude_session_id.clone());
                        let _ = tx_out
                            .send(ClientMsg::SessionTurnStarted {
                                session_id,
                                claude_session_id,
                            })
                            .await;
                        init_done = true;
                    }
                }
                if tx_out
                    .send(ClientMsg::SessionEvent {
                        session_id,
                        event: line,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        let stderr_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                tracing::debug!(session = %session_id, stderr = %line);
            }
        });

        let status = child.wait().await;
        let _ = stdout_task.await;
        let _ = stderr_task.await;

        state_arc.lock().unwrap().active_pid = None;

        let (exit_code, error) = match status {
            Ok(s) => (s.code().unwrap_or(-1), None),
            Err(e) => (-1, Some(format!("wait: {}", e))),
        };
        let _ = tx
            .send(ClientMsg::SessionTurnEnded {
                session_id,
                exit_code,
                error,
            })
            .await;
    }

    async fn interrupt(&self, session_id: Uuid, tx: mpsc::Sender<ClientMsg>) {
        let pid = self
            .sessions
            .get(&session_id)
            .and_then(|e| e.value().lock().unwrap().active_pid);
        let Some(pid) = pid else {
            let _ = tx
                .send(ClientMsg::SessionError {
                    session_id,
                    message: "no active turn to interrupt".into(),
                })
                .await;
            return;
        };
        send_signal(pid, libc::SIGINT);
    }

    async fn switch_workspace(
        self: Arc<Self>,
        session_id: Uuid,
        workspace: String,
        tx: mpsc::Sender<ClientMsg>,
    ) {
        if let Err(e) = validate_workspace_name(&workspace) {
            let _ = tx
                .send(ClientMsg::SessionError {
                    session_id,
                    message: e,
                })
                .await;
            return;
        }
        let Some(state_arc) = self.sessions.get(&session_id).map(|e| e.value().clone()) else {
            let _ = tx
                .send(ClientMsg::SessionError {
                    session_id,
                    message: "unknown session".into(),
                })
                .await;
            return;
        };
        // Interrupt any active turn first.
        let pid = state_arc.lock().unwrap().active_pid;
        if let Some(pid) = pid {
            send_signal(pid, libc::SIGTERM);
            // Poll until the turn task clears active_pid (claude exits).
            for _ in 0..50 {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                if state_arc.lock().unwrap().active_pid.is_none() {
                    break;
                }
            }
        }
        let cwd = self.workspace_root().join(&workspace);
        if let Err(e) = tokio::fs::create_dir_all(&cwd).await {
            let _ = tx
                .send(ClientMsg::SessionError {
                    session_id,
                    message: format!("create workspace: {}", e),
                })
                .await;
            return;
        }
        {
            let mut s = state_arc.lock().unwrap();
            s.workspace = workspace.clone();
            s.cwd = cwd.clone();
            s.claude_session_id = None;
        }
        let _ = tx
            .send(ClientMsg::SessionWorkspaceSwitched {
                session_id,
                workspace,
                cwd: cwd.display().to_string(),
            })
            .await;
    }

    async fn stop_session(&self, session_id: Uuid, tx: mpsc::Sender<ClientMsg>) {
        if let Some((_, state_arc)) = self.sessions.remove(&session_id) {
            if let Some(pid) = state_arc.lock().unwrap().active_pid {
                send_signal(pid, libc::SIGTERM);
            }
        }
        let _ = tx
            .send(ClientMsg::SessionClosed {
                session_id,
                reason: None,
            })
            .await;
    }

    async fn workspace_list(&self, request_id: Uuid, tx: mpsc::Sender<ClientMsg>) {
        let root = self.workspace_root();
        let _ = tokio::fs::create_dir_all(&root).await;
        let mut items = Vec::new();
        let mut error: Option<String> = None;
        match tokio::fs::read_dir(&root).await {
            Ok(mut rd) => loop {
                match rd.next_entry().await {
                    Ok(Some(entry)) => {
                        let Ok(ft) = entry.file_type().await else {
                            continue;
                        };
                        if !ft.is_dir() {
                            continue;
                        }
                        let Some(name) = entry.file_name().to_str().map(String::from) else {
                            continue;
                        };
                        if name.starts_with('.') {
                            continue;
                        }
                        items.push(name);
                    }
                    Ok(None) => break,
                    Err(e) => {
                        error = Some(format!("read_dir: {}", e));
                        break;
                    }
                }
            },
            Err(e) => error = Some(format!("read_dir: {}", e)),
        }
        items.sort();
        let _ = tx
            .send(ClientMsg::WorkspaceListResult {
                request_id,
                items,
                error,
            })
            .await;
    }

    async fn workspace_create(&self, request_id: Uuid, name: String, tx: mpsc::Sender<ClientMsg>) {
        let error = match validate_workspace_name(&name) {
            Err(e) => Some(e),
            Ok(()) => {
                let dir = self.workspace_root().join(&name);
                if dir.exists() {
                    Some(format!("workspace '{}' already exists", name))
                } else {
                    tokio::fs::create_dir_all(&dir)
                        .await
                        .err()
                        .map(|e| format!("create: {}", e))
                }
            }
        };
        let _ = tx
            .send(ClientMsg::WorkspaceCreateResult { request_id, error })
            .await;
    }

    async fn workspace_delete(&self, request_id: Uuid, name: String, tx: mpsc::Sender<ClientMsg>) {
        let error = match validate_workspace_name(&name) {
            Err(e) => Some(e),
            Ok(()) => {
                // Refuse if any session currently owns this workspace.
                let in_use = self
                    .sessions
                    .iter()
                    .any(|s| s.value().lock().unwrap().workspace == name);
                if in_use {
                    Some(format!("workspace '{}' is in use by an open session", name))
                } else {
                    let dir = self.workspace_root().join(&name);
                    if !dir.exists() {
                        Some(format!("workspace '{}' does not exist", name))
                    } else {
                        tokio::fs::remove_dir_all(&dir)
                            .await
                            .err()
                            .map(|e| format!("remove: {}", e))
                    }
                }
            }
        };
        let _ = tx
            .send(ClientMsg::WorkspaceDeleteResult { request_id, error })
            .await;
    }

    fn workspace_root(&self) -> PathBuf {
        expand_path(&self.config.workspace_root)
    }
}

fn extract_init_session_id(line: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let kind = v.get("type")?.as_str()?;
    let subtype = v.get("subtype")?.as_str()?;
    if kind == "system" && subtype == "init" {
        v.get("session_id")?.as_str().map(String::from)
    } else {
        None
    }
}

fn validate_workspace_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 63 {
        return Err("workspace name must be 1..=63 chars".into());
    }
    if name.starts_with('-') || name.starts_with('.') {
        return Err("workspace name cannot start with '-' or '.'".into());
    }
    for c in name.chars() {
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_') {
            return Err(format!(
                "invalid char '{}' in workspace name; allowed: lowercase a-z, 0-9, '-', '_'",
                c
            ));
        }
    }
    Ok(())
}

fn expand_path(p: &std::path::Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    p.to_path_buf()
}

fn send_signal(pid: u32, sig: i32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, sig);
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        let _ = sig;
    }
}

async fn read_line_limited<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    limit: usize,
) -> std::io::Result<usize> {
    let mut total = 0;
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            return Ok(total);
        }
        if let Some(idx) = chunk.iter().position(|&b| b == b'\n') {
            let take = idx + 1;
            buf.extend_from_slice(&chunk[..take]);
            reader.consume(take);
            total += take;
            return Ok(total);
        }
        let take = chunk.len().min(limit.saturating_sub(total));
        buf.extend_from_slice(&chunk[..take]);
        reader.consume(take);
        total += take;
        if total >= limit {
            return Err(std::io::Error::other("stream-json line exceeds 10 MiB"));
        }
    }
}
