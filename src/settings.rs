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

use crate::config::{Config, PostprocConfig, SttConfig};
use crate::inject::InjectionMode;
use crate::ipc;
use crate::paths;
use anyhow::{anyhow, Context, Result};
use eframe::egui;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Frames to render before taking the `--screenshot` (lets layout settle).
const SCREENSHOT_DELAY_FRAMES: u32 = 6;
/// How often to refresh the live daemon state in the header.
const DAEMON_POLL: Duration = Duration::from_secs(1);

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
    pp_timeout: u64,
    pp_passes: u8,
    pp_min_chars: usize,
    pp_instructions: String,
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
            pp_timeout: cfg.postproc.timeout_ms,
            pp_passes: cfg.postproc.passes,
            pp_min_chars: cfg.postproc.min_chars,
            pp_instructions: cfg.postproc.instructions.clone(),
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
                timeout_ms: self.pp_timeout,
                passes: self.pp_passes.clamp(1, 3),
                min_chars: self.pp_min_chars,
                instructions: self.pp_instructions.clone(),
            },
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

struct SettingsApp {
    edit: Editable,
    config_path: PathBuf,
    status: Option<StatusMsg>,
    /// False when the config could not be loaded; the Save button is then
    /// disabled so a default-filled form can never overwrite the user file.
    loaded_ok: bool,
    /// Raw file text as loaded, used to refuse clobbering concurrent edits.
    loaded_text: String,
    daemon_online: bool,
    daemon_state: String,
    last_poll: Instant,
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
        cc.egui_ctx.set_theme(egui::Theme::Dark);
        let loaded_text = fs::read_to_string(&config_path).unwrap_or_default();
        let (edit, loaded_ok, status) = match Config::load() {
            Ok(cfg) => (Editable::from_config(&cfg), true, None),
            Err(error) => (
                Editable::from_config(&Config::default()),
                false,
                Some(StatusMsg {
                    text: format!("Failed to load config (saving disabled): {error:#}"),
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
                let side = ((ui.available_width() - Self::MAX_W) * 0.5).max(0.0);
                ui.horizontal(|ui| {
                    ui.add_space(side);
                    ui.vertical(|ui| {
                        ui.set_width(Self::MAX_W);
                        self.form(ui);
                    });
                });
            });
    }

    fn form(&mut self, ui: &mut egui::Ui) {
        self.header(ui);
        ui.add_space(2.0);
        if let Some(status) = &self.status {
            let color = if status.ok { OK } else { ERR };
            ui.colored_label(color, &status.text);
        }
        ui.add_space(8.0);

        Self::section(
            ui,
            "General",
            "Injection, warm-up, audio, vocabulary",
            |ui| {
                self.general_section(ui);
            },
        );
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
                    .color(BG),
            )
            .fill(ACCENT)
            .rounding(egui::Rounding::same(8.0));
            if ui.add_enabled(self.loaded_ok, save).clicked() {
                self.save();
            }
            let reload = egui::Button::new("Reload from disk")
                .fill(PANEL_ALT)
                .stroke(egui::Stroke::new(1.0_f32, BORDER))
                .rounding(egui::Rounding::same(8.0));
            if ui.add(reload).clicked() {
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
                    .color(ACCENT),
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

    /// Always-open group: accent tick + title + hint above a bordered panel.
    fn section(ui: &mut egui::Ui, title: &str, hint: &str, add: impl FnOnce(&mut egui::Ui)) {
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(3.0, 14.0), egui::Sense::hover());
            ui.painter()
                .rect_filled(rect, egui::Rounding::same(1.5), ACCENT);
            ui.add_space(2.0);
            ui.label(egui::RichText::new(title).strong().size(13.5));
            ui.add_space(4.0);
            ui.label(egui::RichText::new(hint).weak().small());
        });
        ui.add_space(3.0);
        egui::Frame::group(ui.style())
            .fill(PANEL)
            .stroke(egui::Stroke::new(1.0_f32, BORDER))
            .rounding(egui::Rounding::same(8.0))
            .inner_margin(egui::Margin::symmetric(12.0, 10.0))
            .show(ui, add);
        ui.add_space(10.0);
    }

    /// Live daemon state as a small rounded chip with a status dot.
    fn daemon_badge(&mut self, ui: &mut egui::Ui) {
        let (color, text) = if !self.daemon_online {
            (
                egui::Color32::from_rgb(0x66, 0x6c, 0x76),
                "daemon offline".to_owned(),
            )
        } else {
            match self.daemon_state.as_str() {
                "idle" => (OK, "daemon: idle".to_owned()),
                "recording" => (ACCENT, "daemon: recording".to_owned()),
                other => (WARN, format!("daemon: {other}")),
            }
        };
        let text_width = ui.fonts(|fonts| {
            fonts
                .layout_no_wrap(text.clone(), egui::FontId::proportional(12.0), TEXT)
                .size()
                .x
        }) + 28.0;
        let (rect, _) = ui.allocate_exact_size(egui::vec2(text_width, 24.0), egui::Sense::hover());
        let painter = ui.painter();
        painter.rect(
            rect,
            egui::Rounding::same(12.0),
            PANEL_ALT,
            egui::Stroke::new(1.0_f32, color.linear_multiply(0.4)),
        );
        painter.circle_filled(rect.left_center() + egui::vec2(10.0, 0.0), 3.5, color);
        painter.text(
            rect.left_center() + egui::vec2(20.0, 0.0),
            egui::Align2::LEFT_CENTER,
            text,
            egui::FontId::proportional(12.0),
            TEXT,
        );
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
                        .hint_text("https://…/audio/transcriptions")
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
        match Config::load() {
            Ok(cfg) => {
                self.edit = Editable::from_config(&cfg);
                self.loaded_ok = true;
                self.loaded_text = fs::read_to_string(&self.config_path).unwrap_or_default();
                self.status = Some(StatusMsg {
                    text: "Reloaded from disk".to_owned(),
                    ok: true,
                });
            }
            Err(error) => {
                self.status = Some(StatusMsg {
                    text: format!("Reload failed: {error:#}"),
                    ok: false,
                });
            }
        }
    }

    fn save(&mut self) {
        if !self.loaded_ok {
            self.status = Some(StatusMsg {
                text: "Not saved — the config could not be loaded; fix it and reload first"
                    .to_owned(),
                ok: false,
            });
            return;
        }
        // Refuse to clobber a concurrent external edit (e.g. `cantrip config edit`).
        let current = fs::read_to_string(&self.config_path).unwrap_or_default();
        if current != self.loaded_text {
            self.status = Some(StatusMsg {
                text: "Config changed on disk since opened — click Reload from disk first"
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
        if let Err(error) = save_config_preserving(&self.config_path, &config) {
            self.status = Some(StatusMsg {
                text: format!("Save failed: {error:#}"),
                ok: false,
            });
            return;
        }
        self.loaded_text = fs::read_to_string(&self.config_path).unwrap_or_default();
        match ipc::send(ipc::Command::Reload) {
            Ok(reply) if reply.ok => {
                self.status = Some(StatusMsg {
                    text: "Saved and daemon reloaded".to_owned(),
                    ok: true,
                });
            }
            Ok(reply) => {
                self.status = Some(StatusMsg {
                    text: format!(
                        "Saved to disk, but reload failed: {}",
                        reply.message.unwrap_or_default()
                    ),
                    ok: false,
                });
            }
            Err(_) => {
                self.status = Some(StatusMsg {
                    text: "Saved to disk; daemon not running (start it with: cantrip daemon)"
                        .to_owned(),
                    ok: true,
                });
            }
        }
    }
}

/// Write the edited config back to disk while preserving comments and ordering
/// for every key the window did not touch (so the annotated template survives).
fn save_config_preserving(path: &Path, config: &Config) -> Result<()> {
    let existing = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", path.display()));
        }
    };
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
    fs::write(&tmp, doc.to_string()).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
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
            .with_inner_size([620.0, 700.0])
            .with_maximized(false)
            .with_title("Cantrip Settings"),
        ..Default::default()
    };
    eframe::run_native(
        "cantrip-settings",
        options,
        Box::new(move |cc| {
            apply_theme(&cc.egui_ctx);
            Ok(Box::new(SettingsApp::new(cc, screenshot, config_path)))
        }),
    )
    .map_err(|error| anyhow!("settings window error: {error}"))
}

