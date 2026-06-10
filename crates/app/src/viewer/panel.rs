//! Browser panel — decode screencast JPEG frames to an egui texture and
//! capture mouse / keyboard / IME into [`ViewerInputEvent`]s.
//!
//! This is the *rendering + input* half of the viewer (P4 Task 3); the
//! transport half (the second ws to the hub) is [`super::client`]. The App
//! drives the panel like the terminal one:
//!
//!   * feed frames in:  [`BrowserPanel::set_frame`] on each
//!     `ViewerEvent::Frame(jpeg)` from the [`super::client::ViewerHandle`];
//!   * render + capture: [`BrowserPanel::ui`] each frame, returning the
//!     `Vec<ViewerInputEvent>` to forward up the viewer ws as
//!     `ViewerCommand::SendInput`.
//!
//! The remote browser renders at a fixed *frame viewport* size (the JPEG's
//! pixel dimensions). We draw that texture **letterboxed** into whatever
//! egui rect the panel gets (preserving aspect so the page isn't stretched)
//! and map every captured pointer position from on-screen panel pixels back
//! to frame-viewport pixels, which is what CDP `Input.*` expects. All of
//! that geometry is pure and unit-tested ([`letterbox_rect`],
//! [`panel_to_frame`]); the GUI plumbing around it is smoke-only.
//!
//! Wired into the Session split layout in Task 4 — until then the panel is a
//! self-contained widget, hence the `#[allow(dead_code)]`s.
//!
//! ## Key mapping
//!
//! The agent forwards our `Key { key, code, text, modifiers }` verbatim into
//! CDP `Input.dispatchKeyEvent` (see `agent::browser::screencast::
//! input_to_cdp`), so `key`/`code` must be DOM-style strings (`"Enter"` /
//! `"Enter"`, `"a"` / `"KeyA"`, `"ArrowUp"` / `"ArrowUp"`). We mirror the
//! terminal's split: printable characters ride `egui::Event::Text` (correct
//! casing / layout / dead keys) and become `InsertText`; special and
//! control keys ride `egui::Event::Key` and become `Key` via
//! [`egui_key_to_viewer`]. `modifiers` is the CDP bitmask
//! (Alt=1, Ctrl=2, Meta=4, Shift=8).

use crate::viewer::proto::ViewerInputEvent;
use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

/// CDP modifier bitmask bits, matching what the agent's `input_to_cdp`
/// forwards to `Input.dispatchKeyEvent` / `dispatchMouseEvent`.
const MOD_ALT: i64 = 1;
const MOD_CTRL: i64 = 2;
const MOD_META: i64 = 4;
const MOD_SHIFT: i64 = 8;

/// IME composition state for the browser panel. Mirrors the terminal's
/// `ImeState`: while `preedit` is non-empty the IME owns the keystrokes and
/// we suppress raw Key/Text emission. The committed string is sent as
/// `InsertText` (the CDP path that types a whole string at once), not as
/// per-key events. Kept parallel to the terminal's rather than shared
/// because that one lives in a private `terminal::input` submodule and emits
/// PTY *bytes*; here we emit a viewer *event*.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImeState {
    pub preedit: String,
}

impl ImeState {
    /// Whether a composition is active (non-empty preedit). While active,
    /// Key/Text capture is suppressed.
    pub fn is_composing(&self) -> bool {
        !self.preedit.is_empty()
    }
}

/// Fold one `egui::ImeEvent` into the IME state.
///
/// Returns the new state plus, on a `Commit`, the committed string to send
/// as `ViewerInputEvent::InsertText`. All other transitions yield no event.
///
/// PURE. Mirrors `terminal::input::ime_apply` but yields a viewer event
/// instead of PTY bytes.
///
/// * `Enabled`    -> clear preedit (fresh composition), no commit.
/// * `Preedit(s)` -> store `s` as the live preedit, no commit. (Empty `s`
///                   means the IME dismissed the preedit.)
/// * `Commit(s)`  -> clear preedit, emit `InsertText { s }` (unless empty).
/// * `Disabled`   -> clear preedit, no commit.
pub fn ime_apply(
    mut state: ImeState,
    event: &egui::ImeEvent,
) -> (ImeState, Option<ViewerInputEvent>) {
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
            let ev = if s.is_empty() {
                None
            } else {
                Some(ViewerInputEvent::InsertText { text: s.clone() })
            };
            (state, ev)
        }
        ImeEvent::Disabled => {
            state.preedit.clear();
            (state, None)
        }
    }
}

