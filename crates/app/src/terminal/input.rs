//! Keyboard / IME → PTY bytes.
//!
//! egui delivers typed input two ways: a printable character arrives as
//! `Event::Text(String)`, and *every* key (printable or not) also arrives
//! as `Event::Key{..}`. For a plain letter you get BOTH a `Text("a")` and
//! a `Key{key: A}` in the same frame — so the rule is:
//!
//!   * printable text  -> take it from `Event::Text` (correct casing,
//!     layout, dead keys, AltGr all already resolved by the OS),
//!   * special / control keys (Enter, arrows, Ctrl-C, ...) -> take from
//!     `Event::Key`,
//!   * and NEVER emit a `Key` for a key that egui also delivered as text
//!     in the same frame (that's the dedup).
//!
//! IME composition (Chinese pinyin etc.) arrives as `Event::Ime`. While a
//! preedit is active the IME owns the keystrokes: we suppress Key/Text
//! emission and only emit on `Commit`.
//!
//! Everything here is pure and unit-tested; the GUI plumbing that feeds
//! these `egui::Event`s lives in `mod.rs::TerminalPanel::handle_egui_input`.

use unicode_width::UnicodeWidthChar;

/// How many terminal columns a char occupies: 2 for wide (CJK) glyphs,
/// 1 otherwise. Control chars and zero-width chars are treated as 1 so
/// the cursor never desyncs (the grid always stores them in one cell).
///
/// PURE.
pub fn cell_advance_cols(ch: char) -> usize {
    match UnicodeWidthChar::width(ch) {
        Some(2) => 2,
        _ => 1,
    }
}

/// Translate a single special / control key press into PTY bytes.
///
/// Returns `None` for keys that carry no terminal meaning on their own
/// (e.g. a bare letter with no Ctrl — that text comes via `Event::Text`).
///
/// PURE. `text` is the `Event::Text` payload egui paired with this key in
/// the same frame, if any; it lets us decide a plain letter is "owned by
/// Text" and emit nothing here.
pub fn key_to_bytes(
    key: egui::Key,
    mods: egui::Modifiers,
    text: Option<&str>,
) -> Option<Vec<u8>> {
    use egui::Key;

    // Ctrl + letter -> C0 control byte. Check this BEFORE the text
    // shortcut: Ctrl-C must win even though egui won't have produced text
    // for it anyway. Gate on `ctrl` ONLY (not `command`): on macOS the ⌘
    // key sets `command`/`mac_cmd` but not `ctrl`, and ⌘-C/V belong to the
    // app (copy/paste, Task 5) — they must not become C0 bytes. Real Ctrl
    // sets `ctrl` on every platform.
    if mods.ctrl && !mods.alt {
        if let Some(c) = letter_key_char(key) {
            // (c & 0x1f): 'a'/'A' -> 0x01, 'c' -> 0x03, 'd' -> 0x04, ...
            let ctrl = (c as u8) & 0x1f;
            return Some(vec![ctrl]);
        }
        // A few non-letter Ctrl combos used by shells.
        match key {
            // Ctrl-[ is ESC; Ctrl-\ is FS; Ctrl-] is GS. Best-effort.
            Key::OpenBracket => return Some(vec![0x1b]),
            Key::Backslash => return Some(vec![0x1c]),
            Key::CloseBracket => return Some(vec![0x1d]),
            _ => {}
        }
    }

    let bytes: &[u8] = match key {
        Key::Enter => b"\r",
        Key::Backspace => &[0x7f],
        Key::Tab => b"\t",
        Key::Escape => &[0x1b],
        Key::ArrowUp => b"\x1b[A",
        Key::ArrowDown => b"\x1b[B",
        Key::ArrowRight => b"\x1b[C",
        Key::ArrowLeft => b"\x1b[D",
        Key::Home => b"\x1b[H",
        Key::End => b"\x1b[F",
        Key::PageUp => b"\x1b[5~",
        Key::PageDown => b"\x1b[6~",
        Key::Delete => b"\x1b[3~",
        Key::Insert => b"\x1b[2~",
        // Everything else: if egui gave us text for it, it's printable and
        // handled by the Text path; emit nothing here.
        _ => {
            let _ = text;
            return None;
        }
    };
    Some(bytes.to_vec())
}

