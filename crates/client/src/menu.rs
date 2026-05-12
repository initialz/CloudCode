//! Interactive TUI menu shown before opening a PTY.
//!
//! Two stages:
//!   1. Pick an agent (arrow keys + Enter).
//!   2. Pick a workspace (arrow keys + Enter); `c` creates a new one (text
//!      input prompt), `d` deletes the highlighted one (confirm prompt),
//!      `Esc` goes back to the agent picker.
//!
//! Esc / `q` at the agent picker quits cloudcode.

use crate::input::{parse_keys, ByteRx, MenuKey};
use crate::proto::{ClientToHub, HubToClient};
use crate::wire::{OutFrame, Wire};
use anyhow::{anyhow, Result};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Terminal;
use std::io::stdout;

pub enum MenuOutcome {
    OpenWorkspace { agent: String, workspace: String },
    Quit,
}

pub async fn run(
    wire: &mut Wire,
    bytes: &mut ByteRx,
    account: &str,
    last_agent: Option<&str>,
) -> Result<MenuOutcome> {
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(out);
    let mut term = Terminal::new(backend)?;
    let mut keys = MenuKeyQueue::default();
    let result = run_inner(&mut term, wire, bytes, &mut keys, account, last_agent).await;
    disable_raw_mode().ok();
    execute!(term.backend_mut(), LeaveAlternateScreen).ok();
    term.show_cursor().ok();
    result
}

#[derive(Default)]
struct MenuKeyQueue {
    pending: std::collections::VecDeque<MenuKey>,
}

impl MenuKeyQueue {
    async fn next(&mut self, bytes: &mut ByteRx) -> Option<MenuKey> {
        loop {
            if let Some(k) = self.pending.pop_front() {
                return Some(k);
            }
            let chunk = bytes.recv().await?;
            self.pending.extend(parse_keys(&chunk));
        }
    }
}

