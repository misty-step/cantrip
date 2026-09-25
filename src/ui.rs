//! Shared Lantern material for Cantrip's deliberate windows (Actions,
//! Settings, HUD gallery): egui visuals from the desktop palette, bundled type,
//! and the few components every surface uses. See docs/DESIGN.md.

use crate::fonts;
use crate::theme::{Palette, Tones};
use eframe::egui::{
    self, Color32, FontFamily, FontId, Margin, Response, RichText, Rounding, Sense, Shadow, Stroke,
    TextStyle, Ui, Vec2, WidgetInfo, WidgetType,
};

pub const CARD_RADIUS: f32 = 10.0;
pub const CONTROL_RADIUS: f32 = 7.0;

pub fn color(rgb: [u8; 3]) -> Color32 {
    Color32::from_rgb(rgb[0], rgb[1], rgb[2])
}

pub fn color_alpha(rgb: [u8; 3], alpha: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(
        rgb[0],
        rgb[1],
        rgb[2],
        (alpha.clamp(0.0, 1.0) * 255.0) as u8,
    )
}

/// Install bundled type once for a new window, then apply the palette.
pub fn setup(ctx: &egui::Context, palette: Palette) {
    fonts::install(ctx);
    apply(ctx, palette);
}

/// Apply palette-derived visuals and spacing. Cheap; safe on every refresh.
pub fn apply(ctx: &egui::Context, palette: Palette) {
    let tones = palette.tones();
    let light = palette.is_light();
    let mut visuals = if light {
        egui::Visuals::light()
    } else {
        egui::Visuals::dark()
    };
    let hairline = Stroke::new(1.0_f32, color(tones.hairline));
    let control = Rounding::same(CONTROL_RADIUS);
    visuals.override_text_color = None;
    visuals.panel_fill = color(tones.canvas);
    visuals.window_fill = color(tones.panel);
    visuals.window_stroke = Stroke::new(1.0_f32, color(tones.hairline_strong));
    visuals.window_rounding = Rounding::same(12.0);
    visuals.window_shadow = Shadow {
        offset: Vec2::new(0.0, 12.0),
        blur: 40.0,
        spread: 0.0,
        color: Color32::from_black_alpha(if light { 50 } else { 120 }),
    };
    visuals.popup_shadow = Shadow {
        offset: Vec2::new(0.0, 6.0),
        blur: 18.0,
        spread: 0.0,
        color: Color32::from_black_alpha(if light { 30 } else { 80 }),
    };
    visuals.menu_rounding = Rounding::same(9.0);
    visuals.extreme_bg_color = color(tones.well);
    visuals.faint_bg_color = color(tones.raised);
    visuals.code_bg_color = color(tones.well);
    visuals.hyperlink_color = color(tones.accent);
    visuals.warn_fg_color = color(tones.attention);
    visuals.error_fg_color = color(tones.attention);
    visuals.selection.bg_fill = color(tones.accent_soft);
    visuals.selection.stroke = Stroke::new(1.0_f32, color(tones.text));
    visuals.text_cursor.stroke = Stroke::new(2.0_f32, color(tones.accent));
    visuals.slider_trailing_fill = true;
    visuals.striped = false;
    let widgets = &mut visuals.widgets;
    widgets.noninteractive.bg_fill = color(tones.panel);
    widgets.noninteractive.weak_bg_fill = color(tones.panel);
    widgets.noninteractive.bg_stroke = hairline;
    widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, color(tones.text));
    widgets.noninteractive.rounding = control;
    widgets.inactive.bg_fill = color(tones.raised);
    widgets.inactive.weak_bg_fill = color(tones.raised);
    widgets.inactive.bg_stroke = hairline;
    widgets.inactive.fg_stroke = Stroke::new(1.0_f32, color(tones.text));
    widgets.inactive.rounding = control;
    widgets.hovered.bg_fill = color(crate::theme::mix(tones.raised, tones.text, 0.05));
    widgets.hovered.weak_bg_fill = widgets.hovered.bg_fill;
    widgets.hovered.bg_stroke = Stroke::new(1.0_f32, color(tones.hairline_strong));
    widgets.hovered.fg_stroke = Stroke::new(1.5_f32, color(tones.text));
    widgets.hovered.rounding = control;
    widgets.hovered.expansion = 0.0;
    widgets.active.bg_fill = color(crate::theme::mix(tones.raised, tones.accent, 0.18));
    widgets.active.weak_bg_fill = widgets.active.bg_fill;
    widgets.active.bg_stroke = Stroke::new(1.0_f32, color(tones.accent));
    widgets.active.fg_stroke = Stroke::new(1.5_f32, color(tones.text));
    widgets.active.rounding = control;
    widgets.active.expansion = 0.0;
    widgets.open = widgets.hovered;
    ctx.set_visuals(visuals);
    ctx.style_mut(|style| {
        style.animation_time = 0.08;
        style.text_styles = [
            (TextStyle::Heading, FontId::new(19.0, fonts::semibold())),
            (TextStyle::Body, FontId::new(13.5, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(13.5, fonts::medium())),
            (
                TextStyle::Small,
                FontId::new(12.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Monospace,
                FontId::new(12.5, FontFamily::Monospace),
            ),
        ]
        .into();
        let spacing = &mut style.spacing;
        spacing.item_spacing = Vec2::new(8.0, 8.0);
        spacing.button_padding = Vec2::new(12.0, 6.0);
        spacing.interact_size = Vec2::new(40.0, 30.0);
        spacing.window_margin = Margin::same(20.0);
        spacing.menu_margin = Margin::same(6.0);
        spacing.indent = 16.0;
        spacing.icon_width = 16.0;
        spacing.icon_width_inner = 9.0;
        spacing.icon_spacing = 8.0;
        spacing.combo_height = 260.0;
    });
}

/// The wordmark is lit on the HUD's own matrix: square LED cells in one
/// continuous panel, rest cells faintly visible, lit cells in the accent.
/// Lowercase 5×9 glyphs (x-height 5, two-row ascenders and descenders);
/// unknown characters render as space.
///
/// Optical variants (docs/DESIGN.md, Mark): the display variant draws LEDs with
/// gaps and the rest matrix; where that does not fit, the compact variant packs
/// one-pixel cells without gaps or rest cells, the form proven at 16 px.
pub fn wordmark(ui: &mut Ui, tones: &Tones, text: &str) -> Response {
    const ROWS: usize = 9;
    let glyphs: Vec<[&str; ROWS]> = text.chars().map(led_glyph).collect();
    let columns = (glyphs.len() * 6).saturating_sub(1);
    let ppp = ui.ctx().pixels_per_point();
    // Whole physical pixels keep every LED square and every gap even.
    let extent = |cell: f32, gap: f32| {
        let pitch = cell + gap;
        Vec2::new(
            ((columns as f32 - 1.0).max(0.0) * pitch + cell) / ppp,
            ((ROWS - 1) as f32 * pitch + cell) / ppp,
        )
    };
    let display = ((2.0 * ppp).round().max(2.0), ppp.round().max(1.0));
    let compact = extent(display.0, display.1).x > ui.available_width();
    let (cell, gap) = if compact {
        (ppp.round().max(1.0), 0.0)
    } else {
        display
    };
    let (rect, response) = ui.allocate_exact_size(extent(cell, gap), Sense::hover());
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, text));
    let origin = ui.painter().round_pos_to_pixels(rect.min);
    let painter = ui.painter();
    let pitch = (cell + gap) / ppp;
    for column in 0..columns {
        let glyph = &glyphs[column / 6];
        for (row, line) in glyph.iter().enumerate() {
            let lit = column % 6 < 5 && line.as_bytes()[column % 6] == b'#';
            if !lit && compact {
                continue;
            }
            let (rgb, light) = if lit {
                (tones.accent, 1.0)
            } else {
                (tones.text, 0.07)
            };
            painter.rect_filled(
                egui::Rect::from_min_size(
                    origin + Vec2::new(column as f32 * pitch, row as f32 * pitch),
                    Vec2::splat(cell / ppp),
                ),
                Rounding::ZERO,
                color_alpha(rgb, light),
            );
        }
    }
    response
}

