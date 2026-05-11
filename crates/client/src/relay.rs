//! Raw stdin/stdout PTY relay with Ctrl+\ escape mode for cloudcode commands.

use crate::proto::{AgentInfo, ClientToHub, HubToClient};
use crate::wire::{OutFrame, Wire};
use anyhow::{anyhow, Result};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::io::{Read, Write};
use tokio::sync::mpsc;

const STDIN_CHUNK: usize = 4096;
const ESCAPE_BYTE: u8 = 0x1c; // Ctrl+\

pub struct App {
    pub wire: Wire,
}

#[derive(Debug, Default)]
pub struct Outcome {
    pub last_agent: Option<String>,
    pub next: NextAction,
}

#[derive(Debug, Default)]
pub enum NextAction {
    #[default]
    Quit,
    Reconnect {
        agent: Option<String>,
    },
}

pub async fn run(app: App) -> Result<Outcome> {
    // Wait for Welcome → SessionOpened *before* going raw, so failures print
    // to a sane terminal.
    let mut wire = app.wire;
    let welcome = wire.in_text_rx.recv().await;
    match welcome {
        Some(HubToClient::Welcome { .. }) => {}
        Some(HubToClient::Rejected { reason }) => {
            return Err(anyhow!("hub rejected: {}", reason));
        }
        other => return Err(anyhow!("expected welcome, got {:?}", other.is_some())),
    }
    let (agent, workspace) = match wire.in_text_rx.recv().await {
        Some(HubToClient::SessionOpened {
            agent, workspace, ..
        }) => (agent, workspace),
        Some(HubToClient::Rejected { reason }) => {
            return Err(anyhow!("session rejected: {}", reason));
        }
        other => {
            return Err(anyhow!(
                "expected session_opened, got {:?}",
                other.is_some()
            ))
        }
    };

    if let Some((cols, rows)) = current_terminal_size() {
        let _ = wire
            .out_tx
            .send(OutFrame::Text(ClientToHub::Resize { cols, rows }))
            .await;
    }

    enable_raw_mode()?;
    let result = main_loop(&mut wire, agent, workspace).await;
    disable_raw_mode().ok();
    let mut stdout = std::io::stdout();
    // Best-effort reset: turn off alt-screen, show cursor, kill mouse modes.
    let _ = stdout.write_all(b"\x1b[?1049l\x1b[?25h\x1b[?1000l\x1b[?1006l\r\n");
    let _ = stdout.flush();
    result
}

async fn main_loop(
    wire: &mut Wire,
    initial_agent: String,
    initial_workspace: String,
) -> Result<Outcome> {
    let mut state = State {
        agent: initial_agent,
        workspace: initial_workspace,
        rows: current_terminal_size().map(|(_, r)| r).unwrap_or(24),
        escape: Escape::Off,
        next: NextAction::Quit,
    };

    let (stdin_tx, mut stdin_rx) = mpsc::channel::<Vec<u8>>(64);
    std::thread::Builder::new()
        .name("stdin-pump".into())
        .spawn(move || stdin_pump_loop(stdin_tx))
        .ok();

    #[cfg(unix)]
    let mut winch = spawn_winch_signal();

    loop {
        tokio::select! {
            chunk = stdin_rx.recv() => {
                let Some(chunk) = chunk else { break; };
                if !handle_local_input(&mut state, &chunk, wire).await {
                    break;
                }
            }
            bin = wire.in_bin_rx.recv() => {
                let Some(bytes) = bin else { break; };
                let mut stdout = std::io::stdout();
                if stdout.write_all(&bytes).is_err() {
                    break;
                }
                if stdout.flush().is_err() {
                    break;
                }
                // If we're in escape mode and the PTY just over-painted our
                // prompt row, redraw it.
                if let Escape::On { buf } = &state.escape {
                    let buf = buf.clone();
                    draw_prompt(state.rows, &buf);
                }
            }
            text = wire.in_text_rx.recv() => {
                let Some(frame) = text else { break; };
                if !handle_hub_text(&mut state, frame, wire).await {
                    break;
                }
            }
            _ = winch_tick(&mut winch) => {
                if let Some((cols, rows)) = current_terminal_size() {
                    state.rows = rows;
                    let _ = wire
                        .out_tx
                        .send(OutFrame::Text(ClientToHub::Resize { cols, rows }))
                        .await;
                }
            }
        }
    }
    Ok(Outcome {
        last_agent: Some(state.agent),
        next: state.next,
    })
}