async fn run_inner<B: ratatui::backend::Backend>(
    term: &mut Terminal<B>,
    wire: &mut Wire,
    bytes: &mut ByteRx,
    keys: &mut MenuKeyQueue,
    account: &str,
    last_agent: Option<&str>,
) -> Result<MenuOutcome> {
    'outer: loop {
        // ---- stage 1: agent picker ----
        let agents = list_agents(wire).await?;
        if agents.is_empty() {
            show_message(term, "no agents online", bytes, keys).await?;
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
                    false,
                )
            })?;
            let Some(k) = keys.next(bytes).await else {
                return Ok(MenuOutcome::Quit);
            };
            match handle_list_key(k, &mut a_state, agents.len()) {
                ListAction::Pick => {
                    let picked = agents[a_state.selected().unwrap_or(0)].clone();
                    let mut redraw = |pressed: bool| -> Result<()> {
                        term.draw(|f| {
                            draw_layout(
                                f,
                                account,
                                "Select agent",
                                &agents,
                                &mut a_state,
                                "↑↓ move · Enter pick · Esc/q quit",
                                pressed,
                            )
                        })?;
                        Ok(())
                    };
                    redraw(true)?;
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    redraw(false)?;
                    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
                    break picked;
                }
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
                show_message(term, &format!("error: {}", message), bytes, keys).await?;
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
                    false,
                )
            })?;
            let Some(k) = keys.next(bytes).await else {
                return Ok(MenuOutcome::Quit);
            };
            match k {
                MenuKey::Escape => continue 'outer,
                MenuKey::Char('q') => return Ok(MenuOutcome::Quit),
                MenuKey::Char('c') => {
                    if let Some(name) =
                        prompt_input(term, bytes, keys, "create workspace", "").await?
                    {
                        let name = name.trim().to_string();
                        if !name.is_empty() {
                            create_workspace(wire, &name).await?;
                            w_state.select(None);
                        }
                    }
                }
                MenuKey::Char('d') => {
                    if let Some(sel) = w_state.selected() {
                        if let Some(ws) = workspaces.get(sel) {
                            let confirmed = prompt_confirm(
                                term,
                                bytes,
                                keys,
                                &format!("delete workspace '{}'?", ws),
                            )
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
                            if let Some(ws) = workspaces.get(sel).cloned() {
                                let title = format!("Select workspace on {}", agent);
                                let hint = "↑↓ move · Enter pick · c create · d delete · Esc back · q quit";
                                let mut redraw = |pressed: bool| -> Result<()> {
                                    term.draw(|f| {
                                        draw_layout(
                                            f,
                                            account,
                                            &title,
                                            &workspaces,
                                            &mut w_state,
                                            hint,
                                            pressed,
                                        )
                                    })?;
                                    Ok(())
                                };
                                redraw(true)?;
                                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                                redraw(false)?;
                                tokio::time::sleep(std::time::Duration::from_millis(60)).await;
                                return Ok(MenuOutcome::OpenWorkspace {
                                    agent,
                                    workspace: ws,
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

fn handle_list_key(k: MenuKey, state: &mut ListState, len: usize) -> ListAction {
    if len == 0 {
        if matches!(k, MenuKey::Escape | MenuKey::Char('q')) {
            return ListAction::Quit;
        }
        return ListAction::Pass;
    }
    let cur = state.selected().unwrap_or(0);
    match k {
        MenuKey::Up | MenuKey::Char('k') => {
            state.select(Some(if cur == 0 { len - 1 } else { cur - 1 }));
            ListAction::Pass
        }
        MenuKey::Down | MenuKey::Char('j') => {
            state.select(Some((cur + 1) % len));
            ListAction::Pass
        }
        MenuKey::Home | MenuKey::Char('g') => {
            state.select(Some(0));
            ListAction::Pass
        }
        MenuKey::End | MenuKey::Char('G') => {
            state.select(Some(len - 1));
            ListAction::Pass
        }
        MenuKey::Enter => ListAction::Pick,
        MenuKey::Escape => ListAction::Quit,
        MenuKey::Char('q') => ListAction::Quit,
        MenuKey::Ctrl(3) => ListAction::Quit, // Ctrl+C
        _ => ListAction::Pass,
    }
}

// ---------- retro dialog styling ----------

const DESKTOP_BG: Color = Color::Blue;
const DIALOG_BG: Color = Color::White;
const DIALOG_FG: Color = Color::Black;
const SHADOW_BG: Color = Color::Black;
const HILITE_BG: Color = Color::Blue;
const HILITE_FG: Color = Color::White;
const NUM_FG: Color = Color::Red;

fn paint_desktop(f: &mut ratatui::Frame) {
    let area = f.area();
    f.render_widget(
        Block::default().style(Style::default().bg(DESKTOP_BG)),
        area,
    );
}

/// Centered dialog rect, plus a 2-col / 1-row drop shadow drawn behind it.
/// When `pressed` is true, the dialog falls onto its shadow (same +1
/// / +1 offset as the shadow itself) and the shadow is hidden, giving
/// an "Enter held down" look. The next frame redrawn with
/// `pressed = false` produces the spring-back.
fn paint_dialog_frame(f: &mut ratatui::Frame, want_w: u16, want_h: u16, pressed: bool) -> Rect {
    let area = f.area();
    let w = want_w.min(area.width.saturating_sub(2));
    let h = want_h.min(area.height.saturating_sub(2));
    let base_x = area.x + (area.width.saturating_sub(w + 1)) / 2;
    let base_y = area.y + (area.height.saturating_sub(h + 1)) / 2;
    let dialog = if pressed {
        Rect {
            x: base_x + 1,
            y: base_y + 1,
            width: w,
            height: h,
        }
    } else {
        Rect {
            x: base_x,
            y: base_y,
            width: w,
            height: h,
        }
    };
    if !pressed {
        let shadow = Rect {
            x: dialog.x + 1,
            y: dialog.y + 1,
            width: dialog.width,
            height: dialog.height,
        };
        f.render_widget(
            Block::default().style(Style::default().bg(SHADOW_BG)),
            shadow,
        );
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .style(Style::default().bg(DIALOG_BG).fg(DIALOG_FG));
    let inner = block.inner(dialog);
    f.render_widget(block, dialog);
    inner
}

/// Render the "primary" (Enter-triggered) button. When `pressed` is true,
/// it switches to a depressed look — angle brackets become square ones,
/// the bevel inverts, and the colour dims — so the user gets a moment of
/// "click" feedback before the action fires.
fn ok_button(label: &str, pressed: bool) -> Span<'static> {
    if pressed {
        Span::styled(
            format!("  [ {} ]  ", label),
            Style::default()
                .bg(Color::DarkGray)
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            format!("  < {} >  ", label),
            Style::default()
                .bg(HILITE_BG)
                .fg(HILITE_FG)
                .add_modifier(Modifier::BOLD),
        )
    }
}

const LOGO: &[&str] = &[
    "   ____ _                 _  ____          _      ",
    "  / ___| | ___  _   _  __| |/ ___|___   __| | ___ ",
    " | |   | |/ _ \\| | | |/ _` | |   / _ \\ / _` |/ _ \\",
    " | |___| | (_) | |_| | (_| | |__| (_) | (_| |  __/",
    "  \\____|_|\\___/ \\__,_|\\__,_|\\____\\___/ \\__,_|\\___|",
];
const LOGO_W: u16 = 51;
const LOGO_H: u16 = 5;

fn render_logo(f: &mut ratatui::Frame, area: Rect) {
    let lines: Vec<Line<'static>> = LOGO
        .iter()
        .map(|row| {
            Line::from(Span::styled(
                row.to_string(),
                Style::default()
                    .bg(DIALOG_BG)
                    .fg(HILITE_BG)
                    .add_modifier(Modifier::BOLD),
            ))
            .alignment(Alignment::Center)
        })
        .collect();
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(DIALOG_BG)),
        area,
    );
}

fn hint_bar(f: &mut ratatui::Frame, hint: &str) {
    let area = f.area();
    if area.height == 0 {
        return;
    }
    let rect = Rect {
        x: area.x,
        y: area.y + area.height - 1,
        width: area.width,
        height: 1,
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {} ", hint),
            Style::default().bg(DESKTOP_BG).fg(Color::Gray),
        ))),
        rect,
    );
}

fn draw_layout(
    f: &mut ratatui::Frame,
    account: &str,
    title: &str,
    items: &[String],
    state: &mut ListState,
    hint: &str,
    pressed: bool,
) {
    paint_desktop(f);

    let label_w = items.iter().map(|s| s.chars().count()).max().unwrap_or(0);
    let want_w = ((label_w + 18)
        .max(title.chars().count() + account.len() + 12)
        .max((LOGO_W + 6) as usize)) as u16;
    let want_h = (items.len() as u16 + LOGO_H + 6).max(LOGO_H + 8);

    let inner = paint_dialog_frame(f, want_w, want_h, pressed);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(LOGO_H), // logo
            Constraint::Length(1),      // rule
            Constraint::Length(1),      // title
            Constraint::Length(1),      // rule
            Constraint::Min(3),         // list
        ])
        .split(inner);

    render_logo(f, chunks[0]);

    let rule_w = chunks[1].width as usize;
    let rule_style = Style::default().bg(DIALOG_BG).fg(DIALOG_FG);
    f.render_widget(
        Paragraph::new(Span::styled("─".repeat(rule_w), rule_style)),
        chunks[1],
    );
    f.render_widget(
        Paragraph::new(Span::styled("─".repeat(rule_w), rule_style)),
        chunks[3],
    );

    // title row: " Title:                          [account] "
    let acct_label = format!("[{}]", account);
    let title_text = format!(" {}:", title);
    let pad = (chunks[2].width as usize)
        .saturating_sub(title_text.chars().count() + acct_label.chars().count() + 1);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                title_text,
                Style::default()
                    .fg(DIALOG_FG)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" ".repeat(pad)),
            Span::styled(acct_label, Style::default().fg(Color::DarkGray)),
            Span::raw(" "),
        ]))
        .style(Style::default().bg(DIALOG_BG)),
        chunks[2],
    );

    let list_w = chunks[4].width as usize;
    let selected = state.selected();
    let list_items: Vec<ListItem> = if items.is_empty() {
        let txt = "  (empty — press `c` to create)";
        let pad = list_w.saturating_sub(txt.chars().count());
        vec![ListItem::new(Line::from(vec![
            Span::styled(txt, Style::default().fg(Color::DarkGray).bg(DIALOG_BG)),
            Span::raw(" ".repeat(pad)),
        ]))]
    } else {
        items
            .iter()
            .enumerate()
            .map(|(i, s)| build_row(i, s, list_w, selected))
            .collect()
    };
    // We bake the highlight directly into the items, so the List widget
    // doesn't need its own highlight_style — that would just paint over
    // what we already drew.
    let list = List::new(list_items)
        .style(Style::default().bg(DIALOG_BG))
        .highlight_symbol("");
    let mut blank_state = ListState::default();
    f.render_stateful_widget(list, chunks[4], &mut blank_state);

    hint_bar(f, hint);
}

