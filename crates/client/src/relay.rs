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
use std::sync::{Arc, Mutex};
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

    // MCP handshake cache, owned by the RELAY LOOP (M3 review MEDIUM-2),
    // not by any individual BrowserChannel: a channel teardown (consent
    // deny, BrowserClosed) must not lose the cached initialize +
    // notifications/initialized frames — claude never re-sends them on a
    // live connection, so a later lazy-respawn or cold-start handoff
    // needs this cache to replay into the fresh subprocess.
    let handshake_cache: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    // Last URL claude navigated to (M3 headed warmup): captured from
    // inbound `browser_navigate` tools/call frames so the handoff's
    // synthetic warmup call can reopen the page the human is meant to
    // see, instead of a blank window.
    let mut last_url: Option<String> = None;

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

    // In-flight request tracking (M3): id key → method, recorded when an
    // inbound frame is fed to the subprocess and removed when its
    // response comes back on `browser_out_rx`. Serves the tools/list
    // response rewrite (request_handoff injection) — the response itself
    // carries no method, so we correlate by id.
    let mut inflight: HashMap<String, String> = HashMap::new();

    // Login-page hint state (M3, notify-only heuristic).
    // `hint_shown` prevents re-displaying on every frame within the same
    // gate grant (reset when the gate re-prompts or channel tears down).
    // `hint_up` tracks whether the pill is currently painted so the next
    // outbound frame can clear it.
    let mut hint_shown: bool = false;
    let mut hint_up: bool = false;

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
                        // M2 authorization gate, method-aware: only ACTION
                        // frames (tools/call and anything unrecognized) need
                        // consent. Handshake/metadata (initialize, tools/list,
                        // ping, notifications, responses) flow freely — so the
                        // consent pill appears on claude's FIRST TOOL CALL,
                        // not at its MCP handshake during session boot.
                        //
                        // Consequence: the subprocess lazy-spawn below now
                        // happens on the first PASSIVE frame (initialize) —
                        // before any consent. That is fine and intended:
                        // spawning a local subprocess grants the cloud
                        // nothing; the gate guards ACTIONS. It also warms up
                        // npx before the first action.
                        //
                        // A live grant slides its idle window; otherwise we
                        // block on the inline consent pill before feeding the
                        // subprocess. While the prompt awaits, the other
                        // select! arms are parked — PTY output buffers in its
                        // channel and flushes once the modal closes.
                        let allowed = if method_is_passive(&payload) {
                            // Passive frames neither require nor extend a
                            // grant: don't touch the gate at all.
                            true
                        } else {
                            let now = std::time::Instant::now();
                            match gate.check(now) {
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
                            }
                        };
                        if !allowed {
                            // Reap the subprocess (if one was running)
                            // and tell the agent side so claude gets a
                            // clean JSON-RPC error instead of waiting
                            // out the endpoint timeout.
                            browser = None;
                            inflight.clear();
                            // Reset the login-page hint so it can fire
                            // again after the next grant.
                            hint_shown = false;
                            if hint_up { clear_pill(); hint_up = false; }
                            let _ = wire.out_tx
                                .send(OutFrame::Text(ClientToHub::BrowserClosed {
                                    reason: Some("denied by user".to_string()),
                                }))
                                .await;
                        } else if is_handoff_call(&payload) {
                            // request_handoff is served by the CLIENT
                            // itself, never by playwright-mcp — intercept
                            // AFTER the gate (it's a tools/call, so the
                            // consent gate above already vetted it) and
                            // INSTEAD of feeding the subprocess. Not
                            // tracked in `inflight`: no subprocess
                            // response will ever correlate to this id.
                            //
                            // While the handoff pills are up (and across
                            // the headed/headless restarts), this await
                            // parks every other select! arm — exactly
                            // like the consent pill. Inbound frames
                            // buffer in their channels and each re-enters
                            // the gate/handoff dispatch once we return.
                            run_handoff(
                                bytes,
                                &wire.out_tx,
                                &mut browser,
                                &browser_out_tx,
                                &mut browser_out_rx,
                                &handshake_cache,
                                &mut inflight,
                                last_url.as_deref(),
                                &payload,
                            )
                            .await;
                        } else {
                            if browser.is_none() {
                                match mcp_cmd.clone() {
                                    None => {
                                        // M2 review LOW-2: fast-fail so the agent fails
                                        // pending requests immediately (claude gets -32002
                                        // in milliseconds instead of waiting out 120s).
                                        tracing::warn!("browser MCP command unavailable; failing fast");
                                        let _ = wire.out_tx
                                            .send(OutFrame::Text(ClientToHub::BrowserClosed {
                                                reason: Some("browser subprocess unavailable".to_string()),
                                            }))
                                            .await;
                                    }
                                    Some((prog, args)) => {
                                        let argrefs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
                                        match cc_browser::BrowserChannel::start(&prog, &argrefs, browser_out_tx.clone(), handshake_cache.clone()) {
                                            Ok(ch) => {
                                                // M2 lazy-respawn gap (closed by M3 review
                                                // MEDIUM-2): when this spawn follows an earlier
                                                // teardown (deny / BrowserClosed), the fresh
                                                // subprocess has never seen claude's MCP
                                                // handshake — replay the cached one before the
                                                // current frame is fed. No-op on true first
                                                // boot (empty cache: the handshake frames are
                                                // exactly what is arriving now).
                                                if !handshake_cache.lock().expect("handshake cache mutex").is_empty() {
                                                    let _ = replay_handshake(
                                                        &ch,
                                                        &mut browser_out_rx,
                                                        &handshake_cache,
                                                    )
                                                    .await;
                                                }
                                                browser = Some(ch);
                                            }
                                            Err(e) => {
                                                // M2 review LOW-2: warn and fast-fail so the
                                                // agent fails pending requests immediately.
                                                tracing::warn!(
                                                    error = %e,
                                                    program = %prog,
                                                    args = ?args,
                                                    "failed to start browser MCP subprocess"
                                                );
                                                let _ = wire.out_tx
                                                    .send(OutFrame::Text(ClientToHub::BrowserClosed {
                                                        reason: Some("browser subprocess unavailable".to_string()),
                                                    }))
                                                    .await;
                                            }
                                        }
                                    }
                                }
                            }
                            // Track the last navigated URL (M3 headed
                            // warmup): the handoff's synthetic warmup
                            // call re-opens this page in the visible
                            // window so the human lands where claude
                            // left off.
                            if let Some(url) = extract_navigate_url(&payload) {
                                last_url = Some(url);
                            }
                            // Record id → method before feeding (feed
                            // consumes the payload); only keep the entry
                            // if the feed actually succeeded.
                            let inflight_entry = match (
                                extract_id_key(&payload),
                                extract_method(&payload),
                            ) {
                                (Some(id), Some(method)) => Some((id, method)),
                                _ => None, // notification or response: nothing to correlate
                            };
                            if let Some(ch) = browser.as_ref() {
                                if ch.feed(payload).is_err() {
                                    browser = None;
                                    inflight.clear();
                                    let _ = wire.out_tx
                                        .send(OutFrame::Text(ClientToHub::BrowserClosed {
                                            reason: Some("browser subprocess unavailable".to_string()),
                                        }))
                                        .await;
                                } else if let Some((id, method)) = inflight_entry {
                                    inflight.insert(id, method);
                                }
                            }
                        }
                    }
                    HubToClient::BrowserClosed { .. } => {
                        browser = None; // drop -> kill_on_drop reaps subprocess
                        inflight.clear(); // no responses will arrive for these
                        // Reset login-page hint so it can fire again next time
                        // the channel is re-established.
                        hint_shown = false;
                        if hint_up { clear_pill(); hint_up = false; }
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
                        // A straggler response to a client-synthesized warmup
                        // call (run_handoff timed out waiting and moved on):
                        // claude never issued that request — drop it instead
                        // of forwarding a frame the agent endpoint cannot
                        // correlate. (extract_id_key canonicalizes string ids
                        // with their quotes: "\"cc-warmup-1\"".)
                        if extract_id_key(&payload)
                            .is_some_and(|id| id.starts_with("\"cc-warmup-"))
                        {
                            continue;
                        }

                        // Clear any outstanding login-page hint pill before
                        // this new frame lands (auto-clears on next frame).
                        if hint_up {
                            clear_pill();
                            hint_up = false;
                        }

                        // Correlate the response to its request method via
                        // `inflight`; a tools/list reply gains the
                        // client-side request_handoff tool before claude
                        // sees it. Anything else forwards unchanged.
                        let payload = match extract_id_key(&payload)
                            .and_then(|id| inflight.remove(&id))
                        {
                            Some(m) if m == "tools/list" => inject_handoff_tool(payload),
                            _ => payload,
                        };

                        // Login-page hint (notify-only heuristic, M3).
                        // String-sniff for password-field indicators: not a
                        // judgment, just a prompt to the human that the page
                        // may need login. Once per gate grant (`hint_shown`);
                        // non-blocking — pill stays until the next frame.
                        if !hint_shown && looks_like_login_page(&payload) {
                            hint_shown = true;
                            draw_pill("页面似乎需要登录 — 可让 claude 调 request_handoff 交给你操作");
                            hint_up = true;
                        }

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
/// in-flight MCP request out (120s for tools/call — the only method
/// class that prompts), so claude sees a clean timeout error while the
/// pill stays up — the next action `BrowserRpc` frame lands in
/// `AskUser` again and re-arms the same prompt.
///
/// Every keystroke other than y/Y/n/N/Esc is swallowed; nothing typed
/// while the modal is up is forwarded to the PTY.
///
/// Returns true = approved, false = denied (n/N/Esc, or stdin closing —
/// the relay loop's own recv arm then sees the closed channel on its
/// next iteration and exits cleanly).
async fn prompt_browser_consent(bytes: &mut ByteRx) -> bool {
    prompt_pill(bytes, "云端任务请求操作你的浏览器 — 允许? [y]允许 / [n]拒绝").await
}

/// Generic y/n pill: draw `text`, block on a deliberate keypress answer
/// (same chunk discipline as the consent prompt), clear the pill, return
/// the answer. Shared by the browser consent prompt and the handoff
/// confirmation — the same parking semantics apply (see
/// `prompt_browser_consent`).
async fn prompt_pill(bytes: &mut ByteRx, text: &str) -> bool {
    draw_pill(text);
    let mut in_paste = false;
    let approved = loop {
        let Some(chunk) = bytes.recv().await else { break false };
        match scan_consent_chunk(&chunk, &mut in_paste) {
            ConsentScan::Approve => break true,
            ConsentScan::Deny => break false,
            ConsentScan::Ignore => {} // swallowed — never forwarded to the PTY
        }
    };
    clear_pill();
    approved
}

/// Pill that waits for a deliberate Enter keypress (handoff "human is
/// done" signal). Everything else — pastes, escape sequences, long
/// blobs — is swallowed via the same conservative chunk discipline as
/// the y/n pills. No client-side timeout: the agent endpoint's 600s
/// request_handoff timeout bounds the whole exchange; if it fires,
/// claude gets a clean timeout error while the human keeps the window.
/// Also returns when stdin closes (the relay loop's own recv arm then
/// sees the closed channel and exits cleanly) — we still proceed to
/// restore the headless browser in that case.
async fn wait_for_enter_pill(bytes: &mut ByteRx, text: &str) {
    draw_pill(text);
    let mut in_paste = false;
    loop {
        let Some(chunk) = bytes.recv().await else { break };
        if scan_for_enter(&chunk, &mut in_paste) {
            break;
        }
    }
    clear_pill();
}

/// Draw an inline pill: BEL to ring attention, save the cursor (DECSC),
/// hide it, then paint a bold-yellow line on row 2 — same hide/show
/// pairing as main.rs show_pill()/clear_pill(), but scoped to one line
/// instead of a full-screen repaint. Pair with `clear_pill`.
fn draw_pill(text: &str) {
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(b"\x07\x1b7\x1b[?25l");
    // Fixed line near the top (row 2), cleared first so the pill sits on
    // a clean background; bold yellow like show_pill's title line, reset
    // after (show_pill's `\x1b[{row};1H … \x1b[0m` convention).
    let _ = write!(stdout, "\x1b[2;1H\x1b[2K\x1b[1;33m  {text}  \x1b[0m");
    let _ = stdout.flush();
}

/// Clear the pill line, restore the cursor (DECRC), show it again.
fn clear_pill() {
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(b"\x1b[2;1H\x1b[2K\x1b8\x1b[?25h");
    let _ = stdout.flush();
}

/// The human-handoff interaction flow, run when an allowed
/// `request_handoff` tools/call arrives. The client itself is the
/// server for this tool: it answers the JSON-RPC request directly and
/// never feeds it to playwright-mcp.
///
/// 1. y/n pill with claude's reason. **n** → `-32003 user declined
///    handoff` (the consent gate's grant is untouched — declining one
///    handoff is not revoking browser consent).
/// 2. **y** → restart the subprocess HEADED (visible window; cold-start
///    headed if no subprocess is running yet — e.g. claude called
///    request_handoff as its very first browser action). On cold-start
///    the cached MCP handshake is replayed manually (restart does its
///    own replay internally).
/// 3. Synthetic WARMUP tools/call (`browser_navigate` to the last URL
///    claude visited, or about:blank): playwright-mcp launches its
///    browser only inside the first tools/call — without this the
///    promised visible window never opens while the relay is parked.
///    Retries on the profile-lock race; on failure the Enter pill text
///    warns that the window may not have opened.
/// 4. Pill swaps to "press Enter when done"; wait for a bare Enter.
/// 5. Restart back HEADLESS (lazy — no warmup: claude's next real
///    tools/call launches the headless browser, M2 status quo), then
///    answer the request with a success result telling claude to
///    re-navigate (in-page state is lost on a restart; cookies persist
///    via --user-data-dir).
///
/// Restart failures surface to claude as `-32005` client errors and
/// leave `browser = None` (the next browser frame lazy-respawns).
///
/// Each restart kills in-flight subprocess requests — their responses
/// will never arrive, so `inflight` is cleared; claude's other pending
/// calls (it shouldn't have concurrent browser calls during a handoff)
/// hit the agent-side timeout, which is acceptable.
#[allow(clippy::too_many_arguments)]
async fn run_handoff(
    bytes: &mut ByteRx,
    out_tx: &mpsc::Sender<OutFrame>,
    browser: &mut Option<cc_browser::BrowserChannel>,
    browser_out_tx: &mpsc::Sender<String>,
    browser_out_rx: &mut mpsc::Receiver<String>,
    handshake_cache: &Arc<Mutex<Vec<String>>>,
    inflight: &mut HashMap<String, String>,
    last_url: Option<&str>,
    payload: &str,
) {
    // A handoff "notification" (no id) cannot be answered — drop it.
    let Some(id_raw) = extract_id_key(payload) else { return };
    let reason = extract_handoff_reason(payload);

    // 1. Ask the human.
    let ask = format!("云端请求人工接管浏览器: {reason} — [y]打开窗口 / [n]拒绝");
    if !prompt_pill(bytes, &ask).await {
        send_browser_rpc(
            out_tx,
            jsonrpc_client_error(&id_raw, -32003, "user declined handoff"),
        )
        .await;
        return;
    }

    // 2. Switch to a HEADED browser. If a subprocess is running, restart
    //    it (handshake replayed internally); if not, cold-start headed
    //    and replay the relay-owned handshake cache manually.
    let Some((prog, args)) = cc_browser::mcp_command_headed() else {
        send_browser_rpc(
            out_tx,
            jsonrpc_client_error(&id_raw, -32005, "headed browser unavailable"),
        )
        .await;
        return;
    };
    let argrefs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    inflight.clear(); // restart kills in-flight subprocess requests
    let mut cold_start = false;
    let headed = match browser.take() {
        Some(ch) => ch.restart(&prog, &argrefs, browser_out_tx.clone()).await,
        None => {
            cold_start = true;
            cc_browser::BrowserChannel::start(
                &prog,
                &argrefs,
                browser_out_tx.clone(),
                handshake_cache.clone(),
            )
        }
    };
    match headed {
        Ok(ch) => {
            if cold_start {
                // Cold start never went through restart's internal
                // replay; the fresh subprocess needs claude's cached
                // handshake before it serves tools. Empty cache (claude
                // truly never handshook — shouldn't happen for an
                // allowed tools/call) degrades to the warmup failing.
                let _ = replay_handshake(&ch, browser_out_rx, handshake_cache).await;
            }
            *browser = Some(ch);
        }
        Err(e) => {
            send_browser_rpc(
                out_tx,
                jsonrpc_client_error(
                    &id_raw,
                    -32005,
                    &format!("failed to switch browser to headed: {e}"),
                ),
            )
            .await;
            return; // browser stays None; next frame lazy-respawns
        }
    }

    // 3. Warmup: actually open the window. Failure is non-fatal — the
    //    human still gets the Enter pill (with a warning) and the
    //    handoff flow completes either way.
    let warmed = match browser.as_ref() {
        Some(ch) => warmup_headed(ch, browser_out_rx, last_url).await,
        None => false,
    };

    // 4. The human works in the visible window; Enter hands it back.
    let pill_text = if warmed {
        "浏览器已切到可见窗口,完成人工操作后按回车交还"
    } else {
        "浏览器窗口可能未能打开 — 若无窗口请按回车交还并重试"
    };
    wait_for_enter_pill(bytes, pill_text).await;

    // 5. Switch back to HEADLESS. Deliberately NO warmup here: the
    //    headless relaunch stays lazy — claude's next real tools/call
    //    initializes the browser (M2 status quo), and nobody needs to
    //    SEE a headless window appear.
    let Some((prog, args)) = cc_browser::mcp_command() else {
        // Only reachable with an inconsistent CC_BROWSER_MCP* override.
        *browser = None;
        send_browser_rpc(
            out_tx,
            jsonrpc_client_error(&id_raw, -32005, "headless browser unavailable"),
        )
        .await;
        return;
    };
    let argrefs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    inflight.clear(); // nothing was fed while parked, but stay defensive
    let headless = match browser.take() {
        Some(ch) => ch.restart(&prog, &argrefs, browser_out_tx.clone()).await,
        // Unreachable in practice (step 2 always leaves Some), but if
        // hit, the lazy path tolerates a missing replay until the next
        // frame; keep it simple and just start.
        None => cc_browser::BrowserChannel::start(
            &prog,
            &argrefs,
            browser_out_tx.clone(),
            handshake_cache.clone(),
        ),
    };
    match headless {
        Ok(ch) => *browser = Some(ch),
        Err(e) => {
            send_browser_rpc(
                out_tx,
                jsonrpc_client_error(
                    &id_raw,
                    -32005,
                    &format!("failed to switch browser back to headless: {e}"),
                ),
            )
            .await;
            return; // browser stays None; next frame lazy-respawns
        }
    }

    // 6. Answer the request: tell claude the human is done.
    send_browser_rpc(
        out_tx,
        jsonrpc_client_result_text(
            &id_raw,
            "Human finished. Browser is headless again; cookies persisted. \
             Re-navigate to continue.",
        ),
    )
    .await;
}

/// Send a raw JSON-RPC payload to the hub as a BrowserRpc frame.
/// Best-effort: a send failure means the WS is dead and the relay
/// loop's own arms detect that on the next iteration.
async fn send_browser_rpc(out_tx: &mpsc::Sender<OutFrame>, payload: String) {
    let _ = out_tx
        .send(OutFrame::Text(ClientToHub::BrowserRpc { payload }))
        .await;
}

/// Warmup the freshly-headed subprocess so the visible window actually
/// appears (M3 review HIGH). playwright-mcp launches its browser ONLY
/// inside the first `tools/call` handler — initialize/tools/list never
/// do — so after the headed restart nothing is on screen until a
/// tools/call arrives. While the handoff parks the relay, claude can't
/// send one; we synthesize it: a `browser_navigate` to the last URL
/// claude visited (or about:blank).
///
/// Lock-race mitigation (M3 review MEDIUM-1): the OLD subprocess tree
/// exits via playwright-mcp's stdin-close watchdog (~0.3s typically, up
/// to 15s force-kill under load) — `restart`'s done_rx resolving does
/// NOT mean the old browser released the `--user-data-dir` profile
/// lock. If the warmup lands inside that window, playwright answers
/// "Browser is already in use"; retry with 2s/4s backoff, 3 attempts
/// total (covers everything but the pathological 15s tail).
///
/// Returns true when the warmup response arrived (window should be up);
/// false on feed failure / timeout / persistent lock error — callers
/// degrade the Enter pill text but never abort the handoff.
async fn warmup_headed(
    chan: &cc_browser::BrowserChannel,
    out_rx: &mut mpsc::Receiver<String>,
    last_url: Option<&str>,
) -> bool {
    let url = last_url.unwrap_or("about:blank");
    for attempt in 1u32..=3 {
        if attempt > 1 {
            // Backoff before re-trying the lock race: 2s then 4s.
            tokio::time::sleep(std::time::Duration::from_secs(2u64.pow(attempt - 1))).await;
        }
        let frame = warmup_navigate_frame(attempt, url);
        // String ids "cc-warmup-N" cannot collide with claude's numeric
        // ids; extract_id_key canonicalizes to "\"cc-warmup-N\"".
        let want = extract_id_key(&frame).expect("warmup frame has an id");
        if chan.feed(frame).is_err() {
            return false;
        }
        // Bounded wait: headed Chrome launch + page load can be slow,
        // but must not park the handoff forever.
        let Some(resp) =
            pump_until_response(chan, out_rx, &want, std::time::Duration::from_secs(90)).await
        else {
            tracing::warn!(attempt, "headed warmup timed out");
            return false;
        };
        // The response is SWALLOWED (never forwarded to the hub):
        // claude never issued this request and must not see its reply.
        if !resp.contains("Browser is already in use") {
            return true;
        }
        tracing::warn!(attempt, "headed warmup hit the profile-lock race; retrying");
    }
    false
}

/// Synthetic warmup tools/call frame. Pure for testability; the string
/// id namespace ("cc-warmup-N") is reserved to the client.
fn warmup_navigate_frame(attempt: u32, url: &str) -> String {
    let url_json = serde_json::Value::String(url.to_string());
    format!(
        r#"{{"jsonrpc":"2.0","id":"cc-warmup-{attempt}","method":"tools/call","params":{{"name":"browser_navigate","arguments":{{"url":{url_json}}}}}}}"#
    )
}

/// Drain `out_rx` until the response with id key `want_id` arrives
/// (returned, NOT forwarded) or `timeout` elapses (None).
///
/// The relay select! loop is parked while a handoff/replay runs — this
/// helper is the only reader, so any server→client traffic that arrives
/// mid-wait must be handled HERE or the subprocess wedges:
/// - server-originated REQUESTS (frames with both "method" and "id" —
///   e.g. playwright-mcp may send `roots/list` during its first
///   tools/call init and await the reply, which claude cannot give
///   while we're parked): answered locally with an empty result and fed
///   straight back into the channel.
/// - everything else (notifications, stale pre-restart responses whose
///   requests `inflight.clear()` already wrote off): dropped.
async fn pump_until_response(
    chan: &cc_browser::BrowserChannel,
    out_rx: &mut mpsc::Receiver<String>,
    want_id: &str,
    timeout: std::time::Duration,
) -> Option<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let frame = tokio::time::timeout_at(deadline, out_rx.recv()).await.ok()??;
        if extract_id_key(&frame).as_deref() == Some(want_id) {
            return Some(frame);
        }
        if let Some(reply) = answer_server_request(&frame) {
            if chan.feed(reply).is_err() {
                return None;
            }
            continue;
        }
        // Notification or unrelated response: drop (see doc comment).
    }
}

