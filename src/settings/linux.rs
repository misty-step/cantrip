//! The Linux Settings window: the shared form and transcript history in egui
//! (eframe/glow), themed from the active desktop palette. The header polls the
//! live dictation state every second; the audio source is a PipeWire source
//! name and shortcuts belong to the compositor.

use super::model::{
    self, CopyFailure, CopyPhase, PastTranscript, SettingsForm, TranscriptHistory,
    INJECTION_CHOICES, MOTION_CHOICES,
};
use super::store::injection_str;
use super::{apply_theme, color};
use crate::theme;
use anyhow::{anyhow, Context, Result};
use cantrip_engine::{ipc, paths};
use eframe::egui;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

/// Frames to render before taking the `--screenshot` (lets layout settle).
const SCREENSHOT_DELAY_FRAMES: u32 = 6;
/// How often to refresh the live daemon state in the header.
const DAEMON_POLL: Duration = Duration::from_secs(1);
/// Upper bound on frames a `--screenshot` waits for the history list.
const SCREENSHOT_MAX_FRAMES: u32 = 100;
/// Complete rows visible in the past-transcript list. Odd, so centering a
/// focused Copy button leaves whole rows above and below it.
const HISTORY_VISIBLE_ROWS: f32 = 5.0;
/// Vertical padding inside each past-transcript row.
const ROW_PAD: f32 = 6.0;
/// Gap between a row's capture details and its opening words.
const ROW_LINE_GAP: f32 = 2.0;

/// The live dictation state as the header names it: the engine's stage, or
/// the state itself, with a recording that has no signal yet still starting.
fn dictation_state(status: &ipc::StatusSnapshot) -> String {
    status.stage.as_ref().map_or_else(
        || {
            if status.state == ipc::StateKind::Recording && status.signal.is_none() {
                "starting microphone".to_owned()
            } else {
                status.state_name().to_owned()
            }
        },
        ToString::to_string,
    )
}

