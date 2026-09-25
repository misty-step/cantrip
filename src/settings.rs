//! `cantrip settings` — a small modifiable configuration window.
//!
//! Renders an egui window (eframe/glow on Wayland) that loads
//! `~/.config/cantrip/config.toml`, lets you edit the common settings, and
//! writes them back with `toml_edit` so the annotated comments users keep in the
//! file survive unchanged. Saving sends a `Reload` to the running daemon. The
//! header also shows the live daemon state (idle / recording / processing /
//! offline) by polling the socket every second.
//!
//! `cantrip settings --screenshot <path>` renders the window and dumps a PNG of
//! one frame, then exits. It exists for visual testing on machines without a
//! screenshot utility.

use crate::config::{Config, HudConfig, PostprocConfig, SttConfig, TelemetryConfig};
use crate::hud::{gallery, ScreenshotState};
use crate::inject::InjectionMode;
use crate::ipc;
use crate::theme::{Palette, Tones};
use crate::ui::{self, color, Tone};
use crate::{models, paths, theme};
use anyhow::{anyhow, Context, Result};
use eframe::egui;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

/// Frames to render before taking the `--screenshot` (lets layout settle).
const SCREENSHOT_DELAY_FRAMES: u32 = 6;
/// How often to refresh the live daemon state in the header.
const DAEMON_POLL: Duration = Duration::from_secs(1);
/// Readable column; the form is centred when the window is wider.
const COLUMN_MAX: f32 = 640.0;
/// Window padding around the column.
const WINDOW_PAD: f32 = 20.0;
/// Card content narrower than this stacks each row's label above its control.
const ROW_STACK_BELOW: f32 = 440.0;
/// Width of the label column in side-by-side rows.
const LABEL_WIDTH: f32 = 196.0;

/// A flat, editable view of the config, bound directly to egui text fields.
#[derive(Clone, PartialEq)]
struct Editable {
    injection: InjectionMode,
    keep_warm: bool,
    audio_source: String,
    /// One term per line; commas are accepted as separators too.
    vocabulary: String,
    stt_model: String,
    stt_endpoint: String,
    stt_key: String,
    pp_enabled: bool,
    pp_endpoint: String,
    pp_model: String,
    pp_key: String,
    /// Not editable in the window; carried through saves so a
    /// config-file reasoning effort survives a settings write.
    pp_effort: Option<String>,
    pp_timeout: u64,
    pp_passes: u8,
    pp_min_chars: usize,
    pp_instructions: String,
    pp_decision_model: Option<String>,
    pp_decision_endpoint: Option<String>,
    pp_decision_key: Option<String>,
    /// Not editable in the window; carried through saves so enabling
    /// telemetry in the config file survives a settings write.
    telemetry: TelemetryConfig,
    hud: HudConfig,
    /// Local handoff commands are file-only settings; preserve them across GUI saves.
    handoff: std::collections::BTreeMap<String, crate::config::HandoffTarget>,
}

impl Editable {
    fn from_config(cfg: &Config) -> Self {
        Self {
            injection: cfg.injection,
            keep_warm: cfg.keep_warm,
            audio_source: cfg.audio_source.clone().unwrap_or_default(),
            vocabulary: cfg.vocabulary.join("\n"),
            stt_model: cfg.stt.model.clone(),
            stt_endpoint: cfg.stt.endpoint.clone().unwrap_or_default(),
            stt_key: cfg.stt.api_key_id.clone().unwrap_or_default(),
            pp_enabled: cfg.postproc.enabled,
            pp_endpoint: cfg.postproc.endpoint.clone(),
            pp_model: cfg.postproc.model.clone(),
            pp_key: cfg.postproc.api_key_id.clone().unwrap_or_default(),
            pp_effort: cfg.postproc.reasoning_effort.clone(),
            pp_timeout: cfg.postproc.timeout_ms,
            pp_passes: cfg.postproc.passes,
            pp_min_chars: cfg.postproc.min_chars,
            pp_instructions: cfg.postproc.instructions.clone(),
            pp_decision_model: cfg.postproc.decision_model.clone(),
            pp_decision_endpoint: cfg.postproc.decision_endpoint.clone(),
            pp_decision_key: cfg.postproc.decision_api_key_id.clone(),
            telemetry: cfg.telemetry.clone(),
            hud: cfg.hud,
            handoff: cfg.handoff.clone(),
        }
    }

    fn to_config(&self) -> Config {
        Config {
            injection: self.injection,
            keep_warm: self.keep_warm,
            audio_source: non_empty(self.audio_source.trim()),
            vocabulary: vocabulary_terms(&self.vocabulary)
                .map(str::to_owned)
                .collect(),
            stt: SttConfig {
                model: self.stt_model.trim().to_owned(),
                endpoint: non_empty(self.stt_endpoint.trim()),
                api_key_id: non_empty(self.stt_key.trim()),
            },
            postproc: PostprocConfig {
                enabled: self.pp_enabled,
                endpoint: self.pp_endpoint.trim().to_owned(),
                model: self.pp_model.trim().to_owned(),
                api_key_id: non_empty(self.pp_key.trim()),
                reasoning_effort: self.pp_effort.clone(),
                timeout_ms: self.pp_timeout,
                passes: self.pp_passes,
                min_chars: self.pp_min_chars,
                instructions: self.pp_instructions.clone(),
                decision_model: self.pp_decision_model.clone(),
                decision_endpoint: self.pp_decision_endpoint.clone(),
                decision_api_key_id: self.pp_decision_key.clone(),
            },
            telemetry: self.telemetry.clone(),
            hud: self.hud,
            handoff: self.handoff.clone(),
            selected_handoff: None,
        }
    }
}

/// Vocabulary terms as saved: split on newlines and commas, trimmed, no empties.
fn vocabulary_terms(text: &str) -> impl Iterator<Item = &str> {
    text.split([',', '\n'])
        .map(str::trim)
        .filter(|term| !term.is_empty())
}

/// `Some(text)` for non-empty input, `None` for empty (an absent optional key).
fn non_empty(text: &str) -> Option<String> {
    if text.is_empty() {
        None
    } else {
        Some(text.to_owned())
    }
}

/// Wire form of `InjectionMode` (matches the `lowercase` serde rename).
fn injection_str(mode: InjectionMode) -> &'static str {
    match mode {
        InjectionMode::Auto => "auto",
        InjectionMode::Paste => "paste",
        InjectionMode::Type => "type",
        InjectionMode::Clipboard => "clipboard",
    }
}

/// One sentence per delivery mode (docs/CONFIGURATION.md, `injection`).
fn injection_sentence(mode: InjectionMode) -> &'static str {
    match mode {
        InjectionMode::Auto => {
            "Pastes into the focused app with Ctrl+Shift+V. Falls back to copying or typing only when nothing has been delivered yet."
        }
        InjectionMode::Paste => {
            "Copies to the clipboard and pastes with Ctrl+Shift+V. Never falls back to typing."
        }
        InjectionMode::Type => {
            "Types with a virtual keyboard and never touches the clipboard. Line breaks become spaces."
        }
        InjectionMode::Clipboard => {
            "Copies to the clipboard for you to paste. Sends no keys, and the clipboard is not restored afterwards."
        }
    }
}

/// `stt.model` is shared by both recognition modes but each accepts different
/// values (a local registry name vs. a provider model), so the window keeps a
/// draft for the mode that is not selected. Switching never leaves a model the
/// selected mode rejects, and switching back restores what was there.
struct SttDrafts {
    cloud: bool,
    local_model: String,
    cloud_model: String,
    cloud_endpoint: String,
}