struct State {
    agent: String,
    workspace: String,
    rows: u16,
    escape: Escape,
    next: NextAction,
}

enum Escape {
    Off,
    On { buf: String },
}

/// Returns false to break the main loop.
async fn handle_local_input(state: &mut State, bytes: &[u8], wire: &Wire) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        match &mut state.escape {
            Escape::Off => {
                // Forward bytes up to the next Ctrl+\, then enter escape mode.
                if let Some(pos) = bytes[i..].iter().position(|&b| b == ESCAPE_BYTE) {
                    if pos > 0 {
                        let _ = wire
                            .out_tx
                            .send(OutFrame::Binary(bytes[i..i + pos].to_vec()))
                            .await;
                    }
                    state.escape = Escape::On { buf: String::new() };
                    draw_prompt(state.rows, "");
                    i += pos + 1;
                } else {
                    let _ = wire
                        .out_tx
                        .send(OutFrame::Binary(bytes[i..].to_vec()))
                        .await;
                    return true;
                }
            }
            Escape::On { buf } => {
                let b = bytes[i];
                match b {
                    0x0d | 0x0a => {
                        let cmd = std::mem::take(buf);
                        leave_prompt(state.rows);
                        state.escape = Escape::Off;
                        if !run_escape_command(state, &cmd, wire).await {
                            return false;
                        }
                    }
                    0x1b => {
                        leave_prompt(state.rows);
                        state.escape = Escape::Off;
                    }
                    0x7f | 0x08 => {
                        buf.pop();
                        let s = buf.clone();
                        draw_prompt(state.rows, &s);
                    }
                    0x20..=0x7e => {
                        buf.push(b as char);
                        let s = buf.clone();
                        draw_prompt(state.rows, &s);
                    }
                    _ => {}
                }
                i += 1;
            }
        }
    }
    true
}

/// Returns false to break the main loop.
async fn run_escape_command(state: &mut State, cmd: &str, wire: &Wire) -> bool {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    match parts.as_slice() {
        [] => true,
        ["help"] => {
            status_print("commands: a [list]|use <n>, w [list]|use <n>|create <n>|rm <n>, status, help, quit");
            true
        }
        ["status"] => {
            status_print(&format!(
                "agent={} workspace={}",
                state.agent, state.workspace
            ));
            true
        }
        ["quit"] | ["q"] | ["exit"] => {
            let _ = wire.out_tx.send(OutFrame::Text(ClientToHub::Close)).await;
            state.next = NextAction::Quit;
            false
        }
        ["a"] | ["a", "list"] => {
            let _ = wire
                .out_tx
                .send(OutFrame::Text(ClientToHub::ListAgents))
                .await;
            true
        }
        ["a", "use", name] => {
            if *name == state.agent {
                status_print(&format!("already on agent '{}'", name));
                true
            } else {
                let _ = wire.out_tx.send(OutFrame::Text(ClientToHub::Close)).await;
                state.next = NextAction::Reconnect {
                    agent: Some((*name).into()),
                };
                false
            }
        }
        ["w"] | ["w", "list"] => {
            let _ = wire
                .out_tx
                .send(OutFrame::Text(ClientToHub::ListWorkspaces))
                .await;
            true
        }
        ["w", "use", name] | ["w", "switch", name] => {
            let _ = wire
                .out_tx
                .send(OutFrame::Text(ClientToHub::SwitchWorkspace {
                    workspace: (*name).into(),
                }))
                .await;
            true
        }
        ["w", "create", name] => {
            let _ = wire
                .out_tx
                .send(OutFrame::Text(ClientToHub::CreateWorkspace {
                    name: (*name).into(),
                }))
                .await;
            true
        }
        ["w", "rm", name] | ["w", "delete", name] | ["w", "remove", name] => {
            let _ = wire
                .out_tx
                .send(OutFrame::Text(ClientToHub::DeleteWorkspace {
                    name: (*name).into(),
                }))
                .await;
            true
        }
        _ => {
            status_print(&format!("unknown command: {}; try `help`", cmd));
            true
        }
    }
}