struct SettingsApp {
    form: SettingsForm,
    daemon_online: bool,
    daemon_state: String,
    last_poll: Instant,
    poll_result: Option<Receiver<Result<ipc::StatusSnapshot>>>,
    reload_result: Option<Receiver<Result<ipc::CommandReply>>>,
    /// Past transcripts; loaded and copied off the UI thread.
    history: TranscriptHistory,
    history_load: Option<Receiver<Result<Vec<PastTranscript>>>>,
    copy: Option<Receiver<Result<(), CopyFailure>>>,
    /// The focused Copy button that started a request; focus returns to it.
    copy_focus: Option<egui::Id>,
    palette: theme::Palette,
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
        apply_theme(&cc.egui_ctx, palette);
        let mut app = Self {
            form: SettingsForm::load(config_path),
            daemon_online: false,
            daemon_state: "offline".to_owned(),
            last_poll: Instant::now() - DAEMON_POLL,
            poll_result: None,
            reload_result: None,
            history: TranscriptHistory::default(),
            history_load: None,
            copy: None,
            copy_focus: None,
            palette,
            frames: 0,
            screenshot,
            screenshot_requested: false,
            screenshot_deadline: None,
        };
        app.refresh_history(&cc.egui_ctx);
        app
    }

    /// Maximum content width; the form is centered when the window is wider
    /// (e.g. tiled or fullscreen), so it never stretches or clusters left.
    const MAX_W: f32 = 620.0;

    /// Start a history load unless one is running; listed rows stay visible meanwhile.
    fn refresh_history(&mut self, ctx: &egui::Context) {
        if !self.history.start_load() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let context = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(model::load_transcripts());
            context.request_repaint();
        });
        self.history_load = Some(rx);
    }

    /// Copy one listed transcript. The worker gets only its ID, never the preview.
    fn start_copy(&mut self, index: usize, focus: Option<egui::Id>, ctx: &egui::Context) {
        let Some(id) = self.history.start_copy(index) else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        let context = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(model::copy_transcript(&id));
            context.request_repaint();
        });
        self.copy = Some(rx);
        self.copy_focus = focus;
    }

    /// Apply finished history work; called every frame before drawing.
    fn poll_history(&mut self, ctx: &egui::Context) {
        if let Some(result) = model::completed(self.history_load.as_ref()) {
            self.history_load = None;
            self.history.loaded(result);
        }
        let finished = match self.copy.as_ref().map(Receiver::try_recv) {
            Some(Ok(result)) => Some(result),
            Some(Err(TryRecvError::Disconnected)) => Some(Err(TranscriptHistory::copy_lost())),
            _ => None,
        };
        let Some(result) = finished else {
            return;
        };
        self.copy = None;
        self.history.copied(result);
        if let Some(id) = self.copy_focus.take() {
            ctx.memory_mut(|memory| memory.request_focus(id));
        }
    }

    fn show(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let width = ui.available_width().min(Self::MAX_W);
                let side = ((ui.available_width() - width) * 0.5).max(0.0);
                ui.horizontal(|ui| {
                    ui.add_space(side);
                    ui.vertical(|ui| {
                        ui.set_width(width);
                        self.form(ui);
                    });
                });
            });
    }

    fn form(&mut self, ui: &mut egui::Ui) {
        self.header(ui);
        ui.add_space(2.0);
        if let Some(notice) = &self.form.notice {
            let color = color(if notice.ok {
                self.palette.foreground
            } else {
                self.palette.attention
            });
            ui.colored_label(color, &notice.text);
        }
        if self.form.needs_repair() && ui.button("Repair configuration").clicked() {
            self.form.start_repair();
        }
        if self.form.repairing() {
            self.repair_form(ui);
            return;
        }
        ui.add_space(8.0);

        Self::section(
            ui,
            "Past transcripts",
            "Saved on this computer, newest first. Copy replaces your clipboard.",
            |ui| {
                self.history_section(ui);
            },
        );
        Self::section(
            ui,
            "General",
            "Injection, warm-up, audio, vocabulary",
            |ui| {
                self.general_section(ui);
            },
        );
        Self::section(ui, "HUD", "Quiet by default; captions when needed", |ui| {
            let hud = &mut self.form.edit.hud;
            ui.checkbox(&mut hud.labels, "Always show state labels");
            egui::ComboBox::from_id_salt("reduced-motion")
                .selected_text(match hud.reduced_motion {
                    None => "Motion: follow system",
                    Some(true) => "Motion: reduced",
                    Some(false) => "Motion: normal",
                })
                .show_ui(ui, |ui| {
                    for (choice, label) in MOTION_CHOICES {
                        ui.selectable_value(&mut hud.reduced_motion, choice, label);
                    }
                });
            ui.label(egui::RichText::new("Important exceptions are always labelled.").weak());
        });
        Self::section(
            ui,
            "Transcription",
            "Speech-to-text model and endpoint",
            |ui| {
                self.stt_section(ui);
            },
        );
        Self::section(
            ui,
            "Transcript cleanup",
            "Post-processing behavior and model",
            |ui| {
                self.postproc_section(ui);
            },
        );

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let save = egui::Button::new(
                egui::RichText::new("Save & apply")
                    .strong()
                    .color(color(self.palette.background)),
            )
            .fill(color(self.palette.accent));
            if ui.add_enabled(self.form.can_save(), save).clicked() {
                self.save();
            }
            let reload = egui::Button::new("Reload from disk")
                .fill(color(self.palette.surface))
                .stroke(egui::Stroke::new(1.0_f32, color(self.palette.border)));
            if ui.add_enabled(!self.form.applying(), reload).clicked() {
                self.form.reload_from_disk();
            }
        });
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Cantrip")
                    .strong()
                    .size(21.0)
                    .color(color(self.palette.foreground)),
            );
            ui.label(egui::RichText::new("Settings").size(21.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.daemon_badge(ui);
            });
        });
        ui.label(
            egui::RichText::new(format!("Config: {}", self.form.path().display()))
                .weak()
                .small(),
        );
    }

    fn section(ui: &mut egui::Ui, title: &str, hint: &str, add: impl FnOnce(&mut egui::Ui)) {
        ui.label(egui::RichText::new(title).strong().size(16.0));
        ui.label(egui::RichText::new(hint).weak().small());
        ui.add_space(5.0);
        egui::Frame::group(ui.style())
            .fill(ui.visuals().faint_bg_color)
            .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
            .rounding(egui::Rounding::ZERO)
            .inner_margin(egui::Margin::symmetric(12.0, 10.0))
            .show(ui, add);
        ui.add_space(14.0);
    }

    /// Live daemon state as a small rounded chip with a status dot.
    fn daemon_badge(&mut self, ui: &mut egui::Ui) {
        let (ink, text) = if !self.daemon_online {
            (
                color(self.palette.foreground),
                "Cantrip unreachable".to_owned(),
            )
        } else {
            match self.daemon_state.as_str() {
                "idle" => (color(self.palette.foreground), "dictation: idle".to_owned()),
                other => (color(self.palette.accent), format!("dictation: {other}")),
            }
        };
        let text_width = ui.fonts(|fonts| {
            fonts
                .layout_no_wrap(text.clone(), egui::FontId::proportional(12.0), ink)
                .size()
                .x
        }) + 28.0;
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(text_width, 24.0), egui::Sense::hover());
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Label, ui.is_enabled(), &text)
        });
        let painter = ui.painter();
        painter.rect(
            rect,
            egui::Rounding::ZERO,
            color(self.palette.surface),
            egui::Stroke::new(1.0_f32, ink.linear_multiply(0.4)),
        );
        painter.circle_filled(rect.left_center() + egui::vec2(10.0, 0.0), 3.5, ink);
        painter.text(
            rect.left_center() + egui::vec2(20.0, 0.0),
            egui::Align2::LEFT_CENTER,
            text,
            egui::FontId::proportional(12.0),
            color(self.palette.foreground),
        );
    }

    /// Count and Refresh, a bounded newest-first list, then the latest Copy
    /// result. Shown whatever state the config file or daemon is in.
    fn history_section(&mut self, ui: &mut egui::Ui) {
        let loading = self.history.loading();
        let mut refresh = false;
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(self.history.summary()).weak());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                refresh = ui
                    .add_enabled(!loading, egui::Button::new("Refresh"))
                    .clicked();
            });
        });
        if let Some(error) = self.history.error() {
            ui.colored_label(color(self.palette.attention), error);
        } else if let Some(hint) = self.history.empty_hint() {
            ui.label(egui::RichText::new(hint).weak().small());
        }
        let mut request = None;
        if !self.history.rows().is_empty() {
            egui::Frame::none()
                .fill(ui.visuals().extreme_bg_color)
                .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
                .inner_margin(egui::Margin::symmetric(8.0, 0.0))
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.spacing_mut().scroll = egui::style::ScrollStyle::solid();
                    // Clip at the list edge, so rows never paint over its border and
                    // centering a focused Copy measures the real viewport.
                    ui.visuals_mut().clip_rect_margin = 0.0;
                    let row_height = transcript_row_height(ui);
                    // The form reports no spare height (it sits inside a horizontal
                    // row), so max_height alone collapses to egui's 64-point minimum.
                    let height = row_height * HISTORY_VISIBLE_ROWS;
                    egui::ScrollArea::vertical()
                        .id_salt("past-transcripts")
                        .min_scrolled_height(height)
                        .max_height(height)
                        .auto_shrink([false, true])
                        // Centering the first or last rows aims past either end; jump
                        // to the clamped offset instead of animating rows out of place.
                        .animated(false)
                        .show_rows(ui, row_height, self.history.rows().len(), |ui, range| {
                            request = visible_rows(&self.history, ui, range, self.palette);
                        });
                });
        }
        if let Some(status) = self.history.copy_status() {
            let ink = match status.phase {
                CopyPhase::Copying | CopyPhase::Copied => self.palette.foreground,
                CopyPhase::NotCopied | CopyPhase::Uncertain => self.palette.attention,
            };
            ui.colored_label(color(ink), &status.message);
        }
        if let Some((index, focus)) = request {
            self.start_copy(index, focus, ui.ctx());
        }
        if refresh {
            self.refresh_history(ui.ctx());
        }
    }

    fn general_section(&mut self, ui: &mut egui::Ui) {
        let edit = &mut self.form.edit;
        egui::Grid::new("general")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                ui.label(egui::RichText::new("Injection").weak());
                // Linux offers every mode; its guard verifies each take at delivery.
                egui::ComboBox::from_id_salt("injection")
                    .selected_text(injection_str(edit.injection))
                    .show_ui(ui, |ui| {
                        for (mode, label) in INJECTION_CHOICES {
                            ui.selectable_value(&mut edit.injection, mode, label);
                        }
                    });
                ui.end_row();

                ui.checkbox(
                    &mut edit.keep_warm,
                    "Keep STT model warm (faster dictation)",
                );
                ui.end_row();

                // A PipeWire source name; compositor bindings own the shortcut.
                ui.label(egui::RichText::new("Audio source (empty = default)").weak());
                ui.add(
                    egui::TextEdit::singleline(&mut edit.audio_source)
                        .hint_text("e.g. alsa_input…")
                        .desired_width(f32::INFINITY),
                );
                ui.end_row();

                ui.label(egui::RichText::new("Vocabulary (comma-separated)").weak());
                ui.add(
                    egui::TextEdit::singleline(&mut edit.vocabulary)
                        .hint_text("PipeWire, Parakeet")
                        .desired_width(f32::INFINITY),
                );
                ui.end_row();
            });
    }

    fn stt_section(&mut self, ui: &mut egui::Ui) {
        let edit = &mut self.form.edit;
        egui::Grid::new("stt")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                ui.label(egui::RichText::new("Model (empty endpoint = local)").weak());
                ui.add(
                    egui::TextEdit::singleline(&mut edit.stt_model).desired_width(f32::INFINITY),
                );
                ui.end_row();

                ui.label(egui::RichText::new("Endpoint (empty = local STT)").weak());
                ui.add(
                    egui::TextEdit::singleline(&mut edit.stt_endpoint)
                        .hint_text("https://api.openai.com/v1")
                        .desired_width(f32::INFINITY),
                );
                ui.end_row();

                ui.label(egui::RichText::new("API key id (keyring)").weak());
                ui.add(
                    egui::TextEdit::singleline(&mut edit.stt_key)
                        .hint_text("openai")
                        .desired_width(f32::INFINITY),
                );
                ui.end_row();
            });
    }

    fn postproc_section(&mut self, ui: &mut egui::Ui) {
        let edit = &mut self.form.edit;
        ui.checkbox(&mut edit.pp_enabled, "Clean up the transcript");
        ui.add_space(4.0);
        egui::Grid::new("postproc")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                ui.label(egui::RichText::new("Endpoint").weak());
                ui.add(
                    egui::TextEdit::singleline(&mut edit.pp_endpoint).desired_width(f32::INFINITY),
                );
                ui.end_row();

                ui.label(egui::RichText::new("Model").weak());
                ui.add(egui::TextEdit::singleline(&mut edit.pp_model).desired_width(f32::INFINITY));
                ui.end_row();

                ui.label(egui::RichText::new("API key id (keyring)").weak());
                ui.add(
                    egui::TextEdit::singleline(&mut edit.pp_key)
                        .hint_text("openrouter")
                        .desired_width(f32::INFINITY),
                );
                ui.end_row();

                ui.label(egui::RichText::new("Timeout (ms)").weak());
                ui.add(
                    egui::DragValue::new(&mut edit.pp_timeout)
                        .speed(500)
                        .range(1000..=120_000)
                        .clamp_existing_to_range(false),
                );
                ui.end_row();

                ui.label(egui::RichText::new("Cleanup passes").weak());
                ui.horizontal(|ui| {
                    ui.add(
                        egui::DragValue::new(&mut edit.pp_passes)
                            .speed(0.1)
                            .range(1..=3)
                            .clamp_existing_to_range(false),
                    );
                    ui.label(
                        egui::RichText::new("1 = one cleanup round; 2 adds a proofread pass")
                            .weak()
                            .small(),
                    );
                });
                ui.end_row();

                ui.label(egui::RichText::new("Min chars").weak());
                ui.horizontal(|ui| {
                    ui.add(
                        egui::DragValue::new(&mut edit.pp_min_chars)
                            .speed(1.0)
                            .range(0..=10_000)
                            .clamp_existing_to_range(false),
                    );
                    ui.label(
                        egui::RichText::new("skip cleanup under this length (0 = never skip)")
                            .weak()
                            .small(),
                    );
                });
                ui.end_row();
            });
        ui.label(egui::RichText::new("Instructions (the cleanup behavior)").weak());
        ui.add(
            egui::TextEdit::multiline(&mut edit.pp_instructions)
                .desired_rows(4)
                .desired_width(f32::INFINITY),
        );
    }

    fn save(&mut self) {
        if self.form.save().is_some() {
            self.apply();
        }
    }

    /// Ask the running Cantrip to apply the saved file, off the UI thread.
    fn apply(&mut self) {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(ipc::command(ipc::Command::Reload));
        });
        self.reload_result = Some(rx);
    }

    fn repair_form(&mut self, ui: &mut egui::Ui) {
        let applying = self.form.applying();
        let Some(edited) = self.form.repair_text() else {
            return;
        };
        ui.label("Repair the TOML below. The existing file is kept until the replacement parses and validates.");
        ui.add(
            egui::TextEdit::multiline(edited)
                .code_editor()
                .desired_rows(18)
                .desired_width(f32::INFINITY),
        );
        let mut save = false;
        let mut cancel = false;
        ui.horizontal(|ui| {
            save = ui
                .add_enabled(!applying, egui::Button::new("Save repaired configuration"))
                .clicked();
            cancel = ui.button("Cancel repair").clicked();
        });
        if save && self.form.save_repair().is_some() {
            self.apply();
        }
        if cancel {
            self.form.cancel_repair();
        }
    }
}