/// The browser panel: holds the latest decoded frame as an egui texture plus
/// its pixel dimensions, the IME composition state, and the last-known
/// letterboxed image rect (so input mapping uses the same geometry the last
/// frame was drawn with).
pub struct BrowserPanel {
    /// The uploaded texture for the latest good frame. `None` until the
    /// first successful decode; a decode error keeps the previous texture.
    texture: Option<egui::TextureHandle>,
    /// Frame viewport size in pixels (the JPEG's dimensions) — the
    /// coordinate space CDP input events live in.
    frame_dims: Option<(usize, usize)>,
    /// IME composition state (preedit + commit machine).
    ime: ImeState,
    /// The letterboxed image rect from the most recent `ui()`, in screen
    /// coordinates. Pointer positions are mapped through this. `None` until
    /// the first frame is drawn.
    image_rect: Option<egui::Rect>,
    /// egui id of the panel's focusable area, set on the first `ui()` call,
    /// so focus / IME routing can be reasoned about (Task 4 focus routing).
    focus_id: Option<egui::Id>,
    /// Whether the viewer ws is currently believed to be connected. Drives
    /// the placeholder text before the first frame arrives and after a drop.
    /// Set by the App from `ViewerEvent::{Connected,Disconnected}` (Task 4).
    connected: bool,
}

impl Default for BrowserPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl BrowserPanel {
    /// A fresh panel with no frame yet (renders the idle placeholder).
    pub fn new() -> BrowserPanel {
        BrowserPanel {
            texture: None,
            frame_dims: None,
            ime: ImeState::default(),
            image_rect: None,
            focus_id: None,
            connected: false,
        }
    }

    /// Whether we have a decoded frame to show (vs. the idle placeholder).
    pub fn has_frame(&self) -> bool {
        self.texture.is_some()
    }

    /// Mark the viewer ws connected (frames expected) — flips the placeholder
    /// from "not connected" to "connecting…" until the first frame lands.
    pub fn mark_connected(&mut self) {
        self.connected = true;
    }

    /// Mark the viewer ws disconnected: the panel keeps showing the last good
    /// frame dimmed isn't worth the complexity here, so we drop the texture
    /// and fall back to the placeholder, which now reads "disconnected".
    pub fn mark_disconnected(&mut self) {
        self.connected = false;
        self.texture = None;
        self.frame_dims = None;
        self.image_rect = None;
    }

