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
mod geom;
mod input;
mod render;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{CursorShape, Processor};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use geom::{
    grid_dims, pixel_to_grid, wheel_to_scroll_lines, wrap_paste, ResizeThrottle,
};

pub use fonts::install_cjk_font;
use input::{cell_advance_cols, ime_apply, key_to_bytes, ImeState};
use render::{
    rows_to_runs, term_color_to_rgb, CellView, DEFAULT_BG, DEFAULT_FG,
};

/// Scrollback history retained off the top of the viewport (the project's
/// 50k-line convention). Alacritty's `Config::default()` ships 10k; we bump
/// it so a long `claude` session keeps plenty of context scrollable.
const SCROLLBACK_LINES: usize = 50_000;

/// A request to resize the hub-side PTY, surfaced from `ui()` so the App can
/// forward it as `UiCommand::Resize`. The local grid is resized immediately;
/// this only carries the (debounced) hub notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResizeRequest {
    pub cols: u16,
    pub rows: u16,
}

/// `EventListener` for the `Term`: captures `Event::Bell` (claude rings
/// BEL / `\x07` when it wants the user) into a shared flag the panel
/// folds into its attention state on the next [`TerminalPanel::feed`].
/// Every other event (title change, clipboard, PTY writes, wakeups) is
/// still discarded — a standalone renderer doesn't act on them.
#[derive(Clone)]
struct PanelListener {
    bell: Arc<AtomicBool>,
}

impl EventListener for PanelListener {
    fn send_event(&self, event: Event) {
        if matches!(event, Event::Bell) {
            self.bell.store(true, Ordering::Relaxed);
        }
    }
}

/// The attention-halo decision: any user input clears the flag (they're
/// at the terminal — even if a bell rang the same frame), otherwise a
/// bell sets it and it latches until cleared.
///
/// PURE (table-tested).
fn update_attention(prev: bool, bell: bool, user_input: bool) -> bool {
    if user_input {
        false
    } else {
        prev || bell
    }
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
    term: Term<PanelListener>,
    processor: Processor,
    cols: usize,
    rows: usize,
    /// Bell flag shared with the `Term`'s [`PanelListener`]; swapped
    /// false and folded into `attention` on every `feed`.
    bell: Arc<AtomicBool>,
    /// Attention halo state: set when the terminal rings the bell,
    /// cleared by ANY user input to the terminal (key/IME/paste — see
    /// `ui()`); clicks elsewhere (viewer tabs, sidebar) don't go through
    /// the terminal input path, so they leave it set.
    attention: bool,
    /// IME composition state. While `ime.is_composing()`, keyboard/text
    /// byte emission is suppressed — the IME owns the keystrokes — and the
    /// preedit string is rendered inline (grey + underline) at the cursor.
    ime: ImeState,
    /// egui id for this panel's focusable area, so we can request focus
    /// and route IME events. Set on first `ui()` call.
    focus_id: Option<egui::Id>,
    /// Debounces resize emission to the hub so a window drag doesn't flood
    /// the wire (the local grid still resizes every frame the dims change).
    resize_throttle: ResizeThrottle,
}

/// What one `ui()` frame produced for the App to act on: PTY input bytes
/// (keyboard/IME/paste) and an optional hub resize notification.
#[derive(Debug, Default)]
pub struct UiOutput {
    /// Bytes to send to the hub PTY (`UiCommand::SendInput`). May be empty.
    pub input: Vec<u8>,
    /// A debounced resize to forward to the hub (`UiCommand::Resize`).
    pub resize: Option<ResizeRequest>,
}

