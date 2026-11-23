//! The Settings window: one pane, exactly six controls — Shortcut, Microphone,
//! OpenRouter key, Vocabulary, Meeting folder, Open at login. No tabs.

use super::theme::{self, Palette};
use super::widgets;
use crate::platform::UiPlatform;
use cantrip_core::app::{Command, KeyState, UiSnapshot};
use cantrip_core::shortcut::Chord;
use cantrip_core::surface::{SettingsControl, SETTINGS_CONTROLS};
use eframe::egui::{self, Align, Layout, RichText, Ui, Vec2};
use std::path::{Path, PathBuf};

pub const WIDTH: f32 = 500.0;
const LABEL_W: f32 = 118.0;

#[derive(Default, Debug)]
pub struct SettingsState {
    recording_shortcut: bool,
    shortcut_hint: Option<String>,
    key_editing: bool,
    key_buf: String,
    new_term: String,
    selected_term: Option<String>,
    /// Inline path entry, shown when no native folder picker exists.
    folder_inline: Option<String>,
    picker_note: Option<String>,
}

pub fn show(
    ui: &mut Ui,
    p: &Palette,
    snap: &UiSnapshot,
    st: &mut SettingsState,
    platform: &dyn UiPlatform,
) -> Vec<Command> {
    let mut out = Vec::new();
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(22, 20))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for (i, control) in SETTINGS_CONTROLS.iter().enumerate() {
                if i > 0 {
                    ui.add_space(12.0);
                    widgets::hairline(ui, p);
                    ui.add_space(12.0);
                }
                row(ui, p, *control, |ui| match control {
                    SettingsControl::Shortcut => shortcut(ui, p, snap, st, &mut out),
                    SettingsControl::Microphone => microphone(ui, snap, &mut out),
                    SettingsControl::OpenRouterKey => key(ui, p, snap, st, &mut out),
                    SettingsControl::Vocabulary => vocabulary(ui, p, snap, st, &mut out),
                    SettingsControl::MeetingFolder => folder(ui, p, snap, st, platform, &mut out),
                    SettingsControl::OpenAtLogin => login(ui, p, snap, &mut out),
                });
            }
        });
    out
}

fn row(ui: &mut Ui, p: &Palette, control: SettingsControl, body: impl FnOnce(&mut Ui)) {
    ui.horizontal_top(|ui| {
        // Open at login is a labelled checkbox; its name lives on the control.
        let name = if control == SettingsControl::OpenAtLogin {
            ""
        } else {
            control.label()
        };
        ui.allocate_ui_with_layout(
            Vec2::new(LABEL_W, 26.0),
            Layout::left_to_right(Align::Center),
            |ui| {
                ui.set_min_size(Vec2::new(LABEL_W, 26.0));
                if !name.is_empty() {
                    ui.label(RichText::new(name).color(p.text_muted));
                }
            },
        );
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(8.0, 6.0);
            body(ui)
        });
    });
}

fn inline_error(ui: &mut Ui, p: &Palette, msg: &str) {
    ui.add(
        egui::Label::new(
            RichText::new(msg)
                .font(theme::sans(theme::SMALL))
                .color(p.amber),
        )
        .wrap(),
    );
}

fn note(ui: &mut Ui, p: &Palette, msg: &str) {
    ui.add(
        egui::Label::new(
            RichText::new(msg)
                .font(theme::sans(theme::SMALL))
                .color(p.text_muted),
        )
        .wrap(),
    );
}

