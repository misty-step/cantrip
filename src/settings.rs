//! `cantrip settings` — a small modifiable configuration window.
//!
//! Renders an egui window (eframe/glow on Wayland) that loads
//! `~/.config/cantrip/config.toml`, lets you edit the common settings, and
//! writes them back with `toml_edit` so the annotated comments users keep in the
//! file survive unchanged. Saving sends a `Reload` to the running daemon. The
//! header also shows the live daemon state (idle / recording / processing /
//! offline) by polling the socket every second.
//!
//! Past transcripts sit above the configuration: saved history read off the UI
//! thread, newest first, each with a Copy button that reads that transcript
//! fresh by ID and hands it to the clipboard. History needs neither a valid
//! config file nor a running daemon.
//!
//! `cantrip settings --screenshot <path>` renders the window, lets the history
//! list load, and dumps a PNG of one frame, then exits. It exists for visual
//! testing on machines without a screenshot utility.

use crate::config::{Config, HudConfig, PostprocConfig, SttConfig, TelemetryConfig};
use crate::inject::{self, DeliveryGuard, InjectionFailureKind, InjectionMode};
use crate::ipc;
use crate::{actions, paths, recovery, theme};
use anyhow::{anyhow, Context, Result};
use eframe::egui;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

/// Frames to render before taking the `--screenshot` (lets layout settle).
const SCREENSHOT_DELAY_FRAMES: u32 = 6;
/// How often to refresh the live daemon state in the header.
const DAEMON_POLL: Duration = Duration::from_secs(1);
/// Upper bound on frames a `--screenshot` waits for the history list.
const SCREENSHOT_MAX_FRAMES: u32 = 100;
/// Visible rows in the past-transcript list; the half row signals more below.
const HISTORY_VISIBLE_ROWS: f32 = 3.5;
/// Vertical padding inside each past-transcript row.
const ROW_PAD: f32 = 6.0;
/// Gap between a row's capture details and its opening words.
const ROW_LINE_GAP: f32 = 2.0;

/// A flat, editable view of the config, bound directly to egui text fields.
struct Editable {
    injection: InjectionMode,
    keep_warm: bool,
    audio_source: String,
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
            vocabulary: cfg.vocabulary.join(", "),
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
            vocabulary: self
                .vocabulary
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
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

struct StatusMsg {
    text: String,
    ok: bool,
}

fn completed_request<T>(receiver: Option<&Receiver<Result<T>>>) -> Option<Result<T>> {
    match receiver?.try_recv() {
        Ok(result) => Some(result),
        Err(TryRecvError::Empty) => None,
        Err(TryRecvError::Disconnected) => {
            Some(Err(anyhow!("The background request stopped unexpectedly")))
        }
    }
}

/// One saved transcript, formatted once per load so frames only borrow text.
struct PastTranscript {
    id: String,
    time: String,
    /// Capture time and duration, as in the recordings list.
    meta: String,
    preview: String,
    partial: bool,
}

impl PastTranscript {
    fn new(transcript: recovery::Transcript) -> Self {
        let take = transcript.take;
        let time = actions::take_time(take.created_at_unix_ms);
        Self {
            meta: format!("{time}   {}", actions::take_duration(take.duration_ms)),
            time,
            preview: transcript.preview,
            partial: take.partial,
            id: take.id,
        }
    }
}

#[derive(Clone, Copy)]
enum CopyPhase {
    Copying,
    Copied,
    NotCopied,
    Uncertain,
}

/// Why a Copy did not finish. Causes are fixed messages, never transcript text.
enum CopyFailure {
    /// Nothing reached the clipboard.
    NotCopied(String),
    /// The clipboard handoff may have happened.
    Uncertain(String),
}

/// The latest Copy request, bound to the transcript ID it was made for.
struct CopyStatus {
    id: String,
    time: String,
    phase: CopyPhase,
    message: String,
}

/// Past transcripts, loaded and copied off the UI thread. Independent of the
/// config file and the daemon, so history stays usable when either fails.
#[derive(Default)]
struct History {
    rows: Vec<PastTranscript>,
    /// "N saved transcripts", formatted when a load succeeds.
    count: String,
    /// True once any load has finished; later loads are refreshes.
    loaded: bool,
    error: Option<String>,
    load: Option<Receiver<Result<Vec<PastTranscript>>>>,
    copy: Option<Receiver<Result<(), CopyFailure>>>,
    copy_status: Option<CopyStatus>,
    /// The focused Copy button that started a request; focus returns to it.
    copy_focus: Option<egui::Id>,
}

impl History {
    /// Start a load unless one is running; listed rows stay visible meanwhile.
    fn refresh(&mut self, ctx: &egui::Context) {
        if self.load.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let context = ctx.clone();
        std::thread::spawn(move || {
            let rows: Result<Vec<_>> = recovery::transcripts()
                .map(|transcripts| transcripts.into_iter().map(PastTranscript::new).collect());
            let _ = tx.send(rows);
            context.request_repaint();
        });
        self.load = Some(rx);
    }

