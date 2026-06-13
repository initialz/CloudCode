//! Raw PTY relay: stdin bytes → hub; hub binary → stdout.
//!
//! Bytes from `crate::input::spawn_byte_reader` are forwarded verbatim, so
//! every terminal escape sequence (DA1/DA2 responses, cursor position
//! reports, mouse events, anything claude's UI library queries) reaches
//! the remote PTY exactly as the terminal produced it.

use crate::input::ByteRx;
use crate::mouse_filter::MouseModeStripper;
use crate::paste_detect::{parse_paste_paths, wrap_as_paste, PasteDetector, PasteEvent};
use crate::proto::{ClientToHub, HubToClient};
use crate::wire::{OutFrame, Wire};
use anyhow::Result;
use base64::Engine;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::collections::HashMap;
use std::io::Write;
use tokio::sync::mpsc;
use uuid::Uuid;

/// Workspace-relative directory dropped files are uploaded into, shared
/// with Phase 1 (webterm) and the agent's auto-`mkdir` on write.
const UPLOAD_DIR: &str = ".cloudcode/uploads";
/// Upload chunk size — matches the hub's HTTP upload path (64 KiB
/// base64-encoded `FsWriteChunk` frames).
const UPLOAD_CHUNK: usize = 64 * 1024;

/// What ended the relay loop.
#[derive(Debug)]
pub enum RelayOutcome {
    /// Hub closed the session cleanly (`SessionClosed` frame, or `Close`
    /// from the local side). Caller should leave alt-screen + return to
    /// the menu.
    Closed,
    /// One of the wire channels went `None` — the underlying WS is dead.
    /// Caller is expected to attempt reconnect; terminal state is left
    /// untouched (still in alt-screen + raw mode) so a status banner can
    /// paint on top.
    HubLost,
}

/// Enter alt-screen + raw mode. Idempotent against an already-set
/// terminal in the sense that running it twice is harmless (the second
/// `?1049h` is a no-op when we're already on alt-screen).
pub fn enter_session_mode() -> Result<()> {
    enable_raw_mode()?;
    // (Approach A) Force every mouse-tracking mode OFF before we
    // hand the terminal over to the remote PTY stream. Reason: an
    // earlier cloudcode invocation (or a foreign TUI in the same
    // iTerm2 tab) might have crashed mid-session and left mouse
    // tracking enabled in the emulator's state. We then proceed to
    // strip every `?1000h` etc that the remote claude sends — but if
    // the emulator was already in mouse-tracking mode, our filter
    // keeps it there forever. The explicit `l` reset gives us a
    // clean baseline so subsequent strips actually "stick".
    //
    // Wipe the main screen + scrollback FIRST, then reset mouse,
    // then enter alt-screen and clear it. Background: claude (v2.x)
    // dumps its chat UI to main-screen scrollback when it exits, so
    // by the time a new cloudcode invocation enters alt-screen the
    // previous session's chat is sitting just above in the local
    // terminal's scrollback. iTerm2's default config keeps that
    // scrollback visible behind alt-screen, so the user perceives
    // the old chat "stacked on top of" the new one. Clearing main
    // + scrollback before entering alt-screen is the only escape-
    // only way to make the duplicate go away — the cost is the few
    // lines of shell history above where the user typed
    // `cloudcode`, which is an acceptable trade for a full-screen
    // TUI client.
    //
    //   [H        — cursor to top-left of main screen
    //   [2J       — erase the visible main-screen viewport
    //   [3J       — erase saved scrollback lines (xterm/iTerm/kitty)
    //   ?1000-1016l — reset every X11/SGR mouse-tracking variant
    //   ?47l etc  — ensure we're NOT in alt-screen (stay in main
    //               screen so native scrollback works; the filter
    //               also strips any alt-screen escapes from the
    //               agent stream)
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(
        b"\x1b[?1049l\x1b[?1047l\x1b[?47l\
          \x1b[H\x1b[2J\x1b[3J\
          \x1b[?1000l\x1b[?1001l\x1b[?1002l\x1b[?1003l\
          \x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1016l",
    );
    let _ = stdout.flush();
    Ok(())
}

/// Leave alt-screen + raw mode. Always called by the caller once the
/// reconnect loop gives up or the session ends cleanly.
pub fn leave_session_mode() {
    disable_raw_mode().ok();
    let mut stdout = std::io::stdout();
    // Best-effort reset of alt-screen / cursor / every mouse-tracking
    // variant so the next program in this iTerm2 tab inherits a
    // clean state.
    let _ = stdout.write_all(
        b"\x1b[?25h\
          \x1b[?1000l\x1b[?1001l\x1b[?1002l\x1b[?1003l\
          \x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1016l\r\n",
    );
    let _ = stdout.flush();
}