    /// Decode a screencast JPEG and update the texture in place.
    ///
    /// On decode failure we log and keep the last good frame (a single
    /// corrupt frame shouldn't blank the panel). The texture is updated in
    /// place via `TextureHandle::set` after the first `load_texture` so we
    /// don't churn texture allocations every frame.
    pub fn set_frame(&mut self, ctx: &egui::Context, jpeg: &[u8]) {
        let (rgba, w, h) = match decode_jpeg_to_rgba(jpeg) {
            Some(t) => t,
            None => {
                tracing::debug!("browser panel: JPEG decode failed; keeping last frame");
                return;
            }
        };
        let image = egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba);
        match &mut self.texture {
            Some(tex) => tex.set(image, texture_options()),
            None => {
                self.texture = Some(ctx.load_texture(
                    "browser-frame",
                    image,
                    texture_options(),
                ));
            }
        }
        self.frame_dims = Some((w, h));
        // A frame implies a live ws (covers the case where the Frame event
        // is drained before/without an explicit Connected).
        self.connected = true;
    }

    /// Render the panel into `ui` and return the input events captured this
    /// frame (to forward up the viewer ws as `ViewerCommand::SendInput`).
    ///
    /// Draws the latest frame letterboxed into the available rect; with no
    /// frame yet it shows a centered "browser idle" placeholder. Mouse
    /// events are captured while hovered, keyboard/IME while focused, all in
    /// frame-viewport pixel coordinates.
    pub fn ui(&mut self, ui: &mut egui::Ui) -> Vec<ViewerInputEvent> {
        let mut events = Vec::new();

        // Take the whole available area as a click-and-drag focusable region.
        let avail = ui.available_size();
        let (response, painter) =
            ui.allocate_painter(avail, egui::Sense::click_and_drag());
        let panel_rect = response.rect;
        self.focus_id = Some(response.id);

        // Backdrop: a dark fill so the letterbox bars read as "outside the
        // page" rather than transparent gaps.
        painter.rect_filled(panel_rect, 0.0, egui::Color32::from_gray(20));

        let frame = self.texture.as_ref().zip(self.frame_dims);
        let Some((texture, (fw, fh))) = frame else {
            // No frame yet → placeholder, and nothing to capture. The text
            // reflects whether the ws is up (connecting/waiting) or down.
            self.image_rect = None;
            let msg = if self.connected {
                "connecting to browser…"
            } else {
                "browser idle / not connected"
            };
            painter.text(
                panel_rect.center(),
                egui::Align2::CENTER_CENTER,
                msg,
                egui::FontId::proportional(16.0),
                egui::Color32::from_gray(140),
            );
            return events;
        };

        // Letterbox the frame inside the panel, preserving aspect ratio.
        let image_rect = letterbox_rect(panel_rect, fw, fh);
        self.image_rect = Some(image_rect);
        painter.image(
            texture.id(),
            image_rect,
            // Full texture (uv 0,0 .. 1,1).
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );

        // Grab focus on press so keyboard/IME route here.
        if response.clicked() || response.drag_started() {
            response.request_focus();
        }
        let focused = response.has_focus();

        // --- Mouse: move (hovered), buttons (press/release), wheel ---
        self.capture_mouse(ui, &response, image_rect, fw, fh, &mut events);

        // Anchor the OS IME candidate window near the pointer (or panel
        // center) while focused, so composition appears in a sane place.
        if focused {
            let anchor = response
                .hover_pos()
                .unwrap_or_else(|| image_rect.center());
            let cursor_rect = egui::Rect::from_min_size(anchor, egui::vec2(1.0, 16.0));
            ui.ctx().output_mut(|o| {
                o.ime = Some(egui::output::IMEOutput {
                    rect: panel_rect,
                    cursor_rect,
                });
            });
        }

        // --- Keyboard / IME (focused only) ---
        if focused {
            let in_events: Vec<egui::Event> =
                ui.input(|i| i.filtered_events(&egui::EventFilter {
                    tab: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: true,
                }));
            self.capture_keys(&in_events, &mut events);
        }

        events
    }

    /// Capture pointer move / button / wheel into viewer events, mapping
    /// screen px → frame-viewport px through `image_rect`.
    fn capture_mouse(
        &self,
        ui: &egui::Ui,
        response: &egui::Response,
        image_rect: egui::Rect,
        fw: usize,
        fh: usize,
        out: &mut Vec<ViewerInputEvent>,
    ) {
        // Pointer move (only while hovering the image area).
        if response.hovered() {
            if let Some(pos) = response.hover_pos() {
                let (x, y) = panel_to_frame(pos, image_rect, fw, fh);
                out.push(ViewerInputEvent::MouseMove { x, y });
            }
        }

        // Button presses/releases. egui's `interact_pointer_pos` is the
        // position the interaction started/occurred at.
        let click_count = 1; // double-click detection is Task 4+ polish.
        if let Some(pos) = response.interact_pointer_pos() {
            let (x, y) = panel_to_frame(pos, image_rect, fw, fh);
            // Map egui pointer buttons; egui reports primary/secondary/middle.
            for (btn, name) in [
                (egui::PointerButton::Primary, "left"),
                (egui::PointerButton::Secondary, "right"),
                (egui::PointerButton::Middle, "middle"),
            ] {
                if response.ctx.input(|i| i.pointer.button_pressed(btn)) {
                    out.push(ViewerInputEvent::MouseButton {
                        x,
                        y,
                        button: name.to_string(),
                        down: true,
                        click_count,
                    });
                }
                if response.ctx.input(|i| i.pointer.button_released(btn)) {
                    out.push(ViewerInputEvent::MouseButton {
                        x,
                        y,
                        button: name.to_string(),
                        down: false,
                        click_count,
                    });
                }
            }
        }

        // Wheel (only while hovering). CDP wants deltaX/deltaY in CSS px;
        // egui's smooth scroll delta is in points, which matches closely
        // enough for a P4 viewer. Sign: egui scroll-up is positive y; CDP
        // deltaY is positive when scrolling *down*, so negate y.
        if response.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta);
            if scroll.x != 0.0 || scroll.y != 0.0 {
                if let Some(pos) = response.hover_pos() {
                    let (x, y) = panel_to_frame(pos, image_rect, fw, fh);
                    out.push(ViewerInputEvent::Wheel {
                        x,
                        y,
                        dx: -scroll.x as f64,
                        dy: -scroll.y as f64,
                    });
                }
            }
        }
    }

    /// Capture keyboard + IME into viewer events. Mirrors the terminal's
    /// `handle_egui_input` dedup: printable text → `InsertText`, special
    /// keys → `Key`, and while composing the IME owns everything.
    fn capture_keys(&mut self, in_events: &[egui::Event], out: &mut Vec<ViewerInputEvent>) {
        for ev in in_events {
            match ev {
                egui::Event::Ime(ime_event) => {
                    let state = std::mem::take(&mut self.ime);
                    let (next, commit) = ime_apply(state, ime_event);
                    self.ime = next;
                    if let Some(viewer_ev) = commit {
                        out.push(viewer_ev);
                    }
                }
                // Printable text (not during composition) → InsertText, the
                // CDP path that types a whole string with correct
                // casing/layout already resolved by the OS.
                egui::Event::Text(s) if !self.ime.is_composing() => {
                    if !s.is_empty() {
                        out.push(ViewerInputEvent::InsertText { text: s.clone() });
                    }
                }
                // Special / control keys (Enter, arrows, Ctrl-combos) →
                // Key. egui sends both Key and Text for plain letters, so
                // `egui_key_to_viewer` returns None for those (handled by
                // Text above) — that's the dedup.
                egui::Event::Key {
                    key,
                    pressed,
                    modifiers,
                    ..
                } if !self.ime.is_composing() => {
                    if let Some(viewer_ev) =
                        egui_key_to_viewer(*key, *modifiers, *pressed)
                    {
                        out.push(viewer_ev);
                    }
                }
                _ => {}
            }
        }
    }
}

