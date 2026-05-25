//! Raw PTY relay: stdin bytes → hub; hub binary → stdout.
//!
//! Bytes from `crate::input::spawn_byte_reader` are forwarded verbatim, so
//! every terminal escape sequence (DA1/DA2 responses, cursor position
//! reports, mouse events, anything claude's UI library queries) reaches
//! the remote PTY exactly as the terminal produced it.

use crate::input::ByteRx;
use crate::mouse_filter::MouseModeStripper;
use crate::proto::{ClientToHub, HubToClient};
use crate::wire::{OutFrame, Wire};
use anyhow::Result;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::io::Write;
use tokio::sync::mpsc;

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
    //   ?1049h    — switch to alt-screen, save cursor, clear it
    //   [H        — cursor home in alt-screen
    //   [2J       — defensive re-clear in case ?1049h didn't
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(
        b"\x1b[H\x1b[2J\x1b[3J\
          \x1b[?1000l\x1b[?1001l\x1b[?1002l\x1b[?1003l\
          \x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1016l\
          \x1b[?1049h\x1b[H\x1b[2J",
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
        b"\x1b[?1049l\x1b[?25h\
          \x1b[?1000l\x1b[?1001l\x1b[?1002l\x1b[?1003l\
          \x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1016l\r\n",
    );
    let _ = stdout.flush();
}

/// Run the raw PTY relay once. Caller must have already set up the
/// terminal via `enter_session_mode()`. Returns `RelayOutcome::Closed`
/// for clean exits and `RelayOutcome::HubLost` when the WS dies — that
/// distinction lets the outer loop decide whether to reconnect.
pub async fn run(wire: &mut Wire, bytes: &mut ByteRx) -> Result<RelayOutcome> {
    relay_loop(wire, bytes).await
}

async fn relay_loop(wire: &mut Wire, bytes: &mut ByteRx) -> Result<RelayOutcome> {
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

    loop {
        tokio::select! {
            chunk = bytes.recv() => {
                // ByteRx (stdin reader) ending is "user closed stdin",
                // not a hub failure — close cleanly so the outer loop
                // returns to the menu instead of reconnecting forever.
                let Some(chunk) = chunk else { return Ok(RelayOutcome::Closed); };
                if wire.out_tx.send(OutFrame::Binary(chunk)).await.is_err() {
                    return Ok(RelayOutcome::HubLost);
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
                    _ => {}
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
