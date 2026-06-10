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
use std::time::Duration;

/// Reconnect backoff bounds — mirrors the CLI client
/// (`crates/client/src/main.rs`): start at 500ms, double each failed
/// attempt, cap at 30s. Keeping these identical means the desktop app
/// and the terminal client recover from a hub blip on the same cadence.
const BACKOFF_START: Duration = Duration::from_millis(500);
const BACKOFF_CAP: Duration = Duration::from_secs(30);
/// How long to wait for the hub's `Welcome` after a reconnect before
/// giving up on that attempt and looping (same value the client uses).
const WELCOME_TIMEOUT: Duration = Duration::from_secs(10);

fn bump(d: Duration) -> Duration {
    (d * 2).min(BACKOFF_CAP)
}

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
    /// Terminal resize (debounced pixel→grid from `TerminalPanel::ui`),
    /// forwarded to the hub as `ClientToHub::Resize`.
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
        /// Hub-minted PTY session id (as a string) — the app uses it to
        /// open the browser-panel viewer ws (`/v1/viewer/ws?session=<id>`).
        session_id: String,
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

/// Why the steady-state `event_loop` returned.
enum LoopExit {
    /// The user asked to quit (window close / `UiCommand::Close`), or the
    /// UI side of a channel went away. Tear everything down, no reconnect.
    UserQuit,
    /// The wire to the hub died (closed channel or send error) mid-session.
    /// The outer `run` loop reconnects with backoff.
    WireLost,
}

async fn run(
    cfg: HubConfig,
    cmd_rx: Receiver<UiCommand>,
    event_tx: Sender<BackendEvent>,
    ctx: egui::Context,
) {
    let _ = rustls::crypto::ring::default_provider().install_default();

    // (1) First connect + handshake. A failure here (bad URL, hub down at
    //     launch, bad token) is surfaced as an Error screen rather than
    //     spinning the reconnect loop forever — the user hasn't gotten
    //     anywhere yet, so a clear failure is the right UX.
    let mut wire = match connect_and_welcome(&cfg, &event_tx, &ctx).await {
        ConnectResult::Connected(w) => w,
        ConnectResult::Fatal(msg) => {
            emit(&event_tx, &ctx, BackendEvent::SessionError(msg));
            return;
        }
        ConnectResult::UiGone => return,
    };

    // (2) Steady state: pump the wire until it dies or the user quits. On
    //     a wire death we emit Disconnected (UI shows "reconnecting…" and
    //     greys the terminal), reconnect with exponential backoff, then
    //     emit Connected again. The reducer lands the user back on the
    //     picker (the live tmux session, if any, is reattached by
    //     reopening the workspace) — see `state::apply_event`.
    loop {
        match event_loop(&mut wire, &cmd_rx, &event_tx, &ctx).await {
            LoopExit::UserQuit => return,
            LoopExit::WireLost => {
                if !emit(&event_tx, &ctx, BackendEvent::Disconnected) {
                    return; // UI gone
                }
                match reconnect(&cfg, &cmd_rx, &event_tx, &ctx).await {
                    Some(w) => wire = w,
                    None => return, // user quit or UI gone while reconnecting
                }
                // Connected re-emitted by `reconnect`; loop back to pump
                // the fresh wire.
            }
        }
    }
}

enum ConnectResult {
    Connected(Wire),
    /// A non-retryable failure (bad config, hub rejection) — surface it.
    Fatal(String),
    /// The UI went away before we finished — just stop.
    UiGone,
}

/// Connect once and wait for the hub `Welcome`, emitting `Connected` on
/// success. Used for the initial connect (failures are fatal) and as the
/// per-attempt body of `reconnect` (failures there just retry).
async fn connect_and_welcome(
    cfg: &HubConfig,
    event_tx: &Sender<BackendEvent>,
    ctx: &egui::Context,
) -> ConnectResult {
    let mut wire = match wire::connect(&cfg.hub_url, &cfg.token).await {
        Ok(w) => w,
        Err(e) => return ConnectResult::Fatal(format!("connect: {e:#}")),
    };

    match tokio::time::timeout(WELCOME_TIMEOUT, wire.in_text_rx.recv()).await {
        Ok(Some(HubToClient::Welcome { account })) => {
            if emit(event_tx, ctx, BackendEvent::Connected { account }) {
                ConnectResult::Connected(wire)
            } else {
                ConnectResult::UiGone
            }
        }
        Ok(Some(HubToClient::Rejected { reason })) => {
            ConnectResult::Fatal(format!("hub rejected: {reason}"))
        }
        // Timed out, wrong frame, or wire closed: treat as a soft failure
        // so the reconnect loop retries (the initial connect promotes this
        // to a fatal Error via its own handling below).
        _ => ConnectResult::Fatal("expected welcome from hub".into()),
    }
}

