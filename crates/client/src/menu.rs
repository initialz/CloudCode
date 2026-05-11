//! Interactive TUI menu shown before opening a PTY.
//!
//! Two stages:
//!   1. Pick an agent (arrow keys + Enter).
//!   2. Pick a workspace (arrow keys + Enter); `c` creates a new one (text
//!      input prompt), `d` deletes the highlighted one (confirm prompt),
//!      `Esc` goes back to the agent picker.
//!
//! Esc / `q` at the agent picker quits cloudcode.

use crate::input::KeyRx;
use crate::proto::{ClientToHub, HubToClient};
use crate::wire::{OutFrame, Wire};
use anyhow::{anyhow, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Terminal;
use std::io::stdout;

pub enum MenuOutcome {
    OpenWorkspace { agent: String, workspace: String },
    Quit,
}

pub async fn run(
    wire: &mut Wire,
    keys: &mut KeyRx,
    account: &str,
    last_agent: Option<&str>,
) -> Result<MenuOutcome> {
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(out);
    let mut term = Terminal::new(backend)?;
    let result = run_inner(&mut term, wire, keys, account, last_agent).await;
    disable_raw_mode().ok();
    execute!(term.backend_mut(), LeaveAlternateScreen).ok();
    term.show_cursor().ok();
    result
}

async fn run_inner<B: ratatui::backend::Backend>(
    term: &mut Terminal<B>,
    wire: &mut Wire,
    keys: &mut KeyRx,
    account: &str,
    last_agent: Option<&str>,
) -> Result<MenuOutcome> {
    'outer: loop {
        // ---- stage 1: agent picker ----
        let agents = list_agents(wire).await?;
        if agents.is_empty() {
            show_message(term, "no agents online", keys).await?;
            return Ok(MenuOutcome::Quit);
        }
        let mut a_state = ListState::default();
        let initial = last_agent
            .and_then(|n| agents.iter().position(|a| a == n))
            .unwrap_or(0);
        a_state.select(Some(initial));

        let agent = loop {
            term.draw(|f| {
                draw_layout(
                    f,
                    account,
                    "Select agent",
                    &agents,
                    &mut a_state,
                    "↑↓ move · Enter pick · Esc/q quit",
                )
            })?;
            let Some(k) = keys.recv().await else {
                return Ok(MenuOutcome::Quit);
            };
            match handle_list_key(k, &mut a_state, agents.len()) {
                ListAction::Pick => break agents[a_state.selected().unwrap_or(0)].clone(),
                ListAction::Quit => return Ok(MenuOutcome::Quit),
                ListAction::Pass => {}
            }
        };

        // bind to the selected agent
        wire.out_tx
            .send(OutFrame::Text(ClientToHub::SelectAgent {
                agent: Some(agent.clone()),
            }))
            .await
            .map_err(|_| anyhow!("hub disconnected"))?;
        match expect_text(wire).await? {
            HubToClient::AgentSelected { .. } => {}
            HubToClient::SessionError { message } => {
                show_message(term, &format!("error: {}", message), keys).await?;
                continue 'outer;
            }
            _ => continue 'outer,
        }
        crate::write_last_agent(&agent);

        // ---- stage 2: workspace picker (loop until pick or Esc back) ----
        let last_ws = crate::read_last_workspace(&agent);
        let mut w_state = ListState::default();
        loop {
            let workspaces = list_workspaces(wire).await?;
            let initial = last_ws
                .as_deref()
                .and_then(|n| workspaces.iter().position(|w| w == n))
                .unwrap_or(0);
            if w_state.selected().is_none() {
                w_state.select(Some(initial.min(workspaces.len().saturating_sub(1))));
            }
            term.draw(|f| {
                draw_layout(
                    f,
                    account,
                    &format!("Select workspace on {}", agent),
                    &workspaces,
                    &mut w_state,
                    "↑↓ move · Enter pick · c create · d delete · Esc back · q quit",
                )
            })?;
            let Some(k) = keys.recv().await else {
                return Ok(MenuOutcome::Quit);
            };
            match k.code {
                KeyCode::Esc => continue 'outer,
                KeyCode::Char('q') if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(MenuOutcome::Quit);
                }
                KeyCode::Char('c') => {
                    if let Some(name) = prompt_input(term, keys, "create workspace", "").await? {
                        let name = name.trim().to_string();
                        if !name.is_empty() {
                            create_workspace(wire, &name).await?;
                            w_state.select(None);
                        }
                    }
                }
                KeyCode::Char('d') => {
                    if let Some(sel) = w_state.selected() {
                        if let Some(ws) = workspaces.get(sel) {
                            let confirmed =
                                prompt_confirm(term, keys, &format!("delete workspace '{}'?", ws))
                                    .await?;
                            if confirmed {
                                delete_workspace(wire, ws).await?;
                                w_state.select(None);
                            }
                        }
                    }
                }
                _ => match handle_list_key(k, &mut w_state, workspaces.len()) {
                    ListAction::Pick => {
                        if let Some(sel) = w_state.selected() {
                            if let Some(ws) = workspaces.get(sel) {
                                return Ok(MenuOutcome::OpenWorkspace {
                                    agent,
                                    workspace: ws.clone(),
                                });
                            }
                        }
                    }
                    ListAction::Quit => return Ok(MenuOutcome::Quit),
                    ListAction::Pass => {}
                },
            }
        }
    }
}