fn led_glyph(character: char) -> [&'static str; 9] {
    match character {
        'a' => [
            ".....", ".....", ".###.", "....#", ".####", "#...#", ".####", ".....", ".....",
        ],
        'c' => [
            ".....", ".....", ".####", "#....", "#....", "#....", ".####", ".....", ".....",
        ],
        'd' => [
            "....#", "....#", ".####", "#...#", "#...#", "#...#", ".####", ".....", ".....",
        ],
        'e' => [
            ".....", ".....", ".###.", "#...#", "#####", "#....", ".####", ".....", ".....",
        ],
        'g' => [
            ".....", ".....", ".####", "#...#", "#...#", "#...#", ".####", "....#", ".###.",
        ],
        'h' => [
            "#....", "#....", "#.##.", "##..#", "#...#", "#...#", "#...#", ".....", ".....",
        ],
        'i' => [
            "..#..", ".....", ".##..", "..#..", "..#..", "..#..", ".###.", ".....", ".....",
        ],
        'l' => [
            ".##..", "..#..", "..#..", "..#..", "..#..", "..#..", ".###.", ".....", ".....",
        ],
        'n' => [
            ".....", ".....", "#.##.", "##..#", "#...#", "#...#", "#...#", ".....", ".....",
        ],
        'o' => [
            ".....", ".....", ".###.", "#...#", "#...#", "#...#", ".###.", ".....", ".....",
        ],
        'p' => [
            ".....", ".....", "####.", "#...#", "#...#", "#...#", "####.", "#....", "#....",
        ],
        'r' => [
            ".....", ".....", "#.##.", "##..#", "#....", "#....", "#....", ".....", ".....",
        ],
        's' => [
            ".....", ".....", ".####", "#....", ".###.", "....#", "####.", ".....", ".....",
        ],
        't' => [
            ".#...", ".#...", "####.", ".#...", ".#...", ".#..#", "..##.", ".....", ".....",
        ],
        'u' => [
            ".....", ".....", "#...#", "#...#", "#...#", "#..##", ".##.#", ".....", ".....",
        ],
        'y' => [
            ".....", ".....", "#...#", "#...#", "#...#", "#...#", ".####", "....#", ".###.",
        ],
        _ => ["....."; 9],
    }
}

