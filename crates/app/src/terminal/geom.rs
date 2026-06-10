//! Pure geometry + throttle helpers for the terminal panel.
//!
//! These are side-effect-free so they can be unit-tested without a GUI:
//!   * [`grid_dims`]   — painter pixels → terminal cols/rows.
//!   * [`ResizeThrottle`] — debounce resize emission during a drag.
//!   * [`wheel_to_scroll_lines`] — scroll pixels → grid line delta.
//!   * [`pixel_to_grid`] — pointer pixels → grid `Point` (display-offset aware).
//!   * [`wrap_paste`]   — wrap pasted text in bracketed-paste markers.
//!
//! The egui/alacritty plumbing that calls these lives in
//! `mod.rs::TerminalPanel`.

use std::time::{Duration, Instant};

use alacritty_terminal::index::{Column, Line, Point, Side};

/// Compute the terminal grid dimensions that fit in a painter region.
///
/// `cols = floor(avail_w / cell_w)`, `rows = floor(avail_h / cell_h)`, each
/// clamped to a minimum of 1 (a zero-size grid is illegal for alacritty).
///
/// PURE.
pub fn grid_dims(avail_w: f32, avail_h: f32, cell_w: f32, cell_h: f32) -> (u16, u16) {
    // Guard against zero/negative cell metrics (uninitialized fonts) so we
    // never divide by zero or produce a wild count.
    let cell_w = cell_w.max(1.0);
    let cell_h = cell_h.max(1.0);
    let cols = (avail_w / cell_w).floor();
    let rows = (avail_h / cell_h).floor();
    let clamp = |v: f32| -> u16 {
        if !v.is_finite() || v < 1.0 {
            1
        } else if v > u16::MAX as f32 {
            u16::MAX
        } else {
            v as u16
        }
    };
    (clamp(cols), clamp(rows))
}

/// Minimum interval between two resize emissions to the hub. Below this we
/// hold the change as `pending` and flush it on the next `update` past the
/// window (trailing-edge send) — so a drag emits at most ~12/s, not every
/// frame, but the final size still lands.
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(80);

/// Debounces terminal resize emission during a window drag.
///
/// `term.resize()` is cheap and happens every time the dims change, but
/// emitting a `Resize` frame to the hub on every drag frame would flood the
/// wire. [`update`](ResizeThrottle::update) returns `Some(dims)` only when
/// the dims actually changed *and* enough time has elapsed since the last
/// send; otherwise it stashes the change as `pending` so a later `update`
/// (or an explicit [`flush_pending`](ResizeThrottle::flush_pending)) lands
/// the trailing edge.
#[derive(Debug, Clone, Default)]
pub struct ResizeThrottle {
    /// The last dims we actually emitted (and the time we emitted them).
    last_sent: Option<((u16, u16), Instant)>,
    /// A changed-but-not-yet-emitted size, held back by the debounce window.
    pending: Option<(u16, u16)>,
}

impl ResizeThrottle {
    pub fn new() -> ResizeThrottle {
        ResizeThrottle::default()
    }

    /// Feed the current grid dims at time `now`. Returns the dims to emit to
    /// the hub, or `None` to suppress this frame.
    ///
    /// Rules:
    ///   * Same dims as last sent (and nothing pending) → `None`.
    ///   * Changed dims, first ever OR ≥ debounce since last send → emit now.
    ///   * Changed dims within the debounce window → stash as `pending`,
    ///     return `None` (a later call past the window flushes it).
    pub fn update(&mut self, dims: (u16, u16), now: Instant) -> Option<(u16, u16)> {
        match self.last_sent {
            None => {
                // First resize ever — always send so the hub leaves 80×24.
                self.last_sent = Some((dims, now));
                self.pending = None;
                Some(dims)
            }
            Some((last_dims, last_at)) => {
                if dims == last_dims {
                    // Settled back to what the hub already has. Clear any
                    // stale pending and stay quiet.
                    self.pending = None;
                    return None;
                }
                if now.duration_since(last_at) >= RESIZE_DEBOUNCE {
                    self.last_sent = Some((dims, now));
                    self.pending = None;
                    Some(dims)
                } else {
                    // Too soon: remember the latest size for a trailing send.
                    self.pending = Some(dims);
                    None
                }
            }
        }
    }