    /// Copy one listed transcript. The worker gets only its ID, never the preview.
    fn start_copy(&mut self, index: usize, focus: Option<egui::Id>, ctx: &egui::Context) {
        if self.copy.is_some() {
            return;
        }
        let Some(row) = self.rows.get(index) else {
            return;
        };
        let id = row.id.clone();
        self.copy_status = Some(CopyStatus {
            id: id.clone(),
            time: row.time.clone(),
            phase: CopyPhase::Copying,
            message: format!("Copying transcript from {}…", row.time),
        });
        let (tx, rx) = mpsc::channel();
        let context = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(copy_transcript(&id));
            context.request_repaint();
        });
        self.copy = Some(rx);
        self.copy_focus = focus;
    }

    /// Apply finished background work; called every frame before drawing.
    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(result) = completed_request(self.load.as_ref()) {
            self.load = None;
            self.loaded = true;
            match result {
                Ok(rows) => {
                    self.count = match rows.len() {
                        1 => "1 saved transcript".to_owned(),
                        count => format!("{count} saved transcripts"),
                    };
                    self.rows = rows;
                    self.error = None;
                }
                Err(error) if self.rows.is_empty() => {
                    self.error = Some(format!("Transcript history could not be read: {error:#}"));
                }
                Err(error) => {
                    self.error = Some(format!(
                        "Transcript history could not be refreshed: {error:#}. The list below may be out of date."
                    ));
                }
            }
        }
        let finished = match self.copy.as_ref().map(Receiver::try_recv) {
            Some(Ok(result)) => Some(result),
            Some(Err(TryRecvError::Disconnected)) => Some(Err(CopyFailure::Uncertain(
                "The copy stopped unexpectedly. Check the clipboard before pasting.".to_owned(),
            ))),
            _ => None,
        };
        let Some(result) = finished else {
            return;
        };
        self.copy = None;
        if let Some(status) = &mut self.copy_status {
            (status.phase, status.message) = match result {
                Ok(()) => (
                    CopyPhase::Copied,
                    format!("Copied transcript from {}. Paste when ready.", status.time),
                ),
                Err(CopyFailure::NotCopied(cause)) => (
                    CopyPhase::NotCopied,
                    format!("Transcript from {} was not copied. {cause}", status.time),
                ),
                Err(CopyFailure::Uncertain(cause)) => (
                    CopyPhase::Uncertain,
                    format!(
                        "Transcript from {} may not have been copied. {cause}",
                        status.time
                    ),
                ),
            };
        }
        if let Some(id) = self.copy_focus.take() {
            ctx.memory_mut(|memory| memory.request_focus(id));
        }
    }

    /// Lay out the rows in `range`. Returns the row whose Copy was chosen, with
    /// the button's id when it had keyboard focus.
    fn visible_rows(
        &self,
        ui: &mut egui::Ui,
        range: std::ops::Range<usize>,
        palette: theme::Palette,
    ) -> Option<(usize, Option<egui::Id>)> {
        let latest = self.copy_status.as_ref();
        let mut request = None;
        for index in range {
            let row = &self.rows[index];
            let phase = latest
                .filter(|status| status.id == row.id)
                .map(|status| status.phase);
            let separator = index + 1 < self.rows.len();
            // show_rows skips one auto id per row and push_id uses exactly one,
            // so widget ids (and keyboard focus) stay stable while scrolling.
            let copy = ui
                .push_id(&row.id, |ui| {
                    transcript_row(ui, row, palette, self.copy.is_none(), phase, separator)
                })
                .inner;
            if copy.clicked() {
                request = Some((index, copy.has_focus().then_some(copy.id)));
            }
        }
        request
    }
}

