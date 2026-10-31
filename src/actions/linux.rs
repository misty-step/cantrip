//! The Linux Recordings and Recovery window: the shared [`Recordings`] state in
//! egui (eframe/glow), themed from the active desktop palette.

use super::effects::Workers;
use super::model::{self, ListNotice, Recordings};
use super::{take_duration, take_time};
use crate::settings::{apply_theme, color};
use crate::theme;
use anyhow::{anyhow, Context, Result};
use cantrip_engine::ipc::StateKind;
use cantrip_engine::recovery::Take;
use eframe::egui;
use std::path::PathBuf;
use std::process::{Command as ProcessCommand, Stdio};
use std::time::{Duration, Instant};

struct Credentials {
    id: String,
    secret: String,
    focus: bool,
}

struct ActionsApp {
    recordings: Recordings,
    workers: Workers,
    focus_recording: bool,
    recording_focus: Option<egui::Id>,
    recording_scroll: f32,
    confirm_focus: bool,
    credentials: Option<Credentials>,
    initial_doctor: bool,
    palette: theme::Palette,
    screenshot: Option<PathBuf>,
    screenshot_started: Option<Instant>,
    frames: u32,
}

/// Open another Cantrip window as its own process.
fn launch(subcommand: &str) -> Result<()> {
    let mut child = ProcessCommand::new(std::env::current_exe()?)
        .arg(subcommand)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("Opening {subcommand}"))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

impl ActionsApp {
    fn new(cc: &eframe::CreationContext<'_>, screenshot: Option<PathBuf>, doctor: bool) -> Self {
        let palette = theme::load();
        apply_theme(&cc.egui_ctx, palette);
        Self {
            recordings: Recordings::default(),
            workers: Workers::default(),
            focus_recording: false,
            recording_focus: None,
            recording_scroll: 0.0,
            confirm_focus: false,
            credentials: None,
            initial_doctor: doctor,
            palette,
            screenshot,
            screenshot_started: None,
            frames: 0,
        }
    }

    /// Run a job the shared state handed out.
    fn run(&mut self, job: Option<model::Job>, ctx: &egui::Context) {
        if let Some(job) = job {
            let context = ctx.clone();
            self.workers.start(job, move || context.request_repaint());
        }
    }

