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

use crate::viewer::proto::{TargetInfo, ViewerInputEvent};
use std::time::{Duration, Instant};
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

/// What one [`BrowserPanel::ui`] frame produced: captured input events to
/// forward as `ViewerCommand::SendInput`, plus an optional tab-bar click to
/// forward as `ViewerCommand::SelectTarget`.
#[derive(Debug, Default)]
pub struct ViewerUiOutput {
    pub input: Vec<ViewerInputEvent>,
    /// Target id of an *inactive* tab the user clicked this frame.
    pub select: Option<String>,
    /// A debounced viewport resize `(width, height)` in logical px to forward
    /// as `ViewerCommand::Resize`, when the panel's render area changed enough
    /// (and the debounce window elapsed). `None` on frames with no settled
    /// resize to emit.
    pub resize: Option<(u32, u32)>,
}

/// Debounce window for viewport resizes, mirroring the terminal's
/// `ResizeThrottle`: emit the first change immediately, then coalesce rapid
/// changes (a window drag) until the wire has been quiet this long.
const VIEWPORT_DEBOUNCE: Duration = Duration::from_millis(100);

/// Minimum change (in logical px, on either axis) before we bother sending a
/// resize. Sub-threshold jitter (sub-pixel layout rounding) is ignored so a
/// still panel never re-sends.
const VIEWPORT_MIN_DELTA: u32 = 4;

/// Debounce + threshold gate for viewport resize emission. Pure logic
/// (no egui), unit-tested. Mirrors `terminal::geom::ResizeThrottle` but in
/// `(u32, u32)` logical px and with a px-delta threshold instead of exact
/// equality (the panel rect wobbles by fractions of a px frame to frame).
#[derive(Debug, Clone, Default)]
pub struct ViewportThrottle {
    /// The last dims we actually emitted (and when).
    last_sent: Option<((u32, u32), Instant)>,
    /// A changed-but-not-yet-emitted size, held back by the debounce window.
    pending: Option<(u32, u32)>,
}

/// Whether `dims` differs from `last` by more than [`VIEWPORT_MIN_DELTA`] on
/// either axis. `None` last (never sent) always counts as changed.
fn viewport_changed(last: Option<(u32, u32)>, dims: (u32, u32)) -> bool {
    match last {
        None => true,
        Some((lw, lh)) => {
            lw.abs_diff(dims.0) >= VIEWPORT_MIN_DELTA || lh.abs_diff(dims.1) >= VIEWPORT_MIN_DELTA
        }
    }
}

impl ViewportThrottle {
    pub fn new() -> ViewportThrottle {
        ViewportThrottle::default()
    }

    /// Feed the current panel viewport at time `now`. Returns the dims to emit
    /// (debounced + thresholded), or `None` to suppress this frame.
    ///
    /// Rules (mirroring `ResizeThrottle::update`):
    ///   * No meaningful change from last sent → `None` (clear any pending).
    ///   * Changed, first ever OR ≥ debounce since last send → emit now.
    ///   * Changed within the window → stash as `pending`, return `None`.
    pub fn update(&mut self, dims: (u32, u32), now: Instant) -> Option<(u32, u32)> {
        match self.last_sent {
            None => {
                self.last_sent = Some((dims, now));
                self.pending = None;
                Some(dims)
            }
            Some((last_dims, last_at)) => {
                if !viewport_changed(Some(last_dims), dims) {
                    self.pending = None;
                    return None;
                }
                if now.duration_since(last_at) >= VIEWPORT_DEBOUNCE {
                    self.last_sent = Some((dims, now));
                    self.pending = None;
                    Some(dims)
                } else {
                    self.pending = Some(dims);
                    None
                }
            }
        }
    }

