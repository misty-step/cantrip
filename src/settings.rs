//! `cantrip settings` — the configuration window.
//!
//! It loads the native `config.toml`, edits the common settings and writes
//! them back with `toml_edit`, so the annotated comments users keep in the file
//! survive unchanged; saving asks the running Cantrip to apply the file. Past
//! transcripts sit beside the configuration: saved history read off the UI
//! thread, newest first, each with a Copy that reads that transcript fresh by
//! ID and hands it to the clipboard. History needs neither a valid config file
//! nor a running Cantrip.
//!
//! [`model`] holds that state for both platforms and [`store`] reads and
//! writes the file. Linux renders it in an egui window (`linux`) with a
//! PipeWire source name and compositor shortcuts. macOS renders it in an
//! AppKit window inside the menu-bar app (`crate::macos::settings`), which
//! also picks the microphone by device, shows the capture owner's microphone
//! access and sets the global dictation shortcut; there `cantrip settings`
//! asks the running app to show its window.
//!
//! `cantrip settings --screenshot <path>` renders the window to a PNG and
//! exits, for visual testing without a screenshot utility.

#[cfg(not(target_os = "macos"))]
mod linux;
pub(crate) mod model;
pub(crate) mod store;

use crate::theme;
use anyhow::Result;
use eframe::egui;
use std::path::PathBuf;

#[cfg(target_os = "macos")]
pub use crate::macos::settings::Pane;

/// Entry point for the `cantrip settings` subcommand.
#[cfg(not(target_os = "macos"))]
pub fn run(screenshot: Option<PathBuf>) -> Result<()> {
    linux::run(screenshot)
}

/// Entry point for the `cantrip settings` subcommand: show the menu-bar app's
/// Settings window, or render `pane` offscreen to the `screenshot` PNG.
#[cfg(target_os = "macos")]
pub fn run(screenshot: Option<PathBuf>, pane: Option<Pane>) -> Result<()> {
    match screenshot {
        Some(path) => crate::macos::settings::screenshot(&path, pane.unwrap_or_default()),
        None => crate::macos::requests::show(crate::macos::requests::Show::Settings),
    }
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

#[cfg(test)]
pub(crate) mod fixture {
    use cantrip_engine::config::{Config, HudConfig, PostprocConfig, SttConfig, TelemetryConfig};
    use cantrip_engine::delivery::InjectionMode;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    /// A private directory for one test's `config.toml`, removed afterwards.
    pub(crate) struct ConfigFixture(PathBuf);

    impl ConfigFixture {
        pub(crate) fn new() -> Self {
            let root = std::env::temp_dir()
                .canonicalize()
                .expect("native temporary directory")
                .join(format!(
                    "ct-settings-{}",
                    cantrip_engine::recovery::new_id()
                ));
            fs::create_dir(&root).expect("private fixture directory");
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).expect("private fixture");
            Self(root)
        }

        pub(crate) fn path(&self) -> PathBuf {
            self.0.join("config.toml")
        }
    }

    impl Drop for ConfigFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    pub(crate) fn sample_config() -> Config {
        Config {
            injection: InjectionMode::Type,
            keep_warm: false,
            audio_source: Some("alsa_input.pci-0000_00_1f.3".to_owned()),
            hotkey: None,
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
}