/// Lay out the rows in `range`. Returns the row whose Copy was chosen, with
/// the button's id when it had keyboard focus.
fn visible_rows(
    history: &TranscriptHistory,
    ui: &mut egui::Ui,
    range: std::ops::Range<usize>,
    palette: theme::Palette,
) -> Option<(usize, Option<egui::Id>)> {
    let rows = history.rows();
    let mut request = None;
    for index in range {
        let row = &rows[index];
        let separator = index + 1 < rows.len();
        // show_rows skips one auto id per row and push_id uses exactly one,
        // so widget ids (and keyboard focus) stay stable while scrolling.
        let copy = ui
            .push_id(&row.id, |ui| {
                transcript_row(
                    ui,
                    row,
                    palette,
                    history.can_copy(),
                    history.phase(&row.id),
                    separator,
                )
            })
            .inner;
        if copy.clicked() {
            request = Some((index, copy.has_focus().then_some(copy.id)));
        }
    }
    request
}

/// Two text lines or the Copy button, whichever is taller, plus padding.
fn transcript_row_height(ui: &egui::Ui) -> f32 {
    let line = ui.text_style_height(&egui::TextStyle::Body);
    let button =
        ui.text_style_height(&egui::TextStyle::Button) + 2.0 * ui.spacing().button_padding.y;
    (2.0 * line + ROW_LINE_GAP).max(button) + 2.0 * ROW_PAD
}