/// Brand palette shared with the HUD pill.
const BG: egui::Color32 = egui::Color32::from_rgb(0x0e, 0x0e, 0x11);
const PANEL: egui::Color32 = egui::Color32::from_rgb(0x15, 0x16, 0x1b);
const PANEL_ALT: egui::Color32 = egui::Color32::from_rgb(0x1b, 0x1c, 0x22);
const BORDER: egui::Color32 = egui::Color32::from_rgb(0x2b, 0x2d, 0x35);
const TEXT: egui::Color32 = egui::Color32::from_rgb(0xf2, 0xf4, 0xf8);
const TEXT_MUTED: egui::Color32 = egui::Color32::from_rgb(0x99, 0x9f, 0xa8);
const ACCENT: egui::Color32 = egui::Color32::from_rgb(0xff, 0x6a, 0x5c);
const OK: egui::Color32 = egui::Color32::from_rgb(0x74, 0xdc, 0x96);
const WARN: egui::Color32 = egui::Color32::from_rgb(0xff, 0xba, 0x4a);
const ERR: egui::Color32 = egui::Color32::from_rgb(0xe5, 0x6a, 0x6a);

/// Force a consistent dark palette that matches the HUD pill, regardless of
/// the desktop theme egui would otherwise inherit.
fn apply_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = BG;
    visuals.window_fill = BG;
    visuals.extreme_bg_color = BG;
    visuals.faint_bg_color = egui::Color32::from_rgb(0x13, 0x14, 0x18);
    visuals.override_text_color = Some(TEXT);
    visuals.hyperlink_color = ACCENT;

    visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0_f32, TEXT_MUTED);
    visuals.widgets.noninteractive.bg_fill = BG;
    visuals.widgets.noninteractive.rounding = egui::Rounding::same(6.0);

    visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0_f32, TEXT);
    visuals.widgets.inactive.bg_fill = PANEL_ALT;
    visuals.widgets.inactive.weak_bg_fill = PANEL_ALT;
    visuals.widgets.inactive.rounding = egui::Rounding::same(6.0);

    visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(0x26, 0x27, 0x2f);
    visuals.widgets.hovered.rounding = egui::Rounding::same(6.0);
    visuals.widgets.active.bg_fill = egui::Color32::from_rgb(0x30, 0x32, 0x3b);
    visuals.widgets.active.rounding = egui::Rounding::same(6.0);

    visuals.selection.bg_fill = ACCENT.linear_multiply(0.35);
    visuals.selection.stroke = egui::Stroke::new(1.0_f32, ACCENT);
    visuals.text_cursor.stroke = egui::Stroke::new(2.0_f32, ACCENT);

    ctx.set_visuals(visuals);
    ctx.style_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(14.0, 7.0);
    });
}