/// Build one list row, baking the highlight directly into the line so we
/// don't have to rely on ratatui's `highlight_style` (which would
/// override Span colours we set deliberately).
fn build_row(i: usize, name: &str, list_w: usize, selected: Option<usize>) -> ListItem<'static> {
    let prefix = format!("  {:>2}  ", i + 1);
    let used = prefix.chars().count() + name.chars().count();
    let pad = list_w.saturating_sub(used);

    if selected == Some(i) {
        let num_style = Style::default()
            .bg(HILITE_BG)
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD);
        let body_style = Style::default()
            .bg(HILITE_BG)
            .fg(HILITE_FG)
            .add_modifier(Modifier::BOLD);
        return ListItem::new(Line::from(vec![
            Span::styled(prefix, num_style),
            Span::styled(name.to_string(), body_style),
            Span::styled(" ".repeat(pad), body_style),
        ]));
    }

    ListItem::new(Line::from(vec![
        Span::styled(
            prefix,
            Style::default()
                .bg(DIALOG_BG)
                .fg(NUM_FG)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            name.to_string(),
            Style::default().bg(DIALOG_BG).fg(DIALOG_FG),
        ),
        Span::styled(" ".repeat(pad), Style::default().bg(DIALOG_BG)),
    ]))
}

fn draw_titled_dialog(
    f: &mut ratatui::Frame,
    title: &str,
    want_w: u16,
    want_h: u16,
) -> Rect {
    paint_desktop(f);
    let inner = paint_dialog_frame(f, want_w, want_h, false);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);

    let pad = (chunks[0].width as usize).saturating_sub(title.chars().count() + 2);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" {}:", title),
                Style::default()
                    .fg(DIALOG_FG)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" ".repeat(pad)),
            Span::raw(" "),
        ]))
        .style(Style::default().bg(DIALOG_BG)),
        chunks[0],
    );
    f.render_widget(
        Paragraph::new(Span::styled(
            "─".repeat(chunks[1].width as usize),
            Style::default().bg(DIALOG_BG).fg(DIALOG_FG),
        )),
        chunks[1],
    );
    chunks[2]
}

