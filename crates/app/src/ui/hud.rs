//! The recording HUD pill: compact, ~32 px tall, auto width, one calm surface
//! with a hairline border. Indicators are painted (not font glyphs) so they look
//! the same with every system font.

use super::theme::{self, Palette};
use super::widgets;
use cantrip_core::surface::{mm_ss, Hud, Tone};
use eframe::egui::{self, Align2, CornerRadius, FontId, Pos2, Rect, Stroke, StrokeKind, Ui, Vec2};

pub const PILL_H: f32 = 32.0;
const PAD_L: f32 = 12.0;
const PAD_R: f32 = 15.0;
const ICON: f32 = 14.0;
const GAP: f32 = 8.0;
const METER_W: f32 = 23.0;

/// Text pieces of one HUD state.
struct Parts {
    /// Main text without any leading glyph (the icon is painted).
    main: String,
    /// Mono prefix (meeting timer).
    timer: Option<String>,
    claim: Option<&'static str>,
    meter: bool,
}

fn parts(hud: &Hud) -> Parts {
    let claim = hud.claim();
    match hud {
        Hud::Meeting { elapsed_secs } => Parts {
            main: "Meeting".into(),
            timer: Some(mm_ss(*elapsed_secs)),
            claim,
            meter: false,
        },
        _ => {
            let t = hud.text();
            let main = t.strip_prefix("✓ ").unwrap_or(&t).to_string();
            Parts {
                main,
                timer: None,
                claim,
                meter: matches!(hud, Hud::Listening),
            }
        }
    }
}

fn main_font() -> FontId {
    theme::sans(theme::BODY)
}

fn claim_font() -> FontId {
    theme::sans(theme::SMALL)
}

/// Logical size of the pill for `hud`.
pub fn pill_size(ctx: &egui::Context, hud: &Hud) -> Vec2 {
    let p = parts(hud);
    let w = ctx.fonts_mut(|f| {
        let mut w = PAD_L + ICON + GAP;
        if let Some(t) = &p.timer {
            w += f
                .layout_no_wrap(t.clone(), theme::mono(theme::BODY), egui::Color32::WHITE)
                .size()
                .x
                + 6.0;
        }
        w += f
            .layout_no_wrap(p.main.clone(), main_font(), egui::Color32::WHITE)
            .size()
            .x;
        if p.meter {
            w += 10.0 + METER_W;
        }
        if let Some(c) = p.claim {
            w += 10.0
                + 1.0
                + 10.0
                + f.layout_no_wrap(c.to_string(), claim_font(), egui::Color32::WHITE)
                    .size()
                    .x;
        }
        w + PAD_R
    });
    Vec2::new(w.ceil(), PILL_H)
}

/// Paint the pill for `hud` with its top-left at `origin`; returns its rect.
pub fn paint_pill(ui: &Ui, p: &Palette, origin: Pos2, hud: &Hud, level: f32) -> Rect {
    let size = pill_size(ui.ctx(), hud);
    let rect = Rect::from_min_size(origin, size);
    let painter = ui.painter();
    painter.rect(
        rect,
        CornerRadius::same((PILL_H / 2.0) as u8),
        p.hud_fill,
        Stroke::new(1.0, p.hud_border),
        StrokeKind::Inside,
    );
    let parts = parts(hud);
    let cy = rect.center().y;
    let icon_c = Pos2::new(rect.left() + PAD_L + ICON / 2.0, cy);
    match hud.tone() {
        Tone::Recording => widgets::dot(ui, icon_c, 5.0, p.red),
        Tone::Working => widgets::spinner(ui, icon_c, 5.5, p.hud_muted),
        Tone::Confirmed => widgets::check(ui, icon_c, ICON, p.green),
        Tone::Attention => widgets::warning(ui, icon_c, ICON, p.amber, p.hud_fill),
        Tone::Neutral => {
            painter.circle_stroke(icon_c, 4.5, Stroke::new(1.5, p.hud_muted));
        }
    }
    let mut x = rect.left() + PAD_L + ICON + GAP;
    if let Some(t) = &parts.timer {
        let g = painter.layout_no_wrap(t.clone(), theme::mono(theme::BODY), p.hud_text);
        let w = g.size().x;
        painter.galley(Pos2::new(x, cy - g.size().y / 2.0), g, p.hud_text);
        x += w + 6.0;
    }
    let g = painter.layout_no_wrap(parts.main.clone(), main_font(), p.hud_text);
    let w = g.size().x;
    painter.galley(Pos2::new(x, cy - g.size().y / 2.0), g, p.hud_text);
    x += w;
    if parts.meter {
        x += 10.0;
        let shape = [0.45, 0.75, 1.0, 0.75, 0.45];
        let lvl = (level.clamp(0.0, 1.0) * 3.0).min(1.0);
        for (i, k) in shape.iter().enumerate() {
            let h = 3.0 + 11.0 * k * lvl.max(0.08);
            let bx = x + i as f32 * 5.0;
            let r = Rect::from_min_max(
                Pos2::new(bx, cy - h / 2.0),
                Pos2::new(bx + 3.0, cy + h / 2.0),
            );
            painter.rect_filled(r, CornerRadius::same(1), p.hud_muted);
        }
        x += METER_W;
    }
    if let Some(c) = parts.claim {
        x += 10.0;
        painter.vline(x, (cy - 8.0)..=(cy + 8.0), Stroke::new(1.0, p.hud_border));
        x += 11.0;
        painter.text(
            Pos2::new(x, cy),
            Align2::LEFT_CENTER,
            c,
            claim_font(),
            p.hud_muted,
        );
    }
    rect
}

/// Accessible description of the HUD (full text + claim).
pub fn describe(hud: &Hud) -> String {
    match hud.claim() {
        Some(c) => format!("{}. {c}", hud.text()),
        None => hud.text(),
    }
}
