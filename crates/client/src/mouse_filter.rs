//! Strip mouse-mode CSI escapes from the agent → terminal byte stream.
//!
//! Approach A trial: by hiding `ESC [ ? 1000 h` (and friends) from the
//! user's local terminal emulator, the emulator never enters
//! "mouse reporting" mode. iTerm2 / Terminal.app / etc keep doing
//! their own native drag-to-select + Cmd+C copy as if claude never
//! requested mouse input. The TUI on the agent side still BELIEVES
//! mouse mode is on (we don't intercept the response), so it'll send
//! `\x1b[<…M` mouse-event style escapes that the emulator no longer
//! emits — meaning claude's permission buttons and other mouse
//! interactions go dark. That's the trade-off we're testing.
//!
//! Modes stripped (the X11/SGR mouse-tracking family):
//!   1000 X10 mouse reporting
//!   1001 highlight tracking
//!   1002 button-event tracking
//!   1003 any-event tracking
//!   1005 UTF-8 mouse encoding
//!   1006 SGR mouse encoding
//!   1015 urxvt mouse encoding
//!   1016 SGR-pixel mouse encoding
//!
//! Also strips alt-screen modes (47, 1047, 1049) so the local
//! terminal stays in the main screen and its native scrollback
//! works. Same approach as webterm's escape filter.

/// DEC private modes to strip. Mouse-tracking family + alt-screen.
const STRIPPED_MODES: &[&[u8]] = &[
    b"1000", b"1001", b"1002", b"1003", b"1005", b"1006", b"1015", b"1016",
    b"47", b"1047", b"1049",
];

pub struct MouseModeStripper {
    /// Bytes after a partial `ESC` start, awaiting either completion
    /// (so we can decide to keep or strip) or enough evidence that
    /// they're not a private CSI (so we flush them to output).
    pending: Vec<u8>,
}

impl Default for MouseModeStripper {
    fn default() -> Self {
        Self::new()
    }
}

impl MouseModeStripper {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
        }
    }

    /// Run the filter over a chunk, returning the chunk minus any
    /// mouse-mode-enabling escapes. State carries between calls so
    /// CSIs split across chunk boundaries are handled correctly.
    pub fn filter(&mut self, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len());
        for &b in input {
            if self.pending.is_empty() {
                if b == 0x1b {
                    self.pending.push(b);
                } else {
                    out.push(b);
                }
                continue;
            }
            self.pending.push(b);
            match decide(&self.pending) {
                Decision::Incomplete => {}
                Decision::Strip => self.pending.clear(),
                Decision::Flush => out.append(&mut self.pending),
            }
        }
        out
    }
}

enum Decision {
    /// Need more bytes before we can choose.
    Incomplete,
    /// Discard the buffered sequence entirely.
    Strip,
    /// Emit the buffered bytes verbatim — it wasn't (or wasn't only) a
    /// mouse-mode CSI.
    Flush,
}