/// Read one transcript fresh by ID and hand it to the existing clipboard lane.
/// The text stays in this worker; failure causes never include it.
fn copy_transcript(id: &str) -> Result<(), CopyFailure> {
    let text = recovery::read_text(id).map_err(|error| {
        CopyFailure::NotCopied(format!(
            "It could not be read from saved history ({error:#}). The clipboard was not changed; choose Refresh to update the list."
        ))
    })?;
    inject::inject(
        &text,
        InjectionMode::Clipboard,
        &DeliveryGuard::capture(),
        &AtomicBool::new(false),
    )
    .map(|_| ())
    .map_err(|failure| match failure.kind {
        InjectionFailureKind::Uncertain => CopyFailure::Uncertain(failure.message),
        _ => CopyFailure::NotCopied(failure.message),
    })
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
    if separator {
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

enum EditableConfigLoad {
    Ready {
        config: Box<Config>,
        text: String,
        warning: Option<StatusMsg>,
    },
    Blocked {
        message: String,
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
                message: format!(
                    "Cannot read config; file was not changed: {error}. Check file ownership and permissions."
                ),
            };
        }
    };
    let config = match toml::from_str::<Config>(&text) {
        Ok(config) => config,
        Err(error) => {
            return EditableConfigLoad::Blocked {
                message: format!(
                    "Cannot parse config; file was not changed: {error}. Choose Repair configuration to edit the original text."
                ),
            };
        }
    };
    let warning = config.validate().err().map(|error| StatusMsg {
        text: format!(
            "Configuration needs repair: {error:#}. Correct the values, then Save & reload daemon"
        ),
        ok: false,
    });
    EditableConfigLoad::Ready {
        config: Box::new(config),
        text,
        warning,
    }
}

