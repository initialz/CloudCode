//! CLI file-drop (Phase 2) bracketed-paste detection + path parsing.
//!
//! claude enables bracketed-paste mode (`ESC[?2004h`) on the remote
//! PTY; that escape reaches the user's terminal through the relay
//! output, so when the user drags a file onto the terminal the
//! emulator wraps the inserted local path in bracketed-paste markers:
//!
//! ```text
//! ESC[200~  /Users/me/a b.png   ESC[201~
//! ```
//!
//! This module is a pure, unit-tested layer over the raw stdin byte
//! stream. `PasteDetector::feed` buffers everything between the start
//! (`ESC[200~`) and end (`ESC[201~`) markers — handling markers split
//! across read chunks — and, on a complete paste under the size cap,
//! hands back the inner content for the relay to inspect. Everything
//! else (bytes outside a paste, or an oversized paste) is forwarded
//! verbatim so normal typing/pasting is never altered.
//!
//! `parse_paste_paths` turns the inner content into candidate path
//! tokens, handling the two common terminal drag encodings:
//! backslash-escaped spaces (`/a/b\ c.png`) and single/double-quoted
//! tokens (`'/a/b c.png'`). The relay then checks each token with
//! `std::fs::metadata(..).is_file()` to decide whether to intercept.

/// `ESC[200~` — bracketed-paste start marker.
const PASTE_START: &[u8] = b"\x1b[200~";
/// `ESC[201~` — bracketed-paste end marker.
const PASTE_END: &[u8] = b"\x1b[201~";

/// Cap on buffered paste content. A paste larger than this is almost
/// certainly real pasted text, not a dragged path; over the cap we
/// stop buffering and forward verbatim (the partial buffer included),
/// so we never hold unbounded memory and never hijack a big paste.
const MAX_PASTE_BYTES: usize = 1024 * 1024; // 1 MiB

/// One unit of output from the detector, in stream order.
#[derive(Debug, PartialEq, Eq)]
pub enum PasteEvent {
    /// Bytes to forward to the remote PTY verbatim (normal input,
    /// or an oversized/aborted paste replayed unchanged).
    Passthrough(Vec<u8>),
    /// A complete bracketed paste whose inner content is `content`
    /// (markers stripped). The caller decides whether it's a file
    /// drop; if not, it should forward the original wrapped bytes —
    /// `wrap_as_paste(&content)` reproduces them.
    Paste { content: Vec<u8> },
}

/// Incremental bracketed-paste detector. Feed it raw stdin chunks;
/// it emits `PasteEvent`s in order. Holds partial state across
/// `feed` calls so a marker (or content) split over two reads is
/// handled correctly.
pub struct PasteDetector {
    /// Bytes seen but not yet classified — either a possible partial
    /// start marker (when not in a paste) or the in-progress paste
    /// content + possible partial end marker (when in a paste).
    buf: Vec<u8>,
    /// True once a full start marker has been consumed and we're
    /// accumulating content toward the end marker.
    in_paste: bool,
    /// Set once `buf` (as paste content) exceeds the cap: we give up
    /// on detection for this paste and stream everything through as
    /// passthrough until the (already-emitted) start + content flush.
    overflowed: bool,
}

impl Default for PasteDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl PasteDetector {
    pub fn new() -> Self {
        PasteDetector {
            buf: Vec::new(),
            in_paste: false,
            overflowed: false,
        }
    }