/// Run the raw PTY relay once. Caller must have already set up the
/// terminal via `enter_session_mode()`. Returns `RelayOutcome::Closed`
/// for clean exits and `RelayOutcome::HubLost` when the WS dies — that
/// distinction lets the outer loop decide whether to reconnect.
pub async fn run(
    wire: &mut Wire,
    bytes: &mut ByteRx,
    agent: &str,
    workspace: &str,
    browser: &crate::mcp_host::BrowserConfig,
) -> Result<RelayOutcome> {
    relay_loop(wire, bytes, agent, workspace, browser).await
}

async fn relay_loop(
    wire: &mut Wire,
    bytes: &mut ByteRx,
    agent: &str,
    workspace: &str,
    browser: &crate::mcp_host::BrowserConfig,
) -> Result<RelayOutcome> {
    // Strip mouse-mode CSI escapes from the agent → terminal stream
    // so the local emulator keeps doing its native drag-to-select /
    // Cmd+C copy. See mouse_filter.rs for the trade-off (claude's
    // mouse interactions go dark in exchange).
    let mut mouse_filter = MouseModeStripper::new();

    if let Some((cols, rows)) = current_terminal_size() {
        let _ = wire
            .out_tx
            .send(OutFrame::Text(ClientToHub::Resize { cols, rows }))
            .await;
    }
    let mut winch = spawn_winch_signal();

    // CLI file-drop (Phase 2) state.
    //
    // `detector` slices the stdin stream into normal passthrough vs.
    // complete bracketed pastes. A paste whose every token is an
    // existing local file is intercepted: we spawn an upload task
    // (so the relay's output arm is never blocked) which streams the
    // file(s) via FsWrite* frames on a cloned out_tx, then injects the
    // resulting `@…` mentions back through `inject_tx`. The relay's
    // text arm routes each `FsWriteResult` to the waiting task via the
    // per-request sender stored in `pending_uploads`.
    let mut detector = PasteDetector::new();
    let mut pending_uploads: HashMap<Uuid, mpsc::Sender<HubToClient>> = HashMap::new();
    let (inject_tx, mut inject_rx) = mpsc::channel::<Vec<u8>>(16);

    // 远程-MCP 宿主(Phase C)。后端命令:env CC_REMOTE_MCP_BACKEND >
    // [browser].backend > 内置 playwright-mcp 默认(决策 P1);None →
    // Hello 能力位为 false,hub/agent 不会给我们发 RemoteMcp 帧 ——
    // 万一异常发来,走下方防御性快速失败臂。
    // 注意:host_out_tx 在本作用域常驻(host 内只持 clone),保证
    // host_out_rx.recv() 永不返回 None 而空转。
    let (host_out_tx, mut host_out_rx) = tokio::sync::mpsc::channel::<String>(64);
    let mut mcp_host: Option<crate::mcp_host::McpHost> = crate::mcp_host::backend_command(browser)
        .map(|b| crate::mcp_host::McpHost::new(b, host_out_tx.clone(), None));

    loop {
        tokio::select! {
            chunk = bytes.recv() => {
                // ByteRx (stdin reader) ending is "user closed stdin",
                // not a hub failure — close cleanly so the outer loop
                // returns to the menu instead of reconnecting forever.
                let Some(chunk) = chunk else { return Ok(RelayOutcome::Closed); };
                for event in detector.feed(&chunk) {
                    match event {
                        PasteEvent::Passthrough(b) => {
                            if wire.out_tx.send(OutFrame::Binary(b)).await.is_err() {
                                return Ok(RelayOutcome::HubLost);
                            }
                        }
                        PasteEvent::Paste { content } => {
                            // Decide: is every token an existing local
                            // file? If so, intercept + upload. Otherwise
                            // forward the paste verbatim (normal paste).
                            let text = String::from_utf8_lossy(&content);
                            let tokens = parse_paste_paths(&text);
                            let is_file_drop = !tokens.is_empty()
                                && tokens.iter().all(|t| {
                                    std::fs::metadata(t).map(|m| m.is_file()).unwrap_or(false)
                                });
                            if is_file_drop {
                                spawn_upload(
                                    &wire.out_tx,
                                    &inject_tx,
                                    &mut pending_uploads,
                                    agent,
                                    workspace,
                                    tokens,
                                );
                            } else if wire
                                .out_tx
                                .send(OutFrame::Binary(wrap_as_paste(&content)))
                                .await
                                .is_err()
                            {
                                return Ok(RelayOutcome::HubLost);
                            }
                        }
                    }
                }
            }
            inject = inject_rx.recv() => {
                // Inject bytes produced by a finished upload task —
                // the `@…` mentions (or an inline failure note). Sent as
                // raw input on the same binary channel typed input uses.
                if let Some(b) = inject {
                    if wire.out_tx.send(OutFrame::Binary(b)).await.is_err() {
                        return Ok(RelayOutcome::HubLost);
                    }
                }
            }
            bin = wire.in_bin_rx.recv() => {
                let Some(bytes) = bin else { return Ok(RelayOutcome::HubLost); };
                let filtered = mouse_filter.filter(&bytes);
                let mut stdout = std::io::stdout();
                if stdout.write_all(&filtered).is_err() { return Ok(RelayOutcome::Closed); }
                if stdout.flush().is_err() { return Ok(RelayOutcome::Closed); }
            }
            text = wire.in_text_rx.recv() => {
                let Some(frame) = text else { return Ok(RelayOutcome::HubLost); };
                match frame {
                    HubToClient::Ping => {
                        let _ = wire.out_tx.send(OutFrame::Text(ClientToHub::Pong)).await;
                    }
                    HubToClient::SessionClosed { .. } => return Ok(RelayOutcome::Closed),
                    HubToClient::SessionError { message } => {
                        tracing::warn!(%message, "session error during relay");
                    }
                    HubToClient::FsWriteResult { request_id, .. } => {
                        // Route the result to its waiting upload task.
                        // Remove the entry once the task has it (each
                        // upload awaits exactly one result per request).
                        if let Some(tx) = pending_uploads.remove(&request_id) {
                            let _ = tx.send(frame).await;
                        }
                    }
                    HubToClient::RemoteMcp { server, payload } => {
                        if server != crate::mcp_host::CC_BROWSER_SERVER {
                            // 计划①只有一个插槽;未知 server 名立即回
                            // Closed,agent 把该会话在飞请求快速失败。
                            let _ = wire
                                .out_tx
                                .send(OutFrame::Text(ClientToHub::RemoteMcpClosed {
                                    server,
                                    reason: Some("unknown remote-MCP server".to_string()),
                                }))
                                .await;
                        } else if let Some(host) = mcp_host.as_mut() {
                            if let Err(e) = host.deliver(payload).await {
                                let _ = wire
                                    .out_tx
                                    .send(OutFrame::Text(ClientToHub::RemoteMcpClosed {
                                        server,
                                        reason: Some(e.to_string()),
                                    }))
                                    .await;
                            }
                        } else {
                            // 能力位为 false 仍收到帧:防御性快速失败。
                            let _ = wire
                                .out_tx
                                .send(OutFrame::Text(ClientToHub::RemoteMcpClosed {
                                    server,
                                    reason: Some(
                                        "no MCP backend configured (check [browser] in \
                                         config.toml or CC_REMOTE_MCP_BACKEND)"
                                            .to_string(),
                                    ),
                                }))
                                .await;
                        }
                    }
                    HubToClient::RemoteMcpClosed { .. } => {
                        if let Some(host) = mcp_host.as_mut() {
                            host.shutdown();
                        }
                    }
                    _ => {}
                }
            }
            out = host_out_rx.recv() => {
                // host_out_tx 常驻本作用域,recv 不会得 None;防御写法。
                if let Some(payload) = out {
                    let _ = wire
                        .out_tx
                        .send(OutFrame::Text(ClientToHub::RemoteMcp {
                            server: crate::mcp_host::CC_BROWSER_SERVER.to_string(),
                            payload,
                        }))
                        .await;
                }
            }
            _ = winch_tick(&mut winch) => {
                if let Some((cols, rows)) = current_terminal_size() {
                    let _ = wire
                        .out_tx
                        .send(OutFrame::Text(ClientToHub::Resize { cols, rows }))
                        .await;
                }
            }
        }
    }
}