struct SettingsApp {
    edit: Editable,
    config_path: PathBuf,
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
    /// Past transcripts; loaded and copied off the UI thread.
    history: History,
    palette: theme::Palette,
    repair: Option<(String, String)>,
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
        let mut history = History::default();
        history.refresh(&cc.egui_ctx);
        let (edit, loaded_ok, loaded_text, status) = match load_editable_config(&config_path) {
            EditableConfigLoad::Ready {
                config,
                text,
                warning,
            } => (Editable::from_config(&config), true, text, warning),
            EditableConfigLoad::Blocked { message } => (
                Editable::from_config(&Config::default()),
                false,
                String::new(),
                Some(StatusMsg {
                    text: message,
                    ok: false,
                }),
            ),
        };
        Self {
            edit,
            config_path,
            status,
            loaded_ok,
            loaded_text,
            daemon_online: false,
            daemon_state: "offline".to_owned(),
            last_poll: Instant::now() - DAEMON_POLL,
            poll_result: None,
            reload_result: None,
            history,
            palette,
            repair: None,
            frames: 0,
            screenshot,
            screenshot_requested: false,
            screenshot_deadline: None,
        }
    }

    /// Maximum content width; the form is centered when the window is wider
    /// (e.g. tiled or fullscreen), so it never stretches or clusters left.
    const MAX_W: f32 = 620.0;

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
        if let Some(status) = &self.status {
            let color = color(if status.ok {
                self.palette.foreground
            } else {
                self.palette.attention
            });
            ui.colored_label(color, &status.text);
        }
        if !self.loaded_ok && self.repair.is_none() && ui.button("Repair configuration").clicked() {
            match fs::read_to_string(&self.config_path) {
                Ok(text) => self.repair = Some((text.clone(), text)),
                Err(_) => self.status = Some(StatusMsg {
                    text:
                        "Cannot read this file. Check its ownership and permissions before editing."
                            .to_owned(),
                    ok: false,
                }),
            }
        }
        if self.repair.is_some() {
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
            ui.checkbox(&mut self.edit.hud.labels, "Always show state labels");
            egui::ComboBox::from_id_salt("reduced-motion")
                .selected_text(match self.edit.hud.reduced_motion {
                    None => "Motion: follow desktop",
                    Some(true) => "Motion: reduced",
                    Some(false) => "Motion: normal",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.edit.hud.reduced_motion, None, "Follow desktop");
                    ui.selectable_value(&mut self.edit.hud.reduced_motion, Some(true), "Reduced");
                    ui.selectable_value(&mut self.edit.hud.reduced_motion, Some(false), "Normal");
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
                egui::RichText::new("Save & reload daemon")
                    .strong()
                    .color(color(self.palette.background)),
            )
            .fill(color(self.palette.accent));
            if ui
                .add_enabled(self.loaded_ok && self.reload_result.is_none(), save)
                .clicked()
            {
                self.save();
            }
            let reload = egui::Button::new("Reload from disk")
                .fill(color(self.palette.surface))
                .stroke(egui::Stroke::new(1.0_f32, color(self.palette.border)));
            if ui
                .add_enabled(self.reload_result.is_none(), reload)
                .clicked()
            {
                self.reload_from_disk();
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
            egui::RichText::new(format!("Config: {}", self.config_path.display()))
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
                "daemon unreachable".to_owned(),
            )
        } else {
            match self.daemon_state.as_str() {
                "idle" => (color(self.palette.foreground), "daemon: idle".to_owned()),
                other => (color(self.palette.accent), format!("daemon: {other}")),
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
        let loading = self.history.load.is_some();
        let summary = if loading && self.history.loaded {
            "Refreshing…"
        } else if loading {
            "Loading transcripts…"
        } else if !self.history.rows.is_empty() {
            self.history.count.as_str()
        } else if self.history.error.is_some() {
            "History unavailable"
        } else {
            "No saved transcripts yet."
        };
        let mut refresh = false;
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(summary).weak());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                refresh = ui
                    .add_enabled(!loading, egui::Button::new("Refresh"))
                    .clicked();
            });
        });
        if let Some(error) = &self.history.error {
            ui.colored_label(color(self.palette.attention), error);
        } else if !loading && self.history.rows.is_empty() {
            ui.label(
                egui::RichText::new("Dictate, then choose Refresh to see it here.")
                    .weak()
                    .small(),
            );
        }
        let mut request = None;
        if !self.history.rows.is_empty() {
            egui::Frame::none()
                .fill(ui.visuals().extreme_bg_color)
                .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
                .inner_margin(egui::Margin::symmetric(8.0, 0.0))
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.spacing_mut().scroll = egui::style::ScrollStyle::solid();
                    let row_height = transcript_row_height(ui);
                    egui::ScrollArea::vertical()
                        .id_salt("past-transcripts")
                        .max_height(row_height * HISTORY_VISIBLE_ROWS)
                        .auto_shrink([false, true])
                        .show_rows(ui, row_height, self.history.rows.len(), |ui, range| {
                            request = self.history.visible_rows(ui, range, self.palette);
                        });
                });
        }
        if let Some(status) = &self.history.copy_status {
            let ink = match status.phase {
                CopyPhase::Copying | CopyPhase::Copied => self.palette.foreground,
                CopyPhase::NotCopied | CopyPhase::Uncertain => self.palette.attention,
            };
            ui.colored_label(color(ink), &status.message);
        }
        if let Some((index, focus)) = request {
            self.history.start_copy(index, focus, ui.ctx());
        }
        if refresh {
            self.history.refresh(ui.ctx());
        }
    }

    fn general_section(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("general")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                ui.label(egui::RichText::new("Injection").weak());
                egui::ComboBox::from_id_salt("injection")
                    .selected_text(injection_str(self.edit.injection))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.edit.injection, InjectionMode::Auto, "Auto");
                        ui.selectable_value(
                            &mut self.edit.injection,
                            InjectionMode::Paste,
                            "Paste",
                        );
                        ui.selectable_value(&mut self.edit.injection, InjectionMode::Type, "Type");
                        ui.selectable_value(
                            &mut self.edit.injection,
                            InjectionMode::Clipboard,
                            "Clipboard",
                        );
                    });
                ui.end_row();

                ui.checkbox(
                    &mut self.edit.keep_warm,
                    "Keep STT model warm (faster dictation)",
                );
                ui.end_row();

                ui.label(egui::RichText::new("Audio source (empty = default)").weak());
                ui.add(
                    egui::TextEdit::singleline(&mut self.edit.audio_source)
                        .hint_text("e.g. alsa_input…")
                        .desired_width(f32::INFINITY),
                );
                ui.end_row();

                ui.label(egui::RichText::new("Vocabulary (comma-separated)").weak());
                ui.add(
                    egui::TextEdit::singleline(&mut self.edit.vocabulary)
                        .hint_text("PipeWire, Parakeet")
                        .desired_width(f32::INFINITY),
                );
                ui.end_row();
            });
    }

    fn stt_section(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("stt")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                ui.label(egui::RichText::new("Model (empty endpoint = local)").weak());
                ui.add(
                    egui::TextEdit::singleline(&mut self.edit.stt_model)
                        .desired_width(f32::INFINITY),
                );
                ui.end_row();

                ui.label(egui::RichText::new("Endpoint (empty = local STT)").weak());
                ui.add(
                    egui::TextEdit::singleline(&mut self.edit.stt_endpoint)
                        .hint_text("https://api.openai.com/v1")
                        .desired_width(f32::INFINITY),
                );
                ui.end_row();

                ui.label(egui::RichText::new("API key id (keyring)").weak());
                ui.add(
                    egui::TextEdit::singleline(&mut self.edit.stt_key)
                        .hint_text("openai")
                        .desired_width(f32::INFINITY),
                );
                ui.end_row();
            });
    }

    fn postproc_section(&mut self, ui: &mut egui::Ui) {
        ui.checkbox(&mut self.edit.pp_enabled, "Clean up the transcript");
        ui.add_space(4.0);
        egui::Grid::new("postproc")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                ui.label(egui::RichText::new("Endpoint").weak());
                ui.add(
                    egui::TextEdit::singleline(&mut self.edit.pp_endpoint)
                        .desired_width(f32::INFINITY),
                );
                ui.end_row();

                ui.label(egui::RichText::new("Model").weak());
                ui.add(
                    egui::TextEdit::singleline(&mut self.edit.pp_model)
                        .desired_width(f32::INFINITY),
                );
                ui.end_row();

                ui.label(egui::RichText::new("API key id (keyring)").weak());
                ui.add(
                    egui::TextEdit::singleline(&mut self.edit.pp_key)
                        .hint_text("openrouter")
                        .desired_width(f32::INFINITY),
                );
                ui.end_row();

                ui.label(egui::RichText::new("Timeout (ms)").weak());
                ui.add(
                    egui::DragValue::new(&mut self.edit.pp_timeout)
                        .speed(500)
                        .range(1000..=120_000)
                        .clamp_existing_to_range(false),
                );
                ui.end_row();

                ui.label(egui::RichText::new("Cleanup passes").weak());
                ui.horizontal(|ui| {
                    ui.add(
                        egui::DragValue::new(&mut self.edit.pp_passes)
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
                        egui::DragValue::new(&mut self.edit.pp_min_chars)
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
            egui::TextEdit::multiline(&mut self.edit.pp_instructions)
                .desired_rows(4)
                .desired_width(f32::INFINITY),
        );
    }

    fn reload_from_disk(&mut self) {
        match load_editable_config(&self.config_path) {
            EditableConfigLoad::Ready {
                config,
                text,
                warning,
            } => {
                self.edit = Editable::from_config(&config);
                self.loaded_ok = true;
                self.loaded_text = text;
                self.status = warning.or_else(|| {
                    Some(StatusMsg {
                        text: "Reloaded from disk".to_owned(),
                        ok: true,
                    })
                });
            }
            EditableConfigLoad::Blocked { message } => {
                self.loaded_ok = false;
                self.status = Some(StatusMsg {
                    text: message,
                    ok: false,
                });
            }
        }
    }

    fn save(&mut self) {
        if self.reload_result.is_some() {
            return;
        }
        if !self.loaded_ok {
            self.status = Some(StatusMsg {
                text: "Not saved — the config could not be loaded; fix it and reload first"
                    .to_owned(),
                ok: false,
            });
            return;
        }
        let config = self.edit.to_config();
        if let Err(error) = config.validate() {
            self.status = Some(StatusMsg {
                text: format!("Not saved — {error:#}"),
                ok: false,
            });
            return;
        }
        match save_config_preserving(&self.config_path, &config, &self.loaded_text) {
            Ok(text) => self.loaded_text = text,
            Err(error) => {
                self.status = Some(StatusMsg {
                    text: format!("Save failed: {error:#}"),
                    ok: false,
                });
                return;
            }
        }
        self.reload_daemon();
    }

    fn reload_daemon(&mut self) {
        self.status = Some(StatusMsg {
            text: "Saved to disk; applying to the daemon…".to_owned(),
            ok: true,
        });
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(ipc::command(ipc::Command::Reload));
        });
        self.reload_result = Some(rx);
    }

    fn repair_form(&mut self, ui: &mut egui::Ui) {
        let Some((_, edited)) = &mut self.repair else {
            return;
        };
        ui.label("Repair the TOML below. The existing file is kept until the replacement parses and validates.");
        ui.add(
            egui::TextEdit::multiline(edited)
                .code_editor()
                .desired_rows(18)
                .desired_width(f32::INFINITY),
        );
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    self.reload_result.is_none(),
                    egui::Button::new("Save repaired configuration"),
                )
                .clicked()
            {
                let Some((original, edited)) = self.repair.as_ref() else {
                    return;
                };
                let result = (|| -> Result<Config> {
                    anyhow::ensure!(
                        fs::read_to_string(&self.config_path)? == *original,
                        "Configuration changed on disk; reopen repair before saving"
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
                        self.edit = Editable::from_config(&config);
                        self.loaded_ok = true;
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
            if ui.button("Cancel repair").clicked() {
                self.repair = None;
            }
        });
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
        "Config changed on disk since opened — click Reload from disk first"
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

pub(crate) fn color(rgb: [u8; 3]) -> egui::Color32 {
    egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2])
}

