//! The popover: exactly the 7 standing controls and, only while something is
//! staged, the one contextual recovery row. Labels come from
//! `cantrip_core::surface`, so names cannot drift from the contract.

use super::theme::{self, Palette};
use super::widgets::{self, ROW_H};
use cantrip_core::app::{Command, UiSnapshot};
use cantrip_core::surface::{RecoveryAction, StandingControl, MODES};
use eframe::egui::{self, Align, Layout, Sense, Ui, Vec2};

pub const WIDTH: f32 = 288.0;

#[derive(Default, Debug)]
pub struct PopoverState {
    /// Item id whose Discard… is awaiting confirmation.
    pub confirm_discard: Option<String>,
}

/// What the popover asks the shell to do besides sending commands.
#[derive(Debug, PartialEq)]
pub enum Action {
    Send(Command),
    OpenSettings,
    Close,
}

pub fn show(ui: &mut Ui, p: &Palette, snap: &UiSnapshot, st: &mut PopoverState) -> Vec<Action> {
    let mut out = Vec::new();
    let label = |c: StandingControl| c.label(snap.dictating, snap.meeting_active);

    // Header: "Cantrip" + status.
    ui.horizontal(|ui| {
        ui.add_space(10.0);
        ui.label(
            egui::RichText::new("Cantrip")
                .font(theme::sans(theme::TITLE))
                .strong(),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add_space(10.0);
            ui.label(
                egui::RichText::new(&snap.status)
                    .font(theme::sans(theme::SMALL))
                    .color(p.text_muted),
            );
            let (r, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
            let busy = snap.dictating || snap.meeting_active;
            let color = if busy { p.red } else { p.text_muted };
            widgets::dot(ui, r.center(), if busy { 4.0 } else { 3.5 }, color);
        });
    });
    ui.add_space(2.0);
    widgets::hairline(ui, p);

    // (1) Start/stop dictation, shortcut right-aligned in tabular mono.
    let shortcut = snap.shortcut.display();
    if widgets::menu_row(
        ui,
        p,
        label(StandingControl::Dictation),
        Some(&shortcut),
        true,
    )
    .clicked()
    {
        out.push(Action::Send(Command::ToggleDictation));
        out.push(Action::Close);
    }
    // (2) Start/stop meeting.
    if widgets::menu_row(ui, p, label(StandingControl::Meeting), None, true).clicked() {
        out.push(Action::Send(Command::ToggleMeeting));
        out.push(Action::Close);
    }
    // (3) Mode.
    labelled_row(ui, p, label(StandingControl::Mode), |ui| {
        let names: Vec<&str> = MODES.iter().map(|m| m.label()).collect();
        let sel = MODES.iter().position(|m| *m == snap.mode).unwrap_or(0);
        if let Some(i) = widgets::segmented(ui, p, "mode", &names, sel) {
            out.push(Action::Send(Command::SetMode(MODES[i])));
        }
    });
    // (4) Post-Processing.
    labelled_row(ui, p, label(StandingControl::PostProcessing), |ui| {
        let on = snap.post_processing;
        if widgets::switch(ui, p, label(StandingControl::PostProcessing), on).clicked() {
            out.push(Action::Send(Command::SetPostProcessing(!on)));
        }
    });
    // (5) Copy last dictation (visible, disabled when empty).
    if widgets::menu_row(
        ui,
        p,
        label(StandingControl::CopyLastDictation),
        None,
        snap.has_last_dictation,
    )
    .clicked()
    {
        out.push(Action::Send(Command::CopyLastDictation));
        out.push(Action::Close);
    }
    widgets::hairline(ui, p);
    // (6) Settings…  (7) Quit.
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.add_space(2.0);
        if widgets::text_button(ui, p, label(StandingControl::Settings), p.text).clicked() {
            out.push(Action::Send(Command::OpenSettings));
            out.push(Action::OpenSettings);
            out.push(Action::Close);
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add_space(2.0);
            if widgets::text_button(ui, p, label(StandingControl::Quit), p.text).clicked() {
                out.push(Action::Send(Command::Quit));
            }
        });
    });

    // (8) Contextual recovery row — only while an item is staged.
    if let Some(row) = &snap.recovery {
        widgets::hairline(ui, p);
        if st.confirm_discard.as_deref() == Some(row.item_id.as_str()) {
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&row.discard_warning).font(theme::sans(theme::SMALL)),
                    )
                    .wrap(),
                );
            });
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add_space(2.0);
                    if widgets::text_button(ui, p, "Discard", p.red).clicked() {
                        out.push(Action::Send(Command::Recovery {
                            item_id: row.item_id.clone(),
                            action: RecoveryAction::Discard,
                        }));
                        st.confirm_discard = None;
                    }
                    if widgets::text_button(ui, p, "Cancel", p.text).clicked() {
                        st.confirm_discard = None;
                    }
                });
            });
        } else {
            // Text on its own line (wraps), actions right-aligned beneath.
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                let (r, _) = ui.allocate_exact_size(Vec2::new(14.0, 18.0), Sense::hover());
                widgets::warning(ui, r.center(), 13.0, p.amber, p.bg);
                let text = row.text.trim_start_matches('⚠').trim_start();
                ui.add(egui::Label::new(egui::RichText::new(text)).wrap())
                    .widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, &row.text));
            });
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add_space(2.0);
                    for action in row.actions.iter().rev() {
                        let color = if *action == RecoveryAction::Discard {
                            p.text
                        } else {
                            p.accent
                        };
                        if widgets::text_button(ui, p, action.label(), color).clicked() {
                            if *action == RecoveryAction::Discard {
                                st.confirm_discard = Some(row.item_id.clone());
                            } else {
                                out.push(Action::Send(Command::Recovery {
                                    item_id: row.item_id.clone(),
                                    action: *action,
                                }));
                            }
                        }
                    }
                });
            });
        }
    } else {
        st.confirm_discard = None;
    }

    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        if st.confirm_discard.is_some() {
            st.confirm_discard = None;
        } else {
            out.push(Action::Close);
        }
    }
    out
}