/// The lowercase ASCII char for a letter `Key`, or `None` for non-letters.
/// Used to fold Ctrl+letter into a C0 control byte.
fn letter_key_char(key: egui::Key) -> Option<char> {
    use egui::Key::*;
    let c = match key {
        A => 'a', B => 'b', C => 'c', D => 'd', E => 'e', F => 'f',
        G => 'g', H => 'h', I => 'i', J => 'j', K => 'k', L => 'l',
        M => 'm', N => 'n', O => 'o', P => 'p', Q => 'q', R => 'r',
        S => 's', T => 't', U => 'u', V => 'v', W => 'w', X => 'x',
        Y => 'y', Z => 'z',
        _ => return None,
    };
    Some(c)
}

/// The IME composition state machine state. `preedit` is the in-progress
/// (not-yet-committed) composition string, rendered inline at the cursor
/// but NOT sent to the PTY. Empty == no active composition.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImeState {
    pub preedit: String,
}

impl ImeState {
    /// Whether a composition is currently active (non-empty preedit).
    /// While active, Key/Text byte emission is suppressed.
    pub fn is_composing(&self) -> bool {
        !self.preedit.is_empty()
    }
}

/// Fold one `egui::ImeEvent` into the IME state.
///
/// Returns the new state plus, on a `Commit`, the UTF-8 bytes to send to
/// the PTY (the committed Chinese text). All other transitions yield no
/// bytes.
///
/// PURE.
///
/// * `Enabled`         -> clear preedit (fresh composition), no bytes.
/// * `Preedit(s)`      -> store `s` as the live preedit, no bytes. (An
///                        empty `s` means the IME dismissed the preedit.)
/// * `Commit(s)`       -> clear preedit, emit `s.as_bytes()`.
/// * `Disabled`        -> clear preedit, no bytes.
pub fn ime_apply(
    mut state: ImeState,
    event: &egui::ImeEvent,
) -> (ImeState, Option<Vec<u8>>) {
    use egui::ImeEvent;
    match event {
        ImeEvent::Enabled => {
            state.preedit.clear();
            (state, None)
        }
        ImeEvent::Preedit(s) => {
            state.preedit = s.clone();
            (state, None)
        }
        ImeEvent::Commit(s) => {
            state.preedit.clear();
            let bytes = if s.is_empty() {
                None
            } else {
                Some(s.as_bytes().to_vec())
            };
            (state, bytes)
        }
        ImeEvent::Disabled => {
            state.preedit.clear();
            (state, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{ImeEvent, Key, Modifiers};

    fn ctrl() -> Modifiers {
        Modifiers {
            ctrl: true,
            command: true, // on mac egui sets command for ⌘; ctrl path also
            ..Modifiers::default()
        }
    }

    #[test]
    fn enter_is_carriage_return() {
        assert_eq!(
            key_to_bytes(Key::Enter, Modifiers::default(), None),
            Some(b"\r".to_vec())
        );
    }

    #[test]
    fn backspace_is_del() {
        assert_eq!(
            key_to_bytes(Key::Backspace, Modifiers::default(), None),
            Some(vec![0x7f])
        );
    }

    #[test]
    fn tab_and_escape() {
        assert_eq!(
            key_to_bytes(Key::Tab, Modifiers::default(), None),
            Some(b"\t".to_vec())
        );
        assert_eq!(
            key_to_bytes(Key::Escape, Modifiers::default(), None),
            Some(vec![0x1b])
        );
    }

    #[test]
    fn arrows_are_csi() {
        let m = Modifiers::default();
        assert_eq!(key_to_bytes(Key::ArrowUp, m, None), Some(b"\x1b[A".to_vec()));
        assert_eq!(key_to_bytes(Key::ArrowDown, m, None), Some(b"\x1b[B".to_vec()));
        assert_eq!(key_to_bytes(Key::ArrowRight, m, None), Some(b"\x1b[C".to_vec()));
        assert_eq!(key_to_bytes(Key::ArrowLeft, m, None), Some(b"\x1b[D".to_vec()));
    }

    #[test]
    fn nav_keys_are_csi() {
        let m = Modifiers::default();
        assert_eq!(key_to_bytes(Key::Home, m, None), Some(b"\x1b[H".to_vec()));
        assert_eq!(key_to_bytes(Key::End, m, None), Some(b"\x1b[F".to_vec()));
        assert_eq!(key_to_bytes(Key::PageUp, m, None), Some(b"\x1b[5~".to_vec()));
        assert_eq!(key_to_bytes(Key::PageDown, m, None), Some(b"\x1b[6~".to_vec()));
        assert_eq!(key_to_bytes(Key::Delete, m, None), Some(b"\x1b[3~".to_vec()));
    }

    #[test]
    fn ctrl_c_is_etx() {
        assert_eq!(key_to_bytes(Key::C, ctrl(), None), Some(vec![0x03]));
    }

    #[test]
    fn ctrl_d_is_eot() {
        assert_eq!(key_to_bytes(Key::D, ctrl(), None), Some(vec![0x04]));
    }

    #[test]
    fn plain_letter_yields_no_key_bytes() {
        // A bare 'a' produces no Key bytes — the char comes via Event::Text.
        assert_eq!(key_to_bytes(Key::A, Modifiers::default(), Some("a")), None);
        assert_eq!(key_to_bytes(Key::A, Modifiers::default(), None), None);
    }

    #[test]
    fn ctrl_wins_over_text() {
        // Even if egui somehow paired text, Ctrl-C is a control byte.
        assert_eq!(key_to_bytes(Key::C, ctrl(), Some("c")), Some(vec![0x03]));
    }

    #[test]
    fn mac_cmd_without_ctrl_is_not_a_control_byte() {
        // macOS ⌘-C sets command/mac_cmd but NOT ctrl — it belongs to the
        // app (copy), so it must not become 0x03. No text either.
        let cmd = Modifiers {
            command: true,
            mac_cmd: true,
            ..Modifiers::default()
        };
        assert_eq!(key_to_bytes(Key::C, cmd, None), None);
    }

    #[test]
    fn cell_advance_ascii_is_one() {
        assert_eq!(cell_advance_cols('a'), 1);
        assert_eq!(cell_advance_cols('Z'), 1);
        assert_eq!(cell_advance_cols(' '), 1);
    }

    #[test]
    fn cell_advance_cjk_is_two() {
        assert_eq!(cell_advance_cols('你'), 2);
        assert_eq!(cell_advance_cols('好'), 2);
        assert_eq!(cell_advance_cols('，'), 2); // fullwidth comma
    }

    #[test]
    fn cell_advance_control_is_one() {
        assert_eq!(cell_advance_cols('\t'), 1);
        assert_eq!(cell_advance_cols('\n'), 1);
        assert_eq!(cell_advance_cols('\0'), 1);
    }

    // ---- IME state machine ----

    #[test]
    fn ime_enabled_clears_preedit() {
        let s = ImeState { preedit: "stale".into() };
        let (s, bytes) = ime_apply(s, &ImeEvent::Enabled);
        assert_eq!(s.preedit, "");
        assert!(bytes.is_none());
        assert!(!s.is_composing());
    }

    #[test]
    fn ime_preedit_sets_string_no_bytes() {
        let (s, bytes) = ime_apply(ImeState::default(), &ImeEvent::Preedit("ni".into()));
        assert_eq!(s.preedit, "ni");
        assert!(bytes.is_none());
        assert!(s.is_composing());
    }

    #[test]
    fn ime_commit_clears_and_emits_utf8() {
        let s = ImeState { preedit: "ni".into() };
        let (s, bytes) = ime_apply(s, &ImeEvent::Commit("你".into()));
        assert_eq!(s.preedit, "");
        assert_eq!(bytes, Some("你".as_bytes().to_vec()));
        assert!(!s.is_composing());
    }

    #[test]
    fn ime_commit_empty_emits_nothing() {
        let s = ImeState { preedit: "x".into() };
        let (s, bytes) = ime_apply(s, &ImeEvent::Commit(String::new()));
        assert_eq!(s.preedit, "");
        assert!(bytes.is_none());
    }

    #[test]
    fn ime_disabled_clears() {
        let s = ImeState { preedit: "abc".into() };
        let (s, bytes) = ime_apply(s, &ImeEvent::Disabled);
        assert_eq!(s.preedit, "");
        assert!(bytes.is_none());
    }

    #[test]
    fn ime_empty_preedit_means_dismissed() {
        let s = ImeState { preedit: "ni".into() };
        let (s, bytes) = ime_apply(s, &ImeEvent::Preedit(String::new()));
        assert_eq!(s.preedit, "");
        assert!(bytes.is_none());
        assert!(!s.is_composing());
    }
}