/// `buf[0]` is always `ESC` when we get called.
fn decide(buf: &[u8]) -> Decision {
    // After ESC, we need at least one more byte to know if this is a
    // CSI start.
    if buf.len() < 2 {
        return Decision::Incomplete;
    }
    // Anything other than `[` after ESC is not a CSI — let it through.
    if buf[1] != b'[' {
        return Decision::Flush;
    }
    if buf.len() < 3 {
        return Decision::Incomplete;
    }
    // Not a *private* DEC parameter sequence (which is what mouse
    // modes are). Standard CSI (cursor moves, colours, …) flushes.
    if buf[2] != b'?' {
        return Decision::Flush;
    }
    // Need at least one byte past the `?` marker before we can read
    // a final byte — otherwise we'd treat the `?` itself as one.
    if buf.len() < 4 {
        return Decision::Incomplete;
    }
    // CSI private parameter body: digits and semicolons, then a final
    // byte in 0x40..=0x7E.
    let last = *buf.last().unwrap();
    if last.is_ascii_digit() || last == b';' {
        return Decision::Incomplete;
    }
    if !(0x40..=0x7e).contains(&last) {
        // Some non-final, non-parameter byte slipped in — bail.
        return Decision::Flush;
    }
    // Complete private CSI. Only `h` (set) / `l` (reset) are mode
    // toggles; anything else uses ?-params for a different purpose
    // and we let it through.
    if last != b'h' && last != b'l' {
        return Decision::Flush;
    }
    let params = &buf[3..buf.len() - 1];
    // The terminal applies the toggle to EVERY param in the list, so
    // if any one of them is a mouse mode we strip the whole CSI.
    // Cost: a CSI like `?1006;1049h` would lose the alt-screen toggle
    // as collateral. In practice claude emits these one at a time so
    // this loss doesn't show up; if it ever did we'd rewrite the CSI
    // with the surviving params.
    for chunk in params.split(|&c| c == b';') {
        if STRIPPED_MODES.contains(&chunk) {
            return Decision::Strip;
        }
    }
    Decision::Flush
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&[u8]]) -> Vec<u8> {
        let mut s = MouseModeStripper::new();
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend(s.filter(chunk));
        }
        out
    }

    #[test]
    fn strips_x10_enable() {
        assert_eq!(run(&[b"\x1b[?1000h"]), b"");
    }

    #[test]
    fn strips_sgr_enable() {
        assert_eq!(run(&[b"\x1b[?1006h"]), b"");
    }

    #[test]
    fn strips_disable() {
        assert_eq!(run(&[b"\x1b[?1000l"]), b"");
    }

    #[test]
    fn strips_alt_screen_toggle() {
        assert_eq!(run(&[b"\x1b[?1049h"]), b"");
        assert_eq!(run(&[b"\x1b[?1049l"]), b"");
        assert_eq!(run(&[b"\x1b[?47h"]), b"");
        assert_eq!(run(&[b"\x1b[?1047h"]), b"");
    }

    #[test]
    fn keeps_cursor_visibility() {
        assert_eq!(run(&[b"\x1b[?25h"]), b"\x1b[?25h");
        assert_eq!(run(&[b"\x1b[?25l"]), b"\x1b[?25l");
    }

    #[test]
    fn keeps_focus_tracking() {
        // 1004 is focus events; not in our mouse list, pass through.
        assert_eq!(run(&[b"\x1b[?1004h"]), b"\x1b[?1004h");
    }

    #[test]
    fn keeps_text_in_between() {
        assert_eq!(
            run(&[b"hello\x1b[?1000hworld\x1b[?1000lend"]),
            b"helloworldend"
        );
    }

    #[test]
    fn keeps_other_csi() {
        // SGR colour (no ?), cursor move, etc.
        assert_eq!(run(&[b"\x1b[31mred\x1b[0m"]), b"\x1b[31mred\x1b[0m");
        assert_eq!(run(&[b"\x1b[H\x1b[2J"]), b"\x1b[H\x1b[2J");
    }

    #[test]
    fn handles_csi_split_across_chunks() {
        // \x1b[?1000h split into many tiny chunks must still strip.
        let parts: Vec<&[u8]> = vec![
            b"\x1b", b"[", b"?", b"1", b"0", b"0", b"0", b"h",
        ];
        assert_eq!(run(&parts), b"");
    }

    #[test]
    fn keeps_combined_csi_with_alt_screen() {
        // `?1006;1049h` is a combined toggle. Current implementation
        // strips the whole CSI on any mouse-mode hit; documenting
        // this here so a future rewrite that surgically removes only
        // the mouse param can change the assertion deliberately.
        assert_eq!(run(&[b"\x1b[?1006;1049h"]), b"");
    }

    #[test]
    fn lone_esc_buffers_until_resolved() {
        let mut s = MouseModeStripper::new();
        // ESC alone is buffered (could be start of a CSI).
        assert_eq!(s.filter(b"\x1b"), b"");
        // Followed by `A` (cursor up CSI start ANSI), but we only
        // looked at `\x1b[`-style here. The single byte after ESC
        // being `A` means it's NOT a CSI bracket — flush.
        assert_eq!(s.filter(b"A"), b"\x1bA");
    }

    #[test]
    fn lone_esc_then_real_csi() {
        let mut s = MouseModeStripper::new();
        // First the lone ESC (no `[`).
        assert_eq!(s.filter(b"\x1bN"), b"\x1bN"); // SS2 — not CSI, flush
        // Then a real mouse-mode CSI.
        assert_eq!(s.filter(b"\x1b[?1000h"), b"");
    }
}