impl SttDrafts {
    fn from_edit(edit: &Editable) -> Self {
        let cloud = !edit.stt_endpoint.trim().is_empty();
        Self {
            cloud,
            local_model: if cloud {
                SttConfig::default().model
            } else {
                edit.stt_model.clone()
            },
            cloud_model: if cloud {
                edit.stt_model.clone()
            } else {
                String::new()
            },
            cloud_endpoint: edit.stt_endpoint.clone(),
        }
    }

    fn select(&mut self, edit: &mut Editable, cloud: bool) {
        if cloud == self.cloud {
            return;
        }
        if cloud {
            self.local_model = std::mem::replace(&mut edit.stt_model, self.cloud_model.clone());
            edit.stt_endpoint = self.cloud_endpoint.clone();
        } else {
            self.cloud_model = std::mem::replace(&mut edit.stt_model, self.local_model.clone());
            self.cloud_endpoint = std::mem::take(&mut edit.stt_endpoint);
        }
        self.cloud = cloud;
    }
}

struct StatusMsg {
    text: String,
    ok: bool,
}

/// A configuration problem shown first, above every section.
struct Problem {
    title: &'static str,
    summary: &'static str,
    detail: String,
    /// The file cannot be parsed; offer the text repair flow.
    repair: bool,
}

impl Problem {
    fn needs_repair(detail: String) -> Self {
        Self {
            title: "Configuration needs repair",
            summary: "Cantrip can't use one of these values. Correct it below, then Save.",
            detail,
            repair: false,
        }
    }
}

fn completed_request<T>(receiver: Option<&Receiver<Result<T>>>) -> Option<Result<T>> {
    match receiver?.try_recv() {
        Ok(result) => Some(result),
        Err(TryRecvError::Empty) => None,
        Err(TryRecvError::Disconnected) => {
            Some(Err(anyhow!("The daemon request stopped unexpectedly")))
        }
    }
}

enum EditableConfigLoad {
    Ready {
        config: Box<Config>,
        text: String,
        warning: Option<StatusMsg>,
    },
    Blocked {
        problem: Problem,
    },
}

/// Load the real file for the structured editor without weakening the strict
/// `Config::load` path used by the daemon. Parsed values remain editable when
/// validation fails; unreadable or malformed TOML is never replaced.
fn load_editable_config(path: &Path) -> EditableConfigLoad {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
        Err(error) => {
            return EditableConfigLoad::Blocked {
                problem: Problem {
                    title: "This configuration can't be read",
                    summary: "Cantrip couldn't open the file, so nothing was changed. Check its ownership and permissions, then choose Reload.",
                    detail: error.to_string(),
                    repair: false,
                },
            };
        }
    };
    let config = match toml::from_str::<Config>(&text) {
        Ok(config) => config,
        Err(error) => {
            return EditableConfigLoad::Blocked {
                problem: Problem {
                    title: "This configuration can't be read",
                    summary: "The file isn't valid TOML, so its settings can't be shown here. Nothing was changed. Repair it as text; the original is backed up first.",
                    detail: error.to_string().trim_end().to_owned(),
                    repair: true,
                },
            };
        }
    };
    let warning = config.validate().err().map(|error| StatusMsg {
        text: format!("{error:#}"),
        ok: false,
    });
    EditableConfigLoad::Ready {
        config: Box::new(config),
        text,
        warning,
    }
}

/// Production HUD stills for the preview, rebuilt only when their inputs change.
struct HudPreview {
    labels: bool,
    palette: Palette,
    scale: u32,
    backdrop: egui::TextureHandle,
    stills: Vec<(&'static str, egui::TextureHandle, egui::Vec2)>,
}

const PREVIEW_STATES: [(ScreenshotState, &str); 3] = [
    (ScreenshotState::Recording, "Recording"),
    (ScreenshotState::Progress, "Transcribing"),
    (ScreenshotState::DeliveryFailed, "Delivery failed"),
];

impl HudPreview {
    fn build(ctx: &egui::Context, labels: bool, palette: Palette, scale: u32) -> Self {
        let tones = palette.tones();
        let top = theme::mix(tones.canvas, tones.well, 0.5);
        let bottom = theme::mix(tones.canvas, tones.accent, 0.06);
        let steps = 32;
        let backdrop = egui::ColorImage {
            size: [1, steps],
            pixels: (0..steps)
                .map(|step| color(theme::mix(top, bottom, step as f32 / (steps - 1) as f32)))
                .collect(),
        };
        let backdrop = ctx.load_texture(
            "settings-hud-backdrop",
            backdrop,
            egui::TextureOptions::LINEAR,
        );
        let stills = PREVIEW_STATES
            .iter()
            .filter_map(|(state, name)| {
                let image = gallery::still(*state, labels, palette, scale)?;
                let size = egui::vec2(image.size[0] as f32, image.size[1] as f32) / scale as f32;
                let texture = ctx.load_texture(
                    format!("settings-hud-{name}"),
                    image,
                    egui::TextureOptions::LINEAR,
                );
                Some((*name, texture, size))
            })
            .collect();
        Self {
            labels,
            palette,
            scale,
            backdrop,
            stills,
        }
    }
}

struct SettingsApp {
    edit: Editable,
    /// The values last loaded from or saved to disk; `edit != baseline` means
    /// there are unsaved changes.
    baseline: Editable,
    stt: SttDrafts,
    config_path: PathBuf,
    /// A load warning or parse failure, shown before every section.
    problem: Option<Problem>,
    /// Result of the last save, reload or repair, shown in the footer.
    status: Option<StatusMsg>,
    /// False when the file is unreadable or malformed; structured saving is
    /// disabled so defaults can never overwrite configuration we could not parse.
    loaded_ok: bool,
    /// Raw file text as loaded, used to refuse clobbering concurrent edits.
    loaded_text: String,
    daemon_online: bool,
    daemon_state: String,
    last_poll: Instant,
    poll_result: Option<Receiver<anyhow::Result<ipc::StatusSnapshot>>>,
    reload_result: Option<Receiver<anyhow::Result<ipc::CommandReply>>>,
    palette: theme::Palette,
    repair: Option<(String, String)>,
    preview: Option<HudPreview>,
    frames: u32,
    screenshot: Option<PathBuf>,
    screenshot_requested: bool,
    screenshot_deadline: Option<Instant>,
}

impl SettingsApp {
    fn new(
        cc: &eframe::CreationContext<'_>,
        screenshot: Option<PathBuf>,
        config_path: PathBuf,
    ) -> Self {
        let palette = theme::load();
        ui::setup(&cc.egui_ctx, palette);
        let (edit, loaded_ok, loaded_text, problem) = match load_editable_config(&config_path) {
            EditableConfigLoad::Ready {
                config,
                text,
                warning,
            } => (
                Editable::from_config(&config),
                true,
                text,
                warning.map(|warning| Problem::needs_repair(warning.text)),
            ),
            EditableConfigLoad::Blocked { problem } => (
                Editable::from_config(&Config::default()),
                false,
                String::new(),
                Some(problem),
            ),
        };
        Self {
            stt: SttDrafts::from_edit(&edit),
            baseline: edit.clone(),
            edit,
            config_path,
            problem,
            status: None,
            loaded_ok,
            loaded_text,
            daemon_online: false,
            daemon_state: "offline".to_owned(),
            last_poll: Instant::now() - DAEMON_POLL,
            poll_result: None,
            reload_result: None,
            palette,
            repair: None,
            preview: None,
            frames: 0,
            screenshot,
            screenshot_requested: false,
            screenshot_deadline: None,
        }
    }

    fn dirty(&self) -> bool {
        self.loaded_ok && self.edit != self.baseline
    }

    /// Adopt freshly loaded or saved values as the new clean state.
    fn set_clean(&mut self, edit: Editable) {
        self.stt = SttDrafts::from_edit(&edit);
        self.baseline = edit.clone();
        self.edit = edit;
    }