fn shortcut(
    ui: &mut Ui,
    p: &Palette,
    snap: &UiSnapshot,
    st: &mut SettingsState,
    out: &mut Vec<Command>,
) {
    ui.horizontal(|ui| {
        let text = if st.recording_shortcut {
            "Press a shortcut…".to_string()
        } else {
            snap.shortcut.display()
        };
        let font = if st.recording_shortcut {
            theme::sans(theme::BODY)
        } else {
            theme::mono(theme::BODY)
        };
        let color = if st.recording_shortcut {
            p.text_muted
        } else {
            p.text
        };
        egui::Frame::new()
            .fill(p.control)
            .stroke(egui::Stroke::new(
                1.0,
                if st.recording_shortcut {
                    p.accent
                } else {
                    p.hairline
                },
            ))
            .corner_radius(6)
            .inner_margin(egui::Margin::symmetric(10, 4))
            .show(ui, |ui| {
                ui.set_min_width(150.0);
                ui.label(RichText::new(text).font(font).color(color));
            });
        let label = if st.recording_shortcut {
            "Cancel"
        } else {
            "Record"
        };
        if widgets::button(ui, p, label).clicked() {
            st.recording_shortcut = !st.recording_shortcut;
            st.shortcut_hint = None;
        }
    });
    if st.recording_shortcut {
        let events = ui.input(|i| i.events.clone());
        for ev in events {
            if let egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } = ev
            {
                if key == egui::Key::Escape {
                    st.recording_shortcut = false;
                    break;
                }
                let chord = Chord {
                    ctrl: modifiers.ctrl,
                    alt: modifiers.alt,
                    shift: modifiers.shift,
                    meta: modifiers.mac_cmd,
                    key: key.name().to_string(),
                };
                if chord.is_valid() {
                    out.push(Command::SetShortcut(chord));
                    st.recording_shortcut = false;
                    st.shortcut_hint = None;
                } else {
                    st.shortcut_hint = Some("Use at least one modifier with a key.".into());
                }
                break;
            }
        }
    }
    if let Some(h) = &st.shortcut_hint {
        note(ui, p, h);
    }
}

fn microphone(ui: &mut Ui, snap: &UiSnapshot, out: &mut Vec<Command>) {
    let current = match &snap.microphone {
        None => "System default".to_string(),
        Some(id) => snap
            .microphones
            .iter()
            .find(|(d, _)| d == id)
            .map(|(_, n)| n.clone())
            .unwrap_or_else(|| format!("{id} (unavailable)")),
    };
    egui::ComboBox::from_id_salt("microphone")
        .width(280.0)
        .selected_text(current)
        .show_ui(ui, |ui| {
            if ui
                .selectable_label(snap.microphone.is_none(), "System default")
                .clicked()
            {
                out.push(Command::SetMicrophone(None));
            }
            for (id, name) in &snap.microphones {
                let sel = snap.microphone.as_deref() == Some(id.as_str());
                if ui.selectable_label(sel, name).clicked() {
                    out.push(Command::SetMicrophone(Some(id.clone())));
                }
            }
        });
}

fn key(
    ui: &mut Ui,
    p: &Palette,
    snap: &UiSnapshot,
    st: &mut SettingsState,
    out: &mut Vec<Command>,
) {
    if st.key_editing {
        ui.horizontal(|ui| {
            let edit = ui.add(
                egui::TextEdit::singleline(&mut st.key_buf)
                    .password(true)
                    .hint_text("sk-or-…")
                    .desired_width(200.0),
            );
            edit.widget_info(|| {
                egui::WidgetInfo::labeled(egui::Role::TextInput, true, "OpenRouter key")
            });
            let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (widgets::button(ui, p, "Save").clicked() || enter) && !st.key_buf.trim().is_empty()
            {
                out.push(Command::SaveKey(st.key_buf.trim().to_string()));
                st.key_buf.clear();
                st.key_editing = false;
            }
            if widgets::button(ui, p, "Cancel").clicked() {
                st.key_buf.clear();
                st.key_editing = false;
            }
        });
    } else {
        ui.horizontal(|ui| match &snap.key {
            KeyState::Saved => {
                ui.label(RichText::new("saved").color(p.text));
                let (r, _) = ui.allocate_exact_size(Vec2::new(14.0, 18.0), egui::Sense::hover());
                widgets::check(ui, r.center(), 12.0, p.green);
                ui.add_space(4.0);
                if widgets::button(ui, p, "Replace…").clicked() {
                    st.key_editing = true;
                }
                if widgets::button(ui, p, "Remove").clicked() {
                    out.push(Command::RemoveKey);
                }
            }
            KeyState::NotSet => {
                if widgets::button(ui, p, "Add key").clicked() {
                    st.key_editing = true;
                }
            }
            KeyState::Error(_) => {
                if widgets::button(ui, p, "Replace…").clicked() {
                    st.key_editing = true;
                }
            }
        });
        if let KeyState::Error(msg) = &snap.key {
            inline_error(ui, p, msg);
        }
    }
    if let Some(e) = &snap.key_error {
        inline_error(ui, p, e);
    }
}