/// Intercept a detected file drop: register one result channel per
/// file in `pending_uploads` and spawn a single task that uploads each
/// file in turn, then injects the resulting `@…` mentions. Runs off the
/// select loop so the relay's output arm is never blocked by an upload.
fn spawn_upload(
    out_tx: &mpsc::Sender<OutFrame>,
    inject_tx: &mpsc::Sender<Vec<u8>>,
    pending_uploads: &mut HashMap<Uuid, mpsc::Sender<HubToClient>>,
    agent: &str,
    workspace: &str,
    tokens: Vec<String>,
) {
    // Per-file request_id + a oneshot-style result channel registered
    // with the loop so the text arm can route each FsWriteResult here.
    let mut jobs: Vec<(Uuid, String, mpsc::Receiver<HubToClient>)> = Vec::new();
    for path in &tokens {
        let request_id = Uuid::new_v4();
        let (res_tx, res_rx) = mpsc::channel::<HubToClient>(1);
        pending_uploads.insert(request_id, res_tx);
        jobs.push((request_id, path.clone(), res_rx));
    }

    let out_tx = out_tx.clone();
    let inject_tx = inject_tx.clone();
    let agent = agent.to_string();
    let workspace = workspace.to_string();

    tokio::spawn(async move {
        let mut mentions: Vec<String> = Vec::new();
        for (request_id, path, res_rx) in jobs {
            let file_name = basename(&path);
            match upload_one_file(&out_tx, request_id, &agent, &workspace, &path, res_rx).await {
                Ok(final_name) => {
                    mentions.push(format!("@{UPLOAD_DIR}/{final_name}"));
                }
                Err(_) => {
                    // Inline failure note so the user sees it; other
                    // files in the batch still proceed.
                    mentions.push(format!("[upload failed: {file_name}]"));
                }
            }
        }
        if !mentions.is_empty() {
            // Space-joined with a trailing space, as raw input bytes —
            // the original local path is never forwarded.
            let inject = format!("{} ", mentions.join(" "));
            let _ = inject_tx.send(inject.into_bytes()).await;
        }
    });
}

