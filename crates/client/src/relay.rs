//! Raw stdin/stdout PTY relay. Once inside, every keystroke goes straight
//! to claude on the remote PTY — no escape mode, no slash commands. Returns
//! when the hub reports the session has ended (claude exited, `/exit`, etc).

use crate::proto::{ClientToHub, HubToClient};
use crate::wire::{OutFrame, Wire};
use anyhow::Result;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::io::Write;
use tokio::sync::mpsc;

pub async fn run(wire: &mut Wire, stdin_rx: &mut mpsc::Receiver<Vec<u8>>) -> Result<()> {
    enable_raw_mode()?;
    let result = relay_loop(wire, stdin_rx).await;
    disable_raw_mode().ok();
    let mut stdout = std::io::stdout();
    // Best-effort reset of alt-screen / cursor / mouse modes.
    let _ = stdout.write_all(b"\x1b[?1049l\x1b[?25h\x1b[?1000l\x1b[?1006l\r\n");
    let _ = stdout.flush();
    result
}

async fn relay_loop(wire: &mut Wire, stdin_rx: &mut mpsc::Receiver<Vec<u8>>) -> Result<()> {
    if let Some((cols, rows)) = current_terminal_size() {
        let _ = wire
            .out_tx
            .send(OutFrame::Text(ClientToHub::Resize { cols, rows }))
            .await;
    }

    #[cfg(unix)]
    let mut winch = spawn_winch_signal();

    loop {
        tokio::select! {
            chunk = stdin_rx.recv() => {
                let Some(chunk) = chunk else { return Ok(()); };
                if wire.out_tx.send(OutFrame::Binary(chunk)).await.is_err() {
                    return Ok(());
                }
            }
            bin = wire.in_bin_rx.recv() => {
                let Some(bytes) = bin else { return Ok(()); };
                let mut stdout = std::io::stdout();
                if stdout.write_all(&bytes).is_err() {
                    return Ok(());
                }
                if stdout.flush().is_err() {
                    return Ok(());
                }
            }
            text = wire.in_text_rx.recv() => {
                let Some(frame) = text else { return Ok(()); };
                match frame {
                    HubToClient::Ping => {
                        let _ = wire.out_tx.send(OutFrame::Text(ClientToHub::Pong)).await;
                    }
                    HubToClient::SessionClosed { .. } => return Ok(()),
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
