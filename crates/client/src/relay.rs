//! Raw stdin/stdout PTY relay between the local terminal and the hub.
//!
//! MVP: no escape mode, no slash commands. Ctrl+\ et al. all flow straight to
//! the remote claude. We add the escape-mode layer in a later task.

use crate::proto::{ClientToHub, HubToClient};
use crate::wire::{OutFrame, Wire};
use anyhow::{anyhow, Result};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::io::{Read, Write};
use tokio::sync::mpsc;

const STDIN_CHUNK: usize = 4096;

pub struct App {
    pub wire: Wire,
}

pub async fn run(mut app: App) -> Result<()> {
    // 1. Wait for Welcome → SessionOpened in sequence; bail with a clear
    //    error before going raw mode if anything is wrong.
    let welcome = app.wire.in_text_rx.recv().await;
    match welcome {
        Some(HubToClient::Welcome { account }) => {
            tracing::debug!(account, "hub welcomed");
        }
        Some(HubToClient::Rejected { reason }) => {
            return Err(anyhow!("hub rejected: {}", reason));
        }
        other => return Err(anyhow!("expected welcome, got {:?}", other.is_some())),
    }
    let opened = app.wire.in_text_rx.recv().await;
    let (_agent, workspace) = match opened {
        Some(HubToClient::SessionOpened {
            agent, workspace, ..
        }) => (agent, workspace),
        Some(HubToClient::Rejected { reason }) => {
            return Err(anyhow!("session rejected: {}", reason));
        }
        other => return Err(anyhow!("expected session_opened, got {:?}", other.is_some())),
    };
    tracing::debug!(workspace, "session opened");

    // 2. Send initial window size so the remote PTY matches our terminal.
    if let Some((cols, rows)) = current_terminal_size() {
        let _ = app
            .wire
            .out_tx
            .send(OutFrame::Text(ClientToHub::Resize { cols, rows }))
            .await;
    }

    enable_raw_mode()?;
    let result = run_inner(&mut app).await;
    disable_raw_mode().ok();
    // Reset terminal to a sane state after raw mode (in case claude left it
    // in alt-screen or weird mouse mode).
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(b"\x1b[?1049l\x1b[?25h\x1b[?1000l\x1b[?1006l\r\n");
    let _ = stdout.flush();
    result
}

async fn run_inner(app: &mut App) -> Result<()> {
    // stdin reader: blocking thread → mpsc.
    let (stdin_tx, mut stdin_rx) = mpsc::channel::<Vec<u8>>(64);
    std::thread::Builder::new()
        .name("stdin-pump".into())
        .spawn(move || stdin_pump_loop(stdin_tx))
        .ok();

    // SIGWINCH watcher (Unix only): send Resize ctrl frames.
    #[cfg(unix)]
    let mut winch = spawn_winch_signal(app.wire.out_tx.clone());

    loop {
        tokio::select! {
            // Local input → forward as binary.
            chunk = stdin_rx.recv() => {
                let Some(chunk) = chunk else { break; };
                if app.wire.out_tx.send(OutFrame::Binary(chunk)).await.is_err() {
                    break;
                }
            }
            // PTY output from hub → write to stdout.
            bin = app.wire.in_bin_rx.recv() => {
                let Some(bytes) = bin else { break; };
                let mut stdout = std::io::stdout();
                if stdout.write_all(&bytes).is_err() {
                    break;
                }
                if stdout.flush().is_err() {
                    break;
                }
            }
            // Out-of-band control frames from hub.
            text = app.wire.in_text_rx.recv() => {
                let Some(frame) = text else { break; };
                if !handle_hub_text(frame, &app.wire.out_tx).await {
                    break;
                }
            }
            // Window-resize.
            _ = winch_tick(&mut winch) => {
                if let Some((cols, rows)) = current_terminal_size() {
                    let _ = app
                        .wire
                        .out_tx
                        .send(OutFrame::Text(ClientToHub::Resize { cols, rows }))
                        .await;
                }
            }
        }
    }
    Ok(())
}

async fn handle_hub_text(frame: HubToClient, _out: &mpsc::Sender<OutFrame>) -> bool {
    match frame {
        HubToClient::Ping => {
            let _ = _out.send(OutFrame::Text(ClientToHub::Pong)).await;
            true
        }
        HubToClient::SessionClosed { reason } => {
            tracing::info!(?reason, "session closed by hub");
            false
        }
        HubToClient::SessionError { message } => {
            tracing::warn!(%message, "session error");
            true
        }
        // Workspace / agent ops: MVP just logs. (Wired up properly when we
        // add escape mode.)
        _ => true,
    }
}

fn stdin_pump_loop(tx: mpsc::Sender<Vec<u8>>) {
    let mut stdin = std::io::stdin().lock();
    let mut buf = [0u8; STDIN_CHUNK];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if tx.blocking_send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

// ---------------------------------------------------------------------------
// Terminal size + SIGWINCH (Unix only; Windows fallback = static initial size)
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn current_terminal_size() -> Option<(u16, u16)> {
    use std::io::IsTerminal;
    let stdout = std::io::stdout();
    if !stdout.is_terminal() {
        return Some((80, 24));
    }
    crossterm::terminal::size().ok()
}

#[cfg(not(unix))]
fn current_terminal_size() -> Option<(u16, u16)> {
    crossterm::terminal::size().ok()
}

#[cfg(unix)]
struct WinchHandle {
    rx: mpsc::Receiver<()>,
}

#[cfg(unix)]
fn spawn_winch_signal(_: mpsc::Sender<OutFrame>) -> WinchHandle {
    let (tx, rx) = mpsc::channel::<()>(8);
    tokio::spawn(async move {
        let mut sig = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()) {
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
fn spawn_winch_signal(_: mpsc::Sender<OutFrame>) -> WinchHandle {
    WinchHandle
}

#[cfg(not(unix))]
async fn winch_tick(_: &mut WinchHandle) -> Option<()> {
    // No SIGWINCH; sleep forever.
    std::future::pending::<()>().await;
    None
}
