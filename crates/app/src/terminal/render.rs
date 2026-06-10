//! Pure rendering helpers for the terminal panel.
//!
//! Everything here is side-effect-free so it can be unit-tested without a
//! GUI: the grid→draw-data lowering (`rows_to_runs`) and the
//! alacritty/vte color → rgb mapping (`term_color_to_rgb`). The actual
//! egui painting lives in `mod.rs::TerminalPanel::ui`, which consumes
//! these.

use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

/// A single grid cell flattened to just what the painter needs: the
/// glyph plus resolved style. `fg`/`bg` are already RGB (default colors
/// resolved by the caller) so the run-merger and painter never touch the
/// alacritty `Color` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellView {
    pub ch: char,
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub inverse: bool,
    pub bold: bool,
}

impl CellView {
    /// The visual (fg, bg) after applying the INVERSE flag — what the
    /// painter actually draws with. Kept here so run-merging groups by
    /// the *effective* colors.
    pub fn effective(&self) -> ([u8; 3], [u8; 3]) {
        if self.inverse {
            (self.bg, self.fg)
        } else {
            (self.fg, self.bg)
        }
    }

    /// Two cells paint with the same style iff their effective colors and
    /// bold match. (The glyph differs per cell; only style merges.)
    fn same_style(&self, other: &CellView) -> bool {
        self.effective() == other.effective() && self.bold == other.bold
    }
}

/// A horizontal run of consecutive same-style cells on one row. `text`
/// is the concatenated glyphs; `col` is the starting column. The painter
/// fills one bg rect for the run then draws `text` once in `fg`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    /// Starting column of the run (0-based).
    pub col: usize,
    /// Number of cells the run spans (== chars in `text`).
    pub len: usize,
    pub text: String,
    /// Effective foreground (post-inverse).
    pub fg: [u8; 3],
    /// Effective background (post-inverse).
    pub bg: [u8; 3],
    pub bold: bool,
}

/// Merge a row of cells into runs of consecutive same-style cells.
///
/// PURE. A style change (effective fg/bg or bold) starts a new run; same
/// style appends to the current run. Empty input yields no runs.
pub fn rows_to_runs(cells: &[CellView]) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    for (i, cell) in cells.iter().enumerate() {
        let (fg, bg) = cell.effective();
        let extend = runs
            .last()
            .map(|_| i > 0 && cell.same_style(&cells[i - 1]))
            .unwrap_or(false);
        if extend {
            let run = runs.last_mut().expect("extend implies a last run");
            run.text.push(cell.ch);
            run.len += 1;
        } else {
            runs.push(Run {
                col: i,
                len: 1,
                text: cell.ch.to_string(),
                fg,
                bg,
                bold: cell.bold,
            });
        }
    }
    runs
}

/// The classic 16-color ANSI palette (the "VGA"-ish defaults Alacritty
/// ships). Indices 0..=15 line up with `NamedColor::Black..=BrightWhite`.
const ANSI_16: [[u8; 3]; 16] = [
    [0x00, 0x00, 0x00], // 0 black
    [0xcc, 0x33, 0x33], // 1 red
    [0x33, 0xcc, 0x33], // 2 green
    [0xcc, 0xcc, 0x33], // 3 yellow
    [0x33, 0x33, 0xcc], // 4 blue
    [0xcc, 0x33, 0xcc], // 5 magenta
    [0x33, 0xcc, 0xcc], // 6 cyan
    [0xcc, 0xcc, 0xcc], // 7 white
    [0x55, 0x55, 0x55], // 8 bright black
    [0xff, 0x55, 0x55], // 9 bright red
    [0x55, 0xff, 0x55], // 10 bright green
    [0xff, 0xff, 0x55], // 11 bright yellow
    [0x55, 0x55, 0xff], // 12 bright blue
    [0xff, 0x55, 0xff], // 13 bright magenta
    [0x55, 0xff, 0xff], // 14 bright cyan
    [0xff, 0xff, 0xff], // 15 bright white
];

