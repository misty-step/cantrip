//! Light and dark palettes, given equal care, and their application to egui.
//!
//! Calm and native: neutral surfaces, hairline borders, one restrained accent
//! for selection and focus. Colour means state only — red = recording, amber =
//! attention, green = confirmed. Every text/background pair meets WCAG 4.5:1 and
//! every indicator / focus ring 3:1 (tested below).

use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle, Vec2};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub dark: bool,
    /// Window background (popover, Settings).
    pub bg: Color32,
    /// Raised control fill (buttons, fields, segmented track).
    pub control: Color32,
    pub control_hover: Color32,
    pub control_active: Color32,
    /// Hairline separators and control borders (decorative).
    pub hairline: Color32,
    /// Borders that identify an interactive control (checkbox, switch off).
    pub control_border: Color32,
    pub text: Color32,
    pub text_muted: Color32,
    pub text_disabled: Color32,
    /// Selection / focus accent.
    pub accent: Color32,
    /// Text drawn on `accent`.
    pub on_accent: Color32,
    pub red: Color32,
    pub amber: Color32,
    pub green: Color32,
    /// HUD pill material (opaque equivalent of the system material).
    pub hud_fill: Color32,
    pub hud_border: Color32,
    pub hud_text: Color32,
    pub hud_muted: Color32,
}

pub const LIGHT: Palette = Palette {
    dark: false,
    bg: Color32::from_rgb(0xF6, 0xF6, 0xF7),
    control: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    control_hover: Color32::from_rgb(0xE9, 0xE9, 0xEC),
    control_active: Color32::from_rgb(0xDE, 0xDE, 0xE2),
    hairline: Color32::from_rgb(0xDA, 0xDA, 0xDE),
    control_border: Color32::from_rgb(0x86, 0x86, 0x8E),
    text: Color32::from_rgb(0x1C, 0x1C, 0x1E),
    text_muted: Color32::from_rgb(0x5C, 0x5C, 0x63),
    text_disabled: Color32::from_rgb(0x6E, 0x6E, 0x75),
    accent: Color32::from_rgb(0x1F, 0x5F, 0xC9),
    on_accent: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    red: Color32::from_rgb(0xD0, 0x2B, 0x2B),
    amber: Color32::from_rgb(0x8F, 0x5B, 0x00),
    green: Color32::from_rgb(0x1E, 0x7B, 0x3A),
    hud_fill: Color32::from_rgb(0xFB, 0xFB, 0xFC),
    hud_border: Color32::from_rgb(0xCF, 0xCF, 0xD5),
    hud_text: Color32::from_rgb(0x1C, 0x1C, 0x1E),
    hud_muted: Color32::from_rgb(0x5C, 0x5C, 0x63),
};

pub const DARK: Palette = Palette {
    dark: true,
    bg: Color32::from_rgb(0x1F, 0x1F, 0x21),
    control: Color32::from_rgb(0x27, 0x27, 0x2A),
    control_hover: Color32::from_rgb(0x37, 0x37, 0x3B),
    control_active: Color32::from_rgb(0x42, 0x42, 0x47),
    hairline: Color32::from_rgb(0x3A, 0x3A, 0x3F),
    control_border: Color32::from_rgb(0x84, 0x84, 0x8C),
    text: Color32::from_rgb(0xF1, 0xF1, 0xF3),
    text_muted: Color32::from_rgb(0xA6, 0xA6, 0xAE),
    text_disabled: Color32::from_rgb(0x8E, 0x8E, 0x96),
    accent: Color32::from_rgb(0x32, 0x70, 0xDB),
    on_accent: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    red: Color32::from_rgb(0xFF, 0x5F, 0x57),
    amber: Color32::from_rgb(0xE8, 0xB3, 0x4A),
    green: Color32::from_rgb(0x4C, 0xC3, 0x6A),
    hud_fill: Color32::from_rgb(0x2A, 0x2A, 0x2D),
    hud_border: Color32::from_rgb(0x48, 0x48, 0x4E),
    hud_text: Color32::from_rgb(0xF1, 0xF1, 0xF3),
    hud_muted: Color32::from_rgb(0xAE, 0xAE, 0xB6),
};

pub fn palette(dark: bool) -> Palette {
    if dark {
        DARK
    } else {
        LIGHT
    }
}

/// Body text size (points) — native utility scale.
pub const BODY: f32 = 13.5;
pub const SMALL: f32 = 12.0;
pub const TITLE: f32 = 14.5;

pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