/// Texture sampling options: linear filtering so the letterboxed frame
/// scales smoothly, clamped at the edges.
fn texture_options() -> egui::TextureOptions {
    egui::TextureOptions {
        magnification: egui::TextureFilter::Linear,
        minification: egui::TextureFilter::Linear,
        ..Default::default()
    }
}

/// Decode a JPEG into tightly-packed RGBA8 plus its pixel dimensions.
///
/// Returns `None` on any decode error (caller keeps the previous frame).
/// Uses `zune-jpeg` with `out_colorspace = RGBA` so the decoder expands
/// grayscale/RGB sources to 4 bytes/px for us — exactly what
/// `ColorImage::from_rgba_unmultiplied` wants.
///
/// PURE (no egui types) so it's unit-testable with an embedded fixture.
pub fn decode_jpeg_to_rgba(bytes: &[u8]) -> Option<(Vec<u8>, usize, usize)> {
    let opts = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut decoder = JpegDecoder::new_with_options(ZCursor::new(bytes), opts);
    let pixels = decoder.decode().ok()?;
    let info = decoder.info()?;
    let (w, h) = (info.width as usize, info.height as usize);
    // Sanity: the buffer must be exactly w*h*4 for the ColorImage to be safe.
    if w == 0 || h == 0 || pixels.len() != w * h * 4 {
        return None;
    }
    Some((pixels, w, h))
}

/// Fit a `frame_w × frame_h` frame inside `panel`, preserving aspect ratio
/// and centering it (letterbox / pillarbox).
///
/// * panel wider than the frame aspect → pillarbox (bars left/right);
/// * panel taller → letterbox (bars top/bottom);
/// * exact aspect match → fills the panel.
///
/// PURE.
pub fn letterbox_rect(panel: egui::Rect, frame_w: usize, frame_h: usize) -> egui::Rect {
    if frame_w == 0 || frame_h == 0 || panel.width() <= 0.0 || panel.height() <= 0.0 {
        return panel;
    }
    let fw = frame_w as f32;
    let fh = frame_h as f32;
    let panel_aspect = panel.width() / panel.height();
    let frame_aspect = fw / fh;

    let (w, h) = if panel_aspect > frame_aspect {
        // Panel is relatively wider → height-bound (pillarbox).
        let h = panel.height();
        (h * frame_aspect, h)
    } else {
        // Panel is relatively taller (or equal) → width-bound (letterbox).
        let w = panel.width();
        (w, w / frame_aspect)
    };
    let center = panel.center();
    egui::Rect::from_center_size(center, egui::vec2(w, h))
}

/// Map a screen position to frame-viewport pixel coordinates.
///
/// `pos` is in screen space; `image_rect` is where the (letterboxed) frame
/// is drawn. The result is in `0..frame_w` / `0..frame_h`, clamped — a
/// pointer in the letterbox bars (outside `image_rect`) clamps to the
/// nearest frame edge so an out-of-bounds drag still produces sane coords.
///
/// PURE.
pub fn panel_to_frame(
    pos: egui::Pos2,
    image_rect: egui::Rect,
    frame_w: usize,
    frame_h: usize,
) -> (f64, f64) {
    if image_rect.width() <= 0.0 || image_rect.height() <= 0.0 {
        return (0.0, 0.0);
    }
    let nx = ((pos.x - image_rect.min.x) / image_rect.width()).clamp(0.0, 1.0);
    let ny = ((pos.y - image_rect.min.y) / image_rect.height()).clamp(0.0, 1.0);
    let x = (nx as f64) * frame_w as f64;
    let y = (ny as f64) * frame_h as f64;
    (
        x.clamp(0.0, frame_w as f64),
        y.clamp(0.0, frame_h as f64),
    )
}