fn vocabulary(
    ui: &mut Ui,
    p: &Palette,
    snap: &UiSnapshot,
    st: &mut SettingsState,
    out: &mut Vec<Command>,
) {
    // One flat list as calm wrapped text; click a term to select it, − removes it.
    if st
        .selected_term
        .as_ref()
        .is_some_and(|t| !snap.vocabulary.contains(t))
    {
        st.selected_term = None;
    }
    egui::Frame::new()
        .fill(p.control)
        .stroke(egui::Stroke::new(1.0, p.hairline))
        .corner_radius(6)
        .inner_margin(egui::Margin::symmetric(6, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical()
                .max_height(150.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    if snap.vocabulary.is_empty() {
                        note(ui, p, "No terms yet.");
                    }
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = Vec2::new(0.0, 2.0);
                        let n = snap.vocabulary.len();
                        for (i, term) in snap.vocabulary.iter().enumerate() {
                            let selected = st.selected_term.as_deref() == Some(term.as_str());
                            let text = if i + 1 < n {
                                format!("{term},")
                            } else {
                                term.clone()
                            };
                            let galley_w = ui
                                .painter()
                                .layout_no_wrap(text.clone(), theme::sans(theme::BODY), p.text)
                                .size()
                                .x;
                            let (rect, r) = ui.allocate_exact_size(
                                Vec2::new(galley_w + 6.0, 22.0),
                                egui::Sense::click(),
                            );
                            r.widget_info(|| {
                                egui::WidgetInfo::selected(
                                    egui::Role::ListItem,
                                    true,
                                    selected,
                                    term,
                                )
                            });
                            let (fill, color) = if selected {
                                (Some(p.accent), p.on_accent)
                            } else if r.hovered() {
                                (Some(p.control_hover), p.text)
                            } else {
                                (None, p.text)
                            };
                            if let Some(f) = fill {
                                ui.painter().rect_filled(rect, 4, f);
                            }
                            ui.painter().text(
                                egui::pos2(rect.left() + 3.0, rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                &text,
                                theme::sans(theme::BODY),
                                color,
                            );
                            widgets::focus_ring(ui, &r, rect, p, 4);
                            if r.clicked() {
                                st.selected_term = if selected { None } else { Some(term.clone()) };
                            }
                        }
                    });
                });
        });
    ui.horizontal(|ui| {
        let edit = ui.add(
            egui::TextEdit::singleline(&mut st.new_term)
                .hint_text("Add a term")
                .desired_width(230.0),
        );
        edit.widget_info(|| {
            egui::WidgetInfo::labeled(egui::Role::TextInput, true, "New vocabulary term")
        });
        let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let add = widgets::button(ui, p, "+").on_hover_text("Add term");
        add.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, "Add term"));
        if (add.clicked() || enter) && !st.new_term.trim().is_empty() {
            out.push(Command::AddTerm(st.new_term.trim().to_string()));
            st.new_term.clear();
        }
        let label = match &st.selected_term {
            Some(t) => format!("Remove {t}"),
            None => "Remove selected term".to_string(),
        };
        let rm = ui
            .add_enabled_ui(st.selected_term.is_some(), |ui| widgets::button(ui, p, "−"))
            .inner;
        rm.widget_info(|| {
            egui::WidgetInfo::labeled(egui::Role::Button, st.selected_term.is_some(), &label)
        });
        let rm = rm.on_hover_text(&label);
        if rm.clicked() {
            if let Some(t) = st.selected_term.take() {
                out.push(Command::RemoveTerm(t));
            }
        }
    });
}

fn folder(
    ui: &mut Ui,
    p: &Palette,
    snap: &UiSnapshot,
    st: &mut SettingsState,
    platform: &dyn UiPlatform,
    out: &mut Vec<Command>,
) {
    ui.horizontal(|ui| {
        let shown = abbreviate(&snap.meeting_folder, 34);
        ui.label(RichText::new(shown).font(theme::mono(theme::SMALL + 0.5)))
            .on_hover_text(snap.meeting_folder.display().to_string());
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if widgets::button(ui, p, "Choose…").clicked() {
                match platform.choose_folder(&snap.meeting_folder) {
                    Ok(Some(path)) => out.push(Command::ChooseMeetingFolder(path)),
                    Ok(None) => {}
                    Err(why) => {
                        st.picker_note = Some(why);
                        st.folder_inline = Some(snap.meeting_folder.display().to_string());
                    }
                }
            }
        });
    });
    if let Some(buf) = &mut st.folder_inline {
        let mut done = false;
        ui.horizontal(|ui| {
            let edit = ui.add(
                egui::TextEdit::singleline(buf)
                    .desired_width(250.0)
                    .font(theme::mono(theme::SMALL + 0.5)),
            );
            edit.widget_info(|| {
                egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Meeting folder path")
            });
            if widgets::button(ui, p, "Use").clicked() && !buf.trim().is_empty() {
                out.push(Command::ChooseMeetingFolder(expand_home(buf.trim())));
                done = true;
            }
            if widgets::button(ui, p, "Cancel").clicked() {
                done = true;
            }
        });
        if done {
            st.folder_inline = None;
            st.picker_note = None;
        }
    }
    if let Some(e) = &snap.folder_error {
        inline_error(ui, p, e);
    }
}