/// Default foreground / background used when a cell carries the
/// `Foreground`/`Background` named color (or an unmapped special).
pub const DEFAULT_FG: [u8; 3] = [0xd0, 0xd0, 0xd0];
pub const DEFAULT_BG: [u8; 3] = [0x10, 0x10, 0x10];

/// Resolve a 256-color palette index to RGB.
///
/// 0..=15  → the named ANSI palette.
/// 16..=231 → the 6×6×6 color cube.
/// 232..=255 → the 24-step grayscale ramp.
fn indexed_to_rgb(i: u8) -> [u8; 3] {
    match i {
        0..=15 => ANSI_16[i as usize],
        16..=231 => {
            let i = i - 16;
            let r = i / 36;
            let g = (i % 36) / 6;
            let b = i % 6;
            let step = |c: u8| if c == 0 { 0 } else { 55 + c * 40 };
            [step(r), step(g), step(b)]
        }
        232..=255 => {
            let level = 8 + (i - 232) * 10;
            [level, level, level]
        }
    }
}

/// Map a named color to RGB. The 16 ANSI names go through the palette;
/// the bright/dim foreground variants and the cursor color fall back to
/// the defaults (good enough for T3 — themes come later).
fn named_to_rgb(name: NamedColor) -> [u8; 3] {
    match name {
        NamedColor::Black => ANSI_16[0],
        NamedColor::Red => ANSI_16[1],
        NamedColor::Green => ANSI_16[2],
        NamedColor::Yellow => ANSI_16[3],
        NamedColor::Blue => ANSI_16[4],
        NamedColor::Magenta => ANSI_16[5],
        NamedColor::Cyan => ANSI_16[6],
        NamedColor::White => ANSI_16[7],
        NamedColor::BrightBlack => ANSI_16[8],
        NamedColor::BrightRed => ANSI_16[9],
        NamedColor::BrightGreen => ANSI_16[10],
        NamedColor::BrightYellow => ANSI_16[11],
        NamedColor::BrightBlue => ANSI_16[12],
        NamedColor::BrightMagenta => ANSI_16[13],
        NamedColor::BrightCyan => ANSI_16[14],
        NamedColor::BrightWhite => ANSI_16[15],
        NamedColor::Foreground | NamedColor::BrightForeground | NamedColor::DimForeground => {
            DEFAULT_FG
        }
        NamedColor::Background => DEFAULT_BG,
        // Dim variants of the 8 base colors — approximate with the base.
        NamedColor::DimBlack => ANSI_16[0],
        NamedColor::DimRed => ANSI_16[1],
        NamedColor::DimGreen => ANSI_16[2],
        NamedColor::DimYellow => ANSI_16[3],
        NamedColor::DimBlue => ANSI_16[4],
        NamedColor::DimMagenta => ANSI_16[5],
        NamedColor::DimCyan => ANSI_16[6],
        NamedColor::DimWhite => ANSI_16[7],
        // Cursor color isn't used as a cell color in our render path.
        NamedColor::Cursor => DEFAULT_FG,
    }
}