    fn refresh(&mut self, ctx: &egui::Context) {
        let context = ctx.clone();
        let collected = self
            .workers
            .tick(&mut self.recordings, Instant::now(), move || {
                context.request_repaint()
            });
        if collected
            .observed
            .is_some_and(|observed| observed.first_selection)
        {
            self.focus_recording = true;
        }
        if collected.close {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if collected.polled {
            self.palette = theme::load();
            apply_theme(ctx, self.palette);
        }
    }

    fn request_close(&mut self, ctx: &egui::Context) {
        if self.recordings.request_close() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
    }

    fn navigate_recordings(
        &mut self,
        ui: &mut egui::Ui,
        count: usize,
        row_stride: f32,
    ) -> Option<usize> {
        if count == 0
            || self.recording_focus.is_none()
            || ui.ctx().memory(|memory| memory.focused()) != self.recording_focus
        {
            return None;
        }
        let key = ui.input_mut(|input| {
            [
                egui::Key::ArrowDown,
                egui::Key::ArrowUp,
                egui::Key::PageDown,
                egui::Key::PageUp,
                egui::Key::Home,
                egui::Key::End,
            ]
            .into_iter()
            .find(|key| input.consume_key(egui::Modifiers::NONE, *key))
        })?;
        let row = moved_recording_row(
            self.recordings.selected_row(),
            count,
            key,
            (180.0 / row_stride).floor().max(1.0) as usize,
        )?;
        let id = self.recordings.row(row)?.id.clone();
        self.recordings.select(Some(id));
        self.focus_recording = true;
        Some(row)
    }

    fn main_view(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Cantrip");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Settings").clicked() {
                    if let Err(error) = launch("settings") {
                        self.recordings.show_problem(error.to_string());
                    }
                }
            });
        });
        ui.label(egui::RichText::new("Saved speech, deliberate actions.").weak());
        ui.add_space(12.0);
        self.status_view(ui);
        if let Some(message) = self.recordings.message() {
            ui.colored_label(
                color(if message.attention {
                    self.palette.attention
                } else {
                    self.palette.foreground
                }),
                &message.text,
            );
        }
        if let Some(busy) = self.recordings.command_busy() {
            ui.label(busy);
        }
        if let Some(busy) = self.recordings.setup_busy() {
            ui.label(busy);
        }
        if self.recordings.cancelling() {
            ui.label(model::CANCELLING);
        }
        ui.add_space(16.0);
        ui.heading("Recordings");
        ui.label(model::METADATA_ONLY);
        ui.add_space(8.0);
        if ui.button("Refresh recordings").clicked() {
            self.recordings.refresh();
        }
        if let Some(summary) = self.recordings.retained_summary() {
            ui.label(summary);
        }
        let mut show_all = self.recordings.showing_all();
        if ui
            .checkbox(&mut show_all, "Include completed history")
            .changed()
        {
            self.recordings.show_all(show_all);
        }
        match self.recordings.list_notice() {
            Some(ListNotice::Loading) => {
                ui.label(model::LOADING);
            }
            Some(ListNotice::Stale) => {
                ui.colored_label(color(self.palette.attention), model::STALE);
            }
            Some(ListNotice::Empty) => {
                ui.label(model::EMPTY);
            }
            Some(ListNotice::NothingWaiting) => {
                ui.label(
                    "Nothing waiting to recover. Include completed history to view saved transcripts.",
                );
            }
            None => {}
        }
        let mut selected = None;
        let mut focused = false;
        let row_count = self.recordings.row_count();
        let row_height = ui.text_style_height(&egui::TextStyle::Body) * 2.0 + 12.0;
        let row_stride = row_height + ui.spacing().item_spacing.y;
        let navigating_to = self.navigate_recordings(ui, row_count, row_stride);
        let mut scroll = egui::ScrollArea::vertical()
            .id_salt("recordings")
            .max_height(180.0);
        if let Some(row) = navigating_to {
            let top = row as f32 * row_stride;
            let offset = if top < self.recording_scroll {
                top
            } else if top + row_height > self.recording_scroll + 180.0 {
                top + row_height - 180.0
            } else {
                self.recording_scroll
            };
            scroll = scroll.vertical_scroll_offset(offset.max(0.0));
        }
        let mut recording_focus = self.recording_focus;
        let chosen = self.recordings.selected_id().map(str::to_owned);
        let area = scroll.show_rows(ui, row_height, row_count, |ui, range| {
            for row in range {
                let Some(take) = self.recordings.row(row) else {
                    continue;
                };
                let label = format!(
                    "{}   {}\n{}; {}{}",
                    take_time(take.created_at_unix_ms),
                    take_duration(take.duration_ms),
                    model::completeness(take),
                    if take.audio_available {
                        "audio saved"
                    } else {
                        "no audio"
                    },
                    if take.unresolved { "; pending" } else { "" }
                );
                let is_selected = chosen.as_deref() == Some(take.id.as_str());
                ui.push_id(&take.id, |ui| {
                    let response = ui.selectable_label(is_selected, label);
                    if self.focus_recording && is_selected {
                        response.request_focus();
                        recording_focus = Some(response.id);
                        if navigating_to.is_some() {
                            response.scroll_to_me(Some(egui::Align::Center));
                        }
                        focused = true;
                    }
                    if response.has_focus() {
                        recording_focus = Some(response.id);
                        ui.ctx().memory_mut(|memory| {
                            memory.set_focus_lock_filter(
                                response.id,
                                egui::EventFilter {
                                    vertical_arrows: true,
                                    ..Default::default()
                                },
                            )
                        });
                    }
                    if response.clicked() || response.gained_focus() {
                        selected = Some(take.id.clone());
                    }
                });
            }
        });
        self.recording_scroll = area.state.offset.y;
        self.recording_focus = recording_focus;
        if focused {
            self.focus_recording = false;
        }
        if let Some(selected) = selected {
            self.recordings.select(Some(selected));
        }
        if let Some(take) = self.recordings.selected().cloned() {
            ui.add_space(12.0);
            self.take_view(ui, &take);
        } else if self.recordings.selection_missing() {
            ui.label(model::MISSING);
        }
        ui.add_space(20.0);
        self.setup_view(ui);
        ui.add_space(14.0);
        ui.label(egui::RichText::new("Tab between controls. Arrow keys, Page Up/Down, Home/End browse every recording. Enter or Space chooses; Escape closes without deleting.").weak().small());
    }

    fn status_view(&mut self, ui: &mut egui::Ui) {
        let mut job = None;
        let actions = self.recordings.live_actions();
        if let Some(status) = self.recordings.status() {
            let mut caption = status.stage.as_ref().map_or_else(
                || match &status.state {
                    StateKind::Idle => "Ready".to_owned(),
                    StateKind::Recording if status.signal.is_none() => {
                        "Starting microphone".to_owned()
                    }
                    StateKind::Recording => format!("Recording · {}s", status.elapsed),
                    _ => "State unknown".to_owned(),
                },
                ToString::to_string,
            );
            // Only a take headed to a non-default target is named, in that target's color.
            let mut text = egui::RichText::new(match &status.handoff {
                Some(handoff) => {
                    caption.push_str(" · to ");
                    caption.push_str(&handoff.label);
                    caption
                }
                None => caption,
            })
            .size(18.0);
            if let Some(handoff) = &status.handoff {
                text = text.color(color(handoff.color));
            }
            ui.label(text);
            let mut stop = false;
            let mut cancel = false;
            ui.horizontal_wrapped(|ui| {
                stop = ui
                    .add_enabled(actions.stop, egui::Button::new("Stop recording"))
                    .clicked();
                cancel = ui
                    .add_enabled(actions.cancel, egui::Button::new("Cancel — do not deliver"))
                    .clicked();
            });
            let mut dismiss = None;
            if let Some(outcome) = self.recordings.outcome() {
                match &outcome.handoff {
                    Some(handoff) => {
                        ui.colored_label(color(handoff.color), model::outcome_text(outcome))
                    }
                    None => ui.label(&outcome.message),
                };
                if let Some(error) = &outcome.error {
                    ui.colored_label(color(self.palette.attention), error);
                }
                if ui
                    .add_enabled(actions.dismiss, egui::Button::new("Dismiss outcome"))
                    .clicked()
                {
                    dismiss = Some(outcome.event_id);
                }
            }
            if let Some(notice) = self.recordings.notice() {
                ui.label(&notice.message);
                if ui
                    .add_enabled(actions.dismiss, egui::Button::new("Dismiss notice"))
                    .clicked()
                {
                    dismiss = Some(notice.event_id);
                }
            }
            if stop {
                job = self.recordings.stop();
            } else if cancel {
                job = self.recordings.cancel();
            } else if let Some(event_id) = dismiss {
                job = self.recordings.dismiss(event_id);
            }
        } else {
            ui.label(
                egui::RichText::new(if self.recordings.observed_once() {
                    "Cantrip is unreachable"
                } else {
                    "Connecting to Cantrip…"
                })
                .size(18.0),
            );
            if self.recordings.observed_once() {
                ui.label(model::OFFLINE);
            }
        }
        self.run(job, ui.ctx());
    }

    fn take_view(&mut self, ui: &mut egui::Ui, take: &Take) {
        let actions = self.recordings.take_actions(take);
        ui.separator();
        ui.label(egui::RichText::new(take_time(take.created_at_unix_ms)).strong());
        ui.label(format!(
            "Duration: {}\nTranscript: {}\nAudio: {}",
            take_duration(take.duration_ms),
            model::completeness(take),
            model::audio(take)
        ));
        ui.label(
            egui::RichText::new(format!("Recording ID: {}", take.id))
                .weak()
                .small(),
        );
        let mut job = None;
        let enabled = self.recordings.command_busy().is_none() && self.recordings.history_fresh();
        ui.add_enabled_ui(enabled, |ui| {
            if ui
                .add_enabled(actions.copy, egui::Button::new("Copy this transcript"))
                .clicked()
            {
                job = self.recordings.copy(&take.id);
            }
            if take.partial && take.text_available {
                ui.label(model::PARTIAL);
            }
            if ui
                .add_enabled(
                    actions.recover_locally,
                    egui::Button::new("Recover locally to clipboard"),
                )
                .clicked()
            {
                job = self.recordings.recover(&take.id, true);
            }
            ui.label(egui::RichText::new(model::LOCAL).weak().small());
            if ui
                .add_enabled(
                    actions.recover_with_provider,
                    egui::Button::new("Recover with configured provider to clipboard"),
                )
                .clicked()
            {
                job = self.recordings.recover(&take.id, false);
            }
            ui.label(egui::RichText::new(model::PROVIDER).weak().small());
            if !take.audio_available {
                ui.label(model::NO_AUDIO);
            }
            ui.add_space(6.0);
            if ui
                .add_enabled(
                    actions.forget,
                    egui::Button::new("Forget retained recording…"),
                )
                .clicked()
                && self.recordings.ask_forget(&take.id)
            {
                self.confirm_focus = true;
            }
        });
        self.run(job, ui.ctx());
    }

    fn setup_view(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.heading("Setup and checks");
        if let Some(status) = self.recordings.setup_status() {
            ui.colored_label(
                color(if status.attention {
                    self.palette.attention
                } else {
                    self.palette.foreground
                }),
                &status.text,
            );
        }
        if let Some(cancellable) = self.recordings.installing() {
            if ui
                .add_enabled(cancellable, egui::Button::new("Cancel model installation"))
                .clicked()
            {
                self.recordings.cancel_install();
            }
        }
        let local_model = self.recordings.local_model_installed();
        let online = self.recordings.online();
        if !local_model {
            ui.label(model::MODEL_MISSING);
        }
        if self.recordings.config_needs_attention() {
            ui.colored_label(color(self.palette.attention), model::CONFIG_ATTENTION);
        }
        let mut job = None;
        let reload = self.recordings.can_reload();
        ui.add_enabled_ui(self.recordings.setup_free(), |ui| {
            ui.horizontal_wrapped(|ui| {
                if !local_model && ui.button("Install local model").clicked() {
                    job = self.recordings.install_model();
                }
                if !online && ui.button("Start Cantrip").clicked() {
                    job = self.recordings.start_daemon();
                }
                if online
                    && ui
                        .add_enabled(reload, egui::Button::new("Reload configuration"))
                        .clicked()
                {
                    job = self.recordings.reload();
                }
                if ui.button("Check setup").clicked() {
                    job = self.recordings.check_setup();
                }
                if ui.button("API key…").clicked() {
                    self.credentials = Some(Credentials {
                        id: self.recordings.key_id().unwrap_or_default().to_owned(),
                        secret: String::new(),
                        focus: true,
                    });
                }
            });
        });
        self.run(job, ui.ctx());
        if let Some(diagnosis) = self.recordings.diagnosis() {
            ui.add_space(8.0);
            ui.label(egui::RichText::new(diagnosis).monospace());
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        if let Some(take) = self.recordings.forget_confirmation().cloned() {
            let enabled = self.recordings.can_confirm_forget();
            let mut confirmed = false;
            let mut close = false;
            egui::Window::new("Forget this recording?")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_max_width(390.0);
                    ui.label(take_time(take.created_at_unix_ms));
                    ui.label(format!(
                        "{}\nRecording ID: {}",
                        take_duration(take.duration_ms),
                        take.id
                    ));
                    ui.label(model::FORGET);
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let keep = ui.button("Keep recording");
                        if self.confirm_focus {
                            keep.request_focus();
                            self.confirm_focus = false;
                        }
                        close = keep.clicked();
                        confirmed = ui
                            .add_enabled(enabled, egui::Button::new("Forget this recording"))
                            .clicked();
                    });
                });
            if confirmed {
                let job = self.recordings.confirm_forget();
                self.run(job, ctx);
            } else if close {
                self.recordings.keep();
            }
        }
        if let Some(credentials) = &mut self.credentials {
            let mut save = false;
            let mut close = false;
            egui::Window::new("Save an API key")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_max_width(390.0);
                    ui.label(model::STORE_HELP);
                    ui.label("Key ID");
                    let id = ui.text_edit_singleline(&mut credentials.id);
                    if credentials.focus {
                        id.request_focus();
                        credentials.focus = false;
                    }
                    ui.label("API key");
                    ui.add(
                        egui::TextEdit::singleline(&mut credentials.secret)
                            .password(true)
                            .desired_width(f32::INFINITY),
                    );
                    ui.horizontal(|ui| {
                        close = ui.button("Cancel").clicked();
                        save = ui
                            .add_enabled(
                                self.recordings
                                    .can_save_key(&credentials.id, &credentials.secret),
                                egui::Button::new("Save in keyring"),
                            )
                            .clicked();
                    });
                });
            if save {
                let id = std::mem::take(&mut credentials.id);
                let secret = std::mem::take(&mut credentials.secret);
                self.credentials = None;
                let job = self.recordings.save_key(id, secret);
                self.run(job, ctx);
            } else if close {
                self.credentials = None;
            }
        }
    }

    fn screenshot(&mut self, ctx: &egui::Context) {
        let Some(path) = &self.screenshot else { return };
        if self.frames >= 6 && self.screenshot_started.is_none() {
            self.screenshot_started = Some(Instant::now());
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot);
        }
        let image = ctx.input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = image {
            if let Err(error) = image::save_buffer(
                path,
                image.as_raw(),
                image.width() as u32,
                image.height() as u32,
                image::ColorType::Rgba8,
            ) {
                eprintln!("actions screenshot could not be saved: {error}");
                std::process::exit(1);
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else if self
            .screenshot_started
            .is_some_and(|started| started.elapsed() > Duration::from_secs(5))
        {
            eprintln!("actions screenshot timed out");
            std::process::exit(1);
        }
        ctx.request_repaint_after(Duration::from_millis(30));
    }
}