fn login(ui: &mut Ui, p: &Palette, snap: &UiSnapshot, out: &mut Vec<Command>) {
    let mut on = snap.open_at_login;
    let label = SettingsControl::OpenAtLogin.label();
    if ui.checkbox(&mut on, label).changed() {
        out.push(Command::SetOpenAtLogin(on));
    }
    if let Some(e) = &snap.login_error {
        inline_error(ui, p, e);
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn expand_home(s: &str) -> PathBuf {
    match (s.strip_prefix("~/"), home()) {
        (Some(rest), Some(h)) => h.join(rest),
        _ if s == "~" => home().unwrap_or_else(|| PathBuf::from(s)),
        _ => PathBuf::from(s),
    }
}

/// `$HOME` as `~`, then middle-ellipsis to at most `max` chars.
pub fn abbreviate(path: &Path, max: usize) -> String {
    let s = path.display().to_string();
    let s = match home().map(|h| h.display().to_string()) {
        Some(h) if !h.is_empty() && h != "/" && s.starts_with(&h) => format!("~{}", &s[h.len()..]),
        _ => s,
    };
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s;
    }
    let keep = max - 1;
    let head = keep / 3;
    let tail = keep - head;
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(&chars[chars.len() - tail..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abbreviate_keeps_short_paths_and_ellipsizes_long_ones() {
        assert_eq!(abbreviate(Path::new("/srv/notes"), 34), "/srv/notes");
        let long = Path::new("/very/long/path/to/some/deeply/nested/meeting/folder/voice");
        let a = abbreviate(long, 30);
        assert_eq!(a.chars().count(), 30);
        assert!(a.contains('…'));
        assert!(a.ends_with("folder/voice"));
    }

    #[test]
    fn exactly_six_settings_rows() {
        assert_eq!(SETTINGS_CONTROLS.len(), 6);
    }
}

#[cfg(test)]
mod ui_tests {
    use super::*;
    use crate::ui::gallery::{fixture, GalleryPlatform};
    use eframe::egui::accesskit::Role;

    fn nodes(snap: &UiSnapshot) -> Vec<(Role, String)> {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut st = SettingsState::default();
        let platform = GalleryPlatform { dark: false };
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            show(ui, &theme::LIGHT, snap, &mut st, &platform);
        });
        out.textures_delta.clear();
        let update = out
            .platform_output
            .accesskit_update
            .expect("accesskit tree");
        update
            .nodes
            .iter()
            .filter_map(|(_, n)| n.label().or(n.value()).map(|l| (n.role(), l.to_string())))
            .collect()
    }

    #[test]
    fn one_pane_with_the_six_named_controls_and_no_tabs() {
        let nodes = nodes(&fixture());
        let labels: Vec<&str> = nodes
            .iter()
            .filter(|(r, _)| *r == Role::Label)
            .map(|(_, l)| l.as_str())
            .collect();
        for name in [
            "Shortcut",
            "Microphone",
            "OpenRouter key",
            "Vocabulary",
            "Meeting folder",
        ] {
            assert!(
                labels.contains(&name),
                "missing row label {name}: {labels:?}"
            );
        }
        assert!(
            nodes
                .iter()
                .any(|(r, l)| *r == Role::CheckBox && l == "Open at login"),
            "{nodes:?}"
        );
        assert!(nodes
            .iter()
            .all(|(r, _)| *r != Role::Tab && *r != Role::TabList));
        for button in ["Record", "Replace…", "Remove", "Choose…", "Add term"] {
            assert!(
                nodes.iter().any(|(r, l)| *r == Role::Button && l == button),
                "missing {button}"
            );
        }
        // The key itself is never rendered; only its state.
        assert!(!nodes.iter().any(|(_, l)| l.starts_with("sk-or")));
    }
}