/// Map an alacritty/vte `Color` (the value stored on each cell) to RGB.
///
/// PURE. `default` is what `NamedColor::Foreground`/`Background` and any
/// special-but-unthemed name resolves to — callers pass `DEFAULT_FG` for
/// fg cells and `DEFAULT_BG` for bg cells so the right default wins.
pub fn term_color_to_rgb(color: Color, default: [u8; 3]) -> [u8; 3] {
    match color {
        Color::Spec(Rgb { r, g, b }) => [r, g, b],
        Color::Indexed(i) => indexed_to_rgb(i),
        Color::Named(NamedColor::Foreground) | Color::Named(NamedColor::Background) => default,
        Color::Named(name) => named_to_rgb(name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(ch: char, fg: [u8; 3], bg: [u8; 3], inverse: bool, bold: bool) -> CellView {
        CellView { ch, fg, bg, inverse, bold }
    }

    const R: [u8; 3] = [0xcc, 0x33, 0x33];
    const G: [u8; 3] = [0x33, 0xcc, 0x33];
    const K: [u8; 3] = [0, 0, 0];

    #[test]
    fn empty_row_has_no_runs() {
        assert!(rows_to_runs(&[]).is_empty());
    }

    #[test]
    fn uniform_row_is_one_run() {
        let row = [
            cell('h', R, K, false, false),
            cell('i', R, K, false, false),
            cell('!', R, K, false, false),
        ];
        let runs = rows_to_runs(&row);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].col, 0);
        assert_eq!(runs[0].len, 3);
        assert_eq!(runs[0].text, "hi!");
        assert_eq!(runs[0].fg, R);
        assert_eq!(runs[0].bg, K);
    }

    #[test]
    fn fg_change_splits_runs() {
        let row = [
            cell('a', R, K, false, false),
            cell('b', G, K, false, false), // fg change
            cell('c', G, K, false, false),
        ];
        let runs = rows_to_runs(&row);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].text, "a");
        assert_eq!(runs[0].fg, R);
        assert_eq!(runs[1].col, 1);
        assert_eq!(runs[1].text, "bc");
        assert_eq!(runs[1].fg, G);
    }

    #[test]
    fn bold_change_splits_runs() {
        let row = [
            cell('a', R, K, false, false),
            cell('b', R, K, false, true), // bold change, same colors
        ];
        let runs = rows_to_runs(&row);
        assert_eq!(runs.len(), 2);
        assert!(!runs[0].bold);
        assert!(runs[1].bold);
    }

    #[test]
    fn inverse_merges_with_swapped_colors() {
        // An inverse R-on-K cell paints as K-on-R; it should NOT merge
        // with a plain R-on-K cell, but SHOULD merge with another inverse
        // R-on-K cell (same effective style).
        let row = [
            cell('a', R, K, false, false), // effective R/K
            cell('b', R, K, true, false),  // effective K/R
            cell('c', R, K, true, false),  // effective K/R
        ];
        let runs = rows_to_runs(&row);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].text, "a");
        assert_eq!(runs[0].fg, R);
        assert_eq!(runs[0].bg, K);
        assert_eq!(runs[1].text, "bc");
        assert_eq!(runs[1].fg, K, "inverse swaps fg/bg");
        assert_eq!(runs[1].bg, R);
    }

    #[test]
    fn named_red_maps_to_palette() {
        assert_eq!(
            term_color_to_rgb(Color::Named(NamedColor::Red), DEFAULT_FG),
            ANSI_16[1]
        );
    }

    #[test]
    fn default_fg_and_bg_resolve_to_passed_default() {
        assert_eq!(
            term_color_to_rgb(Color::Named(NamedColor::Foreground), DEFAULT_FG),
            DEFAULT_FG
        );
        assert_eq!(
            term_color_to_rgb(Color::Named(NamedColor::Background), DEFAULT_BG),
            DEFAULT_BG
        );
    }

    #[test]
    fn spec_color_is_passthrough() {
        assert_eq!(
            term_color_to_rgb(Color::Spec(Rgb { r: 1, g: 2, b: 3 }), DEFAULT_FG),
            [1, 2, 3]
        );
    }

    #[test]
    fn indexed_colors_cover_palette_cube_and_gray() {
        // 0..=15 -> ANSI palette.
        assert_eq!(term_color_to_rgb(Color::Indexed(9), DEFAULT_FG), ANSI_16[9]);
        // 16 is the start of the cube == pure black corner.
        assert_eq!(term_color_to_rgb(Color::Indexed(16), DEFAULT_FG), [0, 0, 0]);
        // 231 is the white corner of the cube (max in all channels).
        assert_eq!(
            term_color_to_rgb(Color::Indexed(231), DEFAULT_FG),
            [255, 255, 255]
        );
        // 232 is the darkest gray step.
        assert_eq!(term_color_to_rgb(Color::Indexed(232), DEFAULT_FG), [8, 8, 8]);
    }
}