/// Local stand-in reply for a server-originated request received while
/// the relay is parked. `roots/list` gets the empty-roots shape it
/// expects; anything else gets a generic empty result — enough to
/// unblock the server's await without inventing capabilities. Returns
/// None when `frame` is not a request (no method+id pair).
fn answer_server_request(frame: &str) -> Option<String> {
    let id_raw = extract_id_key(frame)?;
    let method = extract_method(frame)?;
    let result = if method == "roots/list" {
        r#"{"roots":[]}"#
    } else {
        "{}"
    };
    Some(format!(
        r#"{{"jsonrpc":"2.0","id":{id_raw},"result":{result}}}"#
    ))
}

/// Replay the relay-owned handshake cache into a freshly started
/// channel: feed each cached frame; for the `initialize` request, wait
/// for (and swallow) its response so claude never sees a duplicate.
/// Empty cache → trivially true.
///
/// NOTE: this intentionally duplicates the replay inside
/// `BrowserChannel::restart`, which runs on the bare `McpProcess`
/// BEFORE the pump task exists and therefore can't share this
/// channel-level implementation; factoring them together would mean
/// teaching restart to speak the channel API mid-construction.
async fn replay_handshake(
    chan: &cc_browser::BrowserChannel,
    out_rx: &mut mpsc::Receiver<String>,
    cache: &Arc<Mutex<Vec<String>>>,
) -> bool {
    let frames: Vec<String> = cache.lock().expect("handshake cache mutex").clone();
    for frame in frames {
        let init_id = if extract_method(&frame).as_deref() == Some("initialize") {
            extract_id_key(&frame)
        } else {
            None // notifications/initialized: no response expected
        };
        if chan.feed(frame).is_err() {
            return false;
        }
        if let Some(want) = init_id {
            if pump_until_response(chan, out_rx, &want, std::time::Duration::from_secs(60))
                .await
                .is_none()
            {
                tracing::warn!("timed out waiting for replayed initialize response");
                return false;
            }
        }
    }
    true
}