    /// Feed one chunk of stdin bytes; returns the ordered events
    /// produced. May return an empty vec if the chunk only advanced a
    /// partial marker/content.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<PasteEvent> {
        let mut events = Vec::new();
        self.buf.extend_from_slice(chunk);
        loop {
            if self.in_paste {
                if !self.step_in_paste(&mut events) {
                    break;
                }
            } else if !self.step_outside_paste(&mut events) {
                break;
            }
        }
        events
    }

    /// Progress while NOT inside a paste. Looks for a start marker in
    /// `buf`. Returns true if it made progress and the loop should
    /// re-run.
    fn step_outside_paste(&mut self, events: &mut Vec<PasteEvent>) -> bool {
        match find_subslice(&self.buf, PASTE_START) {
            Some(idx) => {
                // Flush everything before the marker as passthrough.
                if idx > 0 {
                    events.push(PasteEvent::Passthrough(self.buf[..idx].to_vec()));
                }
                // Drop the marker; switch into paste mode.
                self.buf.drain(..idx + PASTE_START.len());
                self.in_paste = true;
                self.overflowed = false;
                true
            }
            None => {
                // No full start marker. Flush everything that can't be
                // the prefix of a future marker; keep a possible
                // partial marker tail buffered.
                let keep = partial_prefix_len(&self.buf, PASTE_START);
                let flush_to = self.buf.len() - keep;
                if flush_to > 0 {
                    events.push(PasteEvent::Passthrough(self.buf[..flush_to].to_vec()));
                    self.buf.drain(..flush_to);
                }
                false
            }
        }
    }

    /// Progress while inside a paste. Looks for the end marker; emits
    /// a `Paste` event when found. Enforces the size cap. Returns true
    /// if it made progress and the loop should re-run.
    fn step_in_paste(&mut self, events: &mut Vec<PasteEvent>) -> bool {
        // Once we've blown the cap we're in passthrough-until-close mode;
        // an end marker just closes the paste (its content was already
        // streamed). Handle that branch before the normal close so an
        // oversized paste delivered in a single chunk still forwards
        // verbatim instead of being captured as a Paste.
        if self.overflowed {
            if let Some(idx) = find_subslice(&self.buf, PASTE_END) {
                let content = self.buf[..idx].to_vec();
                self.buf.drain(..idx + PASTE_END.len());
                self.in_paste = false;
                self.overflowed = false;
                // Replay the trailing content + end marker so the stream
                // stays byte-exact, then carry on outside the paste.
                events.push(PasteEvent::Passthrough([&content[..], PASTE_END].concat()));
                return true;
            }
            // No close yet: keep streaming through, retaining only a
            // possible partial end marker so we can detect the close.
            let keep = partial_prefix_len(&self.buf, PASTE_END);
            let flush_to = self.buf.len() - keep;
            if flush_to > 0 {
                events.push(PasteEvent::Passthrough(self.buf[..flush_to].to_vec()));
                self.buf.drain(..flush_to);
            }
            return false;
        }

        // Cap check FIRST: if the buffered content has blown the cap,
        // abandon detection — even if an end marker sits later in the
        // buffer (a >1 MiB paste is real text, not a dragged path).
        // Emit the start marker as passthrough and switch to overflow
        // mode; the overflow branch (re-entered on the next loop turn)
        // flushes the buffered content and detects the close, keeping
        // the forwarded byte stream identical to the original input.
        if self.buf.len() > MAX_PASTE_BYTES {
            events.push(PasteEvent::Passthrough(PASTE_START.to_vec()));
            self.overflowed = true;
            return true;
        }

        if let Some(idx) = find_subslice(&self.buf, PASTE_END) {
            // Complete paste under the cap.
            let content = self.buf[..idx].to_vec();
            self.buf.drain(..idx + PASTE_END.len());
            self.in_paste = false;
            events.push(PasteEvent::Paste { content });
            return true;
        }

        false
    }
}

/// Reproduce the on-the-wire bytes of a bracketed paste with the given
/// inner content (so a non-file paste can be forwarded unchanged).
pub fn wrap_as_paste(content: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(PASTE_START.len() + content.len() + PASTE_END.len());
    out.extend_from_slice(PASTE_START);
    out.extend_from_slice(content);
    out.extend_from_slice(PASTE_END);
    out
}

/// Find the first occurrence of `needle` in `hay`. Returns its start
/// index, or `None`.
fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Length of the longest suffix of `hay` that is a (non-empty) proper
/// prefix of `needle`. Used to retain a possibly-incomplete marker at
/// the tail of the buffer across chunk boundaries.
fn partial_prefix_len(hay: &[u8], needle: &[u8]) -> usize {
    let max = needle.len().saturating_sub(1).min(hay.len());
    for n in (1..=max).rev() {
        if hay[hay.len() - n..] == needle[..n] {
            return n;
        }
    }
    0
}

