//! Interactive TUI menu shown before opening a PTY.
//!
//! v1.13 flow — single-stage workspace picker. Every workspace is
//! bound to an owning agent at create time, so the picker shows
//! one cross-agent list and Enter on a row routes directly to that
//! workspace's agent. Workspaces whose agent is offline render
//! greyed out and refuse to open with a toast.
//!
//! Keys on the picker:
//!   ↑↓ / j k / g G   move
//!   Enter            open the selected workspace
//!   c                create a workspace (prompts for name, then
//!                    picks an online agent)
//!   d                delete (confirm prompt)
//!   r                reset (kills the saved tmux + claude history)
//!   q / Esc          quit cloudcode

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
    /// User picked a workspace + its bound agent. Caller opens a
    /// session via OpenSession { workspace, agent, ... }.
    OpenWorkspace { agent: String, workspace: String },
    /// User pressed q / Esc at the workspace picker.
    Quit,
}

pub async fn run(
    wire: &mut Wire,
    bytes: &mut ByteRx,
    account: &str,
) -> Result<MenuOutcome> {
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(out);
    let mut term = Terminal::new(backend)?;
    let mut keys = MenuKeyQueue::default();
    let result = run_inner(&mut term, wire, bytes, &mut keys, account).await;
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
) -> Result<MenuOutcome> {
    // After a successful `c`reate the just-created workspace is
    // pinned as the next cursor target so the user can hit Enter
    // immediately to enter claude.
    let mut pending_select: Option<(String, String)> = None;
    let mut w_state = ListState::default();
    let title = "Select workspace";
    let hint = "↑↓ Enter · c create · r reset · d delete · q quit";
    loop {
        let workspaces = list_workspaces(wire).await?;
        // Disambiguate names that exist on more than one agent —
        // bare "proj" when unique, "proj@agentA" / "proj@agentB"
        // when colliding within the list.
        let mut name_counts: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::new();
        for w in &workspaces {
            *name_counts.entry(w.name.as_str()).or_insert(0) += 1;
        }
        let workspace_rows: Vec<PickerRow> = workspaces
            .iter()
            .map(|w| {
                let display = if name_counts.get(w.name.as_str()).copied().unwrap_or(0) > 1 {
                    format!("{}@{}", w.name, w.agent)
                } else {
                    w.name.clone()
                };
                PickerRow {
                    name: display,
                    badge: if !w.agent_online {
                        Some(Badge::offline(&w.agent))
                    } else if w.has_client {
                        Some(Badge::active())
                    } else if w.tmux_alive {
                        Some(Badge::saved())
                    } else {
                        Some(Badge::agent(&w.agent))
                    },
                }
            })
            .collect();
        let preferred = pending_select.take().or_else(|| {
            crate::read_last_workspace_global()
                .and_then(|s| s.split_once('|').map(|(a, n)| (a.to_string(), n.to_string())))
        });
        let initial = preferred
            .as_ref()
            .and_then(|(a, n)| {
                workspaces
                    .iter()
                    .position(|w| &w.agent == a && &w.name == n)
            })
            .unwrap_or(0);
        if w_state.selected().is_none() && !workspaces.is_empty() {
            w_state.select(Some(initial.min(workspaces.len() - 1)));
        }
        term.draw(|f| {
            draw_layout(
                f,
                account,
                title,
                &workspace_rows,
                &mut w_state,
                hint,
                false,
            )
        })?;
        let Some(k) = keys.next(bytes).await else {
            return Ok(MenuOutcome::Quit);
        };
        match k {
            MenuKey::Char('q') | MenuKey::Escape => return Ok(MenuOutcome::Quit),
            MenuKey::Char('c') => {
                if let Some((agent, name)) =
                    prompt_create_workspace(term, wire, bytes, keys).await?
                {
                    pending_select = Some((agent, name));
                    w_state.select(None);
                }
            }
            MenuKey::Char('d') => {
                if let Some(sel) = w_state.selected() {
                    if let Some(ws) = workspaces.get(sel).cloned() {
                        let confirmed = prompt_confirm(
                            term,
                            bytes,
                            keys,
                            &format!("delete workspace '{}' on agent '{}'?", ws.name, ws.agent),
                        )
                        .await?;
                        if confirmed {
                            if let Some(err) =
                                delete_workspace(wire, &ws.name, &ws.agent).await?
                            {
                                show_message(term, &err, bytes, keys).await?;
                            }
                            w_state.select(None);
                        }
                    }
                }
            }
            MenuKey::Char('r') => {
                if let Some(sel) = w_state.selected() {
                    if let Some(ws) = workspaces.get(sel).cloned() {
                        let confirmed = prompt_confirm(
                            term,
                            bytes,
                            keys,
                            &format!(
                                "reset session for '{}' on '{}'? Files stay; tmux + conversation history cleared.",
                                ws.name, ws.agent
                            ),
                        )
                        .await?;
                        if confirmed {
                            if let Some(err) = reset_workspace(wire, &ws.name, &ws.agent).await? {
                                show_message(term, &err, bytes, keys).await?;
                            }
                            w_state.select(None);
                        }
                    }
                }
            }
            _ => match handle_list_key(k, &mut w_state, workspaces.len()) {
                ListAction::Pick => {
                    if let Some(sel) = w_state.selected() {
                        if let Some(ws) = workspaces.get(sel).cloned() {
                            // The list snapshot's `agent_online` goes
                            // briefly stale right after a session exits,
                            // while the bound agent re-registers — so a
                            // "offline" snapshot is re-confirmed live before
                            // we believe it. A stale-offline that's actually
                            // back corrects here and opens; a genuinely
                            // offline agent gets a plain notice (Esc/any key
                            // back to the picker) — we do NOT try to connect
                            // to an offline agent.
                            if !ws.agent_online {
                                let online = list_agents(wire).await.unwrap_or_default();
                                if !online.iter().any(|a| a == &ws.agent) {
                                    show_message(
                                        term,
                                        &format!(
                                            "agent '{}' is offline; can't open '{}'",
                                            ws.agent, ws.name
                                        ),
                                        bytes,
                                        keys,
                                    )
                                    .await?;
                                    continue;
                                }
                            }
                            let mut redraw = |pressed: bool| -> Result<()> {
                                term.draw(|f| {
                                    draw_layout(
                                        f,
                                        account,
                                        title,
                                        &workspace_rows,
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
                            crate::write_last_workspace_global(&ws.agent, &ws.name);
                            return Ok(MenuOutcome::OpenWorkspace {
                                agent: ws.agent,
                                workspace: ws.name,
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

/// `c` create flow: prompts for name, then opens an inline agent
/// picker showing only online agents from the account's ACL.
/// Returns `(agent, name)` on success.
async fn prompt_create_workspace<B: ratatui::backend::Backend>(
    term: &mut Terminal<B>,
    wire: &mut Wire,
    bytes: &mut ByteRx,
    keys: &mut MenuKeyQueue,
) -> Result<Option<(String, String)>> {
    let Some(name) = prompt_input(term, bytes, keys, "create workspace", "").await? else {
        return Ok(None);
    };
    let name = name.trim().to_string();
    if name.is_empty() {
        return Ok(None);
    }
    let agents = list_agents(wire).await?;
    if agents.is_empty() {
        show_message(term, "no agents online", bytes, keys).await?;
        return Ok(None);
    }
    let Some(agent) = pick_agent_inline(term, bytes, keys, &agents, &name).await? else {
        return Ok(None);
    };
    if let Some(err) = create_workspace(wire, &name, &agent).await? {
        show_message(term, &err, bytes, keys).await?;
        return Ok(None);
    }
    Ok(Some((agent, name)))
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
/// The shadow stays at base + (2, 1). Terminal cells are roughly
/// twice as tall as they are wide, so +2 col / +1 row is the offset
/// that *looks* equal on screen. When `pressed` is true the dialog
/// moves +1 col / +1 row — half the shadow's horizontal travel and
/// the minimum representable vertical step. y overlaps the shadow's
/// own y but the shadow still shows on the right; the press reads as
/// a real two-axis tap. Springs back when `pressed = false`.
/// Word-wrap a single-paragraph message to `width` columns. Splits on
/// whitespace; any word longer than `width` is hard-broken so a long
/// URL or path can't blow out the dialog. Width is measured in chars
/// (close enough for the ASCII strings these dialogs carry — system
/// notices like "agent 'foo' is offline; can't open 'bar'").
fn wrap_text(msg: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![msg.to_string()];
    }
    let mut soft: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for word in msg.split_whitespace() {
        let w = word.chars().count();
        if cur_w == 0 {
            cur.push_str(word);
            cur_w = w;
        } else if cur_w + 1 + w <= width {
            cur.push(' ');
            cur.push_str(word);
            cur_w += 1 + w;
        } else {
            soft.push(std::mem::take(&mut cur));
            cur.push_str(word);
            cur_w = w;
        }
    }
    if !cur.is_empty() {
        soft.push(cur);
    }
    if soft.is_empty() {
        return vec![String::new()];
    }
    let mut out: Vec<String> = Vec::new();
    for line in soft {
        if line.chars().count() <= width {
            out.push(line);
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        for chunk in chars.chunks(width) {
            out.push(chunk.iter().collect());
        }
    }
    out
}

/// Inset (left/right padding) applied to dialog body text so it
/// doesn't kiss the border. Used by every helper that puts a
/// free-form message inside a dialog.
const DIALOG_TEXT_PAD: u16 = 2;

fn paint_dialog_frame(f: &mut ratatui::Frame, want_w: u16, want_h: u16, pressed: bool) -> Rect {
    let area = f.area();
    let w = want_w.min(area.width.saturating_sub(4));
    let h = want_h.min(area.height.saturating_sub(3));
    let base_x = area.x + (area.width.saturating_sub(w)) / 2;
    let base_y = area.y + (area.height.saturating_sub(h)) / 2;
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
    let shadow = Rect {
        x: base_x + 2,
        y: base_y + 1,
        width: w,
        height: h,
    };
    f.render_widget(
        Block::default().style(Style::default().bg(SHADOW_BG)),
        shadow,
    );
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
    items: &[PickerRow],
    state: &mut ListState,
    hint: &str,
    pressed: bool,
) {
    paint_desktop(f);

    let label_w = items
        .iter()
        .map(|r| r.name.chars().count() + r.badge.as_ref().map(|b| b.width() + 1).unwrap_or(0))
        .max()
        .unwrap_or(0);
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
            .map(|(i, r)| build_row(i, r, list_w, selected))
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

/// What the picker draws for one row. The agent picker passes
/// `badge: None` for every entry; the workspace picker fills it in
/// from `WorkspaceInfo`.
#[derive(Clone)]
pub struct PickerRow {
    pub name: String,
    pub badge: Option<Badge>,
}

#[derive(Clone)]
pub struct Badge {
    pub glyph: &'static str, // "●" or "·" or "◌"
    /// Owned because workspace badges carry dynamic agent names
    /// (e.g. ` [petez-laptop]`). Always pre-leaded with a space so
    /// the badge reads as separated from the workspace name.
    pub label: String,
    pub color: Color,
}

impl Badge {
    pub fn active() -> Self {
        Badge {
            glyph: "●",
            label: " active".into(),
            color: Color::Red,
        }
    }
    pub fn saved() -> Self {
        Badge {
            glyph: "·",
            label: " saved".into(),
            color: Color::Yellow,
        }
    }
    /// Shown on a workspace whose agent is online but no in-flight
    /// state. Carries the agent name so the user knows where it
    /// lives even when no name collision forces an `@agent` suffix.
    pub fn agent(name: &str) -> Self {
        Badge {
            glyph: "·",
            label: format!(" [{}]", name),
            color: Color::Cyan,
        }
    }
    /// Bound agent is currently offline — workspace shows but is
    /// not openable.
    pub fn offline(agent: &str) -> Self {
        Badge {
            glyph: "◌",
            label: format!(" {} offline", agent),
            color: Color::DarkGray,
        }
    }

    fn width(&self) -> usize {
        // glyph counts as 1 cell; label already starts with a leading space.
        self.glyph.chars().count() + self.label.chars().count()
    }
}

/// Build one list row, baking highlight + optional badge directly
/// into the line so we don't have to rely on ratatui's
/// `highlight_style` (which would clobber the badge colour).
///
/// Right margin: we reserve 1 cell of trailing padding so the badge's
/// right edge lines up with the `[account]` label one row up in the
/// title bar (that one also has a trailing space).
fn build_row(i: usize, row: &PickerRow, list_w: usize, selected: Option<usize>) -> ListItem<'static> {
    let prefix = format!("  {:>2}  ", i + 1);
    let badge_w = row.badge.as_ref().map(|b| b.width()).unwrap_or(0);
    let gutter = if row.badge.is_some() { 1 } else { 0 };
    let right_margin = 1usize;
    let used = prefix.chars().count() + row.name.chars().count() + gutter + badge_w + right_margin;
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
        let mut spans = vec![
            Span::styled(prefix, num_style),
            Span::styled(row.name.clone(), body_style),
            Span::styled(" ".repeat(pad + gutter), body_style),
        ];
        if let Some(b) = row.badge.as_ref() {
            spans.push(Span::styled(
                b.glyph,
                Style::default()
                    .bg(HILITE_BG)
                    .fg(b.color)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                b.label.clone(),
                Style::default().bg(HILITE_BG).fg(HILITE_FG),
            ));
        }
        spans.push(Span::styled(" ", body_style));
        return ListItem::new(Line::from(spans));
    }

    let mut spans = vec![
        Span::styled(
            prefix,
            Style::default()
                .bg(DIALOG_BG)
                .fg(NUM_FG)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            row.name.clone(),
            Style::default().bg(DIALOG_BG).fg(DIALOG_FG),
        ),
        Span::styled(" ".repeat(pad + gutter), Style::default().bg(DIALOG_BG)),
    ];
    if let Some(b) = row.badge.as_ref() {
        spans.push(Span::styled(
            b.glyph,
            Style::default()
                .bg(DIALOG_BG)
                .fg(b.color)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            b.label.clone(),
            Style::default().bg(DIALOG_BG).fg(Color::DarkGray),
        ));
    }
    spans.push(Span::styled(" ", Style::default().bg(DIALOG_BG)));
    ListItem::new(Line::from(spans))
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
    // Dialog width is fixed-ish; height grows with how many lines the
    // message wraps to. Reserve `DIALOG_TEXT_PAD` columns of breathing
    // room on either side so the text doesn't run flush against the
    // border, and a leading + trailing blank line inside the body.
    let dialog_w: u16 = 56;
    let text_w = (dialog_w as usize)
        .saturating_sub(2) // border
        .saturating_sub(DIALOG_TEXT_PAD as usize * 2);
    let lines = wrap_text(msg, text_w);
    // borders(2) + title bar(2) + top pad(1) + lines + bottom pad(1)
    let dialog_h: u16 = 6u16.saturating_add(lines.len() as u16);
    term.draw(|f| {
        let body = draw_titled_dialog(f, "cloudcode", dialog_w, dialog_h);
        let mut text: Vec<Line> = Vec::with_capacity(lines.len() + 2);
        text.push(Line::raw(""));
        for l in &lines {
            text.push(Line::from(Span::raw(format!(
                "{pad}{l}",
                pad = " ".repeat(DIALOG_TEXT_PAD as usize),
            ))));
        }
        f.render_widget(
            Paragraph::new(text).style(Style::default().bg(DIALOG_BG).fg(DIALOG_FG)),
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
    let dialog_w: u16 = 60;
    let text_w = (dialog_w as usize)
        .saturating_sub(2)
        .saturating_sub(DIALOG_TEXT_PAD as usize * 2);
    let wrapped = wrap_text(msg, text_w);
    // borders(2) + title bar(2) + top pad(1) + lines + gap(1) + buttons(1) + bottom pad(1)
    let dialog_h: u16 = 8u16.saturating_add(wrapped.len() as u16);
    let lines_len = wrapped.len() as u16;
    let draw = |term: &mut Terminal<B>, pressed_yes: bool| -> Result<()> {
        let wrapped = wrapped.clone();
        term.draw(move |f| {
            let body = draw_titled_dialog(f, "Confirm", dialog_w, dialog_h);
            let body_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),          // top pad
                    Constraint::Length(lines_len),  // message
                    Constraint::Length(1),          // gap
                    Constraint::Length(1),          // buttons
                    Constraint::Min(0),
                ])
                .split(body);
            let msg_lines: Vec<Line> = wrapped
                .into_iter()
                .map(|l| {
                    Line::from(Span::raw(format!(
                        "{pad}{l}",
                        pad = " ".repeat(DIALOG_TEXT_PAD as usize),
                    )))
                })
                .collect();
            f.render_widget(
                Paragraph::new(msg_lines).style(Style::default().bg(DIALOG_BG).fg(DIALOG_FG)),
                body_chunks[1],
            );
            let yes = ok_button("Yes", pressed_yes);
            let no = Span::styled("  < No >  ", Style::default().bg(DIALOG_BG).fg(DIALOG_FG));
            f.render_widget(
                Paragraph::new(
                    Line::from(vec![yes, Span::raw("    "), no]).alignment(Alignment::Center),
                )
                .style(Style::default().bg(DIALOG_BG)),
                body_chunks[3],
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

/// Used by the create flow: present an inline agent picker over
/// the supplied list (no extra round-trip), titled with what's
/// being created. Returns None on Esc / q.
async fn pick_agent_inline<B: ratatui::backend::Backend>(
    term: &mut Terminal<B>,
    bytes: &mut ByteRx,
    keys: &mut MenuKeyQueue,
    agents: &[String],
    workspace_name: &str,
) -> Result<Option<String>> {
    if agents.is_empty() {
        return Ok(None);
    }
    let mut a_state = ListState::default();
    a_state.select(Some(0));
    let rows: Vec<PickerRow> = agents
        .iter()
        .map(|n| PickerRow {
            name: n.clone(),
            badge: None,
        })
        .collect();
    let title = format!("Create '{}' on which agent?", workspace_name);
    let hint = "↑↓ move · Enter pick · Esc cancel";
    loop {
        term.draw(|f| {
            draw_layout(f, "", &title, &rows, &mut a_state, hint, false)
        })?;
        let Some(k) = keys.next(bytes).await else {
            return Ok(None);
        };
        match handle_list_key(k, &mut a_state, agents.len()) {
            ListAction::Pick => {
                return Ok(Some(agents[a_state.selected().unwrap_or(0)].clone()));
            }
            ListAction::Quit => return Ok(None),
            ListAction::Pass => {}
        }
    }
}

/// Render the agent picker and return the chosen agent's name. Reads
/// `crate::read_last_agent()` lazily so an Esc-back from stage 2
/// highlights the agent the user just stepped away from, even within
/// the same `menu::run` invocation.
#[allow(dead_code)]
async fn pick_agent_stage<B: ratatui::backend::Backend>(
    term: &mut Terminal<B>,
    wire: &mut Wire,
    bytes: &mut ByteRx,
    keys: &mut MenuKeyQueue,
    account: &str,
) -> Result<Option<String>> {
    loop {
        let agents = list_agents(wire).await?;
        if agents.is_empty() {
            show_message(term, "no agents online", bytes, keys).await?;
            return Ok(None);
        }
        let mut a_state = ListState::default();
        // Read the last-used agent now (not when menu::run started)
        // so the highlight tracks the most recent selection.
        let initial = crate::read_last_agent()
            .as_deref()
            .and_then(|n| agents.iter().position(|a| a == n))
            .unwrap_or(0);
        a_state.select(Some(initial));

        let agent_rows: Vec<PickerRow> = agents
            .iter()
            .map(|n| PickerRow {
                name: n.clone(),
                badge: None,
            })
            .collect();
        let picked = loop {
            term.draw(|f| {
                draw_layout(
                    f,
                    account,
                    "Select agent",
                    &agent_rows,
                    &mut a_state,
                    "↑↓ move · Enter pick · Esc/q quit",
                    false,
                )
            })?;
            let Some(k) = keys.next(bytes).await else {
                return Ok(None);
            };
            match handle_list_key(k, &mut a_state, agents.len()) {
                ListAction::Pick => {
                    let p = agents[a_state.selected().unwrap_or(0)].clone();
                    let mut redraw = |pressed: bool| -> Result<()> {
                        term.draw(|f| {
                            draw_layout(
                                f,
                                account,
                                "Select agent",
                                &agent_rows,
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
                    break p;
                }
                ListAction::Quit => return Ok(None),
                ListAction::Pass => {}
            }
        };
        // bind to the chosen agent
        wire.out_tx
            .send(OutFrame::Text(ClientToHub::SelectAgent {
                agent: Some(picked.clone()),
            }))
            .await
            .map_err(|_| anyhow!("hub disconnected"))?;
        match expect_text(wire).await? {
            HubToClient::AgentSelected { .. } => {
                crate::write_last_agent(&picked);
                return Ok(Some(picked));
            }
            HubToClient::SessionError { message } => {
                show_message(term, &format!("error: {}", message), bytes, keys).await?;
                // retry the picker (agent may have just gone offline)
                continue;
            }
            _ => continue,
        }
    }
}

/// Fast-path bind: ask the hub to select `target` directly, no UI.
/// Returns Some(target) on success, None on any miss so the caller
/// can fall back to the normal agent picker. Used after claude exits
/// to land the user back in the workspace picker for the agent they
/// were using.
#[allow(dead_code)]
async fn fast_bind(wire: &mut Wire, target: &str) -> Option<String> {
    // Sanity-check against the hub's view first: if the agent isn't
    // in the allow-listed online set, don't even attempt the bind
    // (otherwise we'd spam a SessionError that the caller would
    // immediately have to swallow).
    let agents = list_agents(wire).await.ok()?;
    if !agents.iter().any(|a| a == target) {
        return None;
    }
    wire.out_tx
        .send(OutFrame::Text(ClientToHub::SelectAgent {
            agent: Some(target.to_string()),
        }))
        .await
        .ok()?;
    match expect_text(wire).await.ok()? {
        HubToClient::AgentSelected { .. } => Some(target.to_string()),
        _ => None,
    }
}

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

async fn list_workspaces(wire: &mut Wire) -> Result<Vec<crate::proto::WorkspaceInfo>> {
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

/// `Ok(None)` = hub accepted; `Ok(Some(err))` = hub returned a
/// SessionError with a human-readable reason the caller should
/// surface in the TUI. Returns `Err` only on transport failure.
async fn create_workspace(wire: &mut Wire, name: &str, agent: &str) -> Result<Option<String>> {
    wire.out_tx
        .send(OutFrame::Text(ClientToHub::CreateWorkspace {
            name: name.into(),
            agent: agent.into(),
        }))
        .await
        .map_err(|_| anyhow!("hub disconnected"))?;
    loop {
        let m = expect_text(wire).await?;
        match m {
            HubToClient::WorkspaceCreated { .. } => return Ok(None),
            HubToClient::SessionError { message } => return Ok(Some(message)),
            HubToClient::Ping => {
                let _ = wire.out_tx.send(OutFrame::Text(ClientToHub::Pong)).await;
            }
            _ => continue,
        }
    }
}

async fn delete_workspace(wire: &mut Wire, name: &str, agent: &str) -> Result<Option<String>> {
    wire.out_tx
        .send(OutFrame::Text(ClientToHub::DeleteWorkspace {
            name: name.into(),
            agent: agent.into(),
        }))
        .await
        .map_err(|_| anyhow!("hub disconnected"))?;
    loop {
        let m = expect_text(wire).await?;
        match m {
            HubToClient::WorkspaceDeleted { .. } => return Ok(None),
            HubToClient::SessionError { message } => return Ok(Some(message)),
            HubToClient::Ping => {
                let _ = wire.out_tx.send(OutFrame::Text(ClientToHub::Pong)).await;
            }
            _ => continue,
        }
    }
}

async fn reset_workspace(wire: &mut Wire, name: &str, agent: &str) -> Result<Option<String>> {
    wire.out_tx
        .send(OutFrame::Text(ClientToHub::ResetWorkspace {
            name: name.into(),
            agent: agent.into(),
        }))
        .await
        .map_err(|_| anyhow!("hub disconnected"))?;
    loop {
        let m = expect_text(wire).await?;
        match m {
            HubToClient::WorkspaceReset { .. } => return Ok(None),
            HubToClient::SessionError { message } => return Ok(Some(message)),
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

#[cfg(test)]
mod tests {
    use super::wrap_text;

    #[test]
    fn wraps_on_word_boundary() {
        let out = wrap_text("agent 'PeteMacBookPro' is offline; can't open 'ws1'", 20);
        assert!(out.len() >= 2);
        for line in &out {
            assert!(line.chars().count() <= 20, "line too long: {:?}", line);
        }
    }

    #[test]
    fn hard_breaks_overlong_words() {
        let out = wrap_text("supercalifragilisticexpialidocious", 8);
        assert_eq!(out.len(), 5);
        assert_eq!(out[0], "supercal");
    }

    #[test]
    fn short_message_stays_one_line() {
        let out = wrap_text("hello world", 40);
        assert_eq!(out, vec!["hello world".to_string()]);
    }

    #[test]
    fn empty_message_yields_one_blank_line() {
        let out = wrap_text("", 20);
        assert_eq!(out, vec![String::new()]);
    }
}