    fn show(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                centred_column(ui, |ui| {
                    ui.add_space(WINDOW_PAD);
                    self.form(ui);
                    ui.add_space(WINDOW_PAD);
                });
            });
    }

    fn form(&mut self, ui: &mut egui::Ui) {
        let tones = self.palette.tones();
        self.header(ui, &tones);
        ui.add_space(16.0);
        if self.repair.is_some() {
            self.repair_form(ui, &tones);
            return;
        }
        self.problem_card(ui, &tones);
        if !self.loaded_ok {
            return;
        }
        section(ui, &tones, |ui| self.stt_section(ui, &tones));
        section(ui, &tones, |ui| self.delivery_section(ui, &tones));
        section(ui, &tones, |ui| self.vocabulary_section(ui, &tones));
        section(ui, &tones, |ui| self.postproc_section(ui, &tones));
        section(ui, &tones, |ui| self.hud_section(ui, &tones));
        section(ui, &tones, |ui| self.microphone_section(ui, &tones));
    }

    fn header(&mut self, ui: &mut egui::Ui, tones: &Tones) {
        ui.horizontal(|ui| {
            ui::wordmark(ui, tones, "settings");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.daemon_chip(ui, tones);
            });
        });
        ui.add(
            egui::Label::new(
                ui::data(self.config_path.display().to_string(), tones)
                    .size(11.5)
                    .color(color(tones.text_faint)),
            )
            .truncate(),
        )
        .on_hover_text(self.config_path.display().to_string());
    }

    /// Live daemon state as a pill with a status dot.
    fn daemon_chip(&self, ui: &mut egui::Ui, tones: &Tones) {
        let (dot, rim, text) = if !self.daemon_online {
            (
                tones.attention,
                tones.attention_line,
                "Cantrip isn't running".to_owned(),
            )
        } else if self.daemon_state == "idle" {
            (
                tones.text_muted,
                tones.hairline_strong,
                "Cantrip is idle".to_owned(),
            )
        } else {
            (
                tones.accent,
                tones.accent_line,
                sentence_case(&self.daemon_state),
            )
        };
        let font = egui::FontId::new(12.5, crate::fonts::medium());
        let galley = ui
            .painter()
            .layout_no_wrap(text.clone(), font, color(tones.text));
        let size = egui::vec2(galley.size().x + 32.0, 26.0);
        let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Label, ui.is_enabled(), &text)
        });
        let painter = ui.painter();
        painter.rect(
            rect,
            egui::Rounding::same(rect.height() / 2.0),
            color(tones.panel),
            egui::Stroke::new(1.0_f32, color(rim)),
        );
        painter.circle_filled(rect.left_center() + egui::vec2(13.0, 0.0), 3.5, color(dot));
        painter.galley(
            egui::pos2(rect.left() + 22.0, rect.center().y - galley.size().y / 2.0),
            galley,
            color(tones.text),
        );
    }

    fn problem_card(&mut self, ui: &mut egui::Ui, tones: &Tones) {
        let Some(problem) = &self.problem else {
            return;
        };
        let mut open_repair = false;
        ui::attention_card(tones).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(ui::heading(problem.title, tones));
            ui.add_space(2.0);
            ui.label(ui::muted(problem.summary, tones));
            ui.add_space(4.0);
            ui::well(tones).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.add(egui::Label::new(ui::data(&problem.detail, tones)).wrap());
            });
            if problem.repair && !self.loaded_ok {
                ui.add_space(4.0);
                open_repair = ui
                    .add(ui::button(tones, Tone::Primary, "Repair configuration"))
                    .clicked();
            }
        });
        ui.add_space(12.0);
        if open_repair {
            match fs::read_to_string(&self.config_path) {
                Ok(text) => {
                    self.status = None;
                    self.repair = Some((text.clone(), text));
                }
                Err(_) => self.status = Some(StatusMsg {
                    text:
                        "Cannot read this file. Check its ownership and permissions before editing."
                            .to_owned(),
                    ok: false,
                }),
            }
        }
    }

    fn stt_section(&mut self, ui: &mut egui::Ui, tones: &Tones) {
        section_heading(ui, tones, "Speech recognition", |_| {});
        ui.label(ui::muted("Where your voice becomes text.", tones));
        ui.add_space(4.0);
        let mut cloud = self.stt.cloud;
        if ui::segmented(
            ui,
            tones,
            &mut cloud,
            &[(false, "On this computer"), (true, "Cloud provider")],
        ) {
            self.stt.select(&mut self.edit, cloud);
        }
        ui.add_space(8.0);
        if self.stt.cloud {
            ui.add(
                egui::Label::new(ui::faint(
                    "Audio is sent to this provider. If it fails, Cantrip transcribes the whole take again with the installed local Parakeet model.",
                    tones,
                ))
                .wrap(),
            );
            ui.add_space(4.0);
            row(ui, tones, "Model", "", |ui, width| {
                text_field(
                    ui,
                    tones,
                    &mut self.edit.stt_model,
                    "Provider model name",
                    width,
                );
            });
            row(
                ui,
                tones,
                "Endpoint",
                "API base URL; Cantrip adds /audio/transcriptions.",
                |ui, width| {
                    text_field(
                        ui,
                        tones,
                        &mut self.edit.stt_endpoint,
                        "https://…/v1",
                        width,
                    );
                },
            );
            row(
                ui,
                tones,
                "Keyring ID",
                "The key itself stays in your OS keyring.",
                |ui, width| {
                    text_field(
                        ui,
                        tones,
                        &mut self.edit.stt_key,
                        "Keyring entry name",
                        width,
                    );
                },
            );
        } else {
            let known = models::MODEL_NAMES.contains(&self.edit.stt_model.trim());
            row(
                ui,
                tones,
                "Model",
                "Runs on your CPU. Audio stays on this computer.",
                |ui, _| {
                    ui.with_layout(egui::Layout::top_down(egui::Align::Max), |ui| {
                        let ink = if known { tones.text } else { tones.attention };
                        ui.label(ui::data(&self.edit.stt_model, tones).color(color(ink)));
                        if !known {
                            let default = SttConfig::default().model;
                            ui.label(ui::faint("Not a local model name.", tones));
                            if ui
                                .add(ui::button(
                                    tones,
                                    Tone::Secondary,
                                    &format!("Use {default}"),
                                ))
                                .clicked()
                            {
                                self.edit.stt_model = default;
                            }
                        }
                    });
                },
            );
        }
        ui.add_space(4.0);
        separator(ui, tones);
        switch_row(
            ui,
            tones,
            &mut self.edit.keep_warm,
            "Keep the local model loaded",
            "Faster first transcription and cloud fallback. Applies after Cantrip restarts.",
        );
    }

    fn delivery_section(&mut self, ui: &mut egui::Ui, tones: &Tones) {
        section_heading(ui, tones, "Delivery", |_| {});
        ui.label(ui::muted(
            "How finished text reaches the app you are using.",
            tones,
        ));
        ui.add_space(4.0);
        ui::segmented(
            ui,
            tones,
            &mut self.edit.injection,
            &[
                (InjectionMode::Auto, "Auto"),
                (InjectionMode::Paste, "Paste"),
                (InjectionMode::Type, "Type"),
                (InjectionMode::Clipboard, "Clipboard"),
            ],
        );
        ui.add_space(2.0);
        ui.add(egui::Label::new(ui::faint(injection_sentence(self.edit.injection), tones)).wrap());
    }

    fn vocabulary_section(&mut self, ui: &mut egui::Ui, tones: &Tones) {
        let count = vocabulary_terms(&self.edit.vocabulary).count();
        section_heading(ui, tones, "Vocabulary", |ui| {
            let noun = if count == 1 { "term" } else { "terms" };
            ui.label(ui::faint(format!("{count} {noun}"), tones));
        });
        ui.label(ui::muted(
            "Exact spellings for names and jargon, given to cleanup and cloud recognition.",
            tones,
        ));
        ui.add_space(4.0);
        ui::well(tones)
            .inner_margin(egui::Margin::symmetric(10.0, 8.0))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                egui::ScrollArea::vertical()
                    .id_salt("vocabulary")
                    .max_height(176.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut self.edit.vocabulary)
                                .id_salt("vocabulary-text")
                                .frame(false)
                                .font(egui::TextStyle::Monospace)
                                .hint_text(ui::faint("One term per line", tones).italics())
                                .desired_rows(4)
                                .desired_width(f32::INFINITY),
                        );
                    });
            });
    }

    fn postproc_section(&mut self, ui: &mut egui::Ui, tones: &Tones) {
        section_heading(ui, tones, "Transcript cleanup", |ui| {
            ui::switch(ui, tones, &mut self.edit.pp_enabled, "Transcript cleanup");
        });
        if !self.edit.pp_enabled {
            ui.label(ui::muted("Transcripts are delivered as recognized.", tones));
            return;
        }
        ui.add(
            egui::Label::new(ui::muted(
                "A chat model tidies each transcript before delivery. Transcript text is sent to this provider; if cleanup fails, the raw transcript is delivered.",
                tones,
            ))
            .wrap(),
        );
        ui.add_space(4.0);
        row(
            ui,
            tones,
            "Endpoint",
            "OpenAI-compatible chat API.",
            |ui, width| {
                text_field(ui, tones, &mut self.edit.pp_endpoint, "https://…/v1", width);
            },
        );
        row(
            ui,
            tones,
            "Model",
            "A model this provider serves.",
            |ui, width| {
                text_field(
                    ui,
                    tones,
                    &mut self.edit.pp_model,
                    "Provider model name",
                    width,
                );
            },
        );
        row(
            ui,
            tones,
            "Keyring ID",
            "Leave empty when the provider needs no key.",
            |ui, width| {
                text_field(
                    ui,
                    tones,
                    &mut self.edit.pp_key,
                    "Keyring entry name",
                    width,
                );
            },
        );
        row(
            ui,
            tones,
            "Instructions",
            "Extra style guidance, added to the built-in cleanup rules.",
            |ui, width| {
                ui::well(tones)
                    .inner_margin(egui::Margin::symmetric(8.0, 6.0))
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut self.edit.pp_instructions)
                                .id_salt("cleanup-instructions")
                                .frame(false)
                                .hint_text(ui::faint("None", tones).italics())
                                .desired_rows(3)
                                .desired_width(width - 18.0),
                        );
                    });
            },
        );
        egui::CollapsingHeader::new(ui::muted("Advanced", tones))
            .id_salt("cleanup-advanced")
            .show(ui, |ui| {
                row(
                    ui,
                    tones,
                    "Timeout (ms)",
                    "After this long, the raw transcript is delivered.",
                    |ui, _| {
                        trailing(ui, |ui| {
                            ui.add(
                                egui::DragValue::new(&mut self.edit.pp_timeout)
                                    .speed(500)
                                    .range(1000..=120_000)
                                    .clamp_existing_to_range(false),
                            );
                        });
                    },
                );
                row(
                    ui,
                    tones,
                    "Passes",
                    "1 is one cleanup round; 2 adds a proofread pass.",
                    |ui, _| {
                        trailing(ui, |ui| {
                            ui.add(
                                egui::DragValue::new(&mut self.edit.pp_passes)
                                    .speed(0.1)
                                    .range(1..=3)
                                    .clamp_existing_to_range(false),
                            );
                        });
                    },
                );
                row(
                    ui,
                    tones,
                    "Minimum characters",
                    "Shorter transcripts skip cleanup. 0 never skips.",
                    |ui, _| {
                        trailing(ui, |ui| {
                            ui.add(
                                egui::DragValue::new(&mut self.edit.pp_min_chars)
                                    .speed(1.0)
                                    .range(0..=10_000)
                                    .clamp_existing_to_range(false),
                            );
                        });
                    },
                );
            });
    }

    fn hud_section(&mut self, ui: &mut egui::Ui, tones: &Tones) {
        section_heading(ui, tones, "HUD", |_| {});
        ui.label(ui::muted(
            "The indicator shown while Cantrip listens and works.",
            tones,
        ));
        ui.add_space(4.0);
        switch_row(
            ui,
            tones,
            &mut self.edit.hud.labels,
            "Always show state labels",
            "Routine stages are wordless; exceptions are always labelled.",
        );
        row(ui, tones, "Motion", "", |ui, _| {
            trailing(ui, |ui| {
                ui::segmented(
                    ui,
                    tones,
                    &mut self.edit.hud.reduced_motion,
                    &[
                        (None, "Follow desktop"),
                        (Some(true), "Reduced"),
                        (Some(false), "Full"),
                    ],
                );
            });
        });
        ui.add_space(4.0);
        self.hud_preview(ui, tones);
    }

    fn hud_preview(&mut self, ui: &mut egui::Ui, tones: &Tones) {
        let scale = if ui.ctx().pixels_per_point() > 1.0 {
            2
        } else {
            1
        };
        let labels = self.edit.hud.labels;
        let stale = self.preview.as_ref().is_none_or(|preview| {
            preview.labels != labels || preview.palette != self.palette || preview.scale != scale
        });
        if stale {
            self.preview = Some(HudPreview::build(ui.ctx(), labels, self.palette, scale));
        }
        let Some(preview) = &self.preview else {
            return;
        };
        let width = ui.available_width();
        let fit = preview
            .stills
            .iter()
            .map(|(_, _, size)| size.x)
            .fold(0.0_f32, f32::max);
        let shrink = if fit > 0.0 {
            (width / fit).min(1.0)
        } else {
            1.0
        };
        let caption_h = 16.0;
        let height: f32 = preview
            .stills
            .iter()
            .map(|(_, _, size)| size.y * shrink + caption_h)
            .sum::<f32>()
            + 20.0;
        let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        painter.add(egui::epaint::RectShape {
            fill_texture_id: preview.backdrop.id(),
            uv: egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            ..egui::epaint::RectShape::filled(
                rect,
                egui::Rounding::same(ui::CONTROL_RADIUS),
                egui::Color32::WHITE,
            )
        });
        painter.rect_stroke(
            rect,
            egui::Rounding::same(ui::CONTROL_RADIUS),
            egui::Stroke::new(1.0_f32, color(tones.hairline)),
        );
        let mut y = rect.top() + 10.0;
        for (name, texture, size) in &preview.stills {
            let size = *size * shrink;
            let image =
                egui::Rect::from_min_size(egui::pos2(rect.center().x - size.x / 2.0, y), size);
            painter.image(
                texture.id(),
                image,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
            y += size.y;
            painter.text(
                egui::pos2(rect.center().x, y),
                egui::Align2::CENTER_TOP,
                *name,
                egui::FontId::proportional(11.5),
                color(tones.text_faint),
            );
            y += caption_h;
        }
    }

    fn microphone_section(&mut self, ui: &mut egui::Ui, tones: &Tones) {
        section_heading(ui, tones, "Microphone", |_| {});
        ui.add_space(2.0);
        row(
            ui,
            tones,
            "Audio source",
            "PipeWire node name. Leave empty for the default input. Applies to the next recording.",
            |ui, width| {
                text_field(
                    ui,
                    tones,
                    &mut self.edit.audio_source,
                    "Default microphone",
                    width,
                );
            },
        );
    }

    /// Pinned save bar: unsaved/result state on the left, Revert and Save on the right.
    fn footer(&mut self, ui: &mut egui::Ui) {
        let tones = self.palette.tones();
        centred_column(ui, |ui| {
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.repair.is_some() {
                        if ui
                            .add_enabled(
                                self.reload_result.is_none(),
                                ui::button(&tones, Tone::Primary, "Save repaired configuration"),
                            )
                            .clicked()
                        {
                            self.save_repair();
                        }
                        if ui
                            .add(ui::button(&tones, Tone::Secondary, "Cancel repair"))
                            .clicked()
                        {
                            self.repair = None;
                        }
                    } else {
                        let dirty = self.dirty();
                        if self.loaded_ok
                            && ui
                                .add_enabled(
                                    dirty && self.reload_result.is_none(),
                                    ui::button(&tones, Tone::Primary, "Save"),
                                )
                                .clicked()
                        {
                            self.save();
                        }
                        let revert = if dirty { "Revert" } else { "Reload" };
                        if ui
                            .add_enabled(
                                self.reload_result.is_none(),
                                ui::button(&tones, Tone::Secondary, revert),
                            )
                            .on_hover_text("Load the file from disk again")
                            .clicked()
                        {
                            self.reload_from_disk();
                        }
                    }
                    ui.add_space(8.0);
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        self.footer_status(ui, &tones);
                    });
                });
            });
        });
    }

    fn footer_status(&self, ui: &mut egui::Ui, tones: &Tones) {
        let dirty = self.repair.is_none() && self.dirty();
        if dirty {
            let (dot, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
            ui.painter()
                .circle_filled(dot.center(), 4.0, color(tones.accent));
            ui.label(egui::RichText::new("Unsaved changes").color(color(tones.text)));
        }
        match &self.status {
            // A stale success message would contradict "Unsaved changes".
            Some(status) if !(dirty && status.ok) => {
                let ink = if status.ok {
                    tones.text_muted
                } else {
                    tones.attention
                };
                ui.add(
                    egui::Label::new(egui::RichText::new(&status.text).color(color(ink))).wrap(),
                );
            }
            None if !dirty && self.loaded_ok && self.repair.is_none() => {
                ui.label(ui::muted("No unsaved changes", tones));
            }
            _ => {}
        }
    }

    fn reload_from_disk(&mut self) {
        match load_editable_config(&self.config_path) {
            EditableConfigLoad::Ready {
                config,
                text,
                warning,
            } => {
                self.set_clean(Editable::from_config(&config));
                self.loaded_ok = true;
                self.loaded_text = text;
                self.problem = warning.map(|warning| Problem::needs_repair(warning.text));
                self.status = Some(StatusMsg {
                    text: "Loaded from disk".to_owned(),
                    ok: true,
                });
            }
            EditableConfigLoad::Blocked { problem } => {
                self.loaded_ok = false;
                self.problem = Some(problem);
                self.status = None;
            }
        }
    }

    fn save(&mut self) {
        if self.reload_result.is_some() {
            return;
        }
        if !self.loaded_ok {
            self.status = Some(StatusMsg {
                text: "Not saved: the file could not be loaded. Repair it or reload first."
                    .to_owned(),
                ok: false,
            });
            return;
        }
        let config = self.edit.to_config();
        if let Err(error) = config.validate() {
            self.status = Some(StatusMsg {
                text: format!("Not saved: {error:#}"),
                ok: false,
            });
            return;
        }
        match save_config_preserving(&self.config_path, &config, &self.loaded_text) {
            Ok(text) => {
                self.loaded_text = text;
                self.baseline = self.edit.clone();
                self.problem = None;
            }
            Err(error) => {
                self.status = Some(StatusMsg {
                    text: format!("Not saved: {error:#}"),
                    ok: false,
                });
                return;
            }
        }
        self.reload_daemon();
    }

    fn reload_daemon(&mut self) {
        self.status = Some(StatusMsg {
            text: "Saved. Applying…".to_owned(),
            ok: true,
        });
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(ipc::command(ipc::Command::Reload));
        });
        self.reload_result = Some(rx);
    }

    fn repair_form(&mut self, ui: &mut egui::Ui, tones: &Tones) {
        let Some((_, edited)) = &mut self.repair else {
            return;
        };
        ui::attention_card(tones).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(ui::heading("Repair configuration", tones));
            ui.add_space(2.0);
            ui.add(
                egui::Label::new(ui::muted(
                    "Edit the original text below. The file on disk is kept until the replacement parses and validates, and a backup is written before it is replaced.",
                    tones,
                ))
                .wrap(),
            );
            if let Some(problem) = &self.problem {
                // The parse location and reason; the caret diagram is redundant
                // next to the full text editor below.
                let reason = problem
                    .detail
                    .lines()
                    .map(str::trim_end)
                    .filter(|line| {
                        let line = line.trim_start();
                        !line.is_empty()
                            && !line.starts_with('|')
                            && !line.split_once(" |").is_some_and(|(number, _)| {
                                number.chars().all(|c| c.is_ascii_digit())
                            })
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                ui.add(
                    egui::Label::new(ui::data(reason, tones).color(color(tones.text_muted)))
                        .wrap(),
                );
            }
            ui.add_space(4.0);
            ui::well(tones).show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(edited)
                        .id_salt("repair-text")
                        .code_editor()
                        .frame(false)
                        .desired_rows(18)
                        .desired_width(f32::INFINITY),
                );
            });
        });
    }

    fn save_repair(&mut self) {
        let Some((original, edited)) = self.repair.as_ref() else {
            return;
        };
        let result = (|| -> Result<Config> {
            anyhow::ensure!(
                fs::read_to_string(&self.config_path)? == *original,
                "the file changed on disk. Cancel repair and open it again before saving"
            );
            let config: Config = toml::from_str(edited).context("TOML is not valid yet")?;
            config.validate()?;
            backup_config(&self.config_path, original)?;
            write_config_atomically(&self.config_path, edited)?;
            Ok(config)
        })();
        match result {
            Ok(config) => {
                self.loaded_text = edited.clone();
                self.set_clean(Editable::from_config(&config));
                self.loaded_ok = true;
                self.problem = None;
                self.repair = None;
                self.reload_daemon();
            }
            Err(error) => {
                self.status = Some(StatusMsg {
                    text: format!("Not saved: {error:#}"),
                    ok: false,
                })
            }
        }
    }
}

