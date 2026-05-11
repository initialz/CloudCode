//! Ratatui TUI for the chat REPL.

use crate::proto::{ClientToHub, HubToClient};
use anyhow::Result;
use crossterm::event::{Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Terminal;
use std::io::stdout;
use std::time::Duration;
use tokio::sync::mpsc;
use unicode_width::UnicodeWidthStr;

pub struct App {
    pub tx: mpsc::Sender<ClientToHub>,
    pub rx: mpsc::Receiver<HubToClient>,
}

#[derive(Debug, Default)]
pub struct Outcome {
    /// The agent name to persist as "last used" if the user did anything
    /// meaningful (opened a session at least once).
    pub last_agent: Option<String>,
    pub next: NextAction,
}

#[derive(Debug, Default)]
pub enum NextAction {
    /// User asked to exit; main() should stop.
    #[default]
    Quit,
    /// User asked to switch to another agent. main() should tear down the
    /// current WS and dial a fresh one with this agent name.
    Reconnect {
        agent: String,
        /// If Some, switch to this workspace on the new agent; otherwise the
        /// existing chosen workspace is reused.
        workspace: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnState {
    Idle,
    Active,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnState {
    Connecting,
    Connected,
    Disconnected,
}

struct State {
    /// Status bar fields
    account: Option<String>,
    agent: Option<String>,
    workspace: Option<String>,
    cwd: Option<String>,
    turn: TurnState,
    conn: ConnState,
    pending_session_open: bool,

    /// Scrolling chat history.
    lines: Vec<Line<'static>>,
    /// Current partial assistant text accumulating into a single block.
    current_assistant: String,

    /// Multi-line input buffer.
    input: String,
    /// Vertical scroll offset (number of lines hidden above the viewport).
    scroll: u16,
    /// True when user scrolled back manually; new lines won't auto-bottom.
    user_scrolled: bool,
}

impl State {
    fn new() -> Self {
        Self {
            account: None,
            agent: None,
            workspace: None,
            cwd: None,
            turn: TurnState::Idle,
            conn: ConnState::Connecting,
            pending_session_open: false,
            lines: Vec::new(),
            current_assistant: String::new(),
            input: String::new(),
            scroll: 0,
            user_scrolled: false,
        }
    }

    fn push_line(&mut self, line: Line<'static>) {
        self.lines.push(line);
    }

    fn push_system(&mut self, text: String) {
        self.push_line(Line::from(Span::styled(
            text,
            Style::default().fg(Color::DarkGray),
        )));
    }

    fn push_user(&mut self, text: &str) {
        for (i, l) in text.lines().enumerate() {
            let prefix = if i == 0 { "you: " } else { "     " };
            self.push_line(Line::from(vec![
                Span::styled(
                    prefix,
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(l.to_string()),
            ]));
        }
    }

    fn push_error(&mut self, text: String) {
        self.push_line(Line::from(Span::styled(
            format!("error: {}", text),
            Style::default().fg(Color::Red),
        )));
    }

    fn flush_assistant(&mut self) {
        if self.current_assistant.is_empty() {
            return;
        }
        let body = std::mem::take(&mut self.current_assistant);
        // Split into lines so wrap is sensible; keep style.
        for (i, l) in body.lines().enumerate() {
            let prefix = if i == 0 { "claude: " } else { "        " };
            self.push_line(Line::from(vec![
                Span::styled(
                    prefix,
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(l.to_string()),
            ]));
        }
    }
}

pub async fn run(app: App) -> Result<Outcome> {
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(out);
    let mut term = Terminal::new(backend)?;
    let result = main_loop(app, &mut term).await;
    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    result
}

async fn main_loop(
    mut app: App,
    term: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
) -> Result<Outcome> {
    let mut state = State::new();
    let mut next_action = NextAction::Quit;

    // crossterm events fed through an mpsc so we can `tokio::select!`.
    let (ev_tx, mut ev_rx) = mpsc::channel::<CtEvent>(64);
    {
        let ev_tx = ev_tx.clone();
        std::thread::spawn(move || loop {
            match crossterm::event::poll(Duration::from_millis(200)) {
                Ok(true) => match crossterm::event::read() {
                    Ok(ev) => {
                        if ev_tx.blocking_send(ev).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        });
    }

    let mut tick = tokio::time::interval(Duration::from_millis(100));

    loop {
        term.draw(|f| draw(f, &state))?;

        tokio::select! {
            Some(ev) = ev_rx.recv() => {
                if let CtEvent::Key(k) = ev {
                    if k.kind == KeyEventKind::Press
                        && !handle_key(&mut state, &mut app, k, &mut next_action).await
                    {
                        break;
                    }
                }
            }
            hub = app.rx.recv() => {
                match hub {
                    Some(frame) => handle_hub(&mut state, frame, &app.tx).await,
                    None => {
                        state.conn = ConnState::Disconnected;
                        state.push_system("disconnected from hub".into());
                    }
                }
            }
            _ = tick.tick() => {
                // Keep redrawing periodically so status bar refreshes.
            }
        }
    }

    // Try to send a graceful Close.
    let _ = app.tx.send(ClientToHub::Close).await;
    Ok(Outcome {
        last_agent: state.agent.clone(),
        next: next_action,
    })
}

async fn handle_key(
    state: &mut State,
    app: &mut App,
    k: KeyEvent,
    next_action: &mut NextAction,
) -> bool {
    // Ctrl-C: interrupt or quit
    if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
        if state.turn == TurnState::Active {
            let _ = app.tx.send(ClientToHub::Interrupt).await;
            state.push_system("interrupt sent".into());
        } else {
            return false;
        }
        return true;
    }
    if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('d') {
        return false;
    }
    match k.code {
        KeyCode::Enter => {
            if k.modifiers.contains(KeyModifiers::ALT) {
                state.input.push('\n');
            } else {
                let text = std::mem::take(&mut state.input);
                if text.trim().is_empty() {
                    return true;
                }
                if let Some(cmd) = text.strip_prefix('/') {
                    if !handle_slash(state, app, cmd, next_action).await {
                        return false;
                    }
                } else {
                    state.push_user(&text);
                    if state.workspace.is_none() {
                        state.push_error("no session open yet".into());
                    } else if state.turn == TurnState::Active {
                        state.push_error("a turn is already running".into());
                    } else {
                        let _ = app.tx.send(ClientToHub::Input { content: text }).await;
                    }
                }
            }
        }
        KeyCode::Backspace => {
            state.input.pop();
        }
        KeyCode::Char(c) => {
            state.input.push(c);
        }
        KeyCode::PageUp => {
            state.user_scrolled = true;
            state.scroll = state.scroll.saturating_add(5);
        }
        KeyCode::PageDown => {
            state.scroll = state.scroll.saturating_sub(5);
            if state.scroll == 0 {
                state.user_scrolled = false;
            }
        }
        _ => {}
    }
    true
}

/// Returns `false` to break the main loop (used by /exit and /agent use).
async fn handle_slash(
    state: &mut State,
    app: &mut App,
    cmd: &str,
    next_action: &mut NextAction,
) -> bool {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    match parts.as_slice() {
        ["help"] => {
            state.push_system(
                "commands: /ws list|create <n>|use <n>|remove <n>, /agent list|use <n>, /reset, /status, /help, /exit"
                    .into(),
            );
        }
        ["exit"] | ["quit"] | ["q"] => {
            let _ = app.tx.send(ClientToHub::Close).await;
            *next_action = NextAction::Quit;
            return false;
        }
        ["status"] => {
            state.push_system(format!(
                "account={} agent={} workspace={} cwd={} turn={:?} conn={:?}",
                state.account.as_deref().unwrap_or("?"),
                state.agent.as_deref().unwrap_or("?"),
                state.workspace.as_deref().unwrap_or("?"),
                state.cwd.as_deref().unwrap_or("?"),
                state.turn,
                state.conn,
            ));
        }
        ["reset"] => {
            if let Some(ws) = state.workspace.clone() {
                // Switch to same name = agent restarts conversation.
                let _ = app
                    .tx
                    .send(ClientToHub::SwitchWorkspace { workspace: ws })
                    .await;
                state.push_system("requesting fresh conversation (same workspace)…".into());
            }
        }
        ["ws", "list"] | ["ws"] => {
            let _ = app.tx.send(ClientToHub::ListWorkspaces).await;
        }
        ["ws", "create", name] => {
            let _ = app
                .tx
                .send(ClientToHub::CreateWorkspace {
                    name: (*name).into(),
                })
                .await;
        }
        ["ws", "use", name] | ["ws", "switch", name] => {
            let _ = app
                .tx
                .send(ClientToHub::SwitchWorkspace {
                    workspace: (*name).into(),
                })
                .await;
            state.push_system(format!("switching to workspace '{}'…", name));
        }
        ["ws", "remove", name] | ["ws", "delete", name] | ["ws", "rm", name] => {
            let _ = app
                .tx
                .send(ClientToHub::DeleteWorkspace {
                    name: (*name).into(),
                })
                .await;
        }
        ["agent"] | ["agent", "list"] | ["agent", "ls"] => {
            let _ = app.tx.send(ClientToHub::ListAgents).await;
        }
        ["agent", "use", name] | ["agent", "switch", name] => {
            if Some(*name) == state.agent.as_deref() {
                state.push_system(format!("already on agent '{}'", name));
            } else {
                *next_action = NextAction::Reconnect {
                    agent: (*name).into(),
                    workspace: None,
                };
                state.push_system(format!("switching to agent '{}'…", name));
                let _ = app.tx.send(ClientToHub::Close).await;
                return false;
            }
        }
        _ => state.push_error(format!("unknown command /{}; try /help", cmd)),
    }
    true
}

async fn handle_hub(state: &mut State, frame: HubToClient, tx: &mpsc::Sender<ClientToHub>) {
    match frame {
        HubToClient::Welcome { account } => {
            state.account = Some(account);
            state.conn = ConnState::Connected;
            if !state.pending_session_open {
                // OpenSession was already sent right after wire connect; this is just a follow-up.
            }
        }
        HubToClient::Rejected { reason } => {
            state.push_error(format!("rejected: {}", reason));
            state.conn = ConnState::Disconnected;
        }
        HubToClient::SessionOpened {
            agent,
            workspace,
            cwd,
        } => {
            state.agent = Some(agent);
            state.workspace = Some(workspace);
            state.cwd = Some(cwd);
            state.pending_session_open = false;
            state.push_system("session opened. type /help for commands.".into());
        }
        HubToClient::TurnStarted => {
            state.turn = TurnState::Active;
            state.current_assistant.clear();
        }
        HubToClient::ClaudeEvent { event } => render_claude_event(state, &event),
        HubToClient::TurnEnded { exit_code, error } => {
            state.flush_assistant();
            if let Some(e) = error {
                state.push_error(e);
            } else if exit_code != 0 {
                state.push_error(format!("claude exited with code {}", exit_code));
            }
            state.turn = TurnState::Idle;
        }
        HubToClient::WorkspaceSwitched { workspace, cwd } => {
            state.workspace = Some(workspace.clone());
            state.cwd = Some(cwd);
            state.push_system(format!("workspace → {}", workspace));
        }
        HubToClient::WorkspaceList { items } => {
            if items.is_empty() {
                state.push_system("no workspaces yet; use :workspace create <name>".into());
            } else {
                state.push_system(format!("workspaces: {}", items.join(", ")));
            }
        }
        HubToClient::WorkspaceCreated { name } => {
            state.push_system(format!("workspace '{}' created", name));
        }
        HubToClient::WorkspaceDeleted { name } => {
            state.push_system(format!("workspace '{}' deleted", name));
        }
        HubToClient::AgentList { items } => {
            if items.is_empty() {
                state.push_system("no agents online".into());
            } else {
                let names: Vec<String> = items
                    .iter()
                    .map(|a| {
                        if a.current {
                            format!("* {}", a.name)
                        } else {
                            format!("  {}", a.name)
                        }
                    })
                    .collect();
                state.push_system(format!("agents:\n{}", names.join("\n")));
            }
        }
        HubToClient::SessionError { message } => state.push_error(message),
        HubToClient::SessionClosed { reason } => {
            state.push_system(format!(
                "session closed{}",
                reason.map(|r| format!(": {}", r)).unwrap_or_default()
            ));
        }
        HubToClient::Ping => {
            let _ = tx.send(ClientToHub::Pong).await;
        }
    }
}

fn render_claude_event(state: &mut State, line: &str) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return;
    };
    let kind = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
    match kind {
        "assistant" => {
            if let Some(content) = v.pointer("/message/content").and_then(|c| c.as_array()) {
                for block in content {
                    let bt = block.get("type").and_then(|x| x.as_str()).unwrap_or("");
                    match bt {
                        "text" => {
                            if let Some(t) = block.get("text").and_then(|x| x.as_str()) {
                                state.current_assistant.push_str(t);
                            }
                        }
                        "tool_use" => {
                            state.flush_assistant();
                            let name = block.get("name").and_then(|x| x.as_str()).unwrap_or("?");
                            state.push_line(Line::from(vec![
                                Span::styled("  → ", Style::default().fg(Color::Yellow)),
                                Span::styled(
                                    format!("tool_use: {}", name),
                                    Style::default().fg(Color::Yellow),
                                ),
                            ]));
                        }
                        _ => {}
                    }
                }
            }
        }
        "result" => {
            state.flush_assistant();
            let cost = v
                .get("total_cost_usd")
                .or_else(|| v.get("cost_usd"))
                .and_then(|x| x.as_f64());
            let usage = v.get("usage");
            let mut bits: Vec<String> = Vec::new();
            if let Some(c) = cost {
                bits.push(format!("cost ${:.4}", c));
            }
            if let Some(t) = usage
                .and_then(|u| u.get("input_tokens"))
                .and_then(|x| x.as_i64())
            {
                bits.push(format!("in {}", t));
            }
            if let Some(t) = usage
                .and_then(|u| u.get("output_tokens"))
                .and_then(|x| x.as_i64())
            {
                bits.push(format!("out {}", t));
            }
            if !bits.is_empty() {
                state.push_line(Line::from(Span::styled(
                    format!("[{}]", bits.join(", ")),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
        _ => {}
    }
}

fn draw(f: &mut ratatui::Frame, state: &State) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // status bar
            Constraint::Min(3),    // chat history
            Constraint::Length(1), // hint line
            Constraint::Length(input_height(&state.input)),
        ])
        .split(area);

    draw_status(f, chunks[0], state);
    draw_chat(f, chunks[1], state);
    draw_hint(f, chunks[2]);
    draw_input(f, chunks[3], state);
}

fn input_height(input: &str) -> u16 {
    let n = input.chars().filter(|c| *c == '\n').count() as u16 + 1;
    n.clamp(3, 6)
}

fn draw_status(f: &mut ratatui::Frame, area: Rect, s: &State) {
    let parts = [
        format!("workspace={}", s.workspace.as_deref().unwrap_or("-")),
        format!("agent={}", s.agent.as_deref().unwrap_or("-")),
        format!(
            "turn={}",
            match s.turn {
                TurnState::Idle => "idle",
                TurnState::Active => "active",
            }
        ),
        format!(
            "conn={}",
            match s.conn {
                ConnState::Connecting => "connecting",
                ConnState::Connected => "connected",
                ConnState::Disconnected => "disconnected",
            }
        ),
    ];
    let text = format!(" {} ", parts.join(" · "));
    let para = Paragraph::new(Span::styled(
        text,
        Style::default()
            .bg(Color::DarkGray)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    ));
    f.render_widget(para, area);
}

fn draw_chat(f: &mut ratatui::Frame, area: Rect, s: &State) {
    // Build display lines: history + (optional) in-progress assistant.
    let mut lines: Vec<Line<'static>> = s.lines.clone();
    if !s.current_assistant.is_empty() {
        for (i, l) in s.current_assistant.lines().enumerate() {
            let prefix = if i == 0 { "claude: " } else { "        " };
            lines.push(Line::from(vec![
                Span::styled(
                    prefix,
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(l.to_string()),
            ]));
        }
    }
    // Auto-scroll to bottom unless the user scrolled.
    let total = lines.len() as u16;
    let view = area.height.saturating_sub(2);
    let auto_scroll = total.saturating_sub(view);
    let scroll = if s.user_scrolled {
        auto_scroll.saturating_sub(s.scroll)
    } else {
        auto_scroll
    };

    let para = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title("chat"))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    f.render_widget(para, area);
}

fn draw_hint(f: &mut ratatui::Frame, area: Rect) {
    let para = Paragraph::new(Span::styled(
        " Enter: send · Alt+Enter: newline · Ctrl-C: interrupt/quit · /help ",
        Style::default().fg(Color::DarkGray),
    ));
    f.render_widget(para, area);
}

fn draw_input(f: &mut ratatui::Frame, area: Rect, s: &State) {
    let block = Block::default().borders(Borders::ALL).title("input");
    let para = Paragraph::new(s.input.as_str())
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
    // Set cursor at end of input (approx; raw cursor placement is hard in
    // wrapped paragraphs — good enough for MVP).
    let last_line_len = s
        .input
        .lines()
        .last()
        .map(|l| l.width() as u16)
        .unwrap_or(0);
    let line_count = s.input.lines().count() as u16;
    if area.width > 2 && area.height > 2 {
        f.set_cursor_position((
            area.x + 1 + last_line_len.min(area.width - 2),
            area.y + 1 + line_count.saturating_sub(1),
        ));
    }
}
