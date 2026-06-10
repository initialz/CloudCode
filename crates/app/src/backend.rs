//! Tokio backend thread + the UI↔backend channel protocol.
//!
//! eframe must own the main (winit) thread, so all async I/O — the
//! `Wire` to the hub, the connect handshake, PTY byte pumping — runs on
//! a dedicated std::thread hosting a current-thread tokio runtime. The
//! UI talks to it over two channels:
//!
//!   UI  --UiCommand-->  Backend   (std::sync::mpsc; UI side is sync)
//!   Backend --BackendEvent--> UI  (std::sync::mpsc; UI drains per frame)
//!
//! After pushing a `BackendEvent` the backend calls
//! `egui::Context::request_repaint()` so the UI wakes even when idle
//! (no pointer movement) — without that, incoming PTY bytes wouldn't
//! show until the next mouse event.

use crate::config::HubConfig;
use crate::wire::{self, OutFrame, Wire};
use cloudcode_proto::{ClientToHub, HubToClient};
use std::sync::mpsc::{Receiver, Sender};

/// Commands the UI sends to the backend.
#[derive(Debug, Clone)]
pub enum UiCommand {
    ListWorkspaces,
    OpenSession { agent: String, workspace: String },
    CreateWorkspace { name: String, agent: String },
    DeleteWorkspace { name: String, agent: String },
    /// Raw bytes typed into the terminal (keyboard/IME → PTY). Produced by
    /// `TerminalPanel::ui` and forwarded to the hub as a binary frame.
    SendInput(Vec<u8>),
    /// Terminal resize. Same story — the panel that computes cols/rows
    /// from pixels lands in Task 3/5.
    #[allow(dead_code)]
    Resize { cols: u16, rows: u16 },
    /// User asked to quit / tear the connection down.
    Close,
}

/// Events the backend sends to the UI.
#[derive(Debug, Clone)]
pub enum BackendEvent {
    Connected {
        account: String,
    },
    Workspaces(Vec<cloudcode_proto::WorkspaceInfo>),
    SessionOpened {
        agent: String,
        workspace: String,
        cwd: String,
    },
    SessionError(String),
    PtyBytes(Vec<u8>),
    Disconnected,
}

/// Handle the UI holds onto: the command sender + event receiver.
pub struct BackendHandle {
    pub cmd_tx: Sender<UiCommand>,
    pub event_rx: Receiver<BackendEvent>,
}

/// Spawn the backend on its own OS thread. `ctx` is the egui context,
/// cloned into the thread so it can `request_repaint()` from outside the
/// UI thread whenever it emits an event.
pub fn spawn(cfg: HubConfig, ctx: egui::Context) -> BackendHandle {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<UiCommand>();
    let (event_tx, event_rx) = std::sync::mpsc::channel::<BackendEvent>();

    std::thread::Builder::new()
        .name("cc-backend".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = event_tx.send(BackendEvent::SessionError(format!(
                        "tokio runtime: {e}"
                    )));
                    ctx.request_repaint();
                    return;
                }
            };
            rt.block_on(run(cfg, cmd_rx, event_tx, ctx));
        })
        .expect("spawn backend thread");

    BackendHandle { cmd_tx, event_rx }
}

/// Emit one event to the UI and wake it. Returns false if the UI side is
/// gone (window closed) so the caller can stop.
fn emit(event_tx: &Sender<BackendEvent>, ctx: &egui::Context, ev: BackendEvent) -> bool {
    let ok = event_tx.send(ev).is_ok();
    ctx.request_repaint();
    ok
}

async fn run(
    cfg: HubConfig,
    cmd_rx: Receiver<UiCommand>,
    event_tx: Sender<BackendEvent>,
    ctx: egui::Context,
) {
    let _ = rustls::crypto::ring::default_provider().install_default();

    // (1) Connect + handshake.
    let mut wire = match wire::connect(&cfg.hub_url, &cfg.token).await {
        Ok(w) => w,
        Err(e) => {
            emit(
                &event_tx,
                &ctx,
                BackendEvent::SessionError(format!("connect: {e:#}")),
            );
            return;
        }
    };

    match wire.in_text_rx.recv().await {
        Some(HubToClient::Welcome { account }) => {
            if !emit(&event_tx, &ctx, BackendEvent::Connected { account }) {
                return;
            }
        }
        Some(HubToClient::Rejected { reason }) => {
            emit(
                &event_tx,
                &ctx,
                BackendEvent::SessionError(format!("hub rejected: {reason}")),
            );
            return;
        }
        _ => {
            emit(
                &event_tx,
                &ctx,
                BackendEvent::SessionError("expected welcome from hub".into()),
            );
            return;
        }
    }

    event_loop(&mut wire, &cmd_rx, &event_tx, &ctx).await;
    emit(&event_tx, &ctx, BackendEvent::Disconnected);
}

