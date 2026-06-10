//! Raw PTY relay: stdin bytes → hub; hub binary → stdout.
//!
//! Bytes from `crate::input::spawn_byte_reader` are forwarded verbatim, so
//! every terminal escape sequence (DA1/DA2 responses, cursor position
//! reports, mouse events, anything claude's UI library queries) reaches
//! the remote PTY exactly as the terminal produced it.

use crate::auth_gate;
use crate::cc_browser;
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
) -> Result<RelayOutcome> {
    relay_loop(wire, bytes, agent, workspace).await
}

async fn relay_loop(
    wire: &mut Wire,
    bytes: &mut ByteRx,
    agent: &str,
    workspace: &str,
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

    // In-session browser MCP channel (M1). Lazily started on the first
    // `BrowserRpc` frame from the hub; output frames flow back to the
    // hub via `browser_out_rx` (forwarded in the select! arm below).
    let mut browser: Option<cc_browser::BrowserChannel> = None;
    let mcp_cmd = cc_browser::mcp_command();
    let (browser_out_tx, mut browser_out_rx) = tokio::sync::mpsc::channel::<String>(64);

    // Session-scoped authorization gate (M2). The first browser frame
    // prompts the user with an inline consent pill; an approval then
    // rides a sliding idle window (default 10 min, configurable via
    // CC_BROWSER_IDLE_TIMEOUT_SECS) before the next frame re-prompts.
    let idle_timeout = std::time::Duration::from_secs(
        std::env::var("CC_BROWSER_IDLE_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(600),
    );
    let mut gate = auth_gate::AuthGate::new(idle_timeout);

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
                    HubToClient::BrowserRpc { payload } => {
                        // M2 authorization gate: a live grant slides its
                        // idle window; otherwise block on the inline
                        // consent pill before touching the subprocess.
                        // While the prompt awaits, the other select! arms
                        // are parked — PTY output buffers in its channel
                        // and flushes once the modal closes.
                        let now = std::time::Instant::now();
                        let allowed = match gate.check(now) {
                            auth_gate::Decision::Allow => {
                                gate.touch(now);
                                true
                            }
                            auth_gate::Decision::AskUser => {
                                if prompt_browser_consent(bytes).await {
                                    gate.grant(std::time::Instant::now());
                                    true
                                } else {
                                    gate.deny();
                                    false
                                }
                            }
                        };
                        if !allowed {
                            // Reap the subprocess (if one was running)
                            // and tell the agent side so claude gets a
                            // clean JSON-RPC error instead of waiting
                            // out the endpoint timeout.
                            browser = None;
                            let _ = wire.out_tx
                                .send(OutFrame::Text(ClientToHub::BrowserClosed {
                                    reason: Some("denied by user".to_string()),
                                }))
                                .await;
                        } else {
                            if browser.is_none() {
                                if let Some((prog, args)) = mcp_cmd.clone() {
                                    let argrefs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
                                    match cc_browser::BrowserChannel::start(&prog, &argrefs, browser_out_tx.clone()) {
                                        Ok(ch) => browser = Some(ch),
                                        Err(e) => tracing::warn!(
                                            error = %e,
                                            program = %prog,
                                            args = ?args,
                                            "failed to start browser MCP subprocess"
                                        ),
                                    }
                                }
                            }
                            if let Some(ch) = browser.as_ref() {
                                if ch.feed(payload).is_err() {
                                    browser = None;
                                    let _ = wire.out_tx
                                        .send(OutFrame::Text(ClientToHub::BrowserClosed {
                                            reason: Some("browser subprocess unavailable".to_string()),
                                        }))
                                        .await;
                                }
                            }
                        }
                    }
                    HubToClient::BrowserClosed { .. } => {
                        browser = None; // drop -> kill_on_drop reaps subprocess
                    }
                    _ => {}
                }
            }
            out = browser_out_rx.recv() => {
                // None is unreachable while relay_loop holds browser_out_tx;
                // guard on `browser` so frames queued before a teardown are
                // dropped instead of being sent after BrowserClosed.
                if let Some(payload) = out {
                    if browser.is_some() {
                        let _ = wire.out_tx
                            .send(OutFrame::Text(ClientToHub::BrowserRpc { payload }))
                            .await;
                    }
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

/// Draw an inline consent pill over the live PTY screen and block on a
/// y/n answer read from the raw stdin stream. The relay's other select!
/// arms are parked while this awaits — inbound PTY output buffers in
/// its channel and flushes after the modal closes.
///
/// No local timeout: we wait indefinitely. The agent endpoint times the
/// in-flight MCP request out at 25s, so claude sees a clean timeout
/// error while the pill stays up — the next `BrowserRpc` frame lands in
/// `AskUser` again and re-arms the same prompt.
///
/// Every keystroke other than y/Y/n/N/Esc is swallowed; nothing typed
/// while the modal is up is forwarded to the PTY.
///
/// Returns true = approved, false = denied (n/N/Esc, or stdin closing —
/// the relay loop's own recv arm then sees the closed channel on its
/// next iteration and exits cleanly).
async fn prompt_browser_consent(bytes: &mut ByteRx) -> bool {
    let mut stdout = std::io::stdout();
    // BEL to ring attention, save the cursor (DECSC), and hide it while
    // the pill is up — same hide/show pairing as main.rs
    // show_pill()/clear_pill(), but scoped to one line instead of a
    // full-screen repaint.
    let _ = stdout.write_all(b"\x07\x1b7\x1b[?25l");
    // Fixed line near the top (row 2), cleared first so the pill sits on
    // a clean background; bold yellow like show_pill's title line, reset
    // after (show_pill's `\x1b[{row};1H … \x1b[0m` convention).
    let text = "云端任务请求操作你的浏览器 — 允许? [y]允许 / [n]拒绝";
    let _ = write!(stdout, "\x1b[2;1H\x1b[2K\x1b[1;33m  {text}  \x1b[0m");
    let _ = stdout.flush();

    let mut in_paste = false;
    let approved = loop {
        let Some(chunk) = bytes.recv().await else { break false };
        match scan_consent_chunk(&chunk, &mut in_paste) {
            ConsentScan::Approve => break true,
            ConsentScan::Deny => break false,
            ConsentScan::Ignore => {} // swallowed — never forwarded to the PTY
        }
    };

    // Clear the pill line, restore the cursor (DECRC), show it again.
    let _ = stdout.write_all(b"\x1b[2;1H\x1b[2K\x1b8\x1b[?25h");
    let _ = stdout.flush();
    approved
}

/// What a stdin chunk means for the consent prompt.
#[derive(Debug, PartialEq, Eq)]
enum ConsentScan {
    Approve,
    Deny,
    Ignore,
}

/// Classify one raw stdin chunk for the consent prompt. Conservative by
/// design: only a deliberate, bare keypress may answer.
///
/// Threat model: bytes that merely *contain* y/n must never answer a
/// consent prompt. A bracketed paste can carry any text ("yes please" in
/// pasted prose would silently APPROVE browser access), and terminals
/// emit unsolicited replies — e.g. a DSR status report `ESC [ 0 n`
/// contains `n` and would spuriously DENY. So:
///
/// - Bracketed-paste regions (`ESC [ 200~` … `ESC [ 201~`) are tracked
///   across chunks via `in_paste` and swallowed wholesale.
/// - Any chunk starting with ESC is an escape sequence (CSI/SS3, a
///   terminal reply, arrow key, paste marker) and is swallowed — never
///   byte-scanned for letters. The single exception: a chunk that is
///   exactly one ESC byte is the Esc key → Deny.
/// - Plain chunks answer only when short (<= 4 bytes, a normal keypress
///   chunk); longer plain blobs are unbracketed pastes → swallowed.
fn scan_consent_chunk(chunk: &[u8], in_paste: &mut bool) -> ConsentScan {
    const PASTE_START: &[u8] = b"\x1b[200~";
    const PASTE_END: &[u8] = b"\x1b[201~";

    // Inside a paste: swallow everything up to (and including) the end
    // marker; content after the end marker in the same chunk is still an
    // ESC-led tail or paste residue — swallow the whole chunk either way.
    if *in_paste {
        if chunk.windows(PASTE_END.len()).any(|w| w == PASTE_END) {
            *in_paste = false;
        }
        return ConsentScan::Ignore;
    }

    // A paste may open anywhere in a chunk (the terminal can coalesce
    // typed bytes with a paste). If it opens without closing in the same
    // chunk, remember we're inside one — and swallow the chunk.
    if let Some(start) = chunk
        .windows(PASTE_START.len())
        .position(|w| w == PASTE_START)
    {
        let after = &chunk[start + PASTE_START.len()..];
        *in_paste = !after.windows(PASTE_END.len()).any(|w| w == PASTE_END);
        return ConsentScan::Ignore;
    }

    if chunk.first() == Some(&0x1b) {
        // Bare Esc keypress.
        if chunk == [0x1b] {
            return ConsentScan::Deny;
        }
        // Any longer ESC-led chunk is an escape sequence (CSI/SS3, a
        // terminal reply like DSR `ESC [ 0 n`, arrow key) — swallow it,
        // never byte-scan it for answer letters.
        return ConsentScan::Ignore;
    }

    // Plain chunk: only a short, keypress-sized chunk may answer. Longer
    // plain chunks are unbracketed paste blobs — swallow them.
    if chunk.len() <= 4 {
        for b in chunk {
            match b {
                b'y' | b'Y' => return ConsentScan::Approve,
                b'n' | b'N' => return ConsentScan::Deny,
                _ => {}
            }
        }
    }
    ConsentScan::Ignore
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

#[cfg(test)]
mod tests {
    use super::{scan_consent_chunk, ConsentScan};

    fn scan(chunk: &[u8]) -> ConsentScan {
        let mut in_paste = false;
        scan_consent_chunk(chunk, &mut in_paste)
    }

    #[test]
    fn bare_keypresses_answer() {
        assert_eq!(scan(b"y"), ConsentScan::Approve);
        assert_eq!(scan(b"N"), ConsentScan::Deny);
        assert_eq!(scan(b"\x1b"), ConsentScan::Deny); // bare Esc
    }

    #[test]
    fn dsr_terminal_reply_is_ignored() {
        // DSR status report contains `n` but must not deny.
        assert_eq!(scan(b"\x1b[0n"), ConsentScan::Ignore);
    }

    #[test]
    fn self_contained_paste_is_ignored() {
        // Paste containing `y` must not approve.
        assert_eq!(scan(b"\x1b[200~hey yes\x1b[201~"), ConsentScan::Ignore);
    }

    #[test]
    fn split_paste_swallows_all_chunks_then_keypress_answers() {
        let mut in_paste = false;
        assert_eq!(
            scan_consent_chunk(b"\x1b[200~hello y", &mut in_paste),
            ConsentScan::Ignore
        );
        assert!(in_paste);
        assert_eq!(
            scan_consent_chunk(b"more\x1b[201~", &mut in_paste),
            ConsentScan::Ignore
        );
        assert!(!in_paste);
        assert_eq!(scan_consent_chunk(b"y", &mut in_paste), ConsentScan::Approve);
    }

    #[test]
    fn long_plain_chunk_is_ignored() {
        // Unbracketed paste blob: contains `y` but is not a keypress.
        assert_eq!(scan(b"yes please"), ConsentScan::Ignore);
    }

    #[test]
    fn short_plain_chunk_without_answer_key_is_ignored() {
        assert_eq!(scan(b"ab"), ConsentScan::Ignore);
    }
}
