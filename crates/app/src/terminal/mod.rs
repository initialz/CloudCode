//! Terminal panel — an `alacritty_terminal::Term` driven by PTY bytes,
//! rendered into egui by a custom painter.
//!
//! Data flow: the backend forwards hub PTY bytes as `BackendEvent::
//! PtyBytes`; `main.rs` feeds them to [`TerminalPanel::feed`], which
//! advances a `vte::ansi::Processor` over the `Term` (the VTE state
//! machine that maintains the grid). Each frame [`TerminalPanel::ui`]
//! reads `term.renderable_content()` and paints the visible grid.
//!
//! Pinned `alacritty_terminal = 0.25` (0.24.2's `tty` module fails to
//! compile against the current `rustix_openpty`; 0.25/0.26 build clean —
//! 0.25 chosen as the nearest stable matching the documented API).
//!
//! GUI can't run headless, so the high-value test is `feed` correctness:
//! we push known byte sequences and assert the resulting grid (see the
//! `tests` module). The pure render lowering lives in [`render`].

mod fonts;
mod input;
mod render;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Point;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{CursorShape, Processor};

pub use fonts::install_cjk_font;
use input::{cell_advance_cols, ime_apply, key_to_bytes, ImeState};
use render::{
    rows_to_runs, term_color_to_rgb, CellView, DEFAULT_BG, DEFAULT_FG,
};

/// No-op `EventListener`: the `Term` emits events (bell, title change,
/// clipboard, PTY writes) that a standalone renderer doesn't need to act
/// on. We discard them.
#[derive(Clone, Copy)]
struct NoopListener;

impl EventListener for NoopListener {
    fn send_event(&self, _event: Event) {}
}

/// Terminal grid size. Implements alacritty's `Dimensions` so it can be
/// handed to `Term::new` / `Term::resize`. No scrollback for T3
/// (`total_lines == screen_lines`); scrollback history is Task 5.
#[derive(Clone, Copy, Debug)]
struct TermSize {
    cols: usize,
    rows: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// The terminal panel: owns the VTE state machine + parser and renders it.
pub struct TerminalPanel {
    term: Term<NoopListener>,
    processor: Processor,
    cols: usize,
    rows: usize,
    /// IME composition state. While `ime.is_composing()`, keyboard/text
    /// byte emission is suppressed — the IME owns the keystrokes — and the
    /// preedit string is rendered inline (grey + underline) at the cursor.
    ime: ImeState,
    /// egui id for this panel's focusable area, so we can request focus
    /// and route IME events. Set on first `ui()` call.
    focus_id: Option<egui::Id>,
}

impl TerminalPanel {
    /// Create a panel with an initial `cols`×`rows` grid. (T3 uses the
    /// hardcoded 80×24 from `OpenSession`; Task 5 makes it dynamic.)
    pub fn new(cols: usize, rows: usize) -> TerminalPanel {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let size = TermSize { cols, rows };
        let config = Config::default();
        let term = Term::new(config, &size, NoopListener);
        TerminalPanel {
            term,
            processor: Processor::new(),
            cols,
            rows,
            ime: ImeState::default(),
            focus_id: None,
        }
    }