/// Upload a single local file via the FsWrite* frames and await its
/// `FsWriteResult`. Returns the agent-reported final name on success.
async fn upload_one_file(
    out_tx: &mpsc::Sender<OutFrame>,
    request_id: Uuid,
    agent: &str,
    workspace: &str,
    path: &str,
    mut res_rx: mpsc::Receiver<HubToClient>,
) -> Result<String, ()> {
    let file_name = basename(path);
    let dest = format!("{UPLOAD_DIR}/{file_name}");

    if out_tx
        .send(OutFrame::Text(ClientToHub::FsWriteInit {
            request_id,
            agent: agent.to_string(),
            workspace: workspace.to_string(),
            path: dest,
        }))
        .await
        .is_err()
    {
        return Err(());
    }

    // Stream the file in 64 KiB base64 chunks, then a terminal eof.
    let data = std::fs::read(path).map_err(|_| ())?;
    for chunk in data.chunks(UPLOAD_CHUNK) {
        let data_b64 = base64::engine::general_purpose::STANDARD.encode(chunk);
        if out_tx
            .send(OutFrame::Text(ClientToHub::FsWriteChunk {
                request_id,
                data_b64,
                eof: false,
            }))
            .await
            .is_err()
        {
            return Err(());
        }
    }
    if out_tx
        .send(OutFrame::Text(ClientToHub::FsWriteChunk {
            request_id,
            data_b64: String::new(),
            eof: true,
        }))
        .await
        .is_err()
    {
        return Err(());
    }

    // Await the result routed in by the relay loop's text arm.
    match res_rx.recv().await {
        Some(HubToClient::FsWriteResult {
            final_name, error, ..
        }) => match (final_name, error) {
            (Some(name), None) => Ok(name),
            _ => Err(()),
        },
        _ => Err(()),
    }
}

/// Workspace-side leaf name for a local path (basename), used both for
/// the upload destination and the `@`-mention.
fn basename(path: &str) -> String {
    path.rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(path)
        .to_string()
}

fn current_terminal_size() -> Option<(u16, u16)> {
    crossterm::terminal::size().ok()
}

#[cfg(unix)]
struct WinchHandle {
    rx: mpsc::Receiver<()>,
}

#[cfg(unix)]
fn spawn_winch_signal() -> WinchHandle {
    let (tx, rx) = mpsc::channel::<()>(8);
    tokio::spawn(async move {
        let mut sig =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()) {
                Ok(s) => s,
                Err(_) => return,
            };
        loop {
            if sig.recv().await.is_none() {
                break;
            }
            if tx.send(()).await.is_err() {
                break;
            }
        }
    });
    WinchHandle { rx }
}

#[cfg(unix)]
async fn winch_tick(h: &mut WinchHandle) -> Option<()> {
    h.rx.recv().await
}

#[cfg(not(unix))]
struct WinchHandle;

#[cfg(not(unix))]
fn spawn_winch_signal() -> WinchHandle {
    WinchHandle
}

#[cfg(not(unix))]
async fn winch_tick(_: &mut WinchHandle) -> Option<()> {
    std::future::pending::<()>().await;
    None
}