/// Lay `add` out in the centred, padded readable column.
fn centred_column(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let available = ui.available_width();
    let width = (available - WINDOW_PAD * 2.0).clamp(0.0, COLUMN_MAX);
    let side = ((available - width) * 0.5).max(0.0);
    ui.horizontal(|ui| {
        ui.add_space(side);
        ui.vertical(|ui| {
            ui.set_width(width);
            add(ui);
        });
    });
}

/// One settings card; cards sit 12 apart.
fn section(ui: &mut egui::Ui, tones: &Tones, add: impl FnOnce(&mut egui::Ui)) {
    ui::card(tones).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 6.0;
        add(ui);
    });
    ui.add_space(12.0);
}

/// Card title with an optional control or fact on the right.
fn section_heading(
    ui: &mut egui::Ui,
    tones: &Tones,
    title: &str,
    trailing_add: impl FnOnce(&mut egui::Ui),
) {
    ui.horizontal(|ui| {
        ui.spacing_mut().interact_size.y = 22.0;
        ui.label(ui::heading(title, tones));
        ui.with_layout(
            egui::Layout::right_to_left(egui::Align::Center),
            trailing_add,
        );
    });
}

/// A labelled setting: label and helper on the left, control on the right;
/// stacked when the card is narrow. `control` receives its available width.
fn row(
    ui: &mut egui::Ui,
    tones: &Tones,
    label: &str,
    helper: &str,
    control: impl FnOnce(&mut egui::Ui, f32),
) {
    let width = ui.available_width();
    ui.add_space(2.0);
    if width < ROW_STACK_BELOW {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            label_block(ui, tones, label, helper);
            control(ui, width);
        });
    } else {
        let gap = 16.0;
        let control_width = width - LABEL_WIDTH - gap;
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.vertical(|ui| {
                ui.set_width(LABEL_WIDTH);
                label_block(ui, tones, label, helper);
            });
            ui.add_space(gap);
            ui.vertical(|ui| {
                ui.set_width(control_width);
                ui.spacing_mut().item_spacing.x = 8.0;
                control(ui, control_width);
            });
        });
    }
    ui.add_space(2.0);
}