/// One fixed-height row: capture time and duration over the opening words, and
/// the latest Copy result for this ID beside its Copy button.
fn transcript_row(
    ui: &mut egui::Ui,
    row: &PastTranscript,
    palette: theme::Palette,
    enabled: bool,
    phase: Option<CopyPhase>,
    separator: bool,
) -> egui::Response {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), transcript_row_height(ui)),
        egui::Sense::hover(),
    );
    // A row ending at the list's lower edge is closed by the list border.
    if separator && rect.bottom() < ui.clip_rect().bottom() - 1.0 {
        let stroke = ui.visuals().widgets.noninteractive.bg_stroke;
        ui.painter()
            .hline(rect.x_range(), rect.bottom() - 0.5, stroke);
    }
    let mut ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(egui::vec2(0.0, ROW_PAD)))
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    let copy = ui.add_enabled(enabled, egui::Button::new("Copy"));
    copy.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            enabled,
            format!("Copy transcript from {}", row.time),
        )
    });
    if copy.gained_focus() {
        copy.scroll_to_me(Some(egui::Align::Center));
    }
    if let Some(phase) = phase {
        let (tag, ink) = match phase {
            CopyPhase::Copying => ("copying…", ui.visuals().weak_text_color()),
            CopyPhase::Copied => ("copied", color(palette.accent)),
            CopyPhase::NotCopied => ("not copied", color(palette.attention)),
            CopyPhase::Uncertain => ("uncertain", color(palette.attention)),
        };
        ui.colored_label(ink, tag);
    }
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = ROW_LINE_GAP;
        ui.add(
            egui::Label::new(egui::RichText::new(&row.meta).weak())
                .truncate()
                .selectable(false),
        );
        let line = ui.text_style_height(&egui::TextStyle::Body);
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), line),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                if row.partial {
                    ui.colored_label(color(palette.attention), "Partial:")
                        .on_hover_text("Saved text may not cover the whole recording.");
                }
                ui.add(egui::Label::new(&row.preview).truncate().selectable(false));
            },
        );
    });
    copy
}

