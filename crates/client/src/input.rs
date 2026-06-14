//! Raw stdin byte pump shared by menu and relay.
//!
//! A blocking std thread reads stdin in chunks and forwards them through an
//! mpsc channel. The PTY relay consumes bytes verbatim — vital for keeping
//! claude's terminal queries (DA1/DA2 responses, cursor position reports,
//! mouse events, anything the terminal echoes back) byte-perfect. The menu
//! reuses the same channel and runs the bytes through a tiny ANSI parser
//! to recover the small set of keys it actually needs.

use std::io::Read;
use tokio::sync::mpsc;

pub type ByteRx = mpsc::Receiver<Vec<u8>>;

const CHUNK_QUEUE: usize = 64;
const READ_BUF: usize = 4096;

pub fn spawn_byte_reader() -> ByteRx {
    let (tx, rx) = mpsc::channel::<Vec<u8>>(CHUNK_QUEUE);
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; READ_BUF];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });
    rx
}

/// Minimal key event surface used by the menu. The parser deliberately
/// covers only what the menu binds — anything else (function keys, mouse,
/// device-attribute responses) is dropped so it can't pollute the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuKey {
    Char(char),
    Enter,
    Backspace,
    Tab,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    /// Ctrl + a/b/.../z (byte value 1..=26).
    Ctrl(u8),
}

/// Parse a buffered chunk of stdin bytes into menu key events. Unknown
/// escape sequences are silently discarded; partial sequences at the tail
/// of the chunk are also discarded (good enough for an interactive menu,
/// where the user keystroke that started the sequence will arrive in the
/// same chunk under raw mode).
pub fn parse_keys(buf: &[u8]) -> Vec<MenuKey> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        let b = buf[i];
        if b == 0x1b {
            // Lone ESC at end of chunk.
            if i + 1 >= buf.len() {
                out.push(MenuKey::Escape);
                i += 1;
                continue;
            }
            let n = buf[i + 1];
            if n == b'[' {
                // CSI: scan to final byte in 0x40..=0x7e
                let mut j = i + 2;
                while j < buf.len() && !(0x40..=0x7e).contains(&buf[j]) {
                    j += 1;
                }
                if j >= buf.len() {
                    break; // incomplete; drop tail
                }
                let params = &buf[i + 2..j];
                let final_b = buf[j];
                if params.is_empty() {
                    match final_b {
                        b'A' => out.push(MenuKey::Up),
                        b'B' => out.push(MenuKey::Down),
                        b'C' => out.push(MenuKey::Right),
                        b'D' => out.push(MenuKey::Left),
                        b'H' => out.push(MenuKey::Home),
                        b'F' => out.push(MenuKey::End),
                        _ => {}
                    }
                }
                // Other CSI sequences (private markers, parameters, etc.) → drop.
                i = j + 1;
                continue;
            } else if n == b'O' {
                // SS3. In DECCKM (application cursor-keys) mode — which
                // tmux / claude enable and can leave set when a session
                // exits back to the menu — the arrow + Home/End keys
                // arrive as `ESC O A`..`ESC O F` instead of the CSI form.
                // Map those exactly like the CSI branch so the menu stays
                // navigable regardless of terminal mode; drop the rest
                // (F1..F4 = P/Q/R/S, etc).
                if i + 2 >= buf.len() {
                    break; // incomplete; drop tail
                }
                match buf[i + 2] {
                    b'A' => out.push(MenuKey::Up),
                    b'B' => out.push(MenuKey::Down),
                    b'C' => out.push(MenuKey::Right),
                    b'D' => out.push(MenuKey::Left),
                    b'H' => out.push(MenuKey::Home),
                    b'F' => out.push(MenuKey::End),
                    _ => {}
                }
                i += 3;
                continue;
            } else {
                // ESC + anything else: surface as ESC, then re-process the
                // byte on the next iteration.
                out.push(MenuKey::Escape);
                i += 1;
                continue;
            }
        }
        match b {
            b'\r' | b'\n' => out.push(MenuKey::Enter),
            0x7f | 0x08 => out.push(MenuKey::Backspace),
            b'\t' => out.push(MenuKey::Tab),
            0x01..=0x1a => out.push(MenuKey::Ctrl(b)),
            0x20..=0x7e => out.push(MenuKey::Char(b as char)),
            _ => {} // discard high bytes; menu inputs are ASCII-only.
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{parse_keys, MenuKey};

    #[test]
    fn csi_arrows_parse() {
        // Normal cursor-keys mode: ESC [ A..D.
        assert_eq!(parse_keys(b"\x1b[A"), vec![MenuKey::Up]);
        assert_eq!(parse_keys(b"\x1b[B"), vec![MenuKey::Down]);
        assert_eq!(parse_keys(b"\x1b[C"), vec![MenuKey::Right]);
        assert_eq!(parse_keys(b"\x1b[D"), vec![MenuKey::Left]);
    }

    #[test]
    fn ss3_arrows_parse_in_application_mode() {
        // DECCKM (application cursor-keys) mode: ESC O A..D. This is the
        // form tmux/claude leave the terminal in after a session exits;
        // the menu must still navigate. Regression guard for the bug
        // where SS3 arrows were dropped and ↑↓ went dead in the picker.
        assert_eq!(parse_keys(b"\x1bOA"), vec![MenuKey::Up]);
        assert_eq!(parse_keys(b"\x1bOB"), vec![MenuKey::Down]);
        assert_eq!(parse_keys(b"\x1bOC"), vec![MenuKey::Right]);
        assert_eq!(parse_keys(b"\x1bOD"), vec![MenuKey::Left]);
        assert_eq!(parse_keys(b"\x1bOH"), vec![MenuKey::Home]);
        assert_eq!(parse_keys(b"\x1bOF"), vec![MenuKey::End]);
    }

    #[test]
    fn ss3_function_keys_dropped() {
        // F1..F4 (ESC O P/Q/R/S) carry no menu binding → dropped, and
        // must not be mistaken for navigation.
        assert!(parse_keys(b"\x1bOP").is_empty());
        assert!(parse_keys(b"\x1bOQ").is_empty());
    }

    #[test]
    fn ss3_arrow_amid_other_keys() {
        // An SS3 arrow sandwiched between plain chars parses cleanly and
        // doesn't swallow its neighbours.
        assert_eq!(
            parse_keys(b"a\x1bOBb"),
            vec![MenuKey::Char('a'), MenuKey::Down, MenuKey::Char('b')]
        );
    }
}