    /// Flush a pending (debounced) size if the window has elapsed. Called on
    /// every frame so the trailing edge of a resize lands after the drag stops.
    pub fn flush_pending(&mut self, now: Instant) -> Option<(u32, u32)> {
        let dims = self.pending?;
        match self.last_sent {
            Some((last_dims, last_at)) => {
                if !viewport_changed(Some(last_dims), dims) {
                    self.pending = None;
                    None
                } else if now.duration_since(last_at) >= VIEWPORT_DEBOUNCE {
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

/// Max tab label width in characters (middle-truncated beyond this).
const TAB_LABEL_MAX: usize = 24;

/// How recently a frame must have arrived for the `LIVE` badge to show.
/// Past this the stream is considered stalled and the badge simply hides
/// (V1 keeps it binary — no separate STALE state; noted in the plan).
const LIVE_FRESH: Duration = Duration::from_secs(2);

/// The browser panel: holds the latest decoded frame as an egui texture plus
/// its pixel dimensions, the IME composition state, and the last-known
/// letterboxed image rect (so input mapping uses the same geometry the last
/// frame was drawn with). P6 adds the tab-bar model: the agent's CDP target
/// list and which target the stream is (believed to be) on.
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
    /// The agent's CDP target list (one tab per entry), from
    /// `ViewerEvent::Targets`. Empty = agent browser idle (no pages).
    targets: Vec<TargetInfo>,
    /// The target id the stream is on. The wire doesn't carry "attached",
    /// so this MIRRORS the agent's auto-select semantics via [`auto_select`]
    /// on every targets update, and is set optimistically on a tab click
    /// (the agent confirms by switching the stream).
    current: Option<String>,
    /// When the last frame arrived — drives the `LIVE` badge freshness
    /// (see [`frame_is_live`]).
    last_frame: Option<Instant>,
    /// Debounce gate for viewport-resize emission: the panel measures its
    /// render area each frame and this decides when to actually send a
    /// `ViewerCommand::Resize` (so a window drag doesn't flood the wire).
    viewport_throttle: ViewportThrottle,
    /// The last `MouseMove` position (frame px) we emitted, used to de-dupe
    /// identical hover positions so an idle pointer doesn't spam the wire
    /// every frame. Reset whenever the held-button mask changes (so the
    /// first move of a drag always goes out). `None` until the first move.
    last_move: Option<(f64, f64)>,
    /// Currently-held pointer-button bitmask (Left=1/Right=2/Middle=4),
    /// tracked from discrete press/release events rather than egui's
    /// per-frame `pointer.button_down` snapshot — that snapshot proved
    /// unreliable mid-drag (a `mouseMoved` was injected with buttons=0, so
    /// the page saw a hover and a slider wouldn't follow). Maintaining it
    /// ourselves from press/release is the robust, time-ordered source.
    held_buttons: u32,
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
            targets: Vec::new(),
            current: None,
            last_frame: None,
            viewport_throttle: ViewportThrottle::new(),
            last_move: None,
            held_buttons: 0,
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
    ///
    /// The tab-bar model is dropped too: targets can change while detached,
    /// and a fresh attach makes the agent push a new list AND auto-select
    /// its *preferred* target (first non-blank, see [`auto_select`]) —
    /// keeping a remembered `current` would desync the highlighted tab from
    /// the actual stream.
    pub fn mark_disconnected(&mut self) {
        self.connected = false;
        self.texture = None;
        self.frame_dims = None;
        self.image_rect = None;
        self.targets.clear();
        self.current = None;
        self.last_frame = None;
        // A fresh attach starts at the agent's default viewport, so the next
        // measured size must be re-sent (not suppressed as "unchanged").
        self.viewport_throttle = ViewportThrottle::new();
    }

    /// Replace the tab-bar model with a fresh targets list (a downlink
    /// `ViewerEvent::Targets`), re-running the agent-mirroring
    /// [`auto_select`] so the highlighted tab tracks the actual stream.
    pub fn set_targets(&mut self, targets: Vec<TargetInfo>) {
        self.current = auto_select(&targets, &self.current);
        self.targets = targets;
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
        self.last_frame = Some(Instant::now());
        // A frame implies a live ws (covers the case where the Frame event
        // is drained before/without an explicit Connected).
        self.connected = true;
    }

    /// Render the panel into `ui` and return what it captured this frame:
    /// input events (to forward as `ViewerCommand::SendInput`) and an
    /// optional tab click (to forward as `ViewerCommand::SelectTarget`).
    ///
    /// Layout: the tab bar strip on top (only when targets exist), then the
    /// latest frame letterboxed into the remaining rect; with no frame yet a
    /// centered placeholder. Mouse events are captured while hovered,
    /// keyboard/IME while focused, all in frame-viewport pixel coordinates.
    pub fn ui(&mut self, ui: &mut egui::Ui) -> ViewerUiOutput {
        let mut out = ViewerUiOutput::default();

        // Tab bar BEFORE the image: a click is surfaced to the caller AND
        // optimistically highlights the tab (the agent confirms by actually
        // switching the stream / pushing a fresh targets list).
        out.select = self.tab_bar(ui);

        // Take the whole remaining area as a click-and-drag focusable region.
        let avail = ui.available_size();
        let (response, painter) =
            ui.allocate_painter(avail, egui::Sense::click_and_drag());
        let panel_rect = response.rect;
        self.focus_id = Some(response.id);

        // Measure the render area (where frames are drawn) in LOGICAL px and
        // ask the agent to reflow the page to it, debounced. We want the page
        // to match the panel, so the target is `panel_rect`'s logical size —
        // the agent applies it via Emulation.setDeviceMetricsOverride, after
        // which the letterbox becomes a near-exact fit. Sub-threshold jitter
        // and rapid drags are coalesced by `ViewportThrottle`. `update`
        // handles the leading edge; `flush_pending` the trailing edge after a
        // drag settles.
        let now = Instant::now();
        let want = (
            panel_rect.width().round().max(0.0) as u32,
            panel_rect.height().round().max(0.0) as u32,
        );
        out.resize = if want.0 == 0 || want.1 == 0 {
            // Zero-area (panel collapsed / not laid out yet) — never send.
            None
        } else {
            self.viewport_throttle
                .update(want, now)
                .or_else(|| self.viewport_throttle.flush_pending(now))
        };

        // Backdrop: a dark fill so the letterbox bars read as "outside the
        // page" rather than transparent gaps. Theme token, not a one-off.
        painter.rect_filled(panel_rect, 0.0, crate::theme::BG0);

        let frame = self.texture.as_ref().zip(self.frame_dims);
        let Some((texture, (fw, fh))) = frame else {
            // No frame yet → placeholder, and nothing to capture. The text
            // reflects whether the ws is up (and, when it is, whether the
            // agent's browser simply has no pages open yet).
            self.image_rect = None;
            let msg = if !self.connected {
                "browser idle / not connected"
            } else if self.targets.is_empty() {
                // Agent browser idle: claude hasn't opened a page yet.
                "agent browser idle — pages claude opens appear here"
            } else {
                "connecting to browser…"
            };
            painter.text(
                panel_rect.center(),
                egui::Align2::CENTER_CENTER,
                msg,
                egui::FontId::proportional(16.0),
                crate::theme::TEXT_MUTED,
            );
            return out;
        };

        // Letterbox the frame inside the panel, preserving aspect ratio.
        let image_rect = letterbox_rect(panel_rect, fw, fh);
        self.image_rect = Some(image_rect);
        painter.image(
            texture.id(),
            image_rect,
            // Full texture (uv 0,0 .. 1,1).
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            // Identity tint (not a chrome color — the frame's own pixels).
            egui::Color32::WHITE,
        );

        // `LIVE · w×h` badge (bottom-right): the honest "this is a mirror"
        // marker, shown only while frames are actually flowing. A stalled
        // stream (>2s without a frame) just hides it (V1 keeps it binary).
        let now = Instant::now();
        if frame_is_live(self.last_frame, now) {
            draw_live_badge(&painter, image_rect, fw, fh);
            // When the stream stops, no event arrives to wake the UI, so
            // the badge would linger until the next repaint. Schedule one
            // for just past the freshness deadline so it expires promptly.
            if let Some(last) = self.last_frame {
                let remaining = LIVE_FRESH.saturating_sub(now.duration_since(last));
                ui.ctx()
                    .request_repaint_after(remaining + Duration::from_millis(50));
            }
        }

        // Grab focus on press so keyboard/IME route here.
        if response.clicked() || response.drag_started() {
            response.request_focus();
        }
        let focused = response.has_focus();

        // --- Mouse: move (hovered), buttons (press/release), wheel ---
        let mut events = Vec::new();
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

        out.input = events;
        out
    }

    /// Render the tab strip (one tab per CDP target) and return the id of
    /// an *inactive* tab clicked this frame, after optimistically marking it
    /// current. Renders nothing when the target list is empty (the
    /// placeholder under it explains the idle state).
    fn tab_bar(&mut self, ui: &mut egui::Ui) -> Option<String> {
        if self.targets.is_empty() {
            return None;
        }
        let mut clicked: Option<String> = None;
        egui::Frame::new()
            .fill(crate::theme::BG1)
            .inner_margin(egui::Margin::symmetric(
                crate::theme::SP_1 as i8,
                crate::theme::SP_1 as i8,
            ))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                // Horizontal scroll (id-salted: the split layout can host
                // other scroll areas) so many tabs scroll instead of
                // clipping off the strip's right edge.
                egui::ScrollArea::horizontal()
                    .id_salt("viewer_tab_strip")
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = crate::theme::SP_1;
                            for t in &self.targets {
                                let active =
                                    self.current.as_deref() == Some(t.id.as_str());
                                let label = tab_label(&t.title, &t.url, TAB_LABEL_MAX);
                                if tab_button(ui, &label, active) && !active {
                                    clicked = Some(t.id.clone());
                                }
                            }
                        });
                    });
            });
        if let Some(id) = &clicked {
            // Optimistic: the agent confirms by switching the stream (and a
            // racing close is corrected by the next Targets event).
            self.current = Some(id.clone());
        }
        clicked
    }

    /// Capture pointer move / button / wheel into viewer events, mapping
    /// screen px → frame-viewport px through `image_rect`.
    fn capture_mouse(
        &mut self,
        ui: &egui::Ui,
        response: &egui::Response,
        image_rect: egui::Rect,
        fw: usize,
        fh: usize,
        out: &mut Vec<ViewerInputEvent>,
    ) {
        // STEP 1 — process button press/release FIRST so `held_buttons` is
        // up to date before we emit this frame's move. We track the held
        // mask from the discrete press/release events (not egui's per-frame
        // `pointer.button_down` snapshot, which read 0 mid-drag and made the
        // injected `mouseMoved` look like a hover → sliders didn't follow).
        let click_count = 1; // double-click detection is Task 4+ polish.
        let press_pos = response.interact_pointer_pos();
        for (btn, name, bit) in [
            (egui::PointerButton::Primary, "left", 1u32),
            (egui::PointerButton::Secondary, "right", 2u32),
            (egui::PointerButton::Middle, "middle", 4u32),
        ] {
            let pressed = response.ctx.input(|i| i.pointer.button_pressed(btn));
            let released = response.ctx.input(|i| i.pointer.button_released(btn));
            if pressed {
                self.held_buttons |= bit;
            }
            if released {
                self.held_buttons &= !bit;
            }
            if (pressed || released) && press_pos.is_some() {
                let (x, y) = panel_to_frame(press_pos.unwrap(), image_rect, fw, fh);
                out.push(ViewerInputEvent::MouseButton {
                    x,
                    y,
                    button: name.to_string(),
                    down: pressed,
                    click_count,
                    // The full held mask AFTER applying this event — what CDP
                    // wants alongside the single `button` that changed.
                    buttons: self.held_buttons,
                });
                // A button-state change starts a fresh move-dedup run so the
                // press's first drag move is never suppressed.
                self.last_move = None;
            }
        }
        let buttons = self.held_buttons;

        // STEP 2 — pointer move, during BOTH hover AND drag. While dragging,
        // the pointer can leave the image rect (e.g. dragging a slider past
        // its end); `interact_pointer_pos` stays valid there and
        // `panel_to_frame` clamps to the frame edge. While merely hovering,
        // use `hover_pos`.
        let dragging = buttons != 0;
        let move_pos = if dragging {
            response.interact_pointer_pos().or_else(|| response.hover_pos())
        } else if response.hovered() {
            response.hover_pos()
        } else {
            None
        };
        if let Some(pos) = move_pos {
            let (x, y) = panel_to_frame(pos, image_rect, fw, fh);
            // Always send while a button is held (so the drag tracks
            // frame-by-frame); while hovering, de-dupe identical positions
            // to avoid per-frame wire spam.
            if buttons != 0 || self.last_move != Some((x, y)) {
                out.push(ViewerInputEvent::MouseMove { x, y, buttons });
                self.last_move = Some((x, y));
            }
        } else {
            // Pointer left entirely: forget the last position so the next
            // hover/drag re-emits its first move.
            self.last_move = None;
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

/// Which target the stream is on after a targets update — the SAME rules
/// the agent's `viewer.rs` forwarder applies (the wire doesn't carry
/// "attached", so the app mirrors the decision instead):
///
///   * `current` still in the list → keep it;
///   * `current` gone (tab closed) or `None` (idle) → the first
///     non-(`about:blank` | `chrome://`) target, falling back to the first;
///   * empty list → `None` (agent browser idle).
///
/// LOCKSTEP: the preference mirrors the agent's `preferred_target`
/// (`agent::browser::viewer`) and its initial attach pick
/// (`pick_page_entry_for_session`) — both prefer a real page over a blank
/// one, so the highlighted tab matches the tab actually being streamed even
/// when the list is `[about:blank, real-page]`. Change one, change both.
///
/// PURE.
pub fn auto_select(targets: &[TargetInfo], current: &Option<String>) -> Option<String> {
    if let Some(cur) = current {
        if targets.iter().any(|t| &t.id == cur) {
            return Some(cur.clone());
        }
    }
    targets
        .iter()
        .find(|t| !t.url.starts_with("about:blank") && !t.url.starts_with("chrome://"))
        .or_else(|| targets.first())
        .map(|t| t.id.clone())
}

/// Tab label text: the page title, falling back to the url's host when the
/// title is empty (e.g. `about:blank`), middle-truncated to `max` chars
/// with a single `…` so both the start and the end stay recognizable.
///
/// PURE.
pub fn tab_label(title: &str, url: &str, max: usize) -> String {
    let title = title.trim();
    let base = if title.is_empty() {
        url_host(url)
    } else {
        title.to_string()
    };
    middle_truncate(&base, max)
}

/// The host part of a url (`https://www.baidu.com/s?x=1` → `www.baidu.com`).
/// Scheme-less urls (`about:blank`) pass through whole; an empty host falls
/// back to the full url so the tab is never blank.
fn url_host(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    if host.is_empty() {
        url.to_string()
    } else {
        host.to_string()
    }
}

/// Middle-truncate `s` to at most `max` chars (char-counted, so CJK titles
/// don't get split mid-codepoint), keeping head + `…` + tail.
fn middle_truncate(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let keep = max - 1; // room for the ellipsis
    let head = keep.div_ceil(2);
    let tail = keep - head;
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(&chars[chars.len() - tail..]);
    out
}

/// Whether the screencast counts as live: a frame arrived within
/// [`LIVE_FRESH`] of `now`. `None` (no frame yet) is never live.
///
/// PURE.
pub fn frame_is_live(last_frame: Option<Instant>, now: Instant) -> bool {
    last_frame.is_some_and(|t| now.duration_since(t) < LIVE_FRESH)
}

/// Paint one tab and report whether it was clicked. Active tab: BG2 fill,
/// ACCENT text + a 2px accent underline; inactive: TEXT_MUTED on the strip's
/// BG1 (subtle BG2 wash on hover). All theme tokens.
fn tab_button(ui: &mut egui::Ui, label: &str, active: bool) -> bool {
    use crate::theme;
    let text_color = if active { theme::ACCENT } else { theme::TEXT_MUTED };
    let galley = ui.painter().layout_no_wrap(
        label.to_string(),
        egui::FontId::proportional(12.0),
        text_color,
    );
    let pad = egui::vec2(theme::SP_2, theme::SP_1);
    let (rect, response) =
        ui.allocate_exact_size(galley.size() + pad * 2.0, egui::Sense::click());
    if !ui.is_rect_visible(rect) {
        return response.clicked();
    }
    let painter = ui.painter();
    if active {
        painter.rect_filled(rect, theme::RADIUS / 2.0, theme::BG2);
    } else if response.hovered() {
        painter.rect_filled(rect, theme::RADIUS / 2.0, theme::BG2.linear_multiply(0.6));
    }
    painter.galley(rect.min + pad, galley, text_color);
    if active {
        let underline = egui::Rect::from_min_max(
            egui::pos2(rect.min.x + 2.0, rect.max.y - 2.0),
            egui::pos2(rect.max.x - 2.0, rect.max.y),
        );
        painter.rect_filled(underline, 1.0, theme::ACCENT);
    }
    response.clicked()
}

/// Paint the `LIVE · w×h` badge into the image's bottom-right corner:
/// small TEXT_MUTED caps on a semi-transparent BG2 pill — the honest "this
/// is a remote mirror" marker.
fn draw_live_badge(painter: &egui::Painter, image_rect: egui::Rect, fw: usize, fh: usize) {
    use crate::theme;
    let galley = painter.layout_no_wrap(
        format!("LIVE · {fw}×{fh}"),
        egui::FontId::proportional(10.0),
        theme::TEXT_MUTED,
    );
    let pad = egui::vec2(theme::SP_1 + 2.0, theme::SP_1);
    let size = galley.size() + pad * 2.0;
    let min = image_rect.max - size - egui::vec2(theme::SP_2, theme::SP_2);
    let rect = egui::Rect::from_min_size(min, size);
    painter.rect_filled(rect, theme::RADIUS / 2.0, theme::BG2.gamma_multiply(0.85));
    painter.galley(rect.min + pad, galley, theme::TEXT_MUTED);
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

    // ---- auto_select (mirrors the agent's forwarder rules) --------------

    fn ti(id: &str) -> TargetInfo {
        TargetInfo {
            id: id.into(),
            title: format!("title {id}"),
            url: format!("https://example.com/{id}"),
            kind: "page".into(),
        }
    }

    fn ti_at(id: &str, url: &str) -> TargetInfo {
        TargetInfo {
            id: id.into(),
            title: String::new(),
            url: url.into(),
            kind: "page".into(),
        }
    }

    #[test]
    fn auto_select_table() {
        let ab = vec![ti("A"), ti("B")];
        // The lockstep cases (mirroring the agent's `preferred_target`):
        // a blank tab ahead of a real page must not win the highlight.
        let blank_real = vec![ti_at("BL", "about:blank"), ti_at("R", "https://example.com/")];
        let chrome_real = vec![ti_at("CH", "chrome://newtab/"), ti_at("R", "https://example.com/")];
        let only_blank = vec![ti_at("BL", "about:blank")];
        let cases: Vec<(&[TargetInfo], Option<&str>, Option<&str>, &str)> = vec![
            (&ab, Some("B"), Some("B"), "current still listed → keep"),
            (&ab, Some("Z"), Some("A"), "current destroyed → first remaining"),
            (&ab, None, Some("A"), "idle + targets appear → first"),
            (&[], Some("A"), None, "all targets gone → idle"),
            (&[], None, None, "stays idle on empty"),
            (&blank_real, None, Some("R"), "[blank, real] at attach → the real page (lockstep with agent)"),
            (&blank_real, Some("Z"), Some("R"), "current destroyed → first NON-BLANK remaining"),
            (&blank_real, Some("BL"), Some("BL"), "current = blank but still listed → keep (no forced hop)"),
            (&chrome_real, None, Some("R"), "[chrome://, real] → the real page"),
            (&only_blank, None, Some("BL"), "only blank → fall back to it"),
        ];
        for (targets, current, want, why) in cases {
            let got = auto_select(targets, &current.map(String::from));
            assert_eq!(got.as_deref(), want, "{why}");
        }
    }

    #[test]
    fn set_targets_runs_auto_select_and_click_is_optimistic() {
        let mut panel = BrowserPanel::new();
        // First list: idle → first target auto-selected.
        panel.set_targets(vec![ti("A"), ti("B")]);
        assert_eq!(panel.current.as_deref(), Some("A"));
        // A (optimistic) tab click, then a refresh keeping B → B sticks.
        panel.current = Some("B".into());
        panel.set_targets(vec![ti("B"), ti("C")]);
        assert_eq!(panel.current.as_deref(), Some("B"));
        // B closes → falls to the first remaining.
        panel.set_targets(vec![ti("C")]);
        assert_eq!(panel.current.as_deref(), Some("C"));
        // Browser idle.
        panel.set_targets(vec![]);
        assert_eq!(panel.current, None);
    }

    #[test]
    fn disconnect_clears_tab_model() {
        // A fresh attach makes the agent push a new list and select its
        // preferred target — stale tabs/current must not survive a drop.
        let mut panel = BrowserPanel::new();
        panel.set_targets(vec![ti("A")]);
        panel.mark_disconnected();
        assert!(panel.targets.is_empty());
        assert_eq!(panel.current, None);
        assert!(panel.last_frame.is_none());
    }

    // ---- tab_label / truncation ------------------------------------------

    #[test]
    fn tab_label_short_title_kept_whole() {
        assert_eq!(tab_label("GitHub", "https://github.com/", 24), "GitHub");
    }

    #[test]
    fn tab_label_long_title_middle_truncated() {
        let long = "An Extremely Long Page Title That Cannot Fit";
        let got = tab_label(long, "https://x.com/", 24);
        assert_eq!(got.chars().count(), 24, "truncated to max chars");
        assert!(got.contains('…'));
        assert!(got.starts_with("An Extremely"), "head preserved: {got}");
        assert!(got.ends_with("Fit"), "tail preserved: {got}");
    }

    #[test]
    fn tab_label_cjk_truncates_on_chars() {
        let long: String = "中".repeat(30);
        let got = tab_label(&long, "https://x.com/", 10);
        assert_eq!(got.chars().count(), 10);
        assert!(got.contains('…'));
    }

    #[test]
    fn tab_label_empty_title_falls_back_to_url_host() {
        assert_eq!(
            tab_label("", "https://www.baidu.com/s?wd=x", 24),
            "www.baidu.com"
        );
        // Scheme-less url (the about:blank tab) passes through whole.
        assert_eq!(tab_label("  ", "about:blank", 24), "about:blank");
    }

    #[test]
    fn tab_label_empty_everything_is_empty() {
        assert_eq!(tab_label("", "", 24), "");
    }

    // ---- frame_is_live ----------------------------------------------------

    #[test]
    fn live_badge_freshness() {
        let t0 = std::time::Instant::now();
        assert!(!frame_is_live(None, t0), "no frame yet → not live");
        assert!(frame_is_live(Some(t0), t0), "fresh frame → live");
        assert!(
            frame_is_live(Some(t0), t0 + Duration::from_millis(1999)),
            "just under the window → live"
        );
        assert!(
            !frame_is_live(Some(t0), t0 + Duration::from_secs(2)),
            "at the boundary → stalled, badge hides"
        );
        assert!(!frame_is_live(Some(t0), t0 + Duration::from_secs(60)));
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

    // ---- ViewportThrottle (mirrors terminal ResizeThrottle, px-delta gate) --

    #[test]
    fn viewport_changed_threshold() {
        assert!(viewport_changed(None, (800, 600)), "first ever → changed");
        // Below the min delta on both axes → unchanged.
        assert!(!viewport_changed(Some((800, 600)), (803, 602)));
        assert!(!viewport_changed(Some((800, 600)), (797, 598)));
        // At/over the threshold on either axis → changed.
        assert!(viewport_changed(Some((800, 600)), (804, 600)));
        assert!(viewport_changed(Some((800, 600)), (800, 596)));
    }

    #[test]
    fn throttle_first_resize_emits_immediately() {
        let mut t = ViewportThrottle::new();
        let t0 = Instant::now();
        assert_eq!(t.update((800, 600), t0), Some((800, 600)));
    }

    #[test]
    fn throttle_suppresses_subthreshold_jitter() {
        let mut t = ViewportThrottle::new();
        let t0 = Instant::now();
        assert_eq!(t.update((800, 600), t0), Some((800, 600)));
        // A 2px wobble well past the window is still ignored.
        assert_eq!(
            t.update((802, 601), t0 + Duration::from_secs(1)),
            None,
            "sub-threshold change must not emit"
        );
    }

    #[test]
    fn throttle_debounces_then_flushes_trailing() {
        let mut t = ViewportThrottle::new();
        let t0 = Instant::now();
        assert_eq!(t.update((800, 600), t0), Some((800, 600)));
        // A real change too soon → stashed, not emitted.
        assert_eq!(t.update((900, 700), t0 + Duration::from_millis(10)), None);
        // Still inside the window → flush is a no-op.
        assert_eq!(t.flush_pending(t0 + Duration::from_millis(20)), None);
        // Past the window → the trailing edge lands.
        assert_eq!(
            t.flush_pending(t0 + Duration::from_millis(120)),
            Some((900, 700))
        );
        // Nothing pending now.
        assert_eq!(t.flush_pending(t0 + Duration::from_millis(300)), None);
    }

    #[test]
    fn throttle_emits_change_after_window_elapses() {
        let mut t = ViewportThrottle::new();
        let t0 = Instant::now();
        assert_eq!(t.update((800, 600), t0), Some((800, 600)));
        // A change after the window emits immediately on update.
        assert_eq!(
            t.update((1000, 800), t0 + Duration::from_millis(150)),
            Some((1000, 800))
        );
    }
}