/// Parse bracketed-paste content into candidate path tokens. Handles
/// the two common terminal drag encodings:
///   - backslash-escaped spaces: `/a/b\ c.png`  -> `/a/b c.png`
///   - single/double-quoted tokens: `'/a/b c.png'` -> `/a/b c.png`
///
/// Tokens are separated by unescaped/unquoted whitespace. Surrounding
/// whitespace/newlines are trimmed. Empty content yields no tokens.
/// This is purely lexical — the caller decides which tokens are real
/// files via `std::fs::metadata`.
pub fn parse_paste_paths(content: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut has_token = false;
    let mut chars = content.chars().peekable();
    // Quote state: None, or the active quote char.
    let mut quote: Option<char> = None;

    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            None => match c {
                '\\' => {
                    // Escape: take the next char literally (a trailing
                    // backslash is kept as-is).
                    if let Some(&next) = chars.peek() {
                        cur.push(next);
                        chars.next();
                        has_token = true;
                    } else {
                        cur.push('\\');
                        has_token = true;
                    }
                }
                '\'' | '"' => {
                    quote = Some(c);
                    has_token = true;
                }
                c if c.is_whitespace() => {
                    if has_token {
                        tokens.push(std::mem::take(&mut cur));
                        has_token = false;
                    }
                }
                c => {
                    cur.push(c);
                    has_token = true;
                }
            },
        }
    }
    if has_token {
        tokens.push(cur);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passthrough_bytes(events: &[PasteEvent]) -> Vec<u8> {
        let mut out = Vec::new();
        for e in events {
            if let PasteEvent::Passthrough(b) = e {
                out.extend_from_slice(b);
            }
        }
        out
    }

    #[test]
    fn no_paste_passes_through_verbatim() {
        let mut d = PasteDetector::new();
        let ev = d.feed(b"hello world\r");
        assert_eq!(ev, vec![PasteEvent::Passthrough(b"hello world\r".to_vec())]);
    }

    #[test]
    fn single_chunk_paste_detected() {
        let mut d = PasteDetector::new();
        let ev = d.feed(b"\x1b[200~/tmp/file.png\x1b[201~");
        assert_eq!(
            ev,
            vec![PasteEvent::Paste {
                content: b"/tmp/file.png".to_vec()
            }]
        );
    }

    #[test]
    fn paste_with_surrounding_passthrough() {
        let mut d = PasteDetector::new();
        let ev = d.feed(b"ab\x1b[200~x\x1b[201~cd");
        assert_eq!(
            ev,
            vec![
                PasteEvent::Passthrough(b"ab".to_vec()),
                PasteEvent::Paste {
                    content: b"x".to_vec()
                },
                PasteEvent::Passthrough(b"cd".to_vec()),
            ]
        );
    }

    #[test]
    fn paste_split_across_chunks() {
        let mut d = PasteDetector::new();
        // Start marker split mid-sequence, content split, end marker split.
        let mut ev = d.feed(b"\x1b[20");
        ev.extend(d.feed(b"0~/a/b"));
        ev.extend(d.feed(b" c.png")); // content (with a raw space)
        ev.extend(d.feed(b"\x1b[20"));
        ev.extend(d.feed(b"1~"));
        // Only a single Paste event should have been produced overall.
        let pastes: Vec<&PasteEvent> = ev
            .iter()
            .filter(|e| matches!(e, PasteEvent::Paste { .. }))
            .collect();
        assert_eq!(pastes.len(), 1);
        assert_eq!(
            pastes[0],
            &PasteEvent::Paste {
                content: b"/a/b c.png".to_vec()
            }
        );
        // No stray passthrough bytes leaked.
        assert!(passthrough_bytes(&ev).is_empty());
    }

    #[test]
    fn partial_start_marker_then_unrelated_flushes() {
        // A lone ESC '[' that turns out NOT to be a paste start must
        // eventually reach the stream.
        let mut d = PasteDetector::new();
        let mut ev = d.feed(b"\x1b[");
        ev.extend(d.feed(b"A")); // ESC [ A = cursor up, not a paste
        assert_eq!(passthrough_bytes(&ev), b"\x1b[A".to_vec());
    }

    #[test]
    fn oversized_paste_forwarded_verbatim() {
        let mut d = PasteDetector::new();
        let big = vec![b'x'; MAX_PASTE_BYTES + 10];
        let mut input = Vec::new();
        input.extend_from_slice(b"\x1b[200~");
        input.extend_from_slice(&big);
        input.extend_from_slice(b"\x1b[201~");
        let ev = d.feed(&input);
        // No Paste event — everything is replayed as passthrough,
        // byte-identical to the original input.
        assert!(!ev.iter().any(|e| matches!(e, PasteEvent::Paste { .. })));
        assert_eq!(passthrough_bytes(&ev), input);
    }

    #[test]
    fn parse_plain_single() {
        assert_eq!(parse_paste_paths("/tmp/a.png"), vec!["/tmp/a.png"]);
    }

    #[test]
    fn parse_backslash_escaped_spaces() {
        assert_eq!(
            parse_paste_paths("/a/b\\ c.png"),
            vec!["/a/b c.png".to_string()]
        );
    }

    #[test]
    fn parse_single_and_double_quoted() {
        assert_eq!(
            parse_paste_paths("'/a/b c.png'"),
            vec!["/a/b c.png".to_string()]
        );
        assert_eq!(
            parse_paste_paths("\"/x/y z.txt\""),
            vec!["/x/y z.txt".to_string()]
        );
    }

    #[test]
    fn parse_multiple_files_mixed_encoding() {
        assert_eq!(
            parse_paste_paths("/a.png '/b c.png' /d/e\\ f.gif"),
            vec![
                "/a.png".to_string(),
                "/b c.png".to_string(),
                "/d/e f.gif".to_string(),
            ]
        );
    }

    #[test]
    fn parse_trims_whitespace_and_newlines() {
        assert_eq!(
            parse_paste_paths("  /tmp/a.png \r\n"),
            vec!["/tmp/a.png".to_string()]
        );
        assert!(parse_paste_paths("   \r\n").is_empty());
    }

    #[test]
    fn parse_non_file_token_is_just_a_token() {
        // The parser is lexical only — "hello" comes back as one token;
        // the caller's metadata() check is what rejects non-files.
        assert_eq!(parse_paste_paths("hello there"), vec!["hello", "there"]);
    }
}