/// The steady-state pump: forward hub frames (text + binary) to the UI
/// and UI commands to the hub. `std::sync::mpsc::Receiver` is blocking
/// and not pollable by tokio, so we drain it with `try_recv` on a short
/// interval inside the `select!` loop (8ms ≈ 120Hz, well below input
/// latency that matters) alongside the wire's async receivers.
async fn event_loop(
    wire: &mut Wire,
    cmd_rx: &Receiver<UiCommand>,
    event_tx: &Sender<BackendEvent>,
    ctx: &egui::Context,
) {
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(8));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            // Hub -> UI: JSON control frames.
            text = wire.in_text_rx.recv() => {
                match text {
                    Some(frame) => {
                        if !handle_hub_frame(wire, frame, event_tx, ctx).await {
                            return;
                        }
                    }
                    None => return, // wire closed
                }
            }
            // Hub -> UI: raw PTY bytes.
            bin = wire.in_bin_rx.recv() => {
                match bin {
                    Some(b) => {
                        if !emit(event_tx, ctx, BackendEvent::PtyBytes(b.to_vec())) {
                            return;
                        }
                    }
                    None => return,
                }
            }
            // Poll UI -> backend commands. std mpsc is blocking, so we
            // drain it non-blockingly each tick.
            _ = tick.tick() => {
                loop {
                    match cmd_rx.try_recv() {
                        Ok(cmd) => {
                            if !handle_command(wire, cmd, event_tx, ctx).await {
                                return;
                            }
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                    }
                }
            }
        }
    }
}

/// Translate a hub frame into a UI event. Returns false to stop the
/// loop (terminal condition).
async fn handle_hub_frame(
    wire: &mut Wire,
    frame: HubToClient,
    event_tx: &Sender<BackendEvent>,
    ctx: &egui::Context,
) -> bool {
    match frame {
        HubToClient::WorkspaceList { items } => emit(event_tx, ctx, BackendEvent::Workspaces(items)),
        HubToClient::WorkspaceCreated { .. } | HubToClient::WorkspaceDeleted { .. } => {
            // Re-list so the picker reflects the change.
            wire.out_tx
                .send(OutFrame::Text(ClientToHub::ListWorkspaces))
                .await
                .is_ok()
        }
        HubToClient::SessionOpened {
            agent,
            workspace,
            cwd,
        } => emit(
            event_tx,
            ctx,
            BackendEvent::SessionOpened {
                agent,
                workspace,
                cwd,
            },
        ),
        HubToClient::SessionError { message } => {
            emit(event_tx, ctx, BackendEvent::SessionError(message))
        }
        HubToClient::SessionClosed { reason } => emit(
            event_tx,
            ctx,
            BackendEvent::SessionError(reason.unwrap_or_else(|| "session closed".into())),
        ),
        HubToClient::Ping => wire
            .out_tx
            .send(OutFrame::Text(ClientToHub::Pong))
            .await
            .is_ok(),
        HubToClient::Rejected { reason } => {
            emit(
                event_tx,
                ctx,
                BackendEvent::SessionError(format!("hub rejected: {reason}")),
            );
            false
        }
        // Frames the picker/session skeleton doesn't act on yet.
        HubToClient::Welcome { .. }
        | HubToClient::AgentSelected { .. }
        | HubToClient::AgentList { .. }
        | HubToClient::WorkspaceReset { .. }
        | HubToClient::FsWriteResult { .. } => true,
    }
}

/// Translate a UI command into hub traffic. Returns false to stop.
async fn handle_command(
    wire: &mut Wire,
    cmd: UiCommand,
    _event_tx: &Sender<BackendEvent>,
    _ctx: &egui::Context,
) -> bool {
    let frame = match cmd {
        UiCommand::ListWorkspaces => ClientToHub::ListWorkspaces,
        UiCommand::OpenSession { agent, workspace } => ClientToHub::OpenSession {
            workspace,
            agent,
            cols: 80,
            rows: 24,
            claude_args: Vec::new(),
            tool: None,
        },
        UiCommand::CreateWorkspace { name, agent } => ClientToHub::CreateWorkspace { name, agent },
        UiCommand::DeleteWorkspace { name, agent } => ClientToHub::DeleteWorkspace { name, agent },
        UiCommand::SendInput(bytes) => {
            return wire.out_tx.send(OutFrame::Binary(bytes)).await.is_ok();
        }
        UiCommand::Resize { cols, rows } => ClientToHub::Resize { cols, rows },
        UiCommand::Close => {
            let _ = wire.out_tx.send(OutFrame::Text(ClientToHub::Close)).await;
            return false;
        }
    };
    wire.out_tx.send(OutFrame::Text(frame)).await.is_ok()
}