/// Tolerant extraction of the navigation target from a
/// `browser_navigate` tools/call: `params.arguments.url`. None for any
/// other method/tool or malformed JSON.
fn extract_navigate_url(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    if v.get("method")?.as_str()? != "tools/call" {
        return None;
    }
    let params = v.get("params")?;
    if params.get("name")?.as_str()? != "browser_navigate" {
        return None;
    }
    params
        .get("arguments")?
        .get("url")?
        .as_str()
        .map(|s| s.to_string())
}

/// Tolerant extraction of `params.arguments.reason` from a
/// request_handoff tools/call. Missing/garbage/empty → a placeholder so
/// the pill always reads sensibly.
fn extract_handoff_reason(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("params")?
                .get("arguments")?
                .get("reason")?
                .as_str()
                .map(|s| s.to_string())
        })
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "(no reason given)".to_string())
}

/// Methods that flow without consent: protocol handshake and metadata.
/// Anything else (tools/call and any unknown/future method) is an action
/// and must pass the consent gate. Default-deny posture: a frame we
/// cannot parse is treated as an action, never waved through.
fn method_is_passive(payload: &str) -> bool {
    // Unparseable payload → NOT passive (default-deny).
    let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) else {
        return false;
    };
    match v.get("method") {
        // Valid JSON without a method field is a response — passive:
        // responses only occur if a request was already allowed through.
        None => true,
        Some(m) => match m.as_str() {
            // Requests/notifications that don't touch the browser.
            Some("initialize") | Some("tools/list") | Some("ping") => true,
            Some(m) if m.starts_with("notifications/") => true,
            // Unknown/future methods, or a non-string `method`: action.
            _ => false,
        },
    }
}