async fn show_message<B: ratatui::backend::Backend>(
    term: &mut Terminal<B>,
    msg: &str,
    bytes: &mut ByteRx,
    keys: &mut MenuKeyQueue,
) -> Result<()> {
    let msg_owned = msg.to_string();
    term.draw(|f| {
        let body = draw_titled_dialog(f, "cloudcode", 50, 7);
        f.render_widget(
            Paragraph::new(Line::from(Span::raw(msg_owned)))
                .style(Style::default().bg(DIALOG_BG).fg(DIALOG_FG)),
            body,
        );
        hint_bar(f, "Any key to continue");
    })?;
    let _ = keys.next(bytes).await;
    Ok(())
}

async fn prompt_input<B: ratatui::backend::Backend>(
    term: &mut Terminal<B>,
    bytes: &mut ByteRx,
    keys: &mut MenuKeyQueue,
    title: &str,
    initial: &str,
) -> Result<Option<String>> {
    let mut buf = initial.to_string();
    loop {
        let title_owned = title.to_string();
        let buf_view = buf.clone();
        term.draw(move |f| {
            let body = draw_titled_dialog(f, &title_owned, 60, 7);
            let body_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(1)])
                .split(body);
            let para = Paragraph::new(Line::from(vec![
                Span::styled("  > ", Style::default().bg(DIALOG_BG).fg(NUM_FG)),
                Span::styled(
                    buf_view.clone(),
                    Style::default().bg(DIALOG_BG).fg(DIALOG_FG),
                ),
                Span::styled("█", Style::default().bg(DIALOG_BG).fg(HILITE_BG)),
            ]))
            .style(Style::default().bg(DIALOG_BG));
            f.render_widget(para, body_chunks[0]);
            f.render_widget(
                Paragraph::new(Span::styled(
                    "                ",
                    Style::default().bg(DIALOG_BG),
                )),
                body_chunks[1],
            );
            hint_bar(f, "Enter accept · Esc cancel");
        })?;
        let Some(k) = keys.next(bytes).await else {
            return Ok(None);
        };
        match k {
            MenuKey::Escape => return Ok(None),
            MenuKey::Enter => return Ok(Some(buf)),
            MenuKey::Backspace => {
                buf.pop();
            }
            MenuKey::Char(c) => {
                buf.push(c);
            }
            _ => {}
        }
    }
}