// ---------------------------------------------------------------------------

enum ListAction {
    Pick,
    Quit,
    Pass,
}

fn handle_list_key(k: KeyEvent, state: &mut ListState, len: usize) -> ListAction {
    if len == 0 {
        if matches!(k.code, KeyCode::Esc | KeyCode::Char('q')) {
            return ListAction::Quit;
        }
        return ListAction::Pass;
    }
    let cur = state.selected().unwrap_or(0);
    match k.code {
        KeyCode::Up | KeyCode::Char('k') => {
            state.select(Some(if cur == 0 { len - 1 } else { cur - 1 }));
            ListAction::Pass
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.select(Some((cur + 1) % len));
            ListAction::Pass
        }
        KeyCode::Home | KeyCode::Char('g') => {
            state.select(Some(0));
            ListAction::Pass
        }
        KeyCode::End | KeyCode::Char('G') => {
            state.select(Some(len - 1));
            ListAction::Pass
        }
        KeyCode::Enter => ListAction::Pick,
        KeyCode::Esc => ListAction::Quit,
        KeyCode::Char('q') if !k.modifiers.contains(KeyModifiers::CONTROL) => ListAction::Quit,
        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => ListAction::Quit,
        _ => ListAction::Pass,
    }
}

fn draw_layout(
    f: &mut ratatui::Frame,
    account: &str,
    title: &str,
    items: &[String],
    state: &mut ListState,
    hint: &str,
) {
    let area = f.area();
    // Mascot is 11 lines tall (10 art + 1 greeting); only show it when the
    // terminal has room. Below ~24 rows we skip it so the picker stays usable.
    let show_mascot = area.height >= 20;
    let mascot_h: u16 = if show_mascot { 11 } else { 0 };

    let constraints = if show_mascot {
        vec![
            ratatui::layout::Constraint::Length(mascot_h),
            ratatui::layout::Constraint::Min(3),
            ratatui::layout::Constraint::Length(1),
        ]
    } else {
        vec![
            ratatui::layout::Constraint::Min(3),
            ratatui::layout::Constraint::Length(1),
        ]
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let (list_area, hint_area) = if show_mascot {
        f.render_widget(mascot_widget(account), chunks[0]);
        (chunks[1], chunks[2])
    } else {
        (chunks[0], chunks[1])
    };

    let list_items: Vec<ListItem> = if items.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "  (empty — press `c` to create)",
            Style::default().fg(Color::DarkGray),
        )))]
    } else {
        items
            .iter()
            .map(|s| ListItem::new(Line::from(Span::raw(s.clone()))))
            .collect()
    };

    let list = List::new(list_items)
        .block(
            Block::default()
                .title(Span::styled(
                    format!(" {} ", title),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    f.render_stateful_widget(list, list_area, state);

    let hint_widget = Paragraph::new(Span::styled(
        format!(" {} ", hint),
        Style::default().fg(Color::DarkGray),
    ));
    f.render_widget(hint_widget, hint_area);
}

/// Cute pixel-ish robot mascot, ~10 rows tall. Designed with box-drawing
/// chars so it stays aligned in monospace fonts.
fn mascot_widget(account: &str) -> Paragraph<'static> {
    let body = Style::default().fg(Color::Cyan);
    let eyes = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let mouth = Style::default().fg(Color::LightMagenta);
    let antenna = Style::default().fg(Color::White);
    let cloud = Style::default().fg(Color::LightCyan);
    let dim = Style::default().fg(Color::DarkGray);

    let lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled(". ° ✦ °  .", cloud)).alignment(Alignment::Center),
        Line::from(Span::styled("│", antenna)).alignment(Alignment::Center),
        Line::from(Span::styled("●", antenna)).alignment(Alignment::Center),
        Line::from(Span::styled("╭─────╮", body)).alignment(Alignment::Center),
        Line::from(vec![
            Span::styled("│ ", body),
            Span::styled("●", eyes),
            Span::raw(" "),
            Span::styled("●", eyes),
            Span::styled(" │", body),
        ])
        .alignment(Alignment::Center),
        Line::from(vec![
            Span::styled("│  ", body),
            Span::styled("◡", mouth),
            Span::styled("  │", body),
        ])
        .alignment(Alignment::Center),
        Line::from(Span::styled("╰──┬─┬──╯", body)).alignment(Alignment::Center),
        Line::from(Span::styled("┌─────┴─┴─────┐", body)).alignment(Alignment::Center),
        Line::from(vec![
            Span::styled("│ ", body),
            Span::styled("░", dim),
            Span::styled(" ▓▓▓▓▓ ", body),
            Span::styled("░", dim),
            Span::styled(" │", body),
        ])
        .alignment(Alignment::Center),
        Line::from(Span::styled("└────┬───┬────┘", body)).alignment(Alignment::Center),
        Line::from(vec![
            Span::styled("hi ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                account.to_string(),
                Style::default()
                    .fg(Color::LightMagenta)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ✨", Style::default().fg(Color::Yellow)),
        ])
        .alignment(Alignment::Center),
    ];

    Paragraph::new(lines)
}