/// CDP modifier bitmask from egui modifiers (Alt=1, Ctrl=2, Meta=4,
/// Shift=8). On macOS the ⌘ key is `mac_cmd` → CDP Meta; `command` is
/// egui's cross-platform "primary" flag (Ctrl on win/linux, ⌘ on mac) so we
/// don't read it here to avoid double-counting.
///
/// PURE.
pub fn modifiers_bitmask(mods: egui::Modifiers) -> i64 {
    let mut m = 0;
    if mods.alt {
        m |= MOD_ALT;
    }
    if mods.ctrl {
        m |= MOD_CTRL;
    }
    if mods.mac_cmd {
        m |= MOD_META;
    }
    if mods.shift {
        m |= MOD_SHIFT;
    }
    m
}

/// Translate one egui key press/release into a `ViewerInputEvent::Key` with
/// DOM-style `key`/`code` strings the agent forwards verbatim to CDP
/// `Input.dispatchKeyEvent`.
///
/// Returns `None` for keys with no standalone meaning here — chiefly plain
/// printable letters/digits, which arrive via `egui::Event::Text` and become
/// `InsertText` (the dedup, mirroring the terminal). A *modified* letter
/// (Ctrl-C etc.) DOES return a `Key` so shortcuts reach the page.
///
/// `key` is the DOM `key` (the value: `"Enter"`, `"ArrowUp"`, `"a"`);
/// `code` is the physical `code` (`"Enter"`, `"ArrowUp"`, `"KeyA"`). The
/// subset covers letters, digits, Enter/Tab/Backspace/Escape/Delete, the
/// arrows, and Home/End/PageUp/PageDown — enough for P4; unmapped keys
/// return `None` (noted in the module docs).
///
/// PURE.
pub fn egui_key_to_viewer(
    key: egui::Key,
    mods: egui::Modifiers,
    pressed: bool,
) -> Option<ViewerInputEvent> {
    let modifiers = modifiers_bitmask(mods);
    let has_mod = mods.ctrl || mods.alt || mods.mac_cmd;

    let (dom_key, dom_code, text): (String, &str, String) = match key {
        egui::Key::Enter => ("Enter".into(), "Enter", "\r".into()),
        egui::Key::Tab => ("Tab".into(), "Tab", "\t".into()),
        egui::Key::Space => (" ".into(), "Space", " ".into()),
        egui::Key::Backspace => ("Backspace".into(), "Backspace", String::new()),
        egui::Key::Escape => ("Escape".into(), "Escape", String::new()),
        egui::Key::Delete => ("Delete".into(), "Delete", String::new()),
        egui::Key::ArrowUp => ("ArrowUp".into(), "ArrowUp", String::new()),
        egui::Key::ArrowDown => ("ArrowDown".into(), "ArrowDown", String::new()),
        egui::Key::ArrowLeft => ("ArrowLeft".into(), "ArrowLeft", String::new()),
        egui::Key::ArrowRight => ("ArrowRight".into(), "ArrowRight", String::new()),
        egui::Key::Home => ("Home".into(), "Home", String::new()),
        egui::Key::End => ("End".into(), "End", String::new()),
        egui::Key::PageUp => ("PageUp".into(), "PageUp", String::new()),
        egui::Key::PageDown => ("PageDown".into(), "PageDown", String::new()),
        _ => {
            // Letters and digits: only emit a Key when modified (a shortcut
            // like Ctrl-C). Unmodified, the character comes via Text →
            // InsertText, so emit nothing here (dedup).
            if let Some((k, code)) = letter_key(key) {
                if !has_mod {
                    return None;
                }
                // Ctrl/⌘ shortcuts carry no inserted text.
                (k.to_string(), code, String::new())
            } else if let Some((k, code)) = digit_key(key) {
                if !has_mod {
                    return None;
                }
                (k.to_string(), code, String::new())
            } else {
                return None;
            }
        }
    };

    Some(ViewerInputEvent::Key {
        key: dom_key,
        code: dom_code.to_string(),
        text,
        down: pressed,
        modifiers,
    })
}