/// Re-establish the wire after a mid-session disconnect, with exponential
/// backoff mirroring the CLI client. Loops until a fresh `Welcome` arrives
/// (emitting `Connected`, which sends the user back to the picker) or the
/// user quits. Returns `None` if the user quit / the UI vanished.
///
/// While we sleep between attempts we still drain UI commands so a
/// `Close` during the outage stops us promptly instead of stranding the
/// backend thread mid-backoff.
async fn reconnect(
    cfg: &HubConfig,
    cmd_rx: &Receiver<UiCommand>,
    event_tx: &Sender<BackendEvent>,
    ctx: &egui::Context,
) -> Option<Wire> {
    let mut backoff = BACKOFF_START;
    loop {
        // Honour a quit request issued during the outage. We can't run
        // commands against a dead wire, but `Close` must still break us
        // out of the loop.
        loop {
            match cmd_rx.try_recv() {
                Ok(UiCommand::Close) => return None,
                Ok(_) => {} // drop other commands; the session is gone
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return None,
            }
        }

        tokio::time::sleep(backoff).await;
        backoff = bump(backoff);

        match connect_and_welcome(cfg, event_tx, ctx).await {
            ConnectResult::Connected(w) => return Some(w),
            ConnectResult::UiGone => return None,
            // Any failure during reconnect is transient — keep retrying.
            // (A genuine bad-token would have failed the initial connect.)
            ConnectResult::Fatal(_) => continue,
        }
    }
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
) -> LoopExit {
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(8));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            // Hub -> UI: JSON control frames.
            text = wire.in_text_rx.recv() => {
                match text {
                    Some(frame) => {
                        if let Some(exit) = handle_hub_frame(wire, frame, event_tx, ctx).await {
                            return exit;
                        }
                    }
                    None => return LoopExit::WireLost, // wire closed
                }
            }
            // Hub -> UI: raw PTY bytes.
            bin = wire.in_bin_rx.recv() => {
                match bin {
                    Some(b) => {
                        if !emit(event_tx, ctx, BackendEvent::PtyBytes(b.to_vec())) {
                            return LoopExit::UserQuit; // UI gone
                        }
                    }
                    None => return LoopExit::WireLost,
                }
            }
            // Poll UI -> backend commands. std mpsc is blocking, so we
            // drain it non-blockingly each tick.
            _ = tick.tick() => {
                loop {
                    match cmd_rx.try_recv() {
                        Ok(cmd) => {
                            if let Some(exit) = handle_command(wire, cmd, event_tx, ctx).await {
                                return exit;
                            }
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        // UI side dropped the sender (window closed).
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            return LoopExit::UserQuit;
                        }
                    }
                }
            }
        }
    }
}

/// Translate a hub frame into a UI event. Returns `Some(LoopExit)` to
/// stop the steady-state loop (the caller then either reconnects or
/// quits); `None` keeps pumping.
///
/// A failed `out_tx.send` (the writer task is gone) means the wire is
/// dead, so we surface `WireLost` and let `run` reconnect. `Rejected`
/// mid-session is likewise treated as a lost connection (the hub closed
/// us) rather than a fatal Error — a fresh `Welcome`-or-bust reconnect
/// will resurface a genuine auth failure.
async fn handle_hub_frame(
    wire: &mut Wire,
    frame: HubToClient,
    event_tx: &Sender<BackendEvent>,
    ctx: &egui::Context,
) -> Option<LoopExit> {
    // `true` → keep looping (None); `false` → wire is dead (WireLost).
    let keep = match frame {
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
            session_id,
        } => emit(
            event_tx,
            ctx,
            BackendEvent::SessionOpened {
                agent,
                workspace,
                cwd,
                session_id: session_id.to_string(),
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
            // The hub dropped us mid-session — reconnect rather than die.
            return Some(LoopExit::WireLost);
        }
        // Frames the picker/session skeleton doesn't act on yet.
        HubToClient::Welcome { .. }
        | HubToClient::AgentSelected { .. }
        | HubToClient::AgentList { .. }
        | HubToClient::WorkspaceReset { .. }
        | HubToClient::FsWriteResult { .. } => true,
    };
    if keep {
        None
    } else {
        Some(LoopExit::WireLost)
    }
}

/// Translate a UI command into hub traffic. Returns `Some(LoopExit)` to
/// stop the loop, `None` to keep pumping.
///
/// `Close` is the only deliberate `UserQuit`. A failed send on any other
/// command means the writer task is gone (wire dead) → `WireLost`, so the
/// outer loop reconnects instead of treating a transient blip as a quit.
async fn handle_command(
    wire: &mut Wire,
    cmd: UiCommand,
    _event_tx: &Sender<BackendEvent>,
    _ctx: &egui::Context,
) -> Option<LoopExit> {
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
            return wire_lost_if_send_failed(wire.out_tx.send(OutFrame::Binary(bytes)).await);
        }
        UiCommand::Resize { cols, rows } => ClientToHub::Resize { cols, rows },
        UiCommand::Close => {
            let _ = wire.out_tx.send(OutFrame::Text(ClientToHub::Close)).await;
            return Some(LoopExit::UserQuit);
        }
    };
    wire_lost_if_send_failed(wire.out_tx.send(OutFrame::Text(frame)).await)
}

/// Map a wire send result to a loop decision: an error means the writer
/// task closed (the wire is dead), so stop with `WireLost`.
fn wire_lost_if_send_failed<E>(r: Result<(), E>) -> Option<LoopExit> {
    if r.is_ok() {
        None
    } else {
        Some(LoopExit::WireLost)
    }
}