/// A row with a label on the left and a control on the right.
fn labelled_row(ui: &mut Ui, p: &Palette, label: &str, control: impl FnOnce(&mut Ui)) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, ROW_H + 2.0), Sense::hover());
    let mut label_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(Vec2::new(10.0, 0.0)))
            .layout(Layout::left_to_right(Align::Center)),
    );
    label_ui.label(egui::RichText::new(label).color(p.text));
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(Vec2::new(10.0, 0.0)))
            .layout(Layout::right_to_left(Align::Center)),
    );
    control(&mut child);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::gallery::fixture;
    use eframe::egui::accesskit::Role;

    /// Accessible (role, name) pairs of interactive nodes in one popover pass.
    fn controls(snap: &UiSnapshot) -> Vec<(Role, String)> {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let p = theme::LIGHT;
        let mut st = PopoverState::default();
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            show(ui, &p, snap, &mut st);
        });
        out.textures_delta.clear();
        let update = out
            .platform_output
            .accesskit_update
            .expect("accesskit tree");
        update
            .nodes
            .iter()
            .filter(|(_, n)| matches!(n.role(), Role::Button | Role::RadioButton | Role::Switch))
            .filter_map(|(_, n)| n.label().map(|l| (n.role(), l.to_string())))
            .collect()
    }

    fn sorted(mut v: Vec<String>) -> Vec<String> {
        v.sort();
        v
    }

    fn expected(extra: &[&str]) -> Vec<String> {
        let base = [
            "Start dictation",
            "Start meeting",
            "Raw",
            "Clean",
            "Elevated",
            "Post-Processing",
            "Copy last dictation",
            "Settings…",
            "Quit",
        ];
        sorted(base.iter().chain(extra).map(|s| s.to_string()).collect())
    }

    #[test]
    fn exactly_seven_standing_controls_without_recovery() {
        // 7 controls; Mode is one control with three segments.
        let mut snap = fixture();
        snap.recovery = None;
        let names = controls(&snap).into_iter().map(|(_, n)| n).collect();
        assert_eq!(sorted(names), expected(&[]));
    }

    #[test]
    fn recovery_row_adds_only_its_actions() {
        let names = controls(&fixture()).into_iter().map(|(_, n)| n).collect();
        assert_eq!(sorted(names), expected(&["Retry", "Discard…"]));
    }

    #[test]
    fn failed_parts_row_offers_save_partial_notes() {
        let mut snap = fixture();
        snap.recovery = Some(cantrip_core::surface::RecoveryRow::for_item(
            "m",
            &cantrip_core::surface::RecoveryKind::MeetingPartsFailed { failed_parts: 2 },
            "14:00",
        ));
        let names = controls(&snap).into_iter().map(|(_, n)| n).collect();
        assert_eq!(
            sorted(names),
            expected(&["Retry", "Save partial notes", "Discard…"])
        );
    }
}