impl eframe::App for ActionsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.frames += 1;
        self.refresh(ctx);
        if self.initial_doctor {
            self.initial_doctor = false;
            let job = self.recordings.check_setup();
            self.run(job, ctx);
        }
        if ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
            if self.recordings.forget_confirmation().is_some() {
                self.recordings.keep();
            } else if self.credentials.is_some() {
                self.credentials = None;
            } else {
                self.request_close(ctx);
            }
        }
        if ctx.input(|input| input.viewport().close_requested()) {
            self.request_close(ctx);
        }
        let dialog = self.recordings.forget_confirmation().is_some() || self.credentials.is_some();
        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(color(self.palette.background))
                    .inner_margin(egui::Margin::same(24.0)),
            )
            .show(ctx, |ui| {
                ui.add_enabled_ui(!dialog, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| self.main_view(ui));
                });
            });
        self.dialogs(ctx);
        self.screenshot(ctx);
        ctx.request_repaint_after(model::POLL);
    }
}

/// Open the egui Recordings and Recovery window until it is closed.
pub(super) fn run(screenshot: Option<PathBuf>, doctor: bool) -> Result<()> {
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_app_id("cantrip-actions")
            .with_inner_size([620.0, 760.0])
            .with_min_inner_size([460.0, 400.0])
            .with_title("Cantrip Actions"),
        ..Default::default()
    };
    eframe::run_native(
        "cantrip-actions",
        options,
        Box::new(move |cc| Ok(Box::new(ActionsApp::new(cc, screenshot, doctor)))),
    )
    .map_err(|error| anyhow!("actions window error: {error}"))
}