/// Full app loop; also handles the `--screenshot` dump then closes.
#[allow(clippy::collapsible_if)]
impl eframe::App for SettingsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.frames += 1;
        if self.last_poll.elapsed() >= DAEMON_POLL {
            self.last_poll = Instant::now();
            match ipc::send(ipc::Command::Status) {
                Ok(reply) => {
                    self.daemon_online = true;
                    self.daemon_state = reply.state;
                }
                Err(_) => {
                    self.daemon_online = false;
                    self.daemon_state = "offline".to_owned();
                }
            }
        }
        ctx.request_repaint_after(DAEMON_POLL);

        egui::CentralPanel::default().show(ctx, |ui| self.show(ui));

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
                timeout_ms: 30_000,
                passes: 1,
                min_chars: 40,
                instructions: "Remove filler words.".to_owned(),
            },
        }
    }

    #[test]
    fn editable_round_trips_config() {
        let original = sample_config();
        assert_eq!(Editable::from_config(&original).to_config(), original);
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
            pp_timeout: 10_000,
            pp_passes: 1,
            pp_min_chars: 40,
            pp_instructions: String::new(),
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
        save_config_preserving(&path, &config).expect("save");
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
        save_config_preserving(&path, &config).expect("save");
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
        assert!(save_config_preserving(&path, &sample_config()).is_err());
        fs::remove_file(&path).ok();
    }
}