/// A boolean setting: the switch stays beside its label at every width.
fn switch_row(ui: &mut egui::Ui, tones: &Tones, on: &mut bool, label: &str, helper: &str) {
    let switch_width = 34.0;
    let gap = 16.0;
    let text_width = (ui.available_width() - switch_width - gap).max(0.0);
    ui.add_space(2.0);
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.vertical(|ui| {
            ui.set_width(text_width);
            label_block(ui, tones, label, helper);
        });
        ui.add_space(gap);
        ui::switch(ui, tones, on, label);
    });
    ui.add_space(2.0);
}

fn label_block(ui: &mut egui::Ui, tones: &Tones, label: &str, helper: &str) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.label(egui::RichText::new(label).color(color(tones.text)));
        if !helper.is_empty() {
            ui.add(egui::Label::new(ui::faint(helper, tones)).wrap());
        }
    });
}

/// Right-align a compact control (switch, stepper, segmented) in its row.
fn trailing(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), add);
}

/// A single-line value in the data face with a placeholder that can never be
/// mistaken for a value.
fn text_field(ui: &mut egui::Ui, tones: &Tones, text: &mut String, hint: &str, width: f32) {
    ui.add(
        egui::TextEdit::singleline(text)
            .font(egui::TextStyle::Monospace)
            .hint_text(ui::faint(hint, tones).italics())
            .margin(egui::vec2(8.0, 6.0))
            .desired_width(width),
    );
}