/// DOM `(key, code)` for a letter key: lowercase value + `"KeyX"` physical
/// code. (Shift/casing for the *value* is resolved by `Event::Text` on the
/// insert path; the bare Key uses the lowercase DOM convention.)
fn letter_key(key: egui::Key) -> Option<(char, &'static str)> {
    use egui::Key::*;
    let pair = match key {
        A => ('a', "KeyA"), B => ('b', "KeyB"), C => ('c', "KeyC"),
        D => ('d', "KeyD"), E => ('e', "KeyE"), F => ('f', "KeyF"),
        G => ('g', "KeyG"), H => ('h', "KeyH"), I => ('i', "KeyI"),
        J => ('j', "KeyJ"), K => ('k', "KeyK"), L => ('l', "KeyL"),
        M => ('m', "KeyM"), N => ('n', "KeyN"), O => ('o', "KeyO"),
        P => ('p', "KeyP"), Q => ('q', "KeyQ"), R => ('r', "KeyR"),
        S => ('s', "KeyS"), T => ('t', "KeyT"), U => ('u', "KeyU"),
        V => ('v', "KeyV"), W => ('w', "KeyW"), X => ('x', "KeyX"),
        Y => ('y', "KeyY"), Z => ('z', "KeyZ"),
        _ => return None,
    };
    Some(pair)
}

