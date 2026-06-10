//! CJK font fallback — loaded at RUNTIME from a system font path.
//!
//! We deliberately do NOT embed a CJK font (`include_bytes!`): a usable
//! CJK font is 10MB+, which bloats the binary and the build. Instead, at
//! startup we probe a per-OS list of well-known system font paths, read
//! the first that exists, and register it as a *fallback* after egui's
//! built-in monospace. Effect: ASCII keeps the crisp built-in monospace,
//! and any glyph the monospace lacks (every CJK codepoint) falls through
//! to the system font.
//!
//! GUI can't run headless, so the testable surface here is the pure path
//! logic: [`cjk_font_candidates`] (per-OS, `cfg`-gated) and
//! [`first_existing`] (probe a list, return the first that exists). The
//! actual `ctx.set_fonts` call in [`install_cjk_font`] is exercised by
//! smoke only.

use std::path::{Path, PathBuf};

/// The per-OS list of CJK system font paths to try, in priority order.
///
/// PURE. `cfg`-gated so each platform compiles to just its own list. The
/// caller probes these with [`first_existing`].
///
/// Note: several of these are `.ttc` font *collections*. egui/ab_glyph
/// loads index 0 of a collection, which for these fonts is the primary
/// CJK face — good enough for fallback. If a `.ttc` ever fails to parse,
/// [`install_cjk_font`] logs and continues (degrades to tofu) rather than
/// panicking.
pub fn cjk_font_candidates() -> Vec<&'static str> {
    #[cfg(target_os = "macos")]
    {
        vec![
            "/System/Library/Fonts/PingFang.ttc",
            "/System/Library/Fonts/STHeiti Light.ttc",
            "/System/Library/Fonts/Hiragino Sans GB.ttc",
        ]
    }
    #[cfg(target_os = "linux")]
    {
        vec![
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/noto/NotoSansMonoCJKsc-Regular.ttf",
            "/usr/share/fonts/noto-cjk/NotoSansMonoCJKsc-Regular.otf",
            "/usr/share/fonts/opentype/noto/NotoSansCJKsc-Regular.otf",
        ]
    }
    #[cfg(target_os = "windows")]
    {
        vec![
            "C:\\Windows\\Fonts\\msyh.ttc",
            "C:\\Windows\\Fonts\\simsun.ttc",
        ]
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "linux",
        target_os = "windows"
    )))]
    {
        Vec::new()
    }
}

/// Return the first path in `paths` that exists on disk, or `None`.
///
/// PURE-ish (touches the filesystem via `Path::exists`, but no other
/// side effects). Testable with temp files.
pub fn first_existing<P: AsRef<Path>>(paths: &[P]) -> Option<PathBuf> {
    paths
        .iter()
        .map(|p| p.as_ref())
        .find(|p| p.exists())
        .map(|p| p.to_path_buf())
}

/// Build a [`egui::FontDefinitions`] with the system CJK font registered
/// as a monospace (and proportional) fallback, and install it on `ctx`.
///
/// Keeps every built-in font, then pushes the CJK font to the END of the
/// Monospace and Proportional fallback chains so ASCII stays on the crisp
/// built-in monospace and only missing glyphs fall through to CJK.
///
/// If no system CJK font is found, or the found file fails to read,
/// logs a warning and leaves egui's default fonts in place (Chinese will
/// render as tofu — the user can drop in a `.ttf` and we'll pick it up).
pub fn install_cjk_font(ctx: &egui::Context) {
    let candidates = cjk_font_candidates();
    let Some(path) = first_existing(&candidates) else {
        tracing::warn!(
            "no CJK font found in {} candidate path(s); Chinese will render as tofu",
            candidates.len()
        );
        return;
    };

    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                "failed to read CJK font {}: {e}; Chinese will render as tofu",
                path.display()
            );
            return;
        }
    };

    tracing::info!(
        "loaded CJK fallback font from {} ({} bytes)",
        path.display(),
        bytes.len()
    );

    let mut fonts = egui::FontDefinitions::default();
    const NAME: &str = "cjk_fallback";
    fonts.font_data.insert(
        NAME.to_owned(),
        egui::FontData::from_owned(bytes),
    );
    // Append (not insert-at-0) so it's the LAST resort: ASCII keeps the
    // built-in face; CJK codepoints fall through to this.
    for family in [egui::FontFamily::Monospace, egui::FontFamily::Proportional] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push(NAME.to_owned());
    }

    ctx.set_fonts(fonts);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_nonempty_on_supported_os() {
        // On the three first-tier desktop OSes we must offer at least one
        // path; on exotic targets an empty list is acceptable.
        let c = cjk_font_candidates();
        if cfg!(any(
            target_os = "macos",
            target_os = "linux",
            target_os = "windows"
        )) {
            assert!(!c.is_empty(), "expected CJK candidates on this OS");
        }
    }

    #[test]
    fn first_existing_picks_first_present() {
        let dir = std::env::temp_dir();
        let missing = dir.join("cloudcode-cjk-nope-aaa.ttc");
        let present = dir.join("cloudcode-cjk-yes-bbb.ttc");
        let _ = std::fs::remove_file(&missing);
        std::fs::write(&present, b"font").unwrap();

        let paths = [missing.clone(), present.clone()];
        let got = first_existing(&paths);
        assert_eq!(got.as_deref(), Some(present.as_path()));

        std::fs::remove_file(&present).unwrap();
    }

    #[test]
    fn first_existing_none_when_all_missing() {
        let dir = std::env::temp_dir();
        let a = dir.join("cloudcode-cjk-none-1.ttc");
        let b = dir.join("cloudcode-cjk-none-2.ttc");
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
        assert!(first_existing(&[a, b]).is_none());
    }

    #[test]
    fn first_existing_returns_earliest_when_multiple_present() {
        let dir = std::env::temp_dir();
        let first = dir.join("cloudcode-cjk-multi-1.ttc");
        let second = dir.join("cloudcode-cjk-multi-2.ttc");
        std::fs::write(&first, b"a").unwrap();
        std::fs::write(&second, b"b").unwrap();
        let got = first_existing(&[first.clone(), second.clone()]);
        assert_eq!(got.as_deref(), Some(first.as_path()));
        std::fs::remove_file(&first).unwrap();
        std::fs::remove_file(&second).unwrap();
    }
}
