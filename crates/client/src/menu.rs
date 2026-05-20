//! Interactive TUI menu shown before opening a PTY.
//!
//! v1.13 flow — workspace-first, agent-second:
//!   1. Pick a workspace (per-account, hub-canonical). `c` creates, `d`
//!      deletes, `r` resets. Esc / `q` quits.
//!   2. Pick an agent (list of online agents). Esc goes back to stage 1.
//!
//! The remembered workspace (`last_workspace_path`) and agent are stored in
//! the state dir so subsequent launches pre-select the same pair.

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
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Terminal;
use std::io::stdout;

pub enum MenuOutcome {
    OpenWorkspace { workspace: String, agent: String },
    /// User quit the menu. `from_workspace_picker` is true when the quit
    /// happened on the stage-1 workspace picker (so the caller can clear
    /// the remembered workspace). False means they quit from the agent
    /// picker; the caller should preserve the workspace for next launch.
    Quit { from_workspace_picker: bool },
}

/// Where the menu should enter.
/// After claude exits the caller passes `AgentPicker { workspace }` so
/// the user lands on the agent picker for the same workspace they used.
pub enum MenuStart {
    WorkspacePicker,
    AgentPicker { workspace: String },
}

/// Standalone confirm dialog used when the hub rejects an OpenSession
/// with `workspace_locked`. Returns true if the user wants to retry
/// with force=true (i.e. take the workspace away from `holder`).
/// Sets up its own TUI scope, mirroring `menu::run`.
pub async fn confirm_force_take(
    bytes: &mut ByteRx,
    workspace: &str,
    holder: &str,
) -> Result<bool> {
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(out);
    let mut term = Terminal::new(backend)?;
    let mut keys = MenuKeyQueue::default();
    let msg = format!(
        "Workspace '{}' is held by agent '{}'. Take it over? \
        Any local edits there that haven't pushed will be lost.",
        workspace, holder
    );
    let result = prompt_confirm(&mut term, bytes, &mut keys, &msg).await;
    disable_raw_mode().ok();
    execute!(term.backend_mut(), LeaveAlternateScreen).ok();
    term.show_cursor().ok();
    result
}