    /// Flush a pending (debounced) size if the window has now elapsed. The
    /// UI calls this on idle frames so the trailing edge of a drag still
    /// reaches the hub even after the pointer stops moving.
    ///
    /// Returns the dims to emit, or `None` if nothing is pending / still
    /// inside the window.
    pub fn flush_pending(&mut self, now: Instant) -> Option<(u16, u16)> {
        let dims = self.pending?;
        match self.last_sent {
            Some((last_dims, last_at)) => {
                if dims == last_dims {
                    self.pending = None;
                    None
                } else if now.duration_since(last_at) >= RESIZE_DEBOUNCE {
                    self.last_sent = Some((dims, now));
                    self.pending = None;
                    Some(dims)
                } else {
                    None
                }
            }
            None => {
                self.last_sent = Some((dims, now));
                self.pending = None;
                Some(dims)
            }
        }
    }
}

/// Convert a vertical scroll delta (egui smooth pixels) into a count of
/// grid lines to scroll. Positive `delta_y_px` (wheel up / content down in
/// egui's convention) scrolls *back into history* (positive line count);
/// negative scrolls toward the bottom.
///
/// One line per `cell_h` pixels, rounded toward zero, but any non-zero
/// sub-line delta still moves at least one line so a gentle trackpad nudge
/// isn't swallowed.
///
/// PURE.
pub fn wheel_to_scroll_lines(delta_y_px: f32, cell_h: f32) -> i32 {
    if delta_y_px == 0.0 || !delta_y_px.is_finite() {
        return 0;
    }
    let cell_h = cell_h.max(1.0);
    let lines = delta_y_px / cell_h;
    let whole = lines.trunc() as i32;
    if whole != 0 {
        whole
    } else {
        // Sub-line nudge: move one line in the gesture's direction.
        if delta_y_px > 0.0 {
            1
        } else {
            -1
        }
    }
}

/// Map a pointer position (pixels relative to the grid's top-left origin)
/// to an alacritty grid [`Point`] plus the [`Side`] of the cell the pointer
/// is on (left/right half — alacritty uses this for selection boundaries).
///
/// `display_offset` is how many lines the viewport is scrolled back into
/// history; it shifts the absolute grid line so a selection started while
/// scrolled up references the right history row.
///
/// The column is clamped to `[0, cols)` and the viewport row to `[0, rows)`
/// so an out-of-bounds drag pins to the edge rather than panicking.
///
/// PURE.
pub fn pixel_to_grid(
    x: f32,
    y: f32,
    cell_w: f32,
    cell_h: f32,
    cols: usize,
    rows: usize,
    display_offset: usize,
) -> (Point, Side) {
    let cell_w = cell_w.max(1.0);
    let cell_h = cell_h.max(1.0);

    let col_f = (x / cell_w).floor();
    let max_col = cols.saturating_sub(1);
    let col = if col_f < 0.0 {
        0
    } else {
        (col_f as usize).min(max_col)
    };

    let row_f = (y / cell_h).floor();
    let max_row = rows.saturating_sub(1);
    let viewport_row = if row_f < 0.0 {
        0
    } else {
        (row_f as usize).min(max_row)
    };

    // Viewport row 0 == top visible line. The absolute grid line is the
    // viewport row minus the display offset (matches alacritty's own
    // `viewport_to_point`).
    let line = Line(viewport_row as i32) - display_offset;

    // Which half of the cell is the pointer on? Left half => the boundary
    // sits to the left of this cell, right half => to its right.
    let cell_left = col as f32 * cell_w;
    let side = if x - cell_left < cell_w / 2.0 {
        Side::Left
    } else {
        Side::Right
    };

    (Point::new(line, Column(col)), side)
}

