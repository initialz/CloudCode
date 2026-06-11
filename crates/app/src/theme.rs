//! Unified design tokens + egui style — the SINGLE source of truth for
//! every piece of chrome in the app (sidebar, top bar, banners, viewer
//! placeholder, modals). cmux's biggest UX lesson was a split theme
//! (sidebar vs terminal looked like two apps); we avoid it by routing
//! all chrome colors through here. The terminal GRID is deliberately
//! NOT themed — its ANSI/VTE palette, selection tint, preedit and
//! cursor colors are term content and stay in `terminal/mod.rs`.
//!
//! Palette: Catppuccin-Mocha-ish (matches the dark terminal content).

use egui::Color32;

// --- Background layers (deepest → most raised) ---
/// Window base.
pub const BG0: Color32 = Color32::from_rgb(0x11, 0x11, 0x1b);
/// Sidebar.
pub const BG1: Color32 = Color32::from_rgb(0x18, 0x18, 0x25);
/// Panels / cards / raised surfaces.
pub const BG2: Color32 = Color32::from_rgb(0x1e, 0x1e, 0x2e);
/// Hairlines, separators, widget outlines.
pub const BORDER: Color32 = Color32::from_rgb(0x31, 0x32, 0x44);

// --- Text ---
pub const TEXT: Color32 = Color32::from_rgb(0xcd, 0xd6, 0xf4);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(0x93, 0x99, 0xb2);
pub const TEXT_FAINT: Color32 = Color32::from_rgb(0x6c, 0x70, 0x86);

// --- Semantic ---
/// The ONE blue. Reserved for "needs your attention": selection,
/// attention halo (T4), unread dots, the active sidebar row's bar.
pub const ACCENT: Color32 = Color32::from_rgb(0x89, 0xb4, 0xfa);
pub const OK: Color32 = Color32::from_rgb(0xa6, 0xe3, 0xa1);
pub const WARN: Color32 = Color32::from_rgb(0xf9, 0xe2, 0xaf);
pub const ERR: Color32 = Color32::from_rgb(0xf3, 0x8b, 0xa8);

// --- Geometry ---
pub const RADIUS: f32 = 6.0;
/// Spacing scale: 4 / 8 / 12 / 16.
pub const SP_1: f32 = 4.0;
pub const SP_2: f32 = 8.0;
pub const SP_3: f32 = 12.0;
pub const SP_4: f32 = 16.0;
/// Default sidebar width (user-resizable; egui persists the drag).
pub const SIDEBAR_W: f32 = 220.0;

/// Install the theme on the egui context. Called ONCE at `App::new`;
/// everything rendered afterwards (panels, buttons, scrollbars,
/// popups) inherits it, so per-widget color overrides should be rare
/// and always use the tokens above.
pub fn apply(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();

    // -- Visuals --
    let v = &mut style.visuals;
    *v = egui::Visuals::dark(); // start from dark, then override

    v.override_text_color = Some(TEXT);
    v.window_fill = BG2; // floating windows / popups read as "raised"
    v.panel_fill = BG0; // CentralPanel base; the sidebar overrides to BG1
    v.faint_bg_color = BG1;
    v.extreme_bg_color = BG0; // text edits, scroll areas
    v.code_bg_color = BG1;
    v.warn_fg_color = WARN;
    v.error_fg_color = ERR;
    v.hyperlink_color = ACCENT;

    v.selection.bg_fill = ACCENT.linear_multiply(0.35);
    v.selection.stroke = egui::Stroke::new(1.0, ACCENT);

    v.window_rounding = egui::Rounding::same(RADIUS);
    v.menu_rounding = egui::Rounding::same(RADIUS);
    v.window_stroke = egui::Stroke::new(1.0, BORDER);
    v.popup_shadow = egui::epaint::Shadow {
        offset: egui::vec2(0.0, 2.0),
        blur: 8.0,
        spread: 0.0,
        color: Color32::from_black_alpha(96),
    };

    // Widget states: flat fills off the layer scale, BORDER hairlines,
    // accent only on active/selected.
    let rounding = egui::Rounding::same(RADIUS);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = BG1;
    w.noninteractive.weak_bg_fill = BG1;
    w.noninteractive.bg_stroke = egui::Stroke::new(1.0, BORDER); // separators
    w.noninteractive.fg_stroke = egui::Stroke::new(1.0, TEXT_MUTED);
    w.noninteractive.rounding = rounding;

    w.inactive.bg_fill = BG2;
    w.inactive.weak_bg_fill = BG2;
    w.inactive.bg_stroke = egui::Stroke::new(1.0, BORDER);
    w.inactive.fg_stroke = egui::Stroke::new(1.0, TEXT);
    w.inactive.rounding = rounding;

    w.hovered.bg_fill = Color32::from_rgb(0x2a, 0x2b, 0x3c); // BG2 + a step
    w.hovered.weak_bg_fill = Color32::from_rgb(0x2a, 0x2b, 0x3c);
    w.hovered.bg_stroke = egui::Stroke::new(1.0, TEXT_FAINT);
    w.hovered.fg_stroke = egui::Stroke::new(1.5, TEXT);
    w.hovered.rounding = rounding;

    w.active.bg_fill = Color32::from_rgb(0x32, 0x33, 0x47);
    w.active.weak_bg_fill = Color32::from_rgb(0x32, 0x33, 0x47);
    w.active.bg_stroke = egui::Stroke::new(1.0, ACCENT);
    w.active.fg_stroke = egui::Stroke::new(1.5, TEXT);
    w.active.rounding = rounding;

    w.open.bg_fill = BG2;
    w.open.weak_bg_fill = BG2;
    w.open.bg_stroke = egui::Stroke::new(1.0, BORDER);
    w.open.fg_stroke = egui::Stroke::new(1.0, TEXT);
    w.open.rounding = rounding;

    // -- Spacing (the 4/8/12/16 scale) --
    let s = &mut style.spacing;
    s.item_spacing = egui::vec2(SP_2, SP_1 + 2.0);
    s.button_padding = egui::vec2(SP_2, SP_1);
    s.menu_margin = egui::Margin::same(SP_2);
    s.window_margin = egui::Margin::same(SP_3);
    s.indent = SP_4;
    // Subtle scrollbar: thin, no background, only visible on hover/use.
    s.scroll.bar_width = 6.0;
    s.scroll.bar_inner_margin = 2.0;
    s.scroll.bar_outer_margin = 0.0;
    s.scroll.floating = true;

    ctx.set_style(style);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `egui::Context::default()` is headless-safe for style work (no GPU
    /// or window needed until painting), so we can smoke-test `apply`.
    #[test]
    fn apply_sets_key_visuals_without_panicking() {
        let ctx = egui::Context::default();
        apply(&ctx);
        let style = ctx.style();
        assert_eq!(style.visuals.panel_fill, BG0);
        assert_eq!(style.visuals.window_fill, BG2);
        assert_eq!(style.visuals.override_text_color, Some(TEXT));
        assert_eq!(style.visuals.selection.stroke.color, ACCENT);
        assert_eq!(style.visuals.widgets.inactive.bg_fill, BG2);
        assert_eq!(
            style.visuals.widgets.noninteractive.bg_stroke.color,
            BORDER
        );
        assert_eq!(style.visuals.window_rounding, egui::Rounding::same(RADIUS));
        assert!(style.spacing.scroll.floating, "scrollbar stays subtle");
    }
}