fn moved_recording_row(
    current: Option<usize>,
    count: usize,
    key: egui::Key,
    page: usize,
) -> Option<usize> {
    let last = count.checked_sub(1)?;
    Some(match key {
        egui::Key::Home => 0,
        egui::Key::End => last,
        egui::Key::ArrowDown => current.map_or(0, |row| row.saturating_add(1).min(last)),
        egui::Key::ArrowUp => current.map_or(last, |row| row.saturating_sub(1)),
        egui::Key::PageDown => current.map_or(0, |row| row.saturating_add(page).min(last)),
        egui::Key::PageUp => current.map_or(last, |row| row.saturating_sub(page)),
        _ => current?.min(last),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_navigation_crosses_virtual_viewports_and_stops_at_boundaries() {
        assert_eq!(
            moved_recording_row(Some(0), 10_000, egui::Key::End, 3),
            Some(9_999)
        );
        assert_eq!(
            moved_recording_row(Some(9_999), 10_000, egui::Key::ArrowDown, 3),
            Some(9_999)
        );
        assert_eq!(
            moved_recording_row(Some(9_999), 10_000, egui::Key::PageUp, 3),
            Some(9_996)
        );
        assert_eq!(
            moved_recording_row(Some(9_996), 10_000, egui::Key::Home, 3),
            Some(0)
        );
        assert_eq!(moved_recording_row(Some(0), 0, egui::Key::End, 3), None);
    }
}