pub fn heading(text: impl Into<String>, tones: &Tones) -> RichText {
    RichText::new(text)
        .family(fonts::semibold())
        .size(15.0)
        .color(color(tones.text))
}

pub fn hero(text: impl Into<String>, tones: &Tones) -> RichText {
    RichText::new(text)
        .family(fonts::semibold())
        .size(19.0)
        .color(color(tones.text))
}

pub fn muted(text: impl Into<String>, tones: &Tones) -> RichText {
    RichText::new(text).color(color(tones.text_muted))
}

pub fn faint(text: impl Into<String>, tones: &Tones) -> RichText {
    RichText::new(text)
        .size(12.0)
        .color(color(tones.text_faint))
}

pub fn data(text: impl Into<String>, tones: &Tones) -> RichText {
    RichText::new(text)
        .family(FontFamily::Monospace)
        .size(12.5)
        .color(color(tones.text))
}

pub fn card(tones: &Tones) -> egui::Frame {
    egui::Frame::none()
        .fill(color(tones.panel))
        .stroke(Stroke::new(1.0_f32, color(tones.hairline)))
        .rounding(Rounding::same(CARD_RADIUS))
        .inner_margin(Margin::symmetric(16.0, 14.0))
}

pub fn attention_card(tones: &Tones) -> egui::Frame {
    card(tones)
        .fill(color(tones.attention_soft))
        .stroke(Stroke::new(1.0_f32, color(tones.attention_line)))
}

/// A recessed panel for machine output (diagnosis, raw configuration text).
pub fn well(tones: &Tones) -> egui::Frame {
    egui::Frame::none()
        .fill(color(tones.well))
        .stroke(Stroke::new(1.0_f32, color(tones.hairline)))
        .rounding(Rounding::same(CONTROL_RADIUS))
        .inner_margin(Margin::symmetric(12.0, 10.0))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Primary,
    Secondary,
    Quiet,
    /// Destructive intent outside confirmation: attention ink, no fill.
    Danger,
    /// The confirmed destructive choice inside a confirmation dialog.
    DangerFilled,
}