    /// Translate a frame of egui input events into PTY bytes.
    ///
    /// Handles the three input channels and their interactions:
    ///   * `Event::Ime` drives the [`ImeState`] machine; `Commit` emits
    ///     the composed text, `Preedit` just updates the inline overlay.
    ///   * `Event::Text` emits printable UTF-8 (the source of truth for
    ///     typed characters — correct casing/layout/dead-keys).
    ///   * `Event::Key` emits special/control bytes (Enter, arrows,
    ///     Ctrl-C, ...) via [`key_to_bytes`].
    ///
    /// Dedup: egui sends BOTH a `Key` and a `Text` for a plain letter, so
    /// `key_to_bytes` returns `None` for printable letters (handled by
    /// Text) and bytes only for special/control keys. While a preedit is
    /// active, all Key/Text emission is suppressed (the IME owns input).
    pub fn handle_egui_input(&mut self, events: &[egui::Event]) -> Vec<u8> {
        let mut out = Vec::new();
        for ev in events {
            match ev {
                egui::Event::Ime(ime_event) => {
                    let state = std::mem::take(&mut self.ime);
                    let (next, commit) = ime_apply(state, ime_event);
                    self.ime = next;
                    if let Some(bytes) = commit {
                        out.extend_from_slice(&bytes);
                    }
                }
                // While composing, the IME owns the keystrokes: drop the
                // raw Text/Key events egui still forwards alongside it.
                egui::Event::Text(s) if !self.ime.is_composing() => {
                    out.extend_from_slice(s.as_bytes());
                }
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } if !self.ime.is_composing() => {
                    if let Some(bytes) = key_to_bytes(*key, *modifiers, None) {
                        out.extend_from_slice(&bytes);
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// Feed raw PTY bytes into the VTE state machine, advancing the grid.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.processor.advance(&mut self.term, bytes);
    }

    /// Resize the terminal grid. Wired to actual pixel→cell sizing in
    /// Task 5; for now it's a method the app can call.
    #[allow(dead_code)]
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        self.term.resize(TermSize { cols, rows });
    }

    /// Snapshot the visible grid into a dense `rows × cols` matrix of
    /// `CellView` (default-filled spaces, then overwritten by the live
    /// cells). Pulls colors/flags off each cell and resolves them to RGB
    /// so the pure render helpers never touch alacritty types.
    ///
    /// Returns `(grid, cursor)` where `cursor` is `(row, col, visible)`
    /// in viewport coordinates.
    fn snapshot(&self) -> (Vec<Vec<CellView>>, (usize, usize, bool)) {
        let cols = self.cols;
        let rows = self.rows;
        let blank = CellView {
            ch: ' ',
            fg: DEFAULT_FG,
            bg: DEFAULT_BG,
            inverse: false,
            bold: false,
        };
        let mut grid = vec![vec![blank; cols]; rows];

        let content = self.term.renderable_content();
        for indexed in content.display_iter {
            let point: Point = indexed.point;
            // `display_iter` is already viewport-relative (line 0 == top
            // visible row) with the display offset applied.
            let line = point.line.0;
            if line < 0 {
                continue; // scrollback above the viewport — skip for T3.
            }
            let r = line as usize;
            let c = point.column.0;
            if r >= rows || c >= cols {
                continue;
            }
            let cell = indexed.cell;
            // Skip the spacer half of a wide (CJK) char; the wide glyph
            // is drawn from its leading cell. (Width handling is Task 4;
            // this just avoids a stray blank overwriting nothing.)
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            grid[r][c] = CellView {
                ch: cell.c,
                fg: term_color_to_rgb(cell.fg, DEFAULT_FG),
                bg: term_color_to_rgb(cell.bg, DEFAULT_BG),
                inverse: cell.flags.contains(Flags::INVERSE),
                bold: cell.flags.contains(Flags::BOLD),
            };
        }

        let cursor = self.term.renderable_content().cursor;
        let cur_line = cursor.point.line.0;
        let visible = !matches!(cursor.shape, CursorShape::Hidden)
            && cur_line >= 0
            && (cur_line as usize) < rows
            && cursor.point.column.0 < cols;
        let cursor = (
            cur_line.max(0) as usize,
            cursor.point.column.0,
            visible,
        );
        (grid, cursor)
    }

    /// Render the terminal grid into `ui` with a custom painter, and
    /// return any PTY bytes the user's keyboard/IME produced this frame.
    ///
    /// The caller (`main.rs`) sends the returned bytes as
    /// `UiCommand::SendInput`. Empty vec == no input this frame.
    pub fn ui(&mut self, ui: &mut egui::Ui) -> Vec<u8> {
        // Monospace cell metrics from the active font. ASCII rides the
        // built-in monospace; CJK glyphs fall back to the system font
        // installed at startup (`fonts::install_cjk_font`). We size cells
        // off the monospace 'M' advance and advance wide glyphs by two
        // cells (see `cell_advance_cols`) so CJK doesn't overlap.
        let font = egui::FontId::monospace(14.0);
        let (cell_w, cell_h) = ui.fonts(|f| {
            let w = f.glyph_width(&font, 'M');
            let h = f.row_height(&font);
            (w, h)
        });
        let cell_w = cell_w.max(1.0);
        let cell_h = cell_h.max(1.0);

        let (grid, (cur_row, cur_col, cur_visible)) = self.snapshot();

        let size = egui::vec2(self.cols as f32 * cell_w, self.rows as f32 * cell_h);
        // Focusable + click so we can own keyboard focus and route IME.
        let (response, painter) =
            ui.allocate_painter(size, egui::Sense::click_and_drag());
        let origin = response.rect.min;
        self.focus_id = Some(response.id);

        // Grab focus on click; keep IME enabled while focused so the OS
        // candidate window targets us.
        if response.clicked() {
            response.request_focus();
        }
        let focused = response.has_focus();

        // Backdrop so unfilled gaps read as the terminal background.
        painter.rect_filled(response.rect, 0.0, rgb(DEFAULT_BG));

        // Background fills batched per run (cheap); glyphs drawn per cell
        // so wide (CJK) glyphs advance two columns and stay grid-aligned.
        for (r, cells) in grid.iter().enumerate() {
            let y = origin.y + r as f32 * cell_h;
            for run in rows_to_runs(cells) {
                if run.bg != DEFAULT_BG {
                    let x = origin.x + run.col as f32 * cell_w;
                    let run_rect = egui::Rect::from_min_size(
                        egui::pos2(x, y),
                        egui::vec2(run.len as f32 * cell_w, cell_h),
                    );
                    painter.rect_filled(run_rect, 0.0, rgb(run.bg));
                }
            }
            // Glyphs: one draw per non-blank cell, positioned by column.
            for (c, cell) in cells.iter().enumerate() {
                if cell.ch == ' ' || cell.ch == '\0' {
                    continue;
                }
                let (fg, _bg) = cell.effective();
                let x = origin.x + c as f32 * cell_w;
                painter.text(
                    egui::pos2(x, y),
                    egui::Align2::LEFT_TOP,
                    cell.ch,
                    font.clone(),
                    rgb(fg),
                );
            }
        }

        // IME preedit overlay: render the in-progress composition inline at
        // the cursor, grey + underlined, width-aware. Not sent to the PTY.
        let mut preedit_end_col = cur_col;
        if !self.ime.preedit.is_empty() {
            let y = origin.y + cur_row as f32 * cell_h;
            let mut col = cur_col;
            for ch in self.ime.preedit.chars() {
                let x = origin.x + col as f32 * cell_w;
                let advance = cell_advance_cols(ch);
                let w = advance as f32 * cell_w;
                // Grey background block under the preedit char.
                painter.rect_filled(
                    egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, cell_h)),
                    0.0,
                    egui::Color32::from_rgb(0x30, 0x30, 0x30),
                );
                painter.text(
                    egui::pos2(x, y),
                    egui::Align2::LEFT_TOP,
                    ch,
                    font.clone(),
                    egui::Color32::from_rgb(0xa0, 0xa0, 0xa0),
                );
                // Underline.
                painter.line_segment(
                    [
                        egui::pos2(x, y + cell_h - 1.0),
                        egui::pos2(x + w, y + cell_h - 1.0),
                    ],
                    egui::Stroke::new(1.0, egui::Color32::from_rgb(0xa0, 0xa0, 0xa0)),
                );
                col += advance;
            }
            preedit_end_col = col;
        }

        // Cursor: a block. While composing, draw it after the preedit so it
        // reads as "you're typing here".
        if cur_visible && self.ime.preedit.is_empty() {
            let x = origin.x + cur_col as f32 * cell_w;
            let y = origin.y + cur_row as f32 * cell_h;
            let cur_rect =
                egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(cell_w, cell_h));
            painter.rect_filled(
                cur_rect,
                0.0,
                egui::Color32::from_rgba_unmultiplied(0xd0, 0xd0, 0xd0, 0x90),
            );
        }