/// Open the egui Settings window until it is closed.
pub(super) fn run(screenshot: Option<PathBuf>) -> Result<()> {
    let config_path = paths::config_file().context("locating config file")?;
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_app_id("cantrip-settings")
            .with_inner_size([620.0, 700.0])
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
        self.poll_history(ctx);
        if let Some(result) = model::completed(self.poll_result.as_ref()) {
            match result {
                Ok(status) => {
                    self.daemon_online = true;
                    self.daemon_state = dictation_state(&status);
                }
                Err(_) => {
                    self.daemon_online = false;
                    self.daemon_state = "unreachable".to_owned();
                }
            }
            self.poll_result = None;
        }
        if let Some(result) = model::completed(self.reload_result.as_ref()) {
            self.form.applied(result);
            self.reload_result = None;
        }
        if self.last_poll.elapsed() >= DAEMON_POLL && self.poll_result.is_none() {
            self.last_poll = Instant::now();
            self.palette = theme::load();
            apply_theme(ctx, self.palette);
            let (tx, rx) = mpsc::channel();
            let context = ctx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(ipc::status());
                context.request_repaint();
            });
            self.poll_result = Some(rx);
        }
        if ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
            if self.form.repairing() {
                self.form.cancel_repair();
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        ctx.request_repaint_after(if self.screenshot.is_some() {
            Duration::from_millis(30)
        } else {
            DAEMON_POLL
        });

        egui::CentralPanel::default().show(ctx, |ui| self.show(ui));

        if self.screenshot.is_some()
            && !self.screenshot_requested
            && self.frames >= SCREENSHOT_DELAY_FRAMES
            && (!self.history.loading() || self.frames >= SCREENSHOT_MAX_FRAMES)
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