pub async fn run(
    wire: &mut Wire,
    bytes: &mut ByteRx,
    account: &str,
    start: MenuStart,
    pending_error: Option<String>,
) -> Result<MenuOutcome> {
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(out);
    let mut term = Terminal::new(backend)?;
    let mut keys = MenuKeyQueue::default();
    // Surface any error the caller picked up after the last menu run
    // (e.g. SessionError from a failed OpenSession). Without this the
    // error gets eprintln'd between alt-screen tear-down and re-entry
    // and the user effectively sees nothing — looks like a silent
    // bounce back to the picker.
    if let Some(msg) = pending_error {
        show_message(&mut term, &msg, bytes, &mut keys).await.ok();
    }
    let result = run_inner(&mut term, wire, bytes, &mut keys, account, start).await;
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
    start: MenuStart,
) -> Result<MenuOutcome> {
    // Fast-path: if we know the workspace to land on (e.g. after claude exits),
    // skip straight to the agent picker for that workspace.
    let mut pending_fast_ws: Option<String> = match start {
        MenuStart::AgentPicker { workspace } => Some(workspace),
        MenuStart::WorkspacePicker => None,
    };

    'outer: loop {
        // ---- stage 1: workspace picker ----
        let fast_ws = pending_fast_ws.take();
        let workspace = match pick_workspace_stage(term, wire, bytes, keys, account, fast_ws).await? {
            Some(ws) => ws,
            None => return Ok(MenuOutcome::Quit { from_workspace_picker: true }),
        };

        // ---- stage 2: agent picker (loop until pick or Esc back) ----
        let last_agent = crate::read_last_agent();
        let mut a_state = ListState::default();
        loop {
            let agents = list_agents(wire).await?;
            if agents.is_empty() {
                show_message(term, "no agents online", bytes, keys).await?;
                continue 'outer;
            }
            // Re-fetch the workspace list so the green-dot "currently
            // holds this workspace" badge stays accurate even when
            // another client force-takes between two renders. Cheap
            // — one extra round-trip per agent-picker entry, not per
            // keypress.
            let lock_holder = list_workspaces(wire)
                .await?
                .into_iter()
                .find(|w| w.name == workspace)
                .and_then(|w| w.locked_by_agent);
            let agent_rows: Vec<PickerRow> = agents
                .iter()
                .map(|n| PickerRow {
                    name: n.clone(),
                    badge: if lock_holder.as_deref() == Some(n.as_str()) {
                        Some(Badge::current_holder())
                    } else {
                        None
                    },
                })
                .collect();
            if a_state.selected().is_none() {
                let initial = last_agent
                    .as_deref()
                    .and_then(|n| agents.iter().position(|a| a == n))
                    .unwrap_or(0);
                a_state.select(Some(initial.min(agents.len().saturating_sub(1))));
            }
            let title = format!("Open '{}' with agent", workspace);
            let hint = "↑↓ Enter pick · Esc back · q quit";
            term.draw(|f| {
                draw_layout(f, account, &title, &agent_rows, &mut a_state, hint, false)
            })?;
            let Some(k) = keys.next(bytes).await else {
                return Ok(MenuOutcome::Quit { from_workspace_picker: false });
            };
            match k {
                MenuKey::Escape => continue 'outer,
                MenuKey::Char('q') => return Ok(MenuOutcome::Quit { from_workspace_picker: false }),
                _ => match handle_list_key(k, &mut a_state, agents.len()) {
                    ListAction::Pick => {
                        if let Some(sel) = a_state.selected() {
                            if let Some(agent) = agents.get(sel).cloned() {
                                let title2 = format!("Open '{}' with agent", workspace);
                                let mut redraw = |pressed: bool| -> Result<()> {
                                    term.draw(|f| {
                                        draw_layout(
                                            f,
                                            account,
                                            &title2,
                                            &agent_rows,
                                            &mut a_state,
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
                                crate::write_last_agent(&agent);
                                return Ok(MenuOutcome::OpenWorkspace { workspace, agent });
                            }
                        }
                    }
                    ListAction::Quit => {
                        return Ok(MenuOutcome::Quit { from_workspace_picker: false })
                    }
                    ListAction::Pass => {}
                },
            }
        }
    }
}

/// Stage 1: workspace picker. Returns the chosen workspace name, or None if
/// the user quit. `fast_ws` is a pre-selected workspace name (skips initial
/// selection UI but still shows the list for orientation).
async fn pick_workspace_stage<B: ratatui::backend::Backend>(
    term: &mut Terminal<B>,
    wire: &mut Wire,
    bytes: &mut ByteRx,
    keys: &mut MenuKeyQueue,
    account: &str,
    fast_ws: Option<String>,
) -> Result<Option<String>> {
    let last_ws = crate::read_last_workspace_global();
    let mut w_state = ListState::default();
    let mut pending_select: Option<String> = fast_ws.or(last_ws);
    loop {
        let workspaces = list_workspaces(wire).await?;
        let workspace_rows: Vec<PickerRow> = workspaces
            .iter()
            .map(|w| PickerRow {
                name: w.name.clone(),
                badge: w.locked_by_agent.as_ref().map(|a| Badge::locked(a)),
            })
            .collect();
        let names: Vec<&str> = workspaces.iter().map(|w| w.name.as_str()).collect();
        if let Some(idx) = decide_initial_selection(
            pending_select.take().as_deref(),
            w_state.selected(),
            &names,
        ) {
            w_state.select(Some(idx));
        }
        term.draw(|f| {
            draw_layout(
                f,
                account,
                "Select workspace",
                &workspace_rows,
                &mut w_state,
                "↑↓ Enter · c create · r reset · d delete · q quit",
                false,
            )
        })?;
        let Some(k) = keys.next(bytes).await else {
            return Ok(None);
        };
        match k {
            MenuKey::Escape | MenuKey::Char('q') => return Ok(None),
            MenuKey::Char('c') => {
                if let Some(name) =
                    prompt_input(term, bytes, keys, "create workspace", "").await?
                {
                    let name = name.trim().to_string();
                    if !name.is_empty() {
                        create_workspace(wire, &name).await?;
                        pending_select = Some(name);
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
                            &format!("delete workspace '{}'?", ws.name),
                        )
                        .await?;
                        if confirmed {
                            delete_workspace(wire, &ws.name).await?;
                            w_state.select(None);
                        }
                    }
                }
            }
            MenuKey::Char('r') => {
                if let Some(sel) = w_state.selected() {
                    if let Some(ws) = workspaces.get(sel) {
                        let confirmed = prompt_confirm(
                            term,
                            bytes,
                            keys,
                            &format!(
                                "reset session for '{}'? Files stay; session history cleared.",
                                ws.name
                            ),
                        )
                        .await?;
                        if confirmed {
                            reset_workspace(wire, &ws.name).await?;
                            w_state.select(None);
                        }
                    }
                }
            }
            _ => match handle_list_key(k, &mut w_state, workspaces.len()) {
                ListAction::Pick => {
                    if let Some(sel) = w_state.selected() {
                        if let Some(ws) = workspaces.get(sel).cloned() {
                            let title = "Select workspace";
                            let hint = "↑↓ Enter · c create · r reset · d delete · q quit";
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
                            crate::write_last_workspace_global(&ws.name);
                            return Ok(Some(ws.name));
                        }
                    }
                }
                ListAction::Quit => return Ok(None),
                ListAction::Pass => {}
            },
        }
    }
}

// ---------------------------------------------------------------------------

enum ListAction {
    Pick,
    Quit,
    Pass,
}

/// Pure helper for the workspace picker's per-iteration "what row
/// should be selected" decision. Lives outside the async menu loop
/// so it's unit-testable. Returns `Some(idx)` when the caller should
/// move selection, `None` when it should leave the existing state
/// untouched (this is the case that the v1.13.0 ship-blocker bug
/// got wrong — every iteration was overwriting the user's arrow-key
/// motion back to row 0).
///
/// Precedence:
///   1. `pending` set and present in the list → that row.
///   2. `pending` set but not present (e.g. workspace was just
///      deleted) → row 0 if the list is non-empty, else None.
///   3. No `pending`, no `current` and list non-empty → row 0
///      (first-paint default).
///   4. No `pending`, `current` is Some → leave it alone (return
///      None) so the user's keyboard motion persists across the
///      next list refresh.
fn decide_initial_selection(
    pending: Option<&str>,
    current: Option<usize>,
    names: &[&str],
) -> Option<usize> {
    if let Some(name) = pending {
        if names.is_empty() {
            return None;
        }
        let idx = names.iter().position(|n| *n == name).unwrap_or(0);
        return Some(idx.min(names.len() - 1));
    }
    if current.is_none() && !names.is_empty() {
        return Some(0);
    }
    None
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

/// What the picker draws for one row. Agent picker passes `badge: None`;
/// workspace picker fills it from `WorkspaceInfo`.
#[derive(Clone)]
pub struct PickerRow {
    pub name: String,
    pub badge: Option<Badge>,
}

/// A badge shown next to a workspace name in the picker.
/// Unlike the old static `active`/`saved` pair, the lock badge carries a
/// dynamic label (the agent name) so we use `String` here.
#[derive(Clone)]
pub struct Badge {
    pub glyph: &'static str,
    pub label: String,
    pub color: Color,
}

impl Badge {
    /// Workspace is locked by the given agent.
    pub fn locked(agent: &str) -> Self {
        Badge {
            glyph: "●",
            label: format!(" {}", agent),
            color: Color::Yellow,
        }
    }

    /// In the agent picker, marks the agent that currently holds the
    /// lock for the workspace the user is opening — a green dot with
    /// no extra label so a screenful of agents stays scannable.
    pub fn current_holder() -> Self {
        Badge {
            glyph: "●",
            label: String::new(),
            color: Color::Green,
        }
    }

    fn width(&self) -> usize {
        self.glyph.chars().count() + self.label.chars().count()
    }
}

/// Build one list row, baking highlight + optional badge directly into the
/// line so we don't rely on ratatui's `highlight_style`.
///
/// Right margin: 1 cell of trailing padding so the badge's right edge lines
/// up with the `[account]` label one row up in the title bar.
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
        if let Some(b) = &row.badge {
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
    if let Some(b) = &row.badge {
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
    // Vertical budget (same accounting as prompt_confirm, minus the
    // buttons row): N + 6.
    let dialog_w: u16 = 72;
    let h_pad: u16 = 2;
    let wrap_w = dialog_w.saturating_sub(2 + 2 * h_pad).max(1);
    let visible = msg.chars().count() as u16;
    let msg_rows = visible.div_ceil(wrap_w).clamp(1, 8);
    let dialog_h: u16 = msg_rows + 6;
    let msg_owned = msg.to_string();
    term.draw(|f| {
        let body = draw_titled_dialog(f, "cloudcode", dialog_w, dialog_h);
        let body_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),        // top pad
                Constraint::Length(msg_rows), // wrapped message
                Constraint::Min(0),           // bottom pad
            ])
            .split(body);
        let msg_h = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(h_pad),
                Constraint::Min(1),
                Constraint::Length(h_pad),
            ])
            .split(body_chunks[1]);
        f.render_widget(
            Paragraph::new(msg_owned)
                .wrap(Wrap { trim: false })
                .style(Style::default().bg(DIALOG_BG).fg(DIALOG_FG)),
            msg_h[1],
        );
        for col in [msg_h[0], msg_h[2]] {
            f.render_widget(Block::default().style(Style::default().bg(DIALOG_BG)), col);
        }
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
    // Dialog sizing. Host-derived agent names are easily 30+ chars,
    // so we widen the box and word-wrap the message.
    //
    // Vertical budget (dialog_h must add up to all of these):
    //   1  top border
    //   1  title row              ← draw_titled_dialog
    //   1  separator row          ← draw_titled_dialog
    //   1  top padding (gap)      ← our Layout below
    //   N  wrapped message rows
    //   1  gap before buttons
    //   1  buttons row
    //   1  bottom padding (gap)
    //   1  bottom border
    //   = N + 8
    //
    // Horizontal budget: body chunk width = dialog_w - 2 (borders).
    // We carve 2 columns of horizontal padding off each side so text
    // doesn't hug the border, leaving (dialog_w - 6) for the actual
    // wrap width.
    let dialog_w: u16 = 72;
    let h_pad: u16 = 2;
    let wrap_w = dialog_w.saturating_sub(2 + 2 * h_pad).max(1);
    let visible = msg.chars().count() as u16;
    let msg_rows = visible.div_ceil(wrap_w).clamp(1, 8);
    let dialog_h: u16 = msg_rows + 8;
    let draw = |term: &mut Terminal<B>, pressed_yes: bool| -> Result<()> {
        let msg_owned = msg.to_string();
        term.draw(move |f| {
            let body = draw_titled_dialog(f, "Confirm", dialog_w, dialog_h);
            let body_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),         // top pad
                    Constraint::Length(msg_rows),  // wrapped message
                    Constraint::Length(1),         // gap before buttons
                    Constraint::Length(1),         // buttons
                    Constraint::Min(0),            // bottom pad / overflow
                ])
                .split(body);
            let msg_h_chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(h_pad), // left pad
                    Constraint::Min(1),        // wrapped message
                    Constraint::Length(h_pad), // right pad
                ])
                .split(body_chunks[1]);
            f.render_widget(
                Paragraph::new(msg_owned)
                    .wrap(Wrap { trim: false })
                    .style(Style::default().bg(DIALOG_BG).fg(DIALOG_FG)),
                msg_h_chunks[1],
            );
            // Paint the left/right pad cells too, otherwise they show
            // through with the desktop background.
            for col in [msg_h_chunks[0], msg_h_chunks[2]] {
                f.render_widget(Block::default().style(Style::default().bg(DIALOG_BG)), col);
            }
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
            HubToClient::SessionError { message, .. } => {
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

async fn reset_workspace(wire: &mut Wire, name: &str) -> Result<()> {
    wire.out_tx
        .send(OutFrame::Text(ClientToHub::ResetWorkspace {
            name: name.into(),
        }))
        .await
        .map_err(|_| anyhow!("hub disconnected"))?;
    loop {
        let m = expect_text(wire).await?;
        match m {
            HubToClient::WorkspaceReset { .. } => return Ok(()),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_match_picks_that_row() {
        let names = ["alpha", "beta", "gamma"];
        assert_eq!(
            decide_initial_selection(Some("beta"), None, &names),
            Some(1)
        );
    }

    #[test]
    fn pending_no_match_falls_back_to_first() {
        let names = ["alpha", "beta"];
        assert_eq!(
            decide_initial_selection(Some("ghost"), None, &names),
            Some(0)
        );
    }

    #[test]
    fn pending_on_empty_list_returns_none() {
        let names: [&str; 0] = [];
        assert_eq!(decide_initial_selection(Some("beta"), None, &names), None);
    }

    #[test]
    fn first_paint_with_no_pending_lands_on_row_zero() {
        let names = ["alpha", "beta"];
        assert_eq!(decide_initial_selection(None, None, &names), Some(0));
    }

    #[test]
    fn empty_list_with_no_pending_returns_none() {
        let names: [&str; 0] = [];
        assert_eq!(decide_initial_selection(None, None, &names), None);
    }

    /// The regression that shipped on v1.13.0 RC: arrow keys appeared
    /// dead because every loop iteration reset selection back to 0.
    /// With the fix, an existing user selection must be left alone
    /// when no pending name is being applied — `decide_initial_selection`
    /// returns None so the caller doesn't touch `w_state`.
    #[test]
    fn existing_selection_is_preserved_across_refresh() {
        let names = ["alpha", "beta", "gamma"];
        assert_eq!(decide_initial_selection(None, Some(2), &names), None);
        assert_eq!(decide_initial_selection(None, Some(0), &names), None);
    }

    /// After a delete shrinks the list, a pending name that still
    /// exists must clamp to a valid index.
    #[test]
    fn pending_clamps_when_list_shrinks() {
        let names = ["alpha"];
        // Pending was set to "alpha" which is still at index 0 —
        // straightforward. The interesting case is when `position`
        // returns 0 but the list is genuinely length-1; ensure no
        // off-by-one.
        assert_eq!(
            decide_initial_selection(Some("alpha"), Some(5), &names),
            Some(0)
        );
    }
}