async fn prompt_confirm<B: ratatui::backend::Backend>(
    term: &mut Terminal<B>,
    bytes: &mut ByteRx,
    keys: &mut MenuKeyQueue,
    msg: &str,
) -> Result<bool> {
    let draw = |term: &mut Terminal<B>, pressed_yes: bool| -> Result<()> {
        let msg_owned = msg.to_string();
        term.draw(move |f| {
            let body = draw_titled_dialog(f, "Confirm", 56, 8);
            let body_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Min(0),
                ])
                .split(body);
            f.render_widget(
                Paragraph::new(Line::from(Span::raw(format!("  {}", msg_owned))))
                    .style(Style::default().bg(DIALOG_BG).fg(DIALOG_FG)),
                body_chunks[0],
            );
            let yes = ok_button("Yes", pressed_yes);
            let no = Span::styled("  < No >  ", Style::default().bg(DIALOG_BG).fg(DIALOG_FG));
            f.render_widget(
                Paragraph::new(
                    Line::from(vec![yes, Span::raw("    "), no]).alignment(Alignment::Center),
                )
                .style(Style::default().bg(DIALOG_BG)),
                body_chunks[2],
            );
            hint_bar(f, "y/Enter yes · n/Esc no");
        })?;
        Ok(())
    };
    loop {
        draw(term, false)?;
        let Some(k) = keys.next(bytes).await else {
            return Ok(false);
        };
        match k {
            MenuKey::Char('y') | MenuKey::Char('Y') | MenuKey::Enter => {
                draw(term, true)?;
                tokio::time::sleep(std::time::Duration::from_millis(120)).await;
                return Ok(true);
            }
            MenuKey::Char('n') | MenuKey::Char('N') | MenuKey::Escape => return Ok(false),
            _ => {}
        }
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