pub fn sans(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

/// Apply `p` to the egui context: visuals, spacing and text styles.
pub fn apply(ctx: &egui::Context, p: &Palette) {
    let mut v = if p.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    v.panel_fill = p.bg;
    v.window_fill = p.bg;
    v.extreme_bg_color = p.control;
    v.faint_bg_color = p.control_hover;
    v.override_text_color = Some(p.text);
    v.hyperlink_color = p.accent;
    v.window_stroke = Stroke::new(1.0, p.hairline);
    v.window_shadow = egui::Shadow::NONE;
    v.popup_shadow = egui::Shadow::NONE;
    v.selection.bg_fill = p.accent;
    v.selection.stroke = Stroke::new(1.0, p.on_accent);
    v.text_cursor.stroke = Stroke::new(1.5, p.accent);
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(8);
    let r = CornerRadius::same(6);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.bg;
    w.noninteractive.weak_bg_fill = p.bg;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.hairline);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.corner_radius = r;
    w.inactive.bg_fill = p.control;
    w.inactive.weak_bg_fill = p.control;
    w.inactive.bg_stroke = Stroke::new(1.0, p.hairline);
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.corner_radius = r;
    w.inactive.expansion = 0.0;
    w.hovered.bg_fill = p.control_hover;
    w.hovered.weak_bg_fill = p.control_hover;
    w.hovered.bg_stroke = Stroke::new(1.0, p.control_border);
    w.hovered.fg_stroke = Stroke::new(1.0, p.text);
    w.hovered.corner_radius = r;
    w.hovered.expansion = 0.0;
    w.active.bg_fill = p.control_active;
    w.active.weak_bg_fill = p.control_active;
    w.active.bg_stroke = Stroke::new(1.0, p.control_border);
    w.active.fg_stroke = Stroke::new(1.0, p.text);
    w.active.corner_radius = r;
    w.active.expansion = 0.0;
    w.open = w.active;
    ctx.set_visuals(v);

    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Small, sans(SMALL)),
            (TextStyle::Body, sans(BODY)),
            (TextStyle::Button, sans(BODY)),
            (TextStyle::Heading, sans(TITLE)),
            (TextStyle::Monospace, mono(BODY - 0.5)),
        ]
        .into();
        let s = &mut style.spacing;
        s.item_spacing = Vec2::new(8.0, 6.0);
        s.button_padding = Vec2::new(10.0, 4.0);
        s.interact_size = Vec2::new(24.0, 26.0);
        s.icon_width = 16.0;
        s.icon_spacing = 8.0;
        s.combo_width = 240.0;
        s.text_edit_width = 240.0;
        style.interaction.selectable_labels = false;
        style.animation_time = if reduced_motion() { 0.0 } else { 0.12 };
    });
}

/// Reduced motion: `GTK_ENABLE_ANIMATIONS=0` or `CANTRIP_REDUCED_MOTION=1`
/// (the latter set by platform code from the OS accessibility preference).
pub fn reduced_motion() -> bool {
    std::env::var("GTK_ENABLE_ANIMATIONS").is_ok_and(|v| v == "0" || v == "false")
        || std::env::var("CANTRIP_REDUCED_MOTION").is_ok_and(|v| v == "1")
}

/// Install system fonts: sans as Proportional, mono as Monospace; egui's
/// bundled fonts remain only as glyph fallbacks.
pub fn install_fonts(ctx: &egui::Context, sans: Option<Vec<u8>>, mono: Option<Vec<u8>>) {
    let mut defs = egui::FontDefinitions::default();
    if let Some(bytes) = sans {
        defs.font_data.insert(
            "system-sans".into(),
            std::sync::Arc::new(egui::FontData::from_owned(bytes)),
        );
        defs.families
            .entry(FontFamily::Proportional)
            .or_default()
            .insert(0, "system-sans".into());
    }
    if let Some(bytes) = mono {
        defs.font_data.insert(
            "system-mono".into(),
            std::sync::Arc::new(egui::FontData::from_owned(bytes)),
        );
        defs.families
            .entry(FontFamily::Monospace)
            .or_default()
            .insert(0, "system-mono".into());
    }
    ctx.set_fonts(defs);
}

#[cfg(test)]
mod tests {
    use super::*;

    // ───────────────────────────── contrast ─────────────────────────────

    fn channel(c: u8) -> f64 {
        let c = c as f64 / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }

    /// WCAG relative luminance.
    fn luminance(c: Color32) -> f64 {
        0.2126 * channel(c.r()) + 0.7152 * channel(c.g()) + 0.0722 * channel(c.b())
    }

    /// WCAG contrast ratio between two opaque colours.
    fn contrast(a: Color32, b: Color32) -> f64 {
        let (la, lb) = (luminance(a), luminance(b));
        let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }

    fn check(p: &Palette) {
        let name = if p.dark { "dark" } else { "light" };
        // Text pairs ≥ 4.5:1.
        let text_pairs = [
            ("text/bg", p.text, p.bg),
            ("text/control", p.text, p.control),
            ("text/hover", p.text, p.control_hover),
            ("text/active", p.text, p.control_active),
            ("muted/bg", p.text_muted, p.bg),
            ("muted/control", p.text_muted, p.control),
            ("muted/hover", p.text_muted, p.control_hover),
            ("disabled/bg", p.text_disabled, p.bg),
            ("on_accent/accent", p.on_accent, p.accent),
            ("amber/bg", p.amber, p.bg),
            ("red/bg", p.red, p.bg),
            ("hud_text/hud", p.hud_text, p.hud_fill),
            ("hud_muted/hud", p.hud_muted, p.hud_fill),
        ];
        for (n, fg, bg) in text_pairs {
            let c = contrast(fg, bg);
            assert!(c >= 4.5, "{name} {n}: {c:.2} < 4.5");
        }
        // Indicators and focus rings ≥ 3:1.
        let ui_pairs = [
            ("accent/bg", p.accent, p.bg),
            ("accent/control", p.accent, p.control),
            ("border/bg", p.control_border, p.bg),
            ("border/control", p.control_border, p.control),
            ("red/hud", p.red, p.hud_fill),
            ("amber/hud", p.amber, p.hud_fill),
            ("green/hud", p.green, p.hud_fill),
            ("green/bg", p.green, p.bg),
        ];
        for (n, fg, bg) in ui_pairs {
            let c = contrast(fg, bg);
            assert!(c >= 3.0, "{name} {n}: {c:.2} < 3.0");
        }
    }

    #[test]
    fn light_palette_meets_wcag() {
        check(&LIGHT);
    }

    #[test]
    fn dark_palette_meets_wcag() {
        check(&DARK);
    }

    #[test]
    fn contrast_math_matches_known_values() {
        assert!((contrast(Color32::BLACK, Color32::WHITE) - 21.0).abs() < 0.01);
        assert!((contrast(Color32::WHITE, Color32::WHITE) - 1.0).abs() < 1e-9);
    }
}