fn separator(ui: &mut egui::Ui, tones: &Tones) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter().hline(
        rect.x_range(),
        rect.center().y,
        egui::Stroke::new(1.0_f32, color(tones.hairline)),
    );
}

/// "transcribing 1/3" → "Transcribing 1/3".
fn sentence_case(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Write the edited config back to disk while preserving comments and ordering
/// for every key the window did not touch (so the annotated template survives).
fn save_config_preserving(path: &Path, config: &Config, expected: &str) -> Result<String> {
    let existing = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", path.display()));
        }
    };
    anyhow::ensure!(
        existing == expected,
        "the file changed on disk after it was opened, so nothing was written. Choose Revert to load it"
    );
    let mut doc: toml_edit::DocumentMut = existing
        .parse()
        .with_context(|| format!("parsing {}", path.display()))?;
    let root = doc.as_table_mut();

    set_preserving_decor(
        root,
        "injection",
        toml_edit::value(injection_str(config.injection)),
    );
    set_preserving_decor(root, "keep_warm", toml_edit::value(config.keep_warm));
    set_or_remove(root, "audio_source", config.audio_source.as_deref());
    let mut vocab = toml_edit::Array::new();
    for term in &config.vocabulary {
        vocab.push(term.as_str());
    }
    set_preserving_decor(root, "vocabulary", toml_edit::value(vocab));

    let hud = ensure_table(root, "hud")?;
    set_preserving_decor(hud, "labels", toml_edit::value(config.hud.labels));
    if let Some(reduced) = config.hud.reduced_motion {
        set_preserving_decor(hud, "reduced_motion", toml_edit::value(reduced));
    } else {
        hud.remove("reduced_motion");
    }

    let stt = ensure_table(root, "stt")?;
    set_preserving_decor(stt, "model", toml_edit::value(config.stt.model.clone()));
    set_or_remove(stt, "endpoint", config.stt.endpoint.as_deref());
    set_or_remove(stt, "api_key_id", config.stt.api_key_id.as_deref());

    let postproc = ensure_table(root, "postproc")?;
    set_preserving_decor(
        postproc,
        "enabled",
        toml_edit::value(config.postproc.enabled),
    );
    set_preserving_decor(
        postproc,
        "endpoint",
        toml_edit::value(config.postproc.endpoint.clone()),
    );
    set_preserving_decor(
        postproc,
        "model",
        toml_edit::value(config.postproc.model.clone()),
    );
    set_or_remove(
        postproc,
        "api_key_id",
        config.postproc.api_key_id.as_deref(),
    );
    set_or_remove(
        postproc,
        "reasoning_effort",
        config.postproc.reasoning_effort.as_deref(),
    );
    // Timeout is a small positive integer (ms); far below i64::MAX in practice.
    set_preserving_decor(
        postproc,
        "timeout_ms",
        toml_edit::value(config.postproc.timeout_ms as i64),
    );
    set_preserving_decor(
        postproc,
        "passes",
        toml_edit::value(config.postproc.passes as i64),
    );
    set_preserving_decor(
        postproc,
        "min_chars",
        toml_edit::value(config.postproc.min_chars as i64),
    );
    set_preserving_decor(
        postproc,
        "instructions",
        toml_edit::value(config.postproc.instructions.clone()),
    );

    let text = doc.to_string();
    write_config_atomically(path, &text)?;
    Ok(text)
}

fn write_config_atomically(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    // Atomic write: temp file in the same directory, then rename, so a crash
    // mid-write can never truncate the user's only config (models.rs convention).
    // The counter keeps concurrent saves from colliding on one temp path.
    let parent = path
        .parent()
        .context("config path has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let tmp = parent.join(format!(
        ".cantrip-config-{}-{}.tmp",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.with_context(|| format!("saving {}", path.display()))
}

fn backup_config(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let parent = path.parent().context("config path has no parent")?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let backup = parent.join(format!(
        "config.toml.before-repair-{stamp}-{}.bak",
        std::process::id()
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&backup)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Assign `item` to `key`, carrying over any decor (inline comment) the key
/// already has, so edited values keep their trailing annotations and column
/// alignment instead of being flattened.
fn set_preserving_decor(table: &mut toml_edit::Table, key: &str, item: toml_edit::Item) {
    let decor = table
        .get(key)
        .and_then(|old| old.as_value())
        .map(|value| value.decor().clone());
    let mut item = item;
    if let (Some(decor), Some(value)) = (decor, item.as_value_mut()) {
        *value.decor_mut() = decor;
    }
    table[key] = item;
}

/// Set `key` to `value`, or remove it entirely when the optional is absent.
fn set_or_remove(table: &mut toml_edit::Table, key: &str, value: Option<&str>) {
    match value {
        Some(value) => set_preserving_decor(table, key, toml_edit::value(value)),
        None => {
            table.remove(key);
        }
    }
}

fn ensure_table<'a>(root: &'a mut toml_edit::Table, key: &str) -> Result<&'a mut toml_edit::Table> {
    let item = root.entry(key).or_insert(toml_edit::table());
    item.as_table_mut()
        .with_context(|| format!("config key '{key}' is not a TOML table"))
}

/// Entry point for the `cantrip settings` subcommand.
pub fn run(screenshot: Option<PathBuf>) -> Result<()> {
    let config_path = paths::config_file().context("locating config file")?;
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_app_id("cantrip-settings")
            .with_inner_size([660.0, 800.0])
            .with_min_inner_size([460.0, 480.0])
            .with_maximized(false)
            .with_title("Cantrip Settings"),
        ..Default::default()
    };
    eframe::run_native(
        "cantrip-settings",
        options,
        Box::new(move |cc| Ok(Box::new(SettingsApp::new(cc, screenshot, config_path)))),
    )
    .map_err(|error| anyhow!("settings window error: {error}"))
}