async fn handle_hub_text(state: &mut State, frame: HubToClient, wire: &Wire) -> bool {
    match frame {
        HubToClient::Ping => {
            let _ = wire.out_tx.send(OutFrame::Text(ClientToHub::Pong)).await;
            true
        }
        HubToClient::SessionClosed { reason } => {
            if let Some(r) = reason {
                status_print(&format!("session closed: {}", r));
            }
            false
        }
        HubToClient::SessionError { message } => {
            status_print(&format!("error: {}", message));
            true
        }
        HubToClient::WorkspaceSwitched { workspace, .. } => {
            state.workspace = workspace.clone();
            status_print(&format!("→ workspace '{}'", workspace));
            true
        }
        HubToClient::AgentList { items } => {
            print_agent_list(&items);
            true
        }
        HubToClient::WorkspaceList { items } => {
            print_workspace_list(&items, &state.workspace);
            true
        }
        HubToClient::WorkspaceCreated { name } => {
            status_print(&format!("workspace '{}' created", name));
            true
        }
        HubToClient::WorkspaceDeleted { name } => {
            status_print(&format!("workspace '{}' deleted", name));
            true
        }
        _ => true,
    }
}

// ---------------------------------------------------------------------------
// stdin pump + winch
// ---------------------------------------------------------------------------

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
// prompt rendering
//
// Reserves the bottom row of the terminal: `\x1B[<rows>;1H` jumps the cursor
// there, `\x1B[2K` clears the line, then we paint `cc> <buf>` reversed; on
// exit we just clear the row and restore the saved cursor (`\x1B[s`/`u`).
// PTY output that lands during escape mode may temporarily over-paint the
// prompt — the next keystroke redraws it.
// ---------------------------------------------------------------------------

fn draw_prompt(rows: u16, buf: &str) {
    let mut out = std::io::stdout();
    let seq = format!(
        "\x1B7\x1B[{};1H\x1B[2K\x1B[7m cc> \x1B[0m{}\x1B8",
        rows, buf
    );
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

fn leave_prompt(rows: u16) {
    let mut out = std::io::stdout();
    let seq = format!("\x1B7\x1B[{};1H\x1B[2K\x1B8", rows);
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

fn status_print(text: &str) {
    // Persist as a one-line message in alt-screen-safe way: write at the
    // bottom row, then sleep is replaced by next keystroke redraw. For now
    // we just emit a newline-bounded message to stderr; raw mode means it
    // appears on whatever line the cursor's on, which is good enough for
    // MVP. (We could route through draw_prompt with a flash, later.)
    let _ = writeln!(std::io::stderr(), "\r\n[cc] {}\r", text);
}

fn print_agent_list(items: &[AgentInfo]) {
    if items.is_empty() {
        status_print("no agents online");
        return;
    }
    let mut s = String::from("agents:\r\n");
    for a in items {
        s.push_str(if a.current { "  * " } else { "    " });
        s.push_str(&a.name);
        s.push_str("\r\n");
    }
    let _ = writeln!(std::io::stderr(), "\r\n[cc] {}", s);
}

fn print_workspace_list(items: &[String], current: &str) {
    if items.is_empty() {
        status_print("no workspaces");
        return;
    }
    let mut s = String::from("workspaces:\r\n");
    for n in items {
        s.push_str(if n == current { "  * " } else { "    " });
        s.push_str(n);
        s.push_str("\r\n");
    }
    let _ = writeln!(std::io::stderr(), "\r\n[cc] {}", s);
}

// ---------------------------------------------------------------------------
// terminal size + SIGWINCH
// ---------------------------------------------------------------------------

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