pub fn button<'a>(tones: &Tones, tone: Tone, text: &str) -> egui::Button<'a> {
    let (ink, fill, stroke) = match tone {
        Tone::Primary => (tones.on_accent, Some(tones.accent), None),
        Tone::Secondary => (tones.text, Some(tones.raised), Some(tones.hairline_strong)),
        Tone::Quiet => (tones.text_muted, None, None),
        Tone::Danger => (tones.attention, None, Some(tones.attention_line)),
        Tone::DangerFilled => (tones.on_attention, Some(tones.attention), None),
    };
    let mut button = egui::Button::new(
        RichText::new(text)
            .family(fonts::medium())
            .color(color(ink)),
    )
    .rounding(Rounding::same(CONTROL_RADIUS))
    .min_size(Vec2::new(0.0, 30.0));
    button = match fill {
        Some(fill) => button.fill(color(fill)),
        None => button.fill(Color32::TRANSPARENT),
    };
    match stroke {
        Some(stroke) => button.stroke(Stroke::new(1.0_f32, color(stroke))),
        None => button.stroke(Stroke::NONE),
    }
}

/// A row of mutually exclusive choices. Every segment is a focusable control;
/// Space or Enter selects it. Returns true when the value changed.
pub fn segmented<T: PartialEq + Copy>(
    ui: &mut Ui,
    tones: &Tones,
    value: &mut T,
    options: &[(T, &str)],
) -> bool {
    let mut changed = false;
    let font = FontId::new(13.0, fonts::medium());
    let padding = Vec2::new(12.0, 5.0);
    let galleys: Vec<_> = options
        .iter()
        .map(|(_, label)| {
            ui.painter()
                .layout_no_wrap((*label).to_owned(), font.clone(), color(tones.text))
        })
        .collect();
    let width: f32 = galleys
        .iter()
        .map(|galley| galley.size().x + padding.x * 2.0)
        .sum::<f32>()
        + 4.0;
    let height = 30.0;
    let (outer, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    let painter = ui.painter_at(outer.expand(2.0));
    painter.rect(
        outer,
        Rounding::same(CONTROL_RADIUS + 1.0),
        color(tones.well),
        Stroke::new(1.0_f32, color(tones.hairline)),
    );
    let mut x = outer.left() + 2.0;
    for ((option, label), galley) in options.iter().zip(galleys) {
        let segment = egui::Rect::from_min_size(
            egui::pos2(x, outer.top() + 2.0),
            Vec2::new(galley.size().x + padding.x * 2.0, height - 4.0),
        );
        x = segment.right();
        let id = ui.id().with(("segment", *label));
        let response = ui.interact(segment, id, Sense::click());
        let selected = *value == *option;
        response.widget_info(|| {
            WidgetInfo::selected(
                WidgetType::SelectableLabel,
                ui.is_enabled(),
                selected,
                *label,
            )
        });
        if response.clicked() && !selected {
            *value = *option;
            changed = true;
        }
        let fill = if selected {
            Some(tones.accent_soft)
        } else if response.hovered() {
            Some(tones.raised)
        } else {
            None
        };
        if let Some(fill) = fill {
            painter.rect_filled(segment, Rounding::same(CONTROL_RADIUS - 1.0), color(fill));
        }
        if selected {
            painter.rect_stroke(
                segment,
                Rounding::same(CONTROL_RADIUS - 1.0),
                Stroke::new(1.0_f32, color(tones.accent_line)),
            );
        }
        if response.has_focus() {
            painter.rect_stroke(
                segment.expand(1.0),
                Rounding::same(CONTROL_RADIUS),
                Stroke::new(2.0_f32, color(tones.accent)),
            );
        }
        let ink = if selected || response.hovered() {
            tones.text
        } else {
            tones.text_muted
        };
        let text_pos = segment.center() - galley.size() / 2.0;
        painter.galley_with_override_text_color(text_pos, galley, color(ink));
    }
    changed
}

/// An iOS-style switch that reads as a checkbox to assistive technology.
pub fn switch(ui: &mut Ui, tones: &Tones, on: &mut bool, label: &str) -> Response {
    let size = Vec2::new(34.0, 20.0);
    let (rect, mut response) = ui.allocate_exact_size(size, Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    response
        .widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, ui.is_enabled(), *on, label));
    let t = ui.ctx().animate_bool_responsive(response.id, *on);
    let track = color(crate::theme::mix(tones.hairline_strong, tones.accent, t));
    let painter = ui.painter();
    painter.rect(rect, Rounding::same(10.0), track, Stroke::NONE);
    let knob_x = egui::lerp((rect.left() + 10.0)..=(rect.right() - 10.0), t);
    let knob = if *on { tones.on_accent } else { tones.panel };
    painter.circle(
        egui::pos2(knob_x, rect.center().y),
        7.0,
        color(knob),
        Stroke::NONE,
    );
    if response.has_focus() {
        painter.rect_stroke(
            rect.expand(2.0),
            Rounding::same(12.0),
            Stroke::new(2.0_f32, color(tones.accent)),
        );
    }
    response
}