async fn show_message<B: ratatui::backend::Backend>(
    term: &mut Terminal<B>,
    msg: &str,
    keys: &mut KeyRx,
) -> Result<()> {
    term.draw(|f| {
        let area = f.area();
        let block = Block::default().title(" cloudcode ").borders(Borders::ALL);
        let p = Paragraph::new(Line::from(Span::raw(msg))).block(block);
        f.render_widget(p, centered_rect(area, 50, 5));
    })?;
    let _ = keys.recv().await;
    Ok(())
}

async fn prompt_input<B: ratatui::backend::Backend>(
    term: &mut Terminal<B>,
    keys: &mut KeyRx,
    title: &str,
    initial: &str,
) -> Result<Option<String>> {
    let mut buf = initial.to_string();
    loop {
        let title_owned = title.to_string();
        let buf_view = buf.clone();
        term.draw(move |f| {
            let area = f.area();
            let rect = centered_rect(area, 60, 5);
            f.render_widget(Clear, rect);
            let block = Block::default()
                .title(format!(" {} ", title_owned))
                .borders(Borders::ALL);
            let inner = block.inner(rect);
            f.render_widget(block, rect);
            let para = Paragraph::new(Line::from(vec![
                Span::styled("> ", Style::default().fg(Color::Cyan)),
                Span::raw(buf_view.clone()),
                Span::styled("█", Style::default().fg(Color::Cyan)),
            ]));
            let hint = Paragraph::new(Span::styled(
                " Enter accept · Esc cancel ",
                Style::default().fg(Color::DarkGray),
            ));
            let inner_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Length(1)])
                .split(inner);
            f.render_widget(para, inner_chunks[0]);
            f.render_widget(hint, inner_chunks[1]);
        })?;
        let Some(k) = keys.recv().await else {
            return Ok(None);
        };
        match k.code {
            KeyCode::Esc => return Ok(None),
            KeyCode::Enter => return Ok(Some(buf)),
            KeyCode::Backspace => {
                buf.pop();
            }
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                buf.push(c);
            }
            _ => {}
        }
    }
}