/// JSON-RPC id as a canonical string key ("1", "\"abc\""), None for
/// notifications/unparseable. Mirrors the agent's extract_id_key
/// (mcp_endpoint.rs) so both ends correlate frames identically.
fn extract_id_key(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    match v.get("id") {
        Some(serde_json::Value::Null) | None => None,
        Some(id) => Some(id.to_string()), // numbers -> "1", strings -> "\"abc\""
    }
}

/// JSON-RPC method name; None for responses (no method) or unparseable
/// bodies.
fn extract_method(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    Some(v.get("method")?.as_str()?.to_string())
}

/// True if this inbound frame is a tools/call for our client-side
/// request_handoff tool (handled locally, never fed to playwright-mcp).
fn is_handoff_call(body: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    v.get("method").and_then(|m| m.as_str()) == Some("tools/call")
        && v.get("params")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            == Some("request_handoff")
}

/// Append the client-side request_handoff tool to a tools/list response.
/// On any parse failure returns the input unchanged (defensive).
///
/// NOTE: this re-serializes claude-bound JSON, so key order may differ
/// from the subprocess's original bytes. Acceptable here because the
/// client IS the endpoint of this rewrite — we author the modified
/// response; it is not opaque transit we must preserve byte-for-byte.
fn inject_handoff_tool(response: String) -> String {
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&response) else {
        return response;
    };
    let Some(tools) = v
        .get_mut("result")
        .and_then(|r| r.get_mut("tools"))
        .and_then(|t| t.as_array_mut())
    else {
        return response;
    };
    tools.push(serde_json::json!({
        "name": "request_handoff",
        "description": "Hand the browser to the human user (visible window) for login/CAPTCHA or anything requiring manual action. The browser restarts headed; in-page state is lost but cookies/logins persist. After the human finishes, the browser returns headless and you should re-navigate to continue.",
        "inputSchema": {"type":"object","properties":{"reason":{"type":"string","description":"Why the human is needed (shown to them)"}},"required":["reason"]}
    }));
    serde_json::to_string(&v).unwrap_or(response)
}