/// Wrap pasted text for the PTY: if the terminal has bracketed-paste mode
/// enabled, surround it with the `ESC[200~` … `ESC[201~` markers so the
/// application (shell, editor) treats it as a single literal paste rather
/// than typed-and-interpreted input. Otherwise send the raw bytes.
///
/// PURE.
pub fn wrap_paste(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        let mut out = Vec::with_capacity(text.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(text.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        text.as_bytes().to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- grid_dims ----

    #[test]
    fn grid_dims_exact_division() {
        // 800/8 = 100 cols, 480/16 = 30 rows, no remainder.
        assert_eq!(grid_dims(800.0, 480.0, 8.0, 16.0), (100, 30));
    }

    #[test]
    fn grid_dims_rounds_down() {
        // 805/8 = 100.6 -> 100; 489/16 = 30.5 -> 30.
        assert_eq!(grid_dims(805.0, 489.0, 8.0, 16.0), (100, 30));
    }

    #[test]
    fn grid_dims_clamps_to_min_one() {
        // A region smaller than one cell still yields a 1×1 grid.
        assert_eq!(grid_dims(3.0, 5.0, 8.0, 16.0), (1, 1));
        assert_eq!(grid_dims(0.0, 0.0, 8.0, 16.0), (1, 1));
    }

    #[test]
    fn grid_dims_guards_bad_cell_metrics() {
        // Zero cell size must not divide-by-zero; clamp cell to 1px.
        let (c, r) = grid_dims(10.0, 10.0, 0.0, 0.0);
        assert_eq!((c, r), (10, 10));
    }

    // ---- ResizeThrottle ----

    #[test]
    fn throttle_sends_first_resize_immediately() {
        let mut t = ResizeThrottle::new();
        let now = Instant::now();
        assert_eq!(t.update((100, 30), now), Some((100, 30)));
    }

    #[test]
    fn throttle_suppresses_unchanged_dims() {
        let mut t = ResizeThrottle::new();
        let now = Instant::now();
        assert_eq!(t.update((100, 30), now), Some((100, 30)));
        // Same dims a moment later -> nothing to send.
        assert_eq!(t.update((100, 30), now + Duration::from_millis(200)), None);
    }

    #[test]
    fn throttle_debounces_rapid_changes_within_window() {
        let mut t = ResizeThrottle::new();
        let t0 = Instant::now();
        assert_eq!(t.update((100, 30), t0), Some((100, 30)));
        // A change 10ms later (inside the 80ms window) is held back.
        assert_eq!(t.update((101, 30), t0 + Duration::from_millis(10)), None);
        assert_eq!(t.update((102, 30), t0 + Duration::from_millis(20)), None);
    }

    #[test]
    fn throttle_sends_change_after_window_elapses() {
        let mut t = ResizeThrottle::new();
        let t0 = Instant::now();
        assert_eq!(t.update((100, 30), t0), Some((100, 30)));
        // A change past the 80ms window flushes immediately with the latest.
        assert_eq!(
            t.update((105, 31), t0 + Duration::from_millis(90)),
            Some((105, 31))
        );
    }

    #[test]
    fn throttle_trailing_flush_lands_final_size() {
        let mut t = ResizeThrottle::new();
        let t0 = Instant::now();
        assert_eq!(t.update((100, 30), t0), Some((100, 30)));
        // Rapid drag: several changes inside the window, all suppressed,
        // but the last one is remembered as pending.
        assert_eq!(t.update((101, 30), t0 + Duration::from_millis(10)), None);
        assert_eq!(t.update((103, 30), t0 + Duration::from_millis(20)), None);
        // Pointer stops; an idle flush past the window emits the trailing size.
        assert_eq!(
            t.flush_pending(t0 + Duration::from_millis(100)),
            Some((103, 30))
        );
        // Nothing left pending afterwards.
        assert_eq!(t.flush_pending(t0 + Duration::from_millis(200)), None);
    }

    #[test]
    fn throttle_flush_noop_without_pending() {
        let mut t = ResizeThrottle::new();
        let t0 = Instant::now();
        assert_eq!(t.update((100, 30), t0), Some((100, 30)));
        assert_eq!(t.flush_pending(t0 + Duration::from_millis(200)), None);
    }

    // ---- wheel_to_scroll_lines ----

    #[test]
    fn wheel_zero_is_zero() {
        assert_eq!(wheel_to_scroll_lines(0.0, 16.0), 0);
    }

    #[test]
    fn wheel_full_lines_round_toward_zero() {
        assert_eq!(wheel_to_scroll_lines(32.0, 16.0), 2);
        assert_eq!(wheel_to_scroll_lines(-48.0, 16.0), -3);
        // 40/16 = 2.5 -> trunc -> 2.
        assert_eq!(wheel_to_scroll_lines(40.0, 16.0), 2);
    }

    #[test]
    fn wheel_sub_line_nudge_moves_one_line() {
        // A small delta (< one cell) still moves a single line in-direction.
        assert_eq!(wheel_to_scroll_lines(4.0, 16.0), 1);
        assert_eq!(wheel_to_scroll_lines(-4.0, 16.0), -1);
    }

    // ---- pixel_to_grid ----

    #[test]
    fn pixel_to_grid_maps_origin_to_zero() {
        let (p, side) = pixel_to_grid(0.0, 0.0, 8.0, 16.0, 80, 24, 0);
        assert_eq!(p.line, Line(0));
        assert_eq!(p.column, Column(0));
        assert_eq!(side, Side::Left);
    }

    #[test]
    fn pixel_to_grid_picks_cell_and_side() {
        // x in [8,16) -> col 1; 8..12 left half, 12..16 right half.
        let (p, side) = pixel_to_grid(9.0, 0.0, 8.0, 16.0, 80, 24, 0);
        assert_eq!(p.column, Column(1));
        assert_eq!(side, Side::Left);
        let (_, side2) = pixel_to_grid(14.0, 0.0, 8.0, 16.0, 80, 24, 0);
        assert_eq!(side2, Side::Right);
    }

    #[test]
    fn pixel_to_grid_row_maps_to_line() {
        // y in [32,48) -> viewport row 2 -> line 2 with no scrollback.
        let (p, _) = pixel_to_grid(0.0, 33.0, 8.0, 16.0, 80, 24, 0);
        assert_eq!(p.line, Line(2));
    }

    #[test]
    fn pixel_to_grid_applies_display_offset() {
        // Scrolled 5 lines back: viewport row 0 references grid line -5.
        let (p, _) = pixel_to_grid(0.0, 0.0, 8.0, 16.0, 80, 24, 5);
        assert_eq!(p.line, Line(-5));
        // Viewport row 3 with offset 5 -> line 3 - 5 = -2.
        let (p2, _) = pixel_to_grid(0.0, 50.0, 8.0, 16.0, 80, 24, 5);
        assert_eq!(p2.line, Line(-2));
    }

    #[test]
    fn pixel_to_grid_clamps_out_of_bounds() {
        // Past the right/bottom edge pins to the last cell, not OOB.
        let (p, _) = pixel_to_grid(10_000.0, 10_000.0, 8.0, 16.0, 80, 24, 0);
        assert_eq!(p.column, Column(79));
        assert_eq!(p.line, Line(23));
        // Negative (above/left of origin) pins to 0.
        let (p2, _) = pixel_to_grid(-5.0, -5.0, 8.0, 16.0, 80, 24, 0);
        assert_eq!(p2.column, Column(0));
        assert_eq!(p2.line, Line(0));
    }

    // ---- wrap_paste ----

    #[test]
    fn wrap_paste_raw_when_not_bracketed() {
        assert_eq!(wrap_paste("hi", false), b"hi".to_vec());
    }

    #[test]
    fn wrap_paste_wraps_when_bracketed() {
        let out = wrap_paste("hi", true);
        let mut expected = Vec::new();
        expected.extend_from_slice(b"\x1b[200~");
        expected.extend_from_slice(b"hi");
        expected.extend_from_slice(b"\x1b[201~");
        assert_eq!(out, expected);
    }

    #[test]
    fn wrap_paste_empty_text() {
        assert_eq!(wrap_paste("", false), Vec::<u8>::new());
        assert_eq!(wrap_paste("", true), b"\x1b[200~\x1b[201~".to_vec());
    }
}