/// Egui adapter for the same live desktop palette as the passive HUD.
pub(crate) fn apply_theme(ctx: &egui::Context, palette: theme::Palette) {
    let mut visuals = egui::Visuals::dark();
    let foreground = color(palette.foreground);
    let accent = color(palette.accent);
    let border = egui::Stroke::new(1.0_f32, color(palette.border));
    visuals.panel_fill = color(palette.background);
    visuals.window_fill = color(palette.background);
    visuals.extreme_bg_color = color(palette.background);
    visuals.faint_bg_color = color(palette.surface);
    visuals.override_text_color = Some(foreground);
    visuals.hyperlink_color = accent;
    visuals.warn_fg_color = color(palette.attention);
    visuals.error_fg_color = color(palette.attention);
    for widget in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.rounding = egui::Rounding::ZERO;
        widget.bg_fill = color(palette.surface);
        widget.weak_bg_fill = color(palette.surface);
        widget.bg_stroke = border;
        widget.fg_stroke = egui::Stroke::new(1.0_f32, foreground);
    }
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, accent);
    visuals.widgets.active.bg_stroke = egui::Stroke::new(2.0_f32, accent);
    visuals.selection.bg_fill = accent.linear_multiply(0.18);
    visuals.selection.stroke = egui::Stroke::new(1.0_f32, accent);
    visuals.text_cursor.stroke = egui::Stroke::new(2.0_f32, accent);
    visuals.window_rounding = egui::Rounding::ZERO;
    ctx.set_visuals(visuals);
    ctx.style_mut(|style| {
        style.animation_time = 0.0;
        for font in style.text_styles.values_mut() {
            font.family = egui::FontFamily::Monospace;
        }
        style.spacing.item_spacing = egui::vec2(10.0, 8.0);
        style.spacing.button_padding = egui::vec2(12.0, 8.0);
    });
}

