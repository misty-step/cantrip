//! Small native-feeling widgets: menu rows, a segmented control, a switch and
//! painted state icons. Every widget is keyboard-focusable where interactive,
//! draws a ≥ 3:1 focus ring, and reports an accessible name.

use super::theme::{self, Palette};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, Pos2, Rect, Response, Role, Sense, Shape, Stroke,
    StrokeKind, Ui, Vec2, WidgetInfo,
};

pub const ROW_H: f32 = 28.0;

/// Paint the focus ring around `rect` when `resp` has keyboard focus.
pub fn focus_ring(ui: &Ui, resp: &Response, rect: Rect, p: &Palette, radius: u8) {
    if resp.has_focus() {
        ui.painter().rect_stroke(
            rect.expand(1.0),
            CornerRadius::same(radius),
            Stroke::new(2.0, p.accent),
            StrokeKind::Outside,
        );
    }
}

/// A full-width menu item: label left, optional muted mono accessory right.
pub fn menu_row(
    ui: &mut Ui,
    p: &Palette,
    label: &str,
    accessory: Option<&str>,
    enabled: bool,
) -> Response {
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(
        Vec2::new(width, ROW_H),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    resp.widget_info(|| WidgetInfo::labeled(Role::Button, enabled, label));
    if ui.is_rect_visible(rect) {
        if enabled && (resp.hovered() || resp.has_focus()) {
            let fill = if resp.is_pointer_button_down_on() {
                p.control_active
            } else {
                p.control_hover
            };
            ui.painter().rect_filled(rect, CornerRadius::same(6), fill);
        }
        let color = if enabled { p.text } else { p.text_disabled };
        ui.painter().text(
            Pos2::new(rect.left() + 10.0, rect.center().y),
            Align2::LEFT_CENTER,
            label,
            theme::sans(theme::BODY),
            color,
        );
        if let Some(acc) = accessory {
            ui.painter().text(
                Pos2::new(rect.right() - 10.0, rect.center().y),
                Align2::RIGHT_CENTER,
                acc,
                theme::mono(theme::SMALL),
                p.text_muted,
            );
        }
    }
    if enabled {
        focus_ring(ui, &resp, rect, p, 6);
    }
    if enabled {
        resp
    } else {
        resp.on_hover_text("Nothing to copy yet")
    }
}

/// A plain push button with a hairline border (native "bordered" button).
pub fn button(ui: &mut Ui, p: &Palette, label: &str) -> Response {
    let color = if ui.is_enabled() {
        p.text
    } else {
        p.text_disabled
    };
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_string(), theme::sans(theme::BODY), color);
    let size = Vec2::new(galley.size().x + 22.0, 26.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    resp.widget_info(|| WidgetInfo::labeled(Role::Button, true, label));
    if ui.is_rect_visible(rect) {
        let fill = if resp.is_pointer_button_down_on() {
            p.control_active
        } else if resp.hovered() {
            p.control_hover
        } else {
            p.control
        };
        ui.painter().rect(
            rect,
            CornerRadius::same(6),
            fill,
            Stroke::new(1.0, p.hairline),
            StrokeKind::Inside,
        );
        ui.painter()
            .galley(rect.center() - galley.size() / 2.0, galley, color);
    }
    focus_ring(ui, &resp, rect, p, 6);
    resp
}

/// A borderless text button (for compact inline actions like Retry / Quit).
pub fn text_button(ui: &mut Ui, p: &Palette, label: &str, color: Color32) -> Response {
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_string(), theme::sans(theme::BODY), color);
    let size = Vec2::new(galley.size().x + 16.0, ROW_H);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    resp.widget_info(|| WidgetInfo::labeled(Role::Button, true, label));
    if ui.is_rect_visible(rect) {
        if resp.hovered() || resp.has_focus() {
            let fill = if resp.is_pointer_button_down_on() {
                p.control_active
            } else {
                p.control_hover
            };
            ui.painter().rect_filled(rect, CornerRadius::same(6), fill);
        }
        ui.painter()
            .galley(rect.center() - galley.size() / 2.0, galley, color);
    }
    focus_ring(ui, &resp, rect, p, 6);
    resp
}