/// Build a JSON-RPC response/error originating from the client itself
/// (the client is the server for the request_handoff tool). `id_raw` is
/// the raw id string as captured by extract_id_key, e.g. "1" or
/// "\"abc\"" — spliced verbatim into the frame.
fn jsonrpc_client_error(id_raw: &str, code: i64, message: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id_raw},"error":{{"code":{code},"message":{msg}}}}}"#,
        msg = serde_json::to_string(message).unwrap_or_else(|_| "\"error\"".to_string())
    )
}

/// Build a client-originated JSON-RPC SUCCESS response whose result is
/// a single MCP text content block (the shape tools/call results use).
/// Same raw-id splicing convention as `jsonrpc_client_error`.
fn jsonrpc_client_result_text(id_raw: &str, text: &str) -> String {
    let result = serde_json::json!({ "content": [{ "type": "text", "text": text }] });
    format!(r#"{{"jsonrpc":"2.0","id":{id_raw},"result":{result}}}"#)
}

/// What a stdin chunk means for the consent prompt.
#[derive(Debug, PartialEq, Eq)]
enum ConsentScan {
    Approve,
    Deny,
    Ignore,
}

/// Shared pre-filter for the pill scanners: what kind of chunk is this?
#[derive(Debug, PartialEq, Eq)]
enum PillChunk {
    /// Paste content, escape sequence/terminal reply, or a long plain
    /// blob — swallow, never interpret as an answer.
    Swallow,
    /// Exactly one ESC byte: the bare Esc key.
    EscKey,
    /// A short plain chunk (<= 4 bytes): a deliberate keypress whose
    /// bytes may be scanned for an answer.
    Keys,
}

/// Classify one raw stdin chunk for a modal pill. Conservative by
/// design: only a deliberate, bare keypress may answer.
///
/// Threat model: bytes that merely *contain* answer keys must never
/// answer a pill. A bracketed paste can carry any text ("yes please" in
/// pasted prose would silently APPROVE browser access; a pasted
/// multi-line snippet would "press Enter" during a handoff), and
/// terminals emit unsolicited replies — e.g. a DSR status report
/// `ESC [ 0 n` contains `n` and would spuriously DENY. So:
///
/// - Bracketed-paste regions (`ESC [ 200~` … `ESC [ 201~`) are tracked
///   across chunks via `in_paste` and swallowed wholesale.
/// - Any chunk starting with ESC is an escape sequence (CSI/SS3, a
///   terminal reply, arrow key, paste marker) and is swallowed — never
///   byte-scanned for letters. The single exception: a chunk that is
///   exactly one ESC byte is the Esc key.
/// - Plain chunks answer only when short (<= 4 bytes, a normal keypress
///   chunk); longer plain blobs are unbracketed pastes → swallowed.
fn classify_pill_chunk(chunk: &[u8], in_paste: &mut bool) -> PillChunk {
    const PASTE_START: &[u8] = b"\x1b[200~";
    const PASTE_END: &[u8] = b"\x1b[201~";

    // Inside a paste: swallow everything up to (and including) the end
    // marker; content after the end marker in the same chunk is still an
    // ESC-led tail or paste residue — swallow the whole chunk either way.
    if *in_paste {
        if chunk.windows(PASTE_END.len()).any(|w| w == PASTE_END) {
            *in_paste = false;
        }
        return PillChunk::Swallow;
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
        return PillChunk::Swallow;
    }

    if chunk.first() == Some(&0x1b) {
        // Bare Esc keypress.
        if chunk == [0x1b] {
            return PillChunk::EscKey;
        }
        // Any longer ESC-led chunk is an escape sequence (CSI/SS3, a
        // terminal reply like DSR `ESC [ 0 n`, arrow key) — swallow it,
        // never byte-scan it for answer letters.
        return PillChunk::Swallow;
    }

    // Plain chunk: only a short, keypress-sized chunk may answer. Longer
    // plain chunks are unbracketed paste blobs — swallow them.
    if chunk.len() <= 4 {
        PillChunk::Keys
    } else {
        PillChunk::Swallow
    }
}

/// Classify one raw stdin chunk for a y/n pill (consent + handoff
/// confirmation). See `classify_pill_chunk` for the chunk discipline.
fn scan_consent_chunk(chunk: &[u8], in_paste: &mut bool) -> ConsentScan {
    match classify_pill_chunk(chunk, in_paste) {
        PillChunk::Swallow => ConsentScan::Ignore,
        PillChunk::EscKey => ConsentScan::Deny,
        PillChunk::Keys => {
            for b in chunk {
                match b {
                    b'y' | b'Y' => return ConsentScan::Approve,
                    b'n' | b'N' => return ConsentScan::Deny,
                    _ => {}
                }
            }
            ConsentScan::Ignore
        }
    }
}

/// True only for a deliberate, bare Enter keypress (`\r` or `\n` in a
/// short plain chunk). Esc, escape sequences, pastes (including ones
/// that contain newlines), and long blobs never count — same chunk
/// discipline as `scan_consent_chunk`.
fn scan_for_enter(chunk: &[u8], in_paste: &mut bool) -> bool {
    classify_pill_chunk(chunk, in_paste) == PillChunk::Keys
        && chunk.iter().any(|&b| b == b'\r' || b == b'\n')
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

/// Heuristic sniff: does an outbound MCP frame look like it contains a
/// password field indicator? Used for the notify-only login-page hint.
///
/// This is a **string sniff, NOT a judgment** — false positives are
/// possible (any page that mentions the word "password" in its DOM
/// snapshot will match). The hint is non-blocking and purely advisory:
/// it tells the human they *may* want to call request_handoff; claude's
/// flow is not altered in any way.
///
/// Patterns (a dumb substring set, deliberately — no parsing):
///   `"type":"password"`  — DOM-attribute form (unescaped)
///   `\"type\":\"password\"` — same attribute inside a JSON string value
///   textbox lines mentioning a password label — playwright-mcp's REAL
///   output shape is an aria-snapshot YAML inside the result's text
///   content, e.g. `- textbox \"Password\" [ref=e12]` or
///   `- textbox \"密码\"`; there is no `type=` attribute in that form,
///   so we scan line-wise (both real newlines and the JSON-escaped
///   `\n` sequence) for a `textbox` line containing a case-insensitive
///   `password` or `密码`.
pub fn looks_like_login_page(frame: &str) -> bool {
    if frame.contains(r#""type":"password""#) || frame.contains(r#"\"type\":\"password\""#) {
        return true;
    }
    frame
        .split('\n')
        .flat_map(|s| s.split("\\n"))
        .any(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("textbox") && (lower.contains("password") || line.contains("密码"))
        })
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
    use super::{
        answer_server_request, extract_handoff_reason, extract_id_key, extract_method,
        extract_navigate_url, inject_handoff_tool, is_handoff_call, jsonrpc_client_error,
        jsonrpc_client_result_text, looks_like_login_page, method_is_passive, scan_consent_chunk,
        scan_for_enter, warmup_navigate_frame, ConsentScan,
    };

    #[test]
    fn handshake_and_metadata_methods_are_passive() {
        assert!(method_is_passive(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#
        ));
        assert!(method_is_passive(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#
        ));
        assert!(method_is_passive(r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#));
        assert!(method_is_passive(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
        ));
    }

    #[test]
    fn action_methods_require_consent() {
        assert!(!method_is_passive(
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"browser_navigate"}}"#
        ));
        // Unknown/future methods default to action (default-deny posture).
        assert!(!method_is_passive(
            r#"{"jsonrpc":"2.0","id":5,"method":"resources/read"}"#
        ));
    }

    #[test]
    fn response_frames_are_passive() {
        // No method field = a response; only occurs if the request was
        // already allowed through.
        assert!(method_is_passive(
            r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#
        ));
    }

    #[test]
    fn malformed_json_is_not_passive() {
        // Default-deny: an unparseable frame must hit the consent gate.
        assert!(!method_is_passive("not json at all"));
        assert!(!method_is_passive(r#"{"method":"initialize""#)); // truncated
        assert!(!method_is_passive(""));
    }

    #[test]
    fn non_string_method_is_not_passive() {
        assert!(!method_is_passive(r#"{"jsonrpc":"2.0","id":6,"method":42}"#));
    }

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

    // ---- extract_id_key ----

    #[test]
    fn id_key_number_and_string() {
        assert_eq!(
            extract_id_key(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#),
            Some("1".to_string())
        );
        assert_eq!(
            extract_id_key(r#"{"jsonrpc":"2.0","id":"abc","result":{}}"#),
            Some("\"abc\"".to_string())
        );
    }

    #[test]
    fn id_key_none_for_notifications_null_and_garbage() {
        // Notification: no id field.
        assert_eq!(
            extract_id_key(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
            None
        );
        // Explicit null id is a notification per JSON-RPC conventions.
        assert_eq!(extract_id_key(r#"{"jsonrpc":"2.0","id":null}"#), None);
        assert_eq!(extract_id_key("not json"), None);
        assert_eq!(extract_id_key(""), None);
    }

    // ---- extract_method ----

    #[test]
    fn method_extraction() {
        assert_eq!(
            extract_method(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#),
            Some("tools/list".to_string())
        );
        // Response: no method.
        assert_eq!(extract_method(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#), None);
        // Non-string method / garbage.
        assert_eq!(extract_method(r#"{"jsonrpc":"2.0","id":1,"method":42}"#), None);
        assert_eq!(extract_method("nope"), None);
    }

    // ---- inject_handoff_tool ----

    #[test]
    fn inject_appends_handoff_and_preserves_existing_tools() {
        let resp = r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"browser_navigate","description":"go","inputSchema":{"type":"object"}}]}}"#;
        let out = inject_handoff_tool(resp.to_string());
        let v: serde_json::Value = serde_json::from_str(&out).expect("output stays valid JSON");
        let tools = v["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "browser_navigate"); // existing tool preserved
        assert_eq!(tools[1]["name"], "request_handoff");
        assert_eq!(tools[1]["inputSchema"]["required"][0], "reason");
        assert!(tools[1]["description"].as_str().unwrap().contains("cookies/logins persist"));
        // Envelope intact.
        assert_eq!(v["id"], 2);
        assert_eq!(v["jsonrpc"], "2.0");
    }

    #[test]
    fn inject_leaves_non_tools_list_json_unchanged() {
        // Valid JSON but no result.tools array — e.g. an error response
        // or a tools/call result.
        let err = r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"nope"}}"#;
        assert_eq!(inject_handoff_tool(err.to_string()), err);
        let call_result = r#"{"jsonrpc":"2.0","id":4,"result":{"content":[]}}"#;
        assert_eq!(inject_handoff_tool(call_result.to_string()), call_result);
    }

    #[test]
    fn inject_leaves_garbage_unchanged() {
        assert_eq!(inject_handoff_tool("not json".to_string()), "not json");
        assert_eq!(inject_handoff_tool(String::new()), "");
    }

    // ---- is_handoff_call ----

    #[test]
    fn handoff_call_detected() {
        assert!(is_handoff_call(
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"request_handoff","arguments":{"reason":"login needed"}}}"#
        ));
    }

    #[test]
    fn other_frames_are_not_handoff_calls() {
        // tools/call for a different tool.
        assert!(!is_handoff_call(
            r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"browser_navigate"}}"#
        ));
        // Non-tools/call method, even if params.name matches.
        assert!(!is_handoff_call(
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/list","params":{"name":"request_handoff"}}"#
        ));
        // Garbage.
        assert!(!is_handoff_call("not json"));
        assert!(!is_handoff_call(""));
    }

    // ---- jsonrpc_client_error ----

    #[test]
    fn client_error_shape() {
        let frame = jsonrpc_client_error("5", -32003, "user declined handoff");
        let v: serde_json::Value = serde_json::from_str(&frame).expect("valid JSON");
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 5);
        assert_eq!(v["error"]["code"], -32003);
        assert_eq!(v["error"]["message"], "user declined handoff");

        // String id splices verbatim (id_raw carries its own quotes).
        let frame = jsonrpc_client_error("\"abc\"", -32003, "user declined");
        let v: serde_json::Value = serde_json::from_str(&frame).expect("valid JSON");
        assert_eq!(v["id"], "abc");
    }

    // ---- jsonrpc_client_result_text ----

    #[test]
    fn client_result_text_shape() {
        let frame = jsonrpc_client_result_text("7", "Human finished.");
        let v: serde_json::Value = serde_json::from_str(&frame).expect("valid JSON");
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 7);
        let content = v["result"]["content"].as_array().expect("content array");
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "Human finished.");
        assert!(v.get("error").is_none());

        // String id splices verbatim (id_raw carries its own quotes);
        // text with quotes/escapes survives via json! serialization.
        let frame = jsonrpc_client_result_text("\"abc\"", "say \"hi\"\nnewline");
        let v: serde_json::Value = serde_json::from_str(&frame).expect("valid JSON");
        assert_eq!(v["id"], "abc");
        assert_eq!(v["result"]["content"][0]["text"], "say \"hi\"\nnewline");
    }

    // ---- extract_handoff_reason ----

    #[test]
    fn handoff_reason_present() {
        assert_eq!(
            extract_handoff_reason(
                r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"request_handoff","arguments":{"reason":"login needed"}}}"#
            ),
            "login needed"
        );
    }

    #[test]
    fn handoff_reason_missing_or_garbage_falls_back() {
        // No arguments at all.
        assert_eq!(
            extract_handoff_reason(
                r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"request_handoff"}}"#
            ),
            "(no reason given)"
        );
        // reason present but not a string.
        assert_eq!(
            extract_handoff_reason(
                r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"request_handoff","arguments":{"reason":42}}}"#
            ),
            "(no reason given)"
        );
        // reason present but blank.
        assert_eq!(
            extract_handoff_reason(
                r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"request_handoff","arguments":{"reason":"  "}}}"#
            ),
            "(no reason given)"
        );
        // Unparseable.
        assert_eq!(extract_handoff_reason("not json"), "(no reason given)");
        assert_eq!(extract_handoff_reason(""), "(no reason given)");
    }

    // ---- scan_for_enter ----

    fn enter(chunk: &[u8]) -> bool {
        let mut in_paste = false;
        scan_for_enter(chunk, &mut in_paste)
    }

    #[test]
    fn bare_enter_answers() {
        assert!(enter(b"\r")); // CR (raw-mode Enter)
        assert!(enter(b"\n")); // LF
    }

    #[test]
    fn non_enter_chunks_do_not_answer() {
        assert!(!enter(b"y")); // letters never end the handoff wait
        assert!(!enter(b"\x1b")); // bare Esc is not Enter
        assert!(!enter(b"\x1b[0n")); // DSR terminal reply: swallowed
        assert!(!enter(b"\x1b[200~line1\nline2\x1b[201~")); // paste with \n: swallowed
        assert!(!enter(b"hello\nworld")); // long plain blob: swallowed
    }

    #[test]
    fn split_paste_with_newlines_never_presses_enter() {
        let mut in_paste = false;
        assert!(!scan_for_enter(b"\x1b[200~line1\n", &mut in_paste));
        assert!(in_paste);
        assert!(!scan_for_enter(b"line2\n\x1b[201~", &mut in_paste));
        assert!(!in_paste);
        // ...then a real keypress answers.
        assert!(scan_for_enter(b"\r", &mut in_paste));
    }

    // ---- looks_like_login_page ----

    #[test]
    fn login_page_sniff_unescaped() {
        // Unescaped DOM-snapshot form — direct JSON attribute.
        assert!(looks_like_login_page(
            r#"{"jsonrpc":"2.0","id":10,"result":{"content":[{"type":"text","text":"<input \"type\":\"password\" />"}]}}"#
        ));
        // Simpler unescaped form as it might appear in a plaintext snapshot.
        assert!(looks_like_login_page(
            r#"{"type":"password","value":""}"#
        ));
    }

    #[test]
    fn login_page_sniff_escaped_variant() {
        // Escaped form inside a JSON string value (playwright-mcp text content).
        let escaped = r#"{"jsonrpc":"2.0","id":10,"result":{"content":[{"type":"text","text":"...\"type\":\"password\"..."}]}}"#;
        assert!(looks_like_login_page(escaped));
    }

    #[test]
    fn login_page_sniff_aria_snapshot_textbox_lines() {
        // playwright-mcp's real shape: aria-snapshot YAML inside the
        // result's text content, with JSON-escaped newlines.
        assert!(looks_like_login_page(
            r#"{"jsonrpc":"2.0","id":10,"result":{"content":[{"type":"text","text":"- textbox \"Username\"\n- textbox \"Password\" [ref=e12]\n- button \"Log in\""}]}}"#
        ));
        // Chinese label.
        assert!(looks_like_login_page(
            r#"{"result":{"content":[{"type":"text","text":"- textbox \"密码\" [ref=e7]"}]}}"#
        ));
        // Case-insensitive within a textbox line.
        assert!(looks_like_login_page(
            "- textbox \"Enter PASSWORD here\"\n- button \"submit\""
        ));
        // Real (unescaped) newlines also split into lines.
        assert!(looks_like_login_page(
            "- textbox \"User\"\n- textbox \"password\""
        ));
    }

    #[test]
    fn plain_frame_does_not_match() {
        // Regular navigation result — no password field.
        assert!(!looks_like_login_page(
            r#"{"jsonrpc":"2.0","id":10,"result":{"content":[{"type":"text","text":"Welcome, user!"}]}}"#
        ));
        // Empty string.
        assert!(!looks_like_login_page(""));
        // Frame mentioning "password" only as prose, not as a type attribute.
        assert!(!looks_like_login_page(
            r#"{"type":"text","text":"Enter your password below"}"#
        ));
        // "password" and "textbox" on DIFFERENT lines never match.
        assert!(!looks_like_login_page(
            "- textbox \"Search\"\\n- text \"Forgot password?\""
        ));
    }

    // ---- extract_navigate_url ----

    #[test]
    fn navigate_url_extracted() {
        assert_eq!(
            extract_navigate_url(
                r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"browser_navigate","arguments":{"url":"https://example.com/login"}}}"#
            ),
            Some("https://example.com/login".to_string())
        );
    }

    #[test]
    fn navigate_url_none_for_other_frames() {
        // Different tool.
        assert_eq!(
            extract_navigate_url(
                r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"browser_click","arguments":{"ref":"e1"}}}"#
            ),
            None
        );
        // Different method, even with a url argument.
        assert_eq!(
            extract_navigate_url(
                r#"{"jsonrpc":"2.0","id":3,"method":"tools/list","params":{"name":"browser_navigate","arguments":{"url":"https://x"}}}"#
            ),
            None
        );
        // Missing url / non-string url / garbage.
        assert_eq!(
            extract_navigate_url(
                r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"browser_navigate","arguments":{}}}"#
            ),
            None
        );
        assert_eq!(
            extract_navigate_url(
                r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"browser_navigate","arguments":{"url":42}}}"#
            ),
            None
        );
        assert_eq!(extract_navigate_url("not json"), None);
        assert_eq!(extract_navigate_url(""), None);
    }

    // ---- warmup_navigate_frame ----

    #[test]
    fn warmup_frame_shape() {
        let frame = warmup_navigate_frame(1, "https://example.com/a?b=1");
        let v: serde_json::Value = serde_json::from_str(&frame).expect("valid JSON");
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], "cc-warmup-1"); // STRING id: can't collide with claude's numeric ids
        assert_eq!(v["method"], "tools/call");
        assert_eq!(v["params"]["name"], "browser_navigate");
        assert_eq!(v["params"]["arguments"]["url"], "https://example.com/a?b=1");
        // The frame's own id round-trips through extract_id_key the way
        // warmup_headed correlates it.
        assert_eq!(extract_id_key(&frame), Some("\"cc-warmup-1\"".to_string()));
    }

    #[test]
    fn warmup_frame_escapes_url_and_numbers_attempts() {
        let frame = warmup_navigate_frame(2, "https://x/\"quote\"");
        let v: serde_json::Value = serde_json::from_str(&frame).expect("valid JSON despite quotes");
        assert_eq!(v["id"], "cc-warmup-2");
        assert_eq!(v["params"]["arguments"]["url"], "https://x/\"quote\"");
    }

    // ---- answer_server_request ----

    #[test]
    fn server_request_roots_list_answered_with_empty_roots() {
        let reply = answer_server_request(r#"{"jsonrpc":"2.0","id":7,"method":"roots/list"}"#)
            .expect("a reply");
        let v: serde_json::Value = serde_json::from_str(&reply).expect("valid JSON");
        assert_eq!(v["id"], 7);
        assert_eq!(v["result"]["roots"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn server_request_unknown_method_gets_generic_empty_result() {
        let reply = answer_server_request(
            r#"{"jsonrpc":"2.0","id":"s1","method":"sampling/createMessage"}"#,
        )
        .expect("a reply");
        let v: serde_json::Value = serde_json::from_str(&reply).expect("valid JSON");
        assert_eq!(v["id"], "s1");
        assert!(v["result"].as_object().unwrap().is_empty());
    }

    #[test]
    fn non_requests_are_not_answered() {
        // Notification: method but no id.
        assert_eq!(
            answer_server_request(r#"{"jsonrpc":"2.0","method":"notifications/progress"}"#),
            None
        );
        // Response: id but no method.
        assert_eq!(
            answer_server_request(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#),
            None
        );
        // Garbage.
        assert_eq!(answer_server_request("nope"), None);
    }
}
