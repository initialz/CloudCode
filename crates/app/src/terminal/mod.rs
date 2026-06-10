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

mod render;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Point;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{CursorShape, Processor};

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
        }
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

    /// Render the terminal grid into `ui` with a custom painter.
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        // Monospace cell metrics from the active font. For T3 we use
        // egui's built-in monospace; the CJK font is Task 4.
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
        let (response, painter) = ui.allocate_painter(size, egui::Sense::click());
        let origin = response.rect.min;

        // Backdrop so unfilled gaps read as the terminal background.
        painter.rect_filled(response.rect, 0.0, rgb(DEFAULT_BG));

        for (r, cells) in grid.iter().enumerate() {
            let y = origin.y + r as f32 * cell_h;
            for run in rows_to_runs(cells) {
                let x = origin.x + run.col as f32 * cell_w;
                let run_rect = egui::Rect::from_min_size(
                    egui::pos2(x, y),
                    egui::vec2(run.len as f32 * cell_w, cell_h),
                );
                // Paint the run's background unless it's the default (the
                // backdrop already covers that — saves a lot of rects).
                if run.bg != DEFAULT_BG {
                    painter.rect_filled(run_rect, 0.0, rgb(run.bg));
                }
                // Skip all-whitespace runs: nothing to draw.
                if run.text.trim().is_empty() {
                    continue;
                }
                let font = if run.bold {
                    egui::FontId::monospace(14.0) // weight bump is Task 4
                } else {
                    egui::FontId::monospace(14.0)
                };
                painter.text(
                    egui::pos2(x, y),
                    egui::Align2::LEFT_TOP,
                    &run.text,
                    font,
                    rgb(run.fg),
                );
            }
        }

        // Cursor: a block for T3 (beam/underline distinction is cosmetic
        // and can follow in Task 4). Drawn semi-transparent over the cell.
        if cur_visible {
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
}