/// Segmented control; returns the newly chosen index when it changes.
pub fn segmented(
    ui: &mut Ui,
    p: &Palette,
    id: &str,
    labels: &[&str],
    selected: usize,
) -> Option<usize> {
    let font = theme::sans(theme::SMALL + 0.5);
    let widths: Vec<f32> = labels
        .iter()
        .map(|l| {
            ui.painter()
                .layout_no_wrap(l.to_string(), font.clone(), p.text)
                .size()
                .x
                + 20.0
        })
        .collect();
    let total: f32 = widths.iter().sum::<f32>() + 4.0;
    let (track, _) = ui.allocate_exact_size(Vec2::new(total, 24.0), Sense::hover());
    ui.painter().rect(
        track,
        CornerRadius::same(6),
        p.control,
        Stroke::new(1.0, p.hairline),
        StrokeKind::Inside,
    );
    let mut x = track.left() + 2.0;
    let mut chosen = None;
    for (i, (label, w)) in labels.iter().zip(&widths).enumerate() {
        let rect = Rect::from_min_size(
            Pos2::new(x, track.top() + 2.0),
            Vec2::new(*w, track.height() - 4.0),
        );
        x += w;
        let resp = ui.interact(rect, ui.scope_id().with(id).with(i), Sense::click());
        let is_sel = i == selected;
        resp.widget_info(|| WidgetInfo::selected(Role::RadioButton, true, is_sel, *label));
        let (fill, color) = if is_sel {
            (Some(p.accent), p.on_accent)
        } else if resp.hovered() {
            (Some(p.control_hover), p.text)
        } else {
            (None, p.text)
        };
        if let Some(f) = fill {
            ui.painter().rect_filled(rect, CornerRadius::same(4), f);
        }
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            *label,
            font.clone(),
            color,
        );
        focus_ring(ui, &resp, rect, p, 4);
        if resp.clicked() && !is_sel {
            chosen = Some(i);
        }
    }
    chosen
}

/// An on/off switch; returns true when toggled.
pub fn switch(ui: &mut Ui, p: &Palette, label: &str, on: bool) -> Response {
    let size = Vec2::new(34.0, 20.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    resp.widget_info(|| WidgetInfo::selected(Role::Switch, true, on, label));
    if resp.clicked() {
        resp.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_with_time(
            resp.id,
            on,
            if theme::reduced_motion() { 0.0 } else { 0.12 },
        );
        let radius = rect.height() / 2.0;
        let (fill, stroke) = if on {
            (p.accent, Stroke::new(1.0, p.accent))
        } else {
            (p.control, Stroke::new(1.0, p.control_border))
        };
        ui.painter().rect(
            rect,
            CornerRadius::same(radius as u8),
            fill,
            stroke,
            StrokeKind::Inside,
        );
        let cx = egui::lerp((rect.left() + radius)..=(rect.right() - radius), t);
        let knob = if on { p.on_accent } else { p.control_border };
        ui.painter()
            .circle_filled(Pos2::new(cx, rect.center().y), radius - 4.0, knob);
    }
    focus_ring(ui, &resp, rect, p, 10);
    resp
}

// ───────────────────────────── painted icons ─────────────────────────────

/// Filled dot (status / recording).
pub fn dot(ui: &Ui, center: Pos2, r: f32, color: Color32) {
    ui.painter().circle_filled(center, r, color);
}

/// Check mark inside a `size` square centred at `c`.
pub fn check(ui: &Ui, c: Pos2, size: f32, color: Color32) {
    let s = size / 2.0;
    let pts = vec![
        Pos2::new(c.x - s * 0.75, c.y + s * 0.02),
        Pos2::new(c.x - s * 0.22, c.y + s * 0.55),
        Pos2::new(c.x + s * 0.8, c.y - s * 0.6),
    ];
    ui.painter()
        .add(Shape::line(pts, Stroke::new(size * 0.16, color)));
}

/// Warning triangle with an exclamation mark.
pub fn warning(ui: &Ui, c: Pos2, size: f32, color: Color32, bg: Color32) {
    let h = size * 0.9;
    let w = size;
    let top = Pos2::new(c.x, c.y - h / 2.0);
    let bl = Pos2::new(c.x - w / 2.0, c.y + h / 2.0);
    let br = Pos2::new(c.x + w / 2.0, c.y + h / 2.0);
    ui.painter().add(Shape::convex_polygon(
        vec![top, br, bl],
        color,
        Stroke::NONE,
    ));
    let bar_top = Pos2::new(c.x, c.y - h * 0.12);
    let bar_bot = Pos2::new(c.x, c.y + h * 0.2);
    ui.painter()
        .line_segment([bar_top, bar_bot], Stroke::new(size * 0.12, bg));
    ui.painter()
        .circle_filled(Pos2::new(c.x, c.y + h * 0.34), size * 0.065, bg);
}

/// Indeterminate spinner (a 270° arc), rotating unless reduced motion.
pub fn spinner(ui: &Ui, c: Pos2, r: f32, color: Color32) {
    let t = if theme::reduced_motion() {
        0.0
    } else {
        ui.input(|i| i.time) as f32 * 4.0
    };
    let n = 24;
    let pts: Vec<Pos2> = (0..=n)
        .map(|i| {
            let a = t + (i as f32 / n as f32) * std::f32::consts::PI * 1.5;
            Pos2::new(c.x + r * a.cos(), c.y + r * a.sin())
        })
        .collect();
    ui.painter().add(Shape::line(pts, Stroke::new(1.8, color)));
}

/// Hairline horizontal separator spanning the available width.
pub fn hairline(ui: &mut Ui, p: &Palette) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 9.0), Sense::hover());
    ui.painter().hline(
        rect.x_range(),
        rect.center().y,
        Stroke::new(1.0, p.hairline),
    );
}