        // Tell the OS where to anchor the IME candidate window: at the end
        // of the current preedit (or the cursor if none). egui forwards
        // this to winit's `set_ime_cursor_area`.
        if focused {
            let cur_x = origin.x + preedit_end_col as f32 * cell_w;
            let cur_y = origin.y + cur_row as f32 * cell_h;
            let cursor_rect = egui::Rect::from_min_size(
                egui::pos2(cur_x, cur_y),
                egui::vec2(cell_w, cell_h),
            );
            ui.ctx().output_mut(|o| {
                o.ime = Some(egui::output::IMEOutput {
                    rect: response.rect,
                    cursor_rect,
                });
            });
        }

        // Consume keyboard/IME events only while we hold focus, so typing
        // into the workspace picker (or elsewhere) isn't swallowed.
        if focused {
            let events: Vec<egui::Event> =
                ui.input(|i| i.filtered_events(&egui::EventFilter {
                    tab: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: true,
                }));
            self.handle_egui_input(&events)
        } else {
            Vec::new()
        }
    }
}

/// `[u8;3]` → opaque egui color.
fn rgb([r, g, b]: [u8; 3]) -> egui::Color32 {
    egui::Color32::from_rgb(r, g, b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::vte::ansi::{Color, NamedColor};

    /// Read the cell at (line, col) from the live grid.
    fn cell_at<'a>(
        panel: &'a TerminalPanel,
        line: i32,
        col: usize,
    ) -> &'a alacritty_terminal::term::cell::Cell {
        let grid = panel.term.grid();
        &grid[Line(line)][Column(col)]
    }

    #[test]
    fn feed_plain_text_lands_in_grid() {
        let mut panel = TerminalPanel::new(80, 24);
        panel.feed(b"hi");
        assert_eq!(cell_at(&panel, 0, 0).c, 'h');
        assert_eq!(cell_at(&panel, 0, 1).c, 'i');
    }

    #[test]
    fn feed_crlf_moves_to_next_row() {
        let mut panel = TerminalPanel::new(80, 24);
        panel.feed(b"a\r\nb");
        assert_eq!(cell_at(&panel, 0, 0).c, 'a');
        // After CR (col 0) + LF (next line), 'b' lands at row 1 col 0.
        assert_eq!(cell_at(&panel, 1, 0).c, 'b');
    }

    #[test]
    fn feed_sgr_red_sets_foreground() {
        let mut panel = TerminalPanel::new(80, 24);
        // ESC[31m -> foreground red, then 'X'.
        panel.feed(b"\x1b[31mX");
        let cell = cell_at(&panel, 0, 0);
        assert_eq!(cell.c, 'X');
        assert_eq!(cell.fg, Color::Named(NamedColor::Red));
    }

    #[test]
    fn feed_sgr_bold_and_reset() {
        let mut panel = TerminalPanel::new(80, 24);
        // Bold 'B', then reset (ESC[0m) then plain 'p'.
        panel.feed(b"\x1b[1mB\x1b[0mp");
        assert!(cell_at(&panel, 0, 0).flags.contains(Flags::BOLD));
        assert_eq!(cell_at(&panel, 0, 0).c, 'B');
        assert!(!cell_at(&panel, 0, 1).flags.contains(Flags::BOLD));
        assert_eq!(cell_at(&panel, 0, 1).c, 'p');
    }

    #[test]
    fn feed_chunked_advances_statefully() {
        // Split an escape sequence across two feed() calls — the parser
        // must keep state between chunks.
        let mut panel = TerminalPanel::new(80, 24);
        panel.feed(b"\x1b[3");
        panel.feed(b"2mG"); // completes ESC[32m (green) then 'G'
        let cell = cell_at(&panel, 0, 0);
        assert_eq!(cell.c, 'G');
        assert_eq!(cell.fg, Color::Named(NamedColor::Green));
    }

    #[test]
    fn snapshot_reflects_fed_content() {
        let mut panel = TerminalPanel::new(80, 24);
        panel.feed(b"\x1b[31mhi");
        let (grid, cursor) = panel.snapshot();
        assert_eq!(grid[0][0].ch, 'h');
        assert_eq!(grid[0][1].ch, 'i');
        // Red foreground resolved to RGB in the snapshot.
        assert_eq!(grid[0][0].fg, render::term_color_to_rgb(
            Color::Named(NamedColor::Red),
            DEFAULT_FG,
        ));
        // Cursor sits just past "hi" at col 2, row 0, and is visible.
        let (cr, cc, vis) = cursor;
        assert_eq!((cr, cc), (0, 2));
        assert!(vis);
    }

    #[test]
    fn resize_changes_dimensions() {
        let mut panel = TerminalPanel::new(80, 24);
        panel.resize(100, 30);
        assert_eq!(panel.cols, 100);
        assert_eq!(panel.rows, 30);
        // Grid must reflect the new width (feeding near the new edge ok).
        panel.feed(b"z");
        assert_eq!(cell_at(&panel, 0, 0).c, 'z');
        let (grid, _) = panel.snapshot();
        assert_eq!(grid.len(), 30);
        assert_eq!(grid[0].len(), 100);
    }

    fn key_ev(key: egui::Key, mods: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: mods,
        }
    }

    #[test]
    fn handle_input_dedups_text_and_key_for_plain_letter() {
        // egui sends BOTH Text('a') and Key{A} for one keystroke; we must
        // emit the byte exactly once (from Text), not twice.
        let mut panel = TerminalPanel::new(80, 24);
        let events = [
            egui::Event::Text("a".into()),
            key_ev(egui::Key::A, egui::Modifiers::default()),
        ];
        assert_eq!(panel.handle_egui_input(&events), b"a".to_vec());
    }

    #[test]
    fn handle_input_enter_key_emits_cr() {
        let mut panel = TerminalPanel::new(80, 24);
        let events = [key_ev(egui::Key::Enter, egui::Modifiers::default())];
        assert_eq!(panel.handle_egui_input(&events), b"\r".to_vec());
    }

    #[test]
    fn handle_input_ctrl_c_emits_etx() {
        let mut panel = TerminalPanel::new(80, 24);
        let mods = egui::Modifiers { ctrl: true, command: true, ..Default::default() };
        let events = [key_ev(egui::Key::C, mods)];
        assert_eq!(panel.handle_egui_input(&events), vec![0x03]);
    }

    #[test]
    fn handle_input_suppresses_keystrokes_while_composing() {
        // Active preedit -> the IME owns input; raw Text/Key are dropped.
        let mut panel = TerminalPanel::new(80, 24);
        // Start a composition.
        let _ = panel.handle_egui_input(&[egui::Event::Ime(
            egui::ImeEvent::Preedit("ni".into()),
        )]);
        assert!(panel.ime.is_composing());
        // A stray Text/Key during composition must NOT reach the PTY.
        let dropped = panel.handle_egui_input(&[
            egui::Event::Text("x".into()),
            key_ev(egui::Key::Enter, egui::Modifiers::default()),
        ]);
        assert!(dropped.is_empty());
    }

    #[test]
    fn handle_input_ime_commit_emits_chinese_bytes() {
        let mut panel = TerminalPanel::new(80, 24);
        let _ = panel.handle_egui_input(&[egui::Event::Ime(
            egui::ImeEvent::Preedit("ni".into()),
        )]);
        let out = panel.handle_egui_input(&[egui::Event::Ime(
            egui::ImeEvent::Commit("你".into()),
        )]);
        assert_eq!(out, "你".as_bytes().to_vec());
        assert!(!panel.ime.is_composing());
    }
}