impl TerminalPanel {
    /// Create a panel with an initial `cols`×`rows` grid. (T3 uses the
    /// hardcoded 80×24 from `OpenSession`; Task 5 makes it dynamic.)
    pub fn new(cols: usize, rows: usize) -> TerminalPanel {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let size = TermSize { cols, rows };
        let config = Config {
            scrolling_history: SCROLLBACK_LINES,
            ..Config::default()
        };
        let bell = Arc::new(AtomicBool::new(false));
        let term = Term::new(config, &size, PanelListener { bell: bell.clone() });
        TerminalPanel {
            term,
            processor: Processor::new(),
            cols,
            rows,
            bell,
            attention: false,
            ime: ImeState::default(),
            focus_id: None,
            resize_throttle: ResizeThrottle::new(),
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
    /// A BEL (`\x07`) in the stream reaches the [`PanelListener`] as
    /// `Event::Bell` and sets the attention flag here.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.processor.advance(&mut self.term, bytes);
        let bell = self.bell.swap(false, Ordering::Relaxed);
        self.attention = update_attention(self.attention, bell, false);
    }

    /// Whether the bell-driven attention halo should show (set by a BEL
    /// in the PTY stream, cleared by any user input to the terminal).
    pub fn attention(&self) -> bool {
        self.attention
    }

    /// Resize the terminal grid to `cols`×`rows`. Called from `ui()` when
    /// the painter region's pixel size implies a different grid; the hub
    /// notification is debounced separately (see `resize_throttle`).
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
    /// Returns `(grid, cursor, selected, display_offset)` where `cursor` is
    /// `(row, col, visible)` in viewport coordinates and `selected` is a
    /// `rows × cols` mask of cells inside the active selection (for the bg
    /// tint). `display_offset` is how far the viewport is scrolled into
    /// history (0 == pinned to the bottom).
    fn snapshot(
        &self,
    ) -> (
        Vec<Vec<CellView>>,
        (usize, usize, bool),
        Vec<Vec<bool>>,
        usize,
    ) {
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
        let mut selected = vec![vec![false; cols]; rows];

        let content = self.term.renderable_content();
        let display_offset = content.display_offset;
        // Selection range is in absolute grid coordinates; convert each
        // cell's absolute line back to a viewport row via the offset.
        let sel_range = content.selection;
        for indexed in content.display_iter {
            let point: Point = indexed.point;
            // `display_iter` yields ABSOLUTE grid coordinates: the topmost
            // visible line is `-display_offset` (history is negative). The
            // viewport row is therefore `line + display_offset`.
            let vrow = point.line.0 + display_offset as i32;
            if vrow < 0 {
                continue; // above the visible region (shouldn't happen).
            }
            let r = vrow as usize;
            let c = point.column.0;
            if r >= rows || c >= cols {
                continue;
            }
            // Selection tint: the range is absolute, so test the absolute
            // point against it.
            if let Some(range) = sel_range {
                if range.contains(point) {
                    selected[r][c] = true;
                }
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
        // Cursor point is absolute too; shift into the viewport.
        let cur_vrow = cursor.point.line.0 + display_offset as i32;
        let visible = !matches!(cursor.shape, CursorShape::Hidden)
            && cur_vrow >= 0
            && (cur_vrow as usize) < rows
            && cursor.point.column.0 < cols;
        let cursor = (
            cur_vrow.max(0) as usize,
            cursor.point.column.0,
            visible,
        );
        (grid, cursor, selected, display_offset)
    }

    /// The current viewport scrollback offset (0 == pinned to the bottom).
    fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// Scroll the display by `lines` (positive = back into history). New
    /// output snapping back to the bottom is handled by alacritty's own
    /// scroll-on-input behavior; this only moves the viewport.
    fn scroll_lines(&mut self, lines: i32) {
        if lines != 0 {
            self.term.scroll_display(Scroll::Delta(lines));
        }
    }

    /// Begin a new simple (drag) selection at the given grid point/side,
    /// replacing any existing selection.
    fn selection_start(&mut self, point: Point, side: Side) {
        self.term.selection =
            Some(Selection::new(SelectionType::Simple, point, side));
    }

    /// Extend the in-progress selection to a new point/side. No-op if no
    /// selection is active.
    fn selection_update(&mut self, point: Point, side: Side) {
        if let Some(sel) = self.term.selection.as_mut() {
            sel.update(point, side);
        }
    }

    /// Clear any active selection (e.g. a plain click with no drag).
    fn selection_clear(&mut self) {
        self.term.selection = None;
    }

    /// The currently-selected text, if any (for copy).
    fn selected_text(&self) -> Option<String> {
        self.term.selection_to_string().filter(|s| !s.is_empty())
    }

    /// Whether the terminal has bracketed-paste mode enabled — decides if
    /// pasted text is wrapped in the `ESC[200~`..`ESC[201~` markers.
    fn bracketed_paste(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// Render the terminal grid into `ui` with a custom painter, and return
    /// a [`UiOutput`] carrying PTY bytes (keyboard/IME/paste) and any
    /// debounced hub resize this frame produced.
    ///
    /// The caller (`main.rs`) forwards `output.input` as
    /// `UiCommand::SendInput` and `output.resize` as `UiCommand::Resize`.
    ///
    /// `avail` is the pixel size the panel may occupy (the painter region);
    /// the grid is sized to fit it (pixels → cols/rows). `now` is the wall
    /// clock used for resize debouncing (an arg so tests stay deterministic).
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        avail: egui::Vec2,
        now: std::time::Instant,
    ) -> UiOutput {
        let mut output = UiOutput::default();

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

        // Pixels → grid. Resize the local grid immediately when it changes;
        // the hub notification is debounced via `resize_throttle` so a drag
        // doesn't flood the wire.
        let (new_cols, new_rows) =
            grid_dims(avail.x, avail.y, cell_w, cell_h);
        if new_cols as usize != self.cols || new_rows as usize != self.rows {
            self.resize(new_cols as usize, new_rows as usize);
        }
        if let Some((c, r)) = self.resize_throttle.update((new_cols, new_rows), now) {
            output.resize = Some(ResizeRequest { cols: c, rows: r });
        } else if let Some((c, r)) = self.resize_throttle.flush_pending(now) {
            // Trailing edge: the drag settled, flush the last held size.
            output.resize = Some(ResizeRequest { cols: c, rows: r });
        }

        let (grid, (cur_row, cur_col, cur_visible), selected, _display_offset) =
            self.snapshot();

        let size = egui::vec2(self.cols as f32 * cell_w, self.rows as f32 * cell_h);
        // Focusable + click so we can own keyboard focus and route IME.
        let (response, painter) =
            ui.allocate_painter(size, egui::Sense::click_and_drag());
        let origin = response.rect.min;
        self.focus_id = Some(response.id);

        // Grab focus on any press; keep IME enabled while focused so the OS
        // candidate window targets us.
        if response.clicked() || response.drag_started() {
            response.request_focus();
        }
        let focused = response.has_focus();

        // --- Mouse selection (simple drag-select) ---
        // Press starts a fresh selection at the cell under the pointer; drag
        // extends it; a plain click (no drag) clears any prior selection.
        if let Some(pos) = response.interact_pointer_pos() {
            let rel = pos - origin;
            let (point, side) = pixel_to_grid(
                rel.x,
                rel.y,
                cell_w,
                cell_h,
                self.cols,
                self.rows,
                self.display_offset(),
            );
            if response.drag_started() {
                self.selection_start(point, side);
            } else if response.dragged() {
                self.selection_update(point, side);
            }
        }
        // A plain click (pressed+released without dragging) collapses the
        // selection so a stray click doesn't leave stale highlight.
        if response.clicked() {
            self.selection_clear();
        }

        // --- Scroll wheel → scrollback ---
        if response.hovered() {
            let scroll_y = ui.input(|i| i.smooth_scroll_delta.y);
            let lines = wheel_to_scroll_lines(scroll_y, cell_h);
            self.scroll_lines(lines);
        }

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
            // Selection tint: a semi-transparent blue wash over selected
            // cells, drawn on top of the bg fill but under the glyphs.
            if let Some(row_sel) = selected.get(r) {
                let mut c = 0;
                while c < row_sel.len() {
                    if row_sel[c] {
                        let start = c;
                        while c < row_sel.len() && row_sel[c] {
                            c += 1;
                        }
                        let x = origin.x + start as f32 * cell_w;
                        let w = (c - start) as f32 * cell_w;
                        painter.rect_filled(
                            egui::Rect::from_min_size(
                                egui::pos2(x, y),
                                egui::vec2(w, cell_h),
                            ),
                            0.0,
                            egui::Color32::from_rgba_unmultiplied(0x40, 0x60, 0xb0, 0x80),
                        );
                    } else {
                        c += 1;
                    }
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

        // Consume keyboard/IME/clipboard events only while we hold focus, so
        // typing into the workspace picker (or elsewhere) isn't swallowed.
        if focused {
            let filter = egui::EventFilter {
                tab: true,
                horizontal_arrows: true,
                vertical_arrows: true,
                escape: true,
            };
            // CRITICAL: lock focus with this filter. Without it, egui ALSO
            // processes Tab/arrows/Escape for its own focus navigation —
            // pressing Esc once would surrender our focus (and arrows would
            // move focus away), so the terminal would receive a key exactly
            // ONCE and then go dead. set_focus_lock_filter tells egui those
            // keys belong to us. (This is what TextEdit does internally.)
            ui.memory_mut(|m| m.set_focus_lock_filter(response.id, filter.clone()));
            let events: Vec<egui::Event> = ui.input(|i| i.filtered_events(&filter));

            // Clipboard first: egui synthesizes Copy (Cmd/Ctrl-C) and
            // Paste(s) (Cmd/Ctrl-V) from the platform shortcuts, so we don't
            // hand-roll the modifier mapping.
            //   * Copy → write the selection to the system clipboard.
            //   * Paste(s) → emit the text as PTY bytes (bracketed if the
            //     app enabled bracketed-paste mode).
            for ev in &events {
                match ev {
                    egui::Event::Copy => {
                        if let Some(text) = self.selected_text() {
                            ui.ctx().copy_text(text);
                        }
                    }
                    egui::Event::Paste(text) => {
                        let bytes = wrap_paste(text, self.bracketed_paste());
                        output.input.extend_from_slice(&bytes);
                    }
                    _ => {}
                }
            }

            output.input.extend(self.handle_egui_input(&events));
        }

        // ANY terminal input this frame (key/IME/paste — every channel
        // funnels into `output.input`) clears the attention halo: the
        // user has answered the bell.
        self.attention = update_attention(self.attention, false, !output.input.is_empty());
        output
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
        let (grid, cursor, _selected, _off) = panel.snapshot();
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
        let (grid, _, _, _) = panel.snapshot();
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
    fn selection_over_row0_returns_text() {
        // High-value: exercise the REAL alacritty Selection API end-to-end.
        // Feed "hello\r\nworld", select row 0 cols 0..=4, assert the
        // extracted text is "hello".
        let mut panel = TerminalPanel::new(80, 24);
        panel.feed(b"hello\r\nworld");

        // Selection covers the whole word: from the left of (0,0) to the
        // right of (0,4) == "hello".
        let start = Point::new(Line(0), Column(0));
        let end = Point::new(Line(0), Column(4));
        panel.selection_start(start, Side::Left);
        panel.selection_update(end, Side::Right);

        assert_eq!(panel.selected_text().as_deref(), Some("hello"));
    }

    #[test]
    fn selection_clear_drops_text() {
        let mut panel = TerminalPanel::new(80, 24);
        panel.feed(b"hello");
        panel.selection_start(Point::new(Line(0), Column(0)), Side::Left);
        panel.selection_update(Point::new(Line(0), Column(4)), Side::Right);
        assert!(panel.selected_text().is_some());
        panel.selection_clear();
        assert!(panel.selected_text().is_none());
    }

    #[test]
    fn scroll_moves_display_offset_into_history() {
        // Fill enough lines to create scrollback, then scroll up and assert
        // the display offset moves (snapshot also follows). 5-row grid so a
        // handful of newlines overflow into history.
        let mut panel = TerminalPanel::new(20, 5);
        for i in 0..20 {
            panel.feed(format!("line{i}\r\n").as_bytes());
        }
        assert_eq!(panel.display_offset(), 0, "starts pinned to bottom");
        panel.scroll_lines(3);
        assert_eq!(panel.display_offset(), 3, "scrolled 3 lines into history");
        // Snapshot reports the same offset.
        let (_g, _c, _s, off) = panel.snapshot();
        assert_eq!(off, 3);
        // Scrolling back down past the bottom clamps at 0.
        panel.scroll_lines(-100);
        assert_eq!(panel.display_offset(), 0);
    }

    #[test]
    fn bracketed_paste_mode_detected() {
        let mut panel = TerminalPanel::new(80, 24);
        assert!(!panel.bracketed_paste(), "off by default");
        // DECSET 2004 enables bracketed paste.
        panel.feed(b"\x1b[?2004h");
        assert!(panel.bracketed_paste(), "enabled after DECSET 2004");
        panel.feed(b"\x1b[?2004l");
        assert!(!panel.bracketed_paste(), "disabled after DECRST 2004");
    }

    // ---- attention (bell-driven halo) -------------------------------

    #[test]
    fn panel_listener_sets_flag_on_bell_only() {
        let bell = Arc::new(AtomicBool::new(false));
        let listener = PanelListener { bell: bell.clone() };
        // Non-bell events are ignored.
        listener.send_event(Event::Wakeup);
        listener.send_event(Event::Title("t".into()));
        assert!(!bell.load(Ordering::Relaxed));
        // Bell sets the flag.
        listener.send_event(Event::Bell);
        assert!(bell.load(Ordering::Relaxed));
    }

    #[test]
    fn update_attention_table() {
        // (prev, bell, user_input) → expected
        let cases = [
            (false, false, false, false, "idle stays idle"),
            (false, true, false, true, "bell sets"),
            (true, false, false, true, "latches until input"),
            (true, false, true, false, "input clears"),
            (false, false, true, false, "input on idle is a no-op"),
            (true, true, true, false, "input wins over a same-frame bell"),
        ];
        for (prev, bell, input, want, why) in cases {
            assert_eq!(update_attention(prev, bell, input), want, "{why}");
        }
    }

    #[test]
    fn feed_bel_byte_sets_attention_via_vte() {
        // The integration-ish path: a real BEL byte through the actual
        // alacritty VTE → Event::Bell → PanelListener → attention.
        let mut panel = TerminalPanel::new(80, 24);
        assert!(!panel.attention(), "starts clear");
        panel.feed(b"claude needs you\x07");
        assert!(panel.attention(), "BEL through the VTE sets attention");
        // The grid still advanced normally around the BEL.
        assert_eq!(cell_at(&panel, 0, 0).c, 'c');
        // Further bell-less output leaves it latched.
        panel.feed(b" more output");
        assert!(panel.attention(), "latches across later output");
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