/// DOM `(key, code)` for a digit row key: the digit char + `"DigitN"`.
fn digit_key(key: egui::Key) -> Option<(char, &'static str)> {
    use egui::Key::*;
    let pair = match key {
        Num0 => ('0', "Digit0"), Num1 => ('1', "Digit1"),
        Num2 => ('2', "Digit2"), Num3 => ('3', "Digit3"),
        Num4 => ('4', "Digit4"), Num5 => ('5', "Digit5"),
        Num6 => ('6', "Digit6"), Num7 => ('7', "Digit7"),
        Num8 => ('8', "Digit8"), Num9 => ('9', "Digit9"),
        _ => return None,
    };
    Some(pair)
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{pos2, Key, Modifiers, Rect};

    // ---- letterbox_rect -------------------------------------------------

    #[test]
    fn letterbox_exact_fit_fills_panel() {
        // 2:1 frame in a 2:1 panel → fills exactly.
        let panel = Rect::from_min_size(pos2(0.0, 0.0), egui::vec2(200.0, 100.0));
        let r = letterbox_rect(panel, 800, 400);
        assert!((r.width() - 200.0).abs() < 0.01);
        assert!((r.height() - 100.0).abs() < 0.01);
        assert!((r.center() - panel.center()).length() < 0.01);
    }

    #[test]
    fn letterbox_wider_panel_pillarboxes() {
        // 1:1 frame in a 2:1 (wide) panel → height-bound, bars left/right.
        let panel = Rect::from_min_size(pos2(0.0, 0.0), egui::vec2(200.0, 100.0));
        let r = letterbox_rect(panel, 100, 100);
        // Height fills, width shrinks to 100 (square), centered.
        assert!((r.height() - 100.0).abs() < 0.01);
        assert!((r.width() - 100.0).abs() < 0.01);
        assert!((r.center() - panel.center()).length() < 0.01);
        // Pillarbox bars: image inset from the panel's left/right edges.
        assert!(r.min.x > panel.min.x);
        assert!(r.max.x < panel.max.x);
    }

    #[test]
    fn letterbox_taller_panel_letterboxes() {
        // 1:1 frame in a 1:2 (tall) panel → width-bound, bars top/bottom.
        let panel = Rect::from_min_size(pos2(0.0, 0.0), egui::vec2(100.0, 200.0));
        let r = letterbox_rect(panel, 100, 100);
        assert!((r.width() - 100.0).abs() < 0.01);
        assert!((r.height() - 100.0).abs() < 0.01);
        assert!(r.min.y > panel.min.y);
        assert!(r.max.y < panel.max.y);
    }

    // ---- panel_to_frame -------------------------------------------------

    #[test]
    fn panel_to_frame_center_maps_to_frame_center() {
        let img = Rect::from_min_size(pos2(10.0, 20.0), egui::vec2(200.0, 100.0));
        let (x, y) = panel_to_frame(img.center(), img, 1000, 500);
        assert!((x - 500.0).abs() < 0.01);
        assert!((y - 250.0).abs() < 0.01);
    }

    #[test]
    fn panel_to_frame_top_left_is_origin() {
        let img = Rect::from_min_size(pos2(10.0, 20.0), egui::vec2(200.0, 100.0));
        let (x, y) = panel_to_frame(img.min, img, 1000, 500);
        assert!((x - 0.0).abs() < 0.01);
        assert!((y - 0.0).abs() < 0.01);
    }

    #[test]
    fn panel_to_frame_bottom_right_is_frame_size() {
        let img = Rect::from_min_size(pos2(10.0, 20.0), egui::vec2(200.0, 100.0));
        let (x, y) = panel_to_frame(img.max, img, 1000, 500);
        assert!((x - 1000.0).abs() < 0.01);
        assert!((y - 500.0).abs() < 0.01);
    }

    #[test]
    fn panel_to_frame_outside_clamps() {
        let img = Rect::from_min_size(pos2(10.0, 20.0), egui::vec2(200.0, 100.0));
        // Far to the upper-left of the image rect → clamps to (0,0).
        let (x, y) = panel_to_frame(pos2(-100.0, -100.0), img, 1000, 500);
        assert_eq!((x, y), (0.0, 0.0));
        // Far to the lower-right → clamps to (frame_w, frame_h).
        let (x, y) = panel_to_frame(pos2(9999.0, 9999.0), img, 1000, 500);
        assert_eq!((x, y), (1000.0, 500.0));
    }

    // ---- modifiers_bitmask ---------------------------------------------

    #[test]
    fn modifiers_bitmask_bits() {
        assert_eq!(modifiers_bitmask(Modifiers::default()), 0);
        assert_eq!(
            modifiers_bitmask(Modifiers { ctrl: true, ..Default::default() }),
            MOD_CTRL
        );
        assert_eq!(
            modifiers_bitmask(Modifiers { shift: true, ..Default::default() }),
            MOD_SHIFT
        );
        assert_eq!(
            modifiers_bitmask(Modifiers { alt: true, ..Default::default() }),
            MOD_ALT
        );
        assert_eq!(
            modifiers_bitmask(Modifiers { mac_cmd: true, ..Default::default() }),
            MOD_META
        );
        // Ctrl+Shift = 2|8 = 10.
        assert_eq!(
            modifiers_bitmask(Modifiers { ctrl: true, shift: true, ..Default::default() }),
            MOD_CTRL | MOD_SHIFT
        );
    }

    // ---- egui_key_to_viewer --------------------------------------------

    fn as_key(ev: Option<ViewerInputEvent>) -> (String, String, String, bool, i64) {
        match ev {
            Some(ViewerInputEvent::Key { key, code, text, down, modifiers }) => {
                (key, code, text, down, modifiers)
            }
            other => panic!("expected Key, got {other:?}"),
        }
    }

    #[test]
    fn key_enter_maps_to_dom_enter() {
        let (k, c, t, down, m) =
            as_key(egui_key_to_viewer(Key::Enter, Modifiers::default(), true));
        assert_eq!(k, "Enter");
        assert_eq!(c, "Enter");
        assert_eq!(t, "\r");
        assert!(down);
        assert_eq!(m, 0);
    }

    #[test]
    fn key_arrow_up_maps() {
        let (k, c, t, _d, _m) =
            as_key(egui_key_to_viewer(Key::ArrowUp, Modifiers::default(), true));
        assert_eq!(k, "ArrowUp");
        assert_eq!(c, "ArrowUp");
        assert_eq!(t, "");
    }

    #[test]
    fn key_release_sets_down_false() {
        let (_k, _c, _t, down, _m) =
            as_key(egui_key_to_viewer(Key::ArrowDown, Modifiers::default(), false));
        assert!(!down);
    }

    #[test]
    fn plain_letter_emits_no_key() {
        // Unmodified 'a' rides Event::Text → InsertText, so no Key here.
        assert!(egui_key_to_viewer(Key::A, Modifiers::default(), true).is_none());
    }

    #[test]
    fn ctrl_c_emits_key_with_ctrl_modifier() {
        let mods = Modifiers { ctrl: true, ..Default::default() };
        let (k, c, t, down, m) = as_key(egui_key_to_viewer(Key::C, mods, true));
        assert_eq!(k, "c");
        assert_eq!(c, "KeyC");
        assert_eq!(t, ""); // shortcut carries no inserted text
        assert!(down);
        assert_eq!(m, MOD_CTRL);
    }

    #[test]
    fn ctrl_digit_emits_key() {
        let mods = Modifiers { ctrl: true, ..Default::default() };
        let (k, c, _t, _down, m) = as_key(egui_key_to_viewer(Key::Num1, mods, true));
        assert_eq!(k, "1");
        assert_eq!(c, "Digit1");
        assert_eq!(m, MOD_CTRL);
    }

    #[test]
    fn plain_digit_emits_no_key() {
        assert!(egui_key_to_viewer(Key::Num1, Modifiers::default(), true).is_none());
    }

    // ---- IME (mirrors terminal/input.rs IME tests) ----------------------

    #[test]
    fn ime_preedit_sets_string_no_commit() {
        let (s, ev) =
            ime_apply(ImeState::default(), &egui::ImeEvent::Preedit("ni".into()));
        assert_eq!(s.preedit, "ni");
        assert!(ev.is_none());
        assert!(s.is_composing());
    }

    #[test]
    fn ime_commit_emits_insert_text() {
        let s = ImeState { preedit: "ni".into() };
        let (s, ev) = ime_apply(s, &egui::ImeEvent::Commit("你".into()));
        assert_eq!(s.preedit, "");
        assert_eq!(ev, Some(ViewerInputEvent::InsertText { text: "你".into() }));
        assert!(!s.is_composing());
    }

    #[test]
    fn ime_commit_empty_emits_nothing() {
        let s = ImeState { preedit: "x".into() };
        let (s, ev) = ime_apply(s, &egui::ImeEvent::Commit(String::new()));
        assert_eq!(s.preedit, "");
        assert!(ev.is_none());
    }

    #[test]
    fn ime_enabled_and_disabled_clear() {
        let (s, ev) =
            ime_apply(ImeState { preedit: "stale".into() }, &egui::ImeEvent::Enabled);
        assert_eq!(s.preedit, "");
        assert!(ev.is_none());
        let (s, ev) =
            ime_apply(ImeState { preedit: "abc".into() }, &egui::ImeEvent::Disabled);
        assert_eq!(s.preedit, "");
        assert!(ev.is_none());
    }

    #[test]
    fn capture_keys_suppresses_while_composing() {
        // While a preedit is active the IME owns input: a stray Text/Key
        // during composition must NOT produce events.
        let mut panel = BrowserPanel::new();
        panel.capture_keys(
            &[egui::Event::Ime(egui::ImeEvent::Preedit("ni".into()))],
            &mut Vec::new(),
        );
        assert!(panel.ime.is_composing());
        let mut out = Vec::new();
        panel.capture_keys(
            &[
                egui::Event::Text("x".into()),
                egui::Event::Key {
                    key: Key::Enter,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::default(),
                },
            ],
            &mut out,
        );
        assert!(out.is_empty(), "composition must suppress raw Key/Text");
    }

    #[test]
    fn capture_keys_text_becomes_insert_text() {
        let mut panel = BrowserPanel::new();
        let mut out = Vec::new();
        panel.capture_keys(&[egui::Event::Text("a".into())], &mut out);
        assert_eq!(out, vec![ViewerInputEvent::InsertText { text: "a".into() }]);
    }

    #[test]
    fn capture_keys_commit_emits_chinese_insert_text() {
        let mut panel = BrowserPanel::new();
        // Start composition, then commit.
        panel.capture_keys(
            &[egui::Event::Ime(egui::ImeEvent::Preedit("ni".into()))],
            &mut Vec::new(),
        );
        let mut out = Vec::new();
        panel.capture_keys(
            &[egui::Event::Ime(egui::ImeEvent::Commit("你".into()))],
            &mut out,
        );
        assert_eq!(out, vec![ViewerInputEvent::InsertText { text: "你".into() }]);
        assert!(!panel.ime.is_composing());
    }

    // ---- JPEG decode (embedded 2x2 fixture) ----------------------------

    /// A real 2×2 baseline JPEG produced by macOS `sips` (red/green/blue/
    /// white quadrants; JPEG is lossy so exact colors aren't asserted —
    /// only that decode yields 2×2 × RGBA = 16 bytes).
    const TINY_JPEG: &[u8] = include_bytes!("testdata/tiny_2x2.jpg");

    #[test]
    fn decode_tiny_jpeg_to_rgba() {
        let (rgba, w, h) = decode_jpeg_to_rgba(TINY_JPEG).expect("decode 2x2 jpeg");
        assert_eq!((w, h), (2, 2));
        assert_eq!(rgba.len(), 2 * 2 * 4, "RGBA = 4 bytes/px");
        // Alpha is opaque for every pixel (RGBA expansion from RGB source).
        for px in rgba.chunks_exact(4) {
            assert_eq!(px[3], 255);
        }
    }

    #[test]
    fn decode_garbage_returns_none() {
        assert!(decode_jpeg_to_rgba(b"not a jpeg at all").is_none());
        assert!(decode_jpeg_to_rgba(&[]).is_none());
    }
}