/// Full app loop; also handles the `--screenshot` dump then closes.
#[allow(clippy::collapsible_if)]
impl eframe::App for SettingsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.frames += 1;
        if let Some(result) = completed_request(self.poll_result.as_ref()) {
            match result {
                Ok(status) => {
                    self.daemon_online = true;
                    self.daemon_state = status.stage.as_ref().map_or_else(
                        || {
                            if status.state == ipc::StateKind::Recording && status.signal.is_none()
                            {
                                "starting microphone".to_owned()
                            } else {
                                status.state_name().to_owned()
                            }
                        },
                        ToString::to_string,
                    );
                }
                Err(_) => {
                    self.daemon_online = false;
                    self.daemon_state = "unreachable".to_owned();
                }
            }
            self.poll_result = None;
        }
        if let Some(result) = completed_request(self.reload_result.as_ref()) {
            self.status = Some(match result {
                Ok(reply) if reply.ok => StatusMsg {
                    text: "Saved and applied".to_owned(),
                    ok: true,
                },
                Ok(reply) => StatusMsg {
                    text: format!(
                        "Saved, but Cantrip didn't apply it: {}",
                        reply
                            .error
                            .or(reply.message)
                            .unwrap_or_else(|| "no reason was given.".to_owned())
                    ),
                    ok: false,
                },
                Err(_) => StatusMsg {
                    text: "Saved. Cantrip isn't running; it uses these settings when it starts."
                        .to_owned(),
                    ok: true,
                },
            });
            self.reload_result = None;
        }
        if self.last_poll.elapsed() >= DAEMON_POLL && self.poll_result.is_none() {
            self.last_poll = Instant::now();
            let palette = theme::load();
            if palette != self.palette {
                self.palette = palette;
                ui::apply(ctx, palette);
            }
            let (tx, rx) = mpsc::channel();
            let context = ctx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(ipc::status());
                context.request_repaint();
            });
            self.poll_result = Some(rx);
        }
        if ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
            if self.repair.is_some() {
                self.repair = None;
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        ctx.request_repaint_after(if self.screenshot.is_some() {
            Duration::from_millis(30)
        } else {
            DAEMON_POLL
        });

        let tones = self.palette.tones();
        egui::TopBottomPanel::bottom("save-footer")
            .frame(
                egui::Frame::none()
                    .fill(color(tones.panel))
                    .inner_margin(egui::Margin::symmetric(0.0, 12.0)),
            )
            .show(ctx, |ui| self.footer(ui));
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(color(tones.canvas)))
            .show(ctx, |ui| self.show(ui));

        if self.screenshot.is_some()
            && !self.screenshot_requested
            && self.frames >= SCREENSHOT_DELAY_FRAMES
        {
            self.screenshot_requested = true;
            self.screenshot_deadline = Some(Instant::now() + Duration::from_secs(5));
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot);
        }
        if let (Some(path), Some(deadline)) = (&self.screenshot, self.screenshot_deadline) {
            if Instant::now() > deadline {
                eprintln!("screenshot timed out (no frame captured)");
                std::process::exit(1);
            }
            let mut shot = None;
            ctx.input_mut(|input| {
                for event in &input.events {
                    if let egui::Event::Screenshot { image, .. } = event {
                        shot = Some((**image).clone());
                    }
                }
            });
            if let Some(image) = shot {
                match image::save_buffer(
                    path,
                    image.as_raw(),
                    image.width() as u32,
                    image.height() as u32,
                    image::ColorType::Rgba8,
                )
                .with_context(|| format!("writing {}", path.display()))
                {
                    Ok(()) => eprintln!("saved screenshot to {}", path.display()),
                    Err(error) => {
                        eprintln!("screenshot save failed: {error:#}");
                        std::process::exit(1);
                    }
                }
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> Config {
        Config {
            injection: InjectionMode::Type,
            keep_warm: false,
            audio_source: Some("alsa_input.pci-0000_00_1f.3".to_owned()),
            vocabulary: vec!["PipeWire".to_owned(), "Parakeet".to_owned()],
            stt: SttConfig {
                model: "parakeet-tdt-0.6b-v3-int8".to_owned(),
                endpoint: None,
                api_key_id: None,
            },
            postproc: PostprocConfig {
                enabled: true,
                endpoint: "http://localhost:11434/v1".to_owned(),
                model: "qwen3:8b".to_owned(),
                api_key_id: None,
                reasoning_effort: Some("low".to_owned()),
                timeout_ms: 30_000,
                passes: 1,
                min_chars: 40,
                instructions: "Remove filler words.".to_owned(),
                decision_model: None,
                decision_endpoint: None,
                decision_api_key_id: None,
            },
            telemetry: TelemetryConfig::default(),
            hud: HudConfig {
                labels: true,
                reduced_motion: Some(true),
            },
            handoff: Default::default(),
            selected_handoff: None,
        }
    }

    #[test]
    fn empty_optional_fields_become_absent() {
        let edit = Editable {
            injection: InjectionMode::Auto,
            keep_warm: true,
            audio_source: String::new(),
            vocabulary: String::new(),
            stt_model: "parakeet-tdt-0.6b-v3-int8".to_owned(),
            stt_endpoint: String::new(),
            stt_key: "  ".to_owned(),
            pp_enabled: false,
            pp_endpoint: "http://localhost:11434/v1".to_owned(),
            pp_model: String::new(),
            pp_key: String::new(),
            pp_effort: None,
            pp_timeout: 10_000,
            pp_passes: 1,
            pp_min_chars: 40,
            pp_instructions: String::new(),
            pp_decision_model: None,
            pp_decision_endpoint: None,
            pp_decision_key: None,
            telemetry: TelemetryConfig::default(),
            hud: HudConfig::default(),
            handoff: Default::default(),
        };
        let config = edit.to_config();
        assert_eq!(config.audio_source, None);
        assert_eq!(config.stt.endpoint, None);
        assert_eq!(config.stt.api_key_id, None);
        assert_eq!(config.postproc.api_key_id, None);
        assert!(config.vocabulary.is_empty());
    }

    #[test]
    fn vocabulary_splits_on_commas_and_trims() {
        let mut edit = Editable::from_config(&sample_config());
        edit.vocabulary = "PipeWire, ,Canary,Parakeet".to_owned();
        assert_eq!(
            edit.to_config().vocabulary,
            vec!["PipeWire", "Canary", "Parakeet"]
        );
    }

    #[test]
    fn save_preserves_comments_and_applies_edits() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("cantrip-settings-test-{}.toml", std::process::id()));
        fs::write(
            &path,
            "# top-level comment\ninjection = \"auto\"  # trailing\n\n[stt]\nmodel = \"parakeet-tdt-0.6b-v3-int8\"\n\n[postproc]\nenabled = false\nmodel = \"\"\n",
        )
        .expect("write fixture");

        let config = Config {
            postproc: PostprocConfig {
                enabled: true,
                endpoint: "http://localhost:11434/v1".to_owned(),
                model: "qwen3:8b".to_owned(),
                ..sample_config().postproc
            },
            ..sample_config()
        };
        let original = fs::read_to_string(&path).expect("original config");
        save_config_preserving(&path, &config, &original).expect("save");
        let text = fs::read_to_string(&path).expect("read back");

        assert!(
            text.contains("# top-level comment"),
            "comment must survive: {text}"
        );
        assert!(
            text.contains("injection = \"type\""),
            "edited value present: {text}"
        );
        assert!(
            text.contains("# trailing"),
            "inline comment on an edited key must survive: {text}"
        );
        assert!(
            text.contains("enabled = true"),
            "postproc enabled edited: {text}"
        );
        assert!(
            text.contains("model = \"qwen3:8b\""),
            "postproc model edited: {text}"
        );

        // Round-trip: the saved file must parse back to the same config.
        let parsed: Config = toml::from_str(&text).expect("re-parse");
        assert!(parsed.postproc.enabled);
        assert_eq!(parsed.postproc.model, "qwen3:8b");
        assert_eq!(parsed.injection, InjectionMode::Type);

        fs::remove_file(&path).ok();
    }

    #[test]
    fn save_removes_optional_key_when_cleared() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "cantrip-settings-test2-{}.toml",
            std::process::id()
        ));
        fs::write(
            &path,
            "[stt]\nmodel = \"parakeet-tdt-0.6b-v3-int8\"\nendpoint = \"https://api.xyz/v1\"\napi_key_id = \"abc\"\n",
        )
        .expect("write fixture");

        let config = Config {
            stt: SttConfig {
                model: "parakeet-tdt-0.6b-v3-int8".to_owned(),
                endpoint: None,
                api_key_id: None,
            },
            ..sample_config()
        };
        let original = fs::read_to_string(&path).expect("original config");
        save_config_preserving(&path, &config, &original).expect("save");
        let text = fs::read_to_string(&path).expect("read back");
        // The [stt] endpoint/api_key_id must be gone; postproc.endpoint is
        // unrelated and legitimately still present.
        let parsed: Config = toml::from_str(&text).expect("re-parse");
        assert_eq!(parsed.stt.endpoint, None, "stt endpoint removed: {text}");
        assert_eq!(
            parsed.stt.api_key_id, None,
            "stt api_key_id removed: {text}"
        );

        fs::remove_file(&path).ok();
    }

    #[test]
    fn saver_rejects_an_unparseable_existing_file() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "cantrip-settings-test3-{}.toml",
            std::process::id()
        ));
        fs::write(&path, "this is [ not toml").expect("write fixture");
        assert!(save_config_preserving(&path, &sample_config(), "this is [ not toml").is_err());
        fs::remove_file(&path).ok();
    }

    #[test]
    fn parsed_invalid_config_keeps_real_values_editable() {
        let path = std::env::temp_dir().join(format!(
            "cantrip-settings-invalid-{}.toml",
            std::process::id()
        ));
        let text = "injection = \"type\"\n[stt]\nmodel = \"retired-model\"\n";
        fs::write(&path, text).expect("write fixture");

        match load_editable_config(&path) {
            EditableConfigLoad::Ready {
                config,
                text: loaded_text,
                warning: Some(warning),
            } => {
                assert_eq!(config.injection, InjectionMode::Type);
                assert_eq!(config.stt.model, "retired-model");
                assert_eq!(loaded_text, text);
                assert!(!warning.ok);
            }
            _ => panic!("parsed validation failure must remain editable"),
        }

        fs::remove_file(path).ok();
    }

    #[test]
    fn malformed_config_is_blocked_without_changing_file() {
        let path = std::env::temp_dir().join(format!(
            "cantrip-settings-malformed-{}.toml",
            std::process::id()
        ));
        let text = b"injection = [not valid TOML";
        fs::write(&path, text).expect("write fixture");

        match load_editable_config(&path) {
            EditableConfigLoad::Blocked { .. } => {}
            EditableConfigLoad::Ready { .. } => {
                panic!("malformed config must disable structured saving")
            }
        }
        assert_eq!(fs::read(&path).expect("read fixture"), text);

        fs::remove_file(path).ok();
    }

    #[test]
    fn corrected_validation_failure_saves_and_reloads() {
        let path = std::env::temp_dir().join(format!(
            "cantrip-settings-repair-{}.toml",
            std::process::id()
        ));
        fs::write(
            &path,
            "# keep me\ninjection = \"type\"\n[stt]\nmodel = \"retired-model\"\n",
        )
        .expect("write fixture");

        let mut config = match load_editable_config(&path) {
            EditableConfigLoad::Ready {
                config,
                warning: Some(_),
                ..
            } => config,
            _ => panic!("fixture must be a parsed validation failure"),
        };
        config.stt.model = "parakeet-tdt-0.6b-v3-int8".to_owned();
        config.validate().expect("correction must validate");
        let original = fs::read_to_string(&path).expect("original config");
        save_config_preserving(&path, &config, &original).expect("save corrected config");

        match load_editable_config(&path) {
            EditableConfigLoad::Ready {
                config,
                warning: None,
                ..
            } => {
                assert_eq!(config.injection, InjectionMode::Type);
                assert_eq!(config.stt.model, "parakeet-tdt-0.6b-v3-int8");
            }
            _ => panic!("corrected config must reload without a warning"),
        }
        assert!(
            fs::read_to_string(&path)
                .expect("read corrected config")
                .contains("# keep me"),
            "repair must preserve existing comments"
        );

        fs::remove_file(path).ok();
    }

    #[test]
    fn following_desktop_removes_motion_override_without_losing_unknown_preferences() {
        let path = std::env::temp_dir().join(format!(
            "cantrip-settings-motion-{}.toml",
            std::process::id()
        ));
        fs::write(&path, "[hud]\nlabels = true\nreduced_motion = true\n# keep personal preference\npersonal_scale = 2\n").expect("fixture");
        let mut config: Config =
            toml::from_str(&fs::read_to_string(&path).expect("read")).expect("parse");
        config.hud.reduced_motion = None;
        let original = fs::read_to_string(&path).expect("original config");
        save_config_preserving(&path, &config, &original).expect("save desktop preference");
        let saved = fs::read_to_string(&path).expect("saved");
        let parsed: Config = toml::from_str(&saved).expect("reparse");
        assert_eq!(parsed.hud.reduced_motion, None);
        assert!(parsed.hud.labels);
        let document: toml::Value = toml::from_str(&saved).expect("document");
        assert_eq!(document["hud"]["personal_scale"].as_integer(), Some(2));
        assert!(saved.contains("# keep personal preference"));
        fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn saving_refuses_to_adopt_concurrent_disk_edits() {
        let path = std::env::temp_dir().join(format!(
            "cantrip-settings-concurrent-{}.toml",
            std::process::id()
        ));
        let original = "injection = \"auto\"\n";
        let changed = "injection = \"clipboard\"\n# external edit\n";
        fs::write(&path, changed).expect("external edit");
        assert!(save_config_preserving(&path, &sample_config(), original).is_err());
        assert_eq!(
            fs::read_to_string(&path).expect("preserved config"),
            changed
        );
        fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn vocabulary_shown_one_per_line_round_trips_unchanged() {
        let config = sample_config();
        let mut edit = Editable::from_config(&config);
        assert_eq!(edit.to_config().vocabulary, config.vocabulary);
        edit.vocabulary
            .push_str("\n  Omarchy  \n\nHyprland, Wayland\n");
        assert_eq!(
            edit.to_config().vocabulary,
            vec!["PipeWire", "Parakeet", "Omarchy", "Hyprland", "Wayland"]
        );
    }

    #[test]
    fn switching_recognition_mode_keeps_each_mode_valid_and_restores_drafts() {
        let mut config = sample_config();
        config.stt = SttConfig {
            model: "provider/transcribe".to_owned(),
            endpoint: Some("https://api.example.com/v1".to_owned()),
            api_key_id: Some("example".to_owned()),
        };
        let mut edit = Editable::from_config(&config);
        let loaded = edit.clone();
        let mut drafts = SttDrafts::from_edit(&edit);
        assert!(drafts.cloud);

        drafts.select(&mut edit, false);
        let local = edit.to_config();
        assert_eq!(local.stt.endpoint, None);
        assert_eq!(local.stt.model, SttConfig::default().model);
        local
            .validate()
            .expect("local mode must save a known local model");

        drafts.select(&mut edit, true);
        assert!(
            edit == loaded,
            "returning to cloud restores the cloud values"
        );
        edit.to_config().validate().expect("restored cloud config");
    }

    #[test]
    fn invalid_cleanup_passes_stay_invalid_until_explicitly_corrected() {
        let mut config = sample_config();
        config.postproc.passes = 0;
        let mut edit = Editable::from_config(&config);
        assert!(edit.to_config().validate().is_err());
        edit.pp_passes = 1;
        assert!(edit.to_config().validate().is_ok());
    }
}