/// Full app loop; also handles the `--screenshot` dump then closes.
#[allow(clippy::collapsible_if)]
impl eframe::App for SettingsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.frames += 1;
        self.history.poll(ctx);
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
                    text: "Saved and daemon reloaded".to_owned(),
                    ok: true,
                },
                Ok(reply) => StatusMsg {
                    text: format!(
                        "Saved to disk. {}",
                        reply
                            .error
                            .or(reply.message)
                            .unwrap_or_else(|| "Daemon did not reload.".to_owned())
                    ),
                    ok: false,
                },
                Err(_) => StatusMsg {
                    text: "Saved to disk; daemon unreachable. Open Cantrip actions for setup."
                        .to_owned(),
                    ok: true,
                },
            });
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

        egui::CentralPanel::default().show(ctx, |ui| self.show(ui));

        if self.screenshot.is_some()
            && !self.screenshot_requested
            && self.frames >= SCREENSHOT_DELAY_FRAMES
            && (self.history.load.is_none() || self.frames >= SCREENSHOT_MAX_FRAMES)
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
    fn invalid_cleanup_passes_stay_invalid_until_explicitly_corrected() {
        let mut config = sample_config();
        config.postproc.passes = 0;
        let mut edit = Editable::from_config(&config);
        assert!(edit.to_config().validate().is_err());
        edit.pp_passes = 1;
        assert!(edit.to_config().validate().is_ok());
    }
}