/// Status glyphs drawn with the HUD's own cell geometry (3 px cells, 5 px pitch).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stamp {
    /// Rest floor only: nothing saved, or state not yet known.
    Rest,
    /// Neutral centre row: idle and ready.
    Idle,
    /// Accent centre band: capture or work in progress.
    Live,
    /// Full accent grid: complete text is saved.
    Complete,
    /// Left half lit: partial text is saved.
    Partial,
    /// Attention centre row: the operator must decide or repair something.
    Attention,
}

pub fn stamp(ui: &mut Ui, tones: &Tones, stamp: Stamp) -> Response {
    const COLUMNS: usize = 12;
    const ROWS: usize = 3;
    let ppp = ui.ctx().pixels_per_point();
    // Snap cell and pitch to whole physical pixels so cells stay square and crisp.
    let cell = (3.0 * ppp).round().max(2.0) / ppp;
    let pitch = (5.0 * ppp).round().max(3.0) / ppp;
    let size = Vec2::new(
        (COLUMNS - 1) as f32 * pitch + cell,
        (ROWS - 1) as f32 * pitch + cell,
    );
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    let origin = ui.painter().round_pos_to_pixels(rect.min);
    for column in 0..COLUMNS {
        for row in 0..ROWS {
            let (rgb, light) = match stamp {
                Stamp::Rest => (tones.text, 0.14),
                Stamp::Idle => (tones.text, if row == 1 { 0.5 } else { 0.14 }),
                Stamp::Live => (tones.accent, if row == 1 { 1.0 } else { 0.4 }),
                Stamp::Complete => (tones.accent, 1.0),
                Stamp::Partial => (tones.accent, if column < COLUMNS / 2 { 1.0 } else { 0.18 }),
                Stamp::Attention => {
                    if row == 1 {
                        (tones.attention, 1.0)
                    } else {
                        (tones.text, 0.14)
                    }
                }
            };
            let min = origin + Vec2::new(column as f32 * pitch, row as f32 * pitch);
            ui.painter().rect_filled(
                egui::Rect::from_min_size(min, Vec2::splat(cell)),
                Rounding::ZERO,
                color_alpha(rgb, light),
            );
        }
    }
    response
}

/// Full-window dimmer beneath modal dialogs.
pub fn scrim(ctx: &egui::Context, tones: &Tones) {
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::PanelResizeLine,
        egui::Id::new("cantrip-scrim"),
    ));
    painter.rect_filled(
        ctx.screen_rect(),
        Rounding::ZERO,
        color_alpha(tones.canvas, 0.62),
    );
}

/// Frame for modal dialogs: raised panel, hairline, soft shadow.
pub fn dialog_frame(ctx: &egui::Context, tones: &Tones) -> egui::Frame {
    egui::Frame::window(&ctx.style())
        .fill(color(tones.panel))
        .stroke(Stroke::new(1.0_f32, color(tones.hairline_strong)))
        .rounding(Rounding::same(12.0))
        .inner_margin(Margin::same(22.0))
}