async fn prompt_confirm<B: ratatui::backend::Backend>(
    term: &mut Terminal<B>,
    keys: &mut KeyRx,
    msg: &str,
) -> Result<bool> {
    loop {
        let msg_owned = msg.to_string();
        term.draw(move |f| {
            let area = f.area();
            let rect = centered_rect(area, 50, 5);
            f.render_widget(Clear, rect);
            let block = Block::default().title(" confirm ").borders(Borders::ALL);
            let inner = block.inner(rect);
            f.render_widget(block, rect);
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Length(1)])
                .split(inner);
            f.render_widget(Paragraph::new(Line::from(Span::raw(msg_owned))), chunks[0]);
            f.render_widget(
                Paragraph::new(Span::styled(
                    " y yes · n/Esc no ",
                    Style::default().fg(Color::DarkGray),
                )),
                chunks[1],
            );
        })?;
        let Some(k) = keys.recv().await else {
            return Ok(false);
        };
        match k.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => return Ok(true),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => return Ok(false),
            _ => {}
        }
    }
}

fn centered_rect(area: Rect, width_pct: u16, height: u16) -> Rect {
    let h = height.min(area.height);
    let w = (area.width * width_pct / 100).clamp(20, area.width);
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}

// ---------------------------------------------------------------------------
// Hub queries (text-only; menu doesn't expect binary frames)
// ---------------------------------------------------------------------------

async fn list_agents(wire: &mut Wire) -> Result<Vec<String>> {
    wire.out_tx
        .send(OutFrame::Text(ClientToHub::ListAgents))
        .await
        .map_err(|_| anyhow!("hub disconnected"))?;
    loop {
        let m = expect_text(wire).await?;
        match m {
            HubToClient::AgentList { items } => {
                return Ok(items.into_iter().map(|a| a.name).collect())
            }
            HubToClient::SessionError { message } => {
                return Err(anyhow!("list agents: {}", message))
            }
            HubToClient::Ping => {
                let _ = wire.out_tx.send(OutFrame::Text(ClientToHub::Pong)).await;
            }
            _ => continue,
        }
    }
}

async fn list_workspaces(wire: &mut Wire) -> Result<Vec<String>> {
    wire.out_tx
        .send(OutFrame::Text(ClientToHub::ListWorkspaces))
        .await
        .map_err(|_| anyhow!("hub disconnected"))?;
    loop {
        let m = expect_text(wire).await?;
        match m {
            HubToClient::WorkspaceList { items } => return Ok(items),
            HubToClient::SessionError { .. } => return Ok(Vec::new()),
            HubToClient::Ping => {
                let _ = wire.out_tx.send(OutFrame::Text(ClientToHub::Pong)).await;
            }
            _ => continue,
        }
    }
}

async fn create_workspace(wire: &mut Wire, name: &str) -> Result<()> {
    wire.out_tx
        .send(OutFrame::Text(ClientToHub::CreateWorkspace {
            name: name.into(),
        }))
        .await
        .map_err(|_| anyhow!("hub disconnected"))?;
    loop {
        let m = expect_text(wire).await?;
        match m {
            HubToClient::WorkspaceCreated { .. } => return Ok(()),
            HubToClient::SessionError { .. } => return Ok(()),
            HubToClient::Ping => {
                let _ = wire.out_tx.send(OutFrame::Text(ClientToHub::Pong)).await;
            }
            _ => continue,
        }
    }
}

async fn delete_workspace(wire: &mut Wire, name: &str) -> Result<()> {
    wire.out_tx
        .send(OutFrame::Text(ClientToHub::DeleteWorkspace {
            name: name.into(),
        }))
        .await
        .map_err(|_| anyhow!("hub disconnected"))?;
    loop {
        let m = expect_text(wire).await?;
        match m {
            HubToClient::WorkspaceDeleted { .. } => return Ok(()),
            HubToClient::SessionError { .. } => return Ok(()),
            HubToClient::Ping => {
                let _ = wire.out_tx.send(OutFrame::Text(ClientToHub::Pong)).await;
            }
            _ => continue,
        }
    }
}

async fn expect_text(wire: &mut Wire) -> Result<HubToClient> {
    wire.in_text_rx
        .recv()
        .await
        .ok_or_else(|| anyhow!("hub disconnected"))
}
