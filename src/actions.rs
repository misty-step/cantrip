//! Deliberately opened, metadata-only recovery and setup window.

use crate::config::Config;
use crate::ipc::{self, Command, StateKind, StatusSnapshot};
use crate::recovery::{self, Take};
use crate::settings::{apply_theme, color};
use crate::{inject, keys, models, theme};
use anyhow::{anyhow, Context, Result};
use eframe::egui;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_secs(2);

struct Observation {
    status: Option<StatusSnapshot>,
    history: Option<Vec<Take>>,
    retained_audio_bytes: Option<u64>,
    revision: HistoryRevision,
    history_unavailable: bool,
    local_model: bool,
    config_ok: bool,
    key_id: Option<String>,
}

#[derive(Clone, PartialEq, Eq)]
struct HistoryRevision {
    epoch: String,
    event: Option<u64>,
    pending: usize,
}

fn observe(previous: Option<HistoryRevision>) -> Observation {
    let status = ipc::status().ok();
    let revision = HistoryRevision {
        epoch: status
            .as_ref()
            .map_or_else(String::new, |status| status.epoch.clone()),
        event: status
            .as_ref()
            .and_then(|status| status.outcome.as_ref().map(|outcome| outcome.event_id)),
        pending: status
            .as_ref()
            .map_or(0, |status| status.pending_recordings),
    };
    let history = if previous.as_ref() != Some(&revision) {
        Some(if status.is_some() {
            ipc::recordings()
        } else {
            recovery::list()
        })
    } else {
        None
    };
    let retained_audio_bytes = history
        .as_ref()
        .and_then(|history| history.as_ref().ok())
        .and_then(|takes| {
            takes
                .iter()
                .filter(|take| take.audio_available)
                .try_fold(0u64, |total, take| {
                    let bytes = std::fs::metadata(recovery::audio_path(&take.id).ok()?)
                        .ok()?
                        .len();
                    total.checked_add(bytes)
                })
        });
    let config = Config::load();
    Observation {
        local_model: models::installed(&models::PARAKEET_V3_INT8)
            .ok()
            .flatten()
            .is_some(),
        config_ok: config.is_ok(),
        key_id: config.as_ref().ok().and_then(|config| {
            config
                .stt
                .api_key_id
                .clone()
                .or_else(|| config.postproc.api_key_id.clone())
        }),
        status,
        history_unavailable: history.as_ref().is_some_and(Result::is_err),
        history: history.and_then(Result::ok),
        retained_audio_bytes,
        revision,
    }
}

enum Action {
    Command(Command),
    InstallModel { cancel: Arc<AtomicBool> },
    Doctor,
    StoreKey { id: String, secret: String },
    StartDaemon,
}

enum ActionResult {
    CommandAccepted,
    Message(String),
    Diagnosis(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActionLane {
    Command,
    Setup,
    Cancel,
}

fn perform(action: Action) -> Result<ActionResult> {
    let message = match action {
        Action::Command(command) => {
            let reply = ipc::command(command)?;
            anyhow::ensure!(
                reply.ok,
                "{}",
                reply
                    .error
                    .or(reply.message)
                    .unwrap_or_else(|| "Cantrip rejected this action.".to_owned())
            );
            return Ok(ActionResult::CommandAccepted);
        }
        Action::InstallModel { cancel } => {
            if let Err(error) = models::ensure_model(&models::PARAKEET_V3_INT8, Some(&cancel)) {
                if error.is::<models::InstallCancelled>() {
                    return Ok(ActionResult::Message(
                        "Model installation cancelled; the previous model is unchanged.".to_owned(),
                    ));
                }
                return Err(error).context("Installing the local model");
            }
            if ipc::command(Command::Reload).is_ok_and(|reply| reply.ok) {
                "Local model installed; daemon reloaded.".to_owned()
            } else {
                "Local model installed. Reload the daemon after the current operation, or start it below.".to_owned()
            }
        }
        Action::Doctor => {
            let (status, stdout, _) = bounded_output(
                ProcessCommand::new(std::env::current_exe()?).arg("doctor"),
                Duration::from_secs(20),
            )
            .context("Running the setup check")?;
            anyhow::ensure!(
                status.success(),
                "Setup check could not finish. Check configuration in Settings."
            );
            return Ok(ActionResult::Diagnosis(stdout));
        }
        Action::StoreKey { id, mut secret } => {
            // Do not echo provider errors or the supplied value, even to the GUI.
            let result = keys::set(id.trim(), &secret);
            secret.clear();
            result.map_err(|_| anyhow!("Key was not saved. Unlock the desktop keyring and use a nonempty key without whitespace."))?;
            "API key saved in the OS keyring. Use this key ID in Settings; the key is not stored in configuration.".to_owned()
        }
        Action::StartDaemon => start_daemon()?,
    };
    Ok(ActionResult::Message(message))
}

fn completed_action(
    receiver: Option<&Receiver<Result<ActionResult>>>,
) -> Option<Result<ActionResult>> {
    match receiver?.try_recv() {
        Ok(result) => Some(result),
        Err(TryRecvError::Empty) => None,
        Err(TryRecvError::Disconnected) => Some(Err(anyhow!(
            "The background action stopped unexpectedly. Check setup before trying again."
        ))),
    }
}

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

fn drain_output(mut reader: impl Read) -> std::io::Result<String> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let keep = read.min((32 * 1024usize).saturating_sub(bytes.len()));
        bytes.extend_from_slice(&buffer[..keep]);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn bounded_output(
    command: &mut ProcessCommand,
    timeout: Duration,
) -> Result<(std::process::ExitStatus, String, String)> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().context("capturing command output")?;
    let stderr = child
        .stderr
        .take()
        .context("capturing command diagnostics")?;
    let stdout = std::thread::spawn(move || drain_output(stdout));
    let stderr = std::thread::spawn(move || drain_output(stderr));
    let deadline = Instant::now() + timeout;
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(anyhow!(
                    "Command timed out; only the owned command process was stopped"
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(error.into());
            }
        }
    };
    let stdout = stdout
        .join()
        .map_err(|_| anyhow!("Command output reader failed"))??;
    let stderr = stderr
        .join()
        .map_err(|_| anyhow!("Command diagnostic reader failed"))??;
    Ok((result?, stdout, stderr))
}

fn unit_properties() -> Result<String> {
    let (status, stdout, stderr) = bounded_output(
        ProcessCommand::new("systemctl").args([
            "--user",
            "--no-pager",
            "show",
            "cantrip.service",
            "--property=LoadState,ActiveState,Result,ExecMainStatus",
        ]),
        Duration::from_secs(5),
    )?;
    anyhow::ensure!(
        status.success() || stdout.lines().any(|line| line == "LoadState=not-found"),
        "Cannot inspect the Cantrip user service; no competing daemon was started. {}",
        stderr.trim()
    );
    Ok(stdout)
}

fn property<'a>(properties: &'a str, key: &str) -> Option<&'a str> {
    properties.lines().find_map(|line| {
        line.split_once('=')
            .filter(|(name, _)| *name == key)
            .map(|(_, value)| value)
    })
}

fn start_daemon() -> Result<String> {
    if ipc::status().is_ok() {
        return Ok("Cantrip is already connected.".to_owned());
    }
    if inject::executable_in_path("systemctl") {
        let properties = unit_properties()?;
        match property(&properties, "LoadState") {
            Some("not-found") => {}
            Some("loaded") => {
                let (status, _, stderr) = bounded_output(
                    ProcessCommand::new("systemctl").args(["--user", "--no-block", "start", "cantrip.service"]),
                    Duration::from_secs(5),
                )?;
                anyhow::ensure!(status.success(), "Cantrip user service did not accept startup: {}", stderr.trim());
                let deadline = Instant::now() + Duration::from_secs(15);
                loop {
                    if ipc::status().is_ok() { return Ok("Cantrip is connected through its existing user service.".to_owned()); }
                    let properties = unit_properties()?;
                    if property(&properties, "ActiveState") == Some("failed") {
                        anyhow::bail!("Cantrip user service failed: result={}, exit={}. Open Settings to repair configuration, then try Start again.",
                            property(&properties, "Result").unwrap_or("unknown"),
                            property(&properties, "ExecMainStatus").unwrap_or("unknown"));
                    }
                    anyhow::ensure!(Instant::now() < deadline, "The user service accepted startup but Cantrip is not reachable yet. The service remains its owner; no second daemon was started.");
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
            Some(state) => anyhow::bail!("Cantrip user service is {state}; repair or unmask that service before starting. No competing daemon was started."),
            None => anyhow::bail!("Cantrip service ownership is unknown; no competing daemon was started."),
        }
    }
    start_direct_daemon()
}

fn start_direct_daemon() -> Result<String> {
    let mut child = ProcessCommand::new(std::env::current_exe()?)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("Starting the Cantrip process")?;
    let mut stderr = child
        .stderr
        .take()
        .context("Capturing startup diagnostics")?;
    let diagnostics = Arc::new(Mutex::new(Vec::new()));
    let captured = diagnostics.clone();
    let reader = std::thread::spawn(move || {
        let mut buffer = [0; 4096];
        while let Ok(read) = stderr.read(&mut buffer) {
            if read == 0 {
                break;
            }
            if let Ok(mut bytes) = captured.lock() {
                let keep = read.min((32 * 1024usize).saturating_sub(bytes.len()));
                bytes.extend_from_slice(&buffer[..keep]);
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let _ = reader.join();
                let detail = diagnostics
                    .lock()
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                    .unwrap_or_default();
                anyhow::bail!(
                    "Cantrip exited during startup ({status}). {}",
                    detail.trim()
                );
            }
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(error).context("Observing the new Cantrip process");
            }
        }
        if ipc::status().is_ok() {
            // Reap only this process; never restart it. Its existing durable
            // daemon log remains authoritative after this view closes.
            std::thread::spawn(move || {
                let _ = child.wait();
                let _ = reader.join();
            });
            return Ok(
                "Cantrip is connected. It will keep running after this window closes.".to_owned(),
            );
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            let detail = diagnostics
                .lock()
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default();
            anyhow::bail!(
                "The new Cantrip process did not become ready and was stopped. {}",
                detail.trim()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

struct Credentials {
    id: String,
    secret: String,
    focus: bool,
}

struct ActionsApp {
    observation: Option<Observation>,
    poll_result: Option<Receiver<Observation>>,
    last_poll: Instant,
    takes: Vec<Take>,
    pending_indices: Vec<usize>,
    retained_audio_bytes: Option<u64>,
    focus_recording: bool,
    recording_focus: Option<egui::Id>,
    recording_scroll: f32,
    history_revision: Option<HistoryRevision>,
    refresh_history: bool,
    action_result: Option<Receiver<Result<ActionResult>>>,
    busy_message: &'static str,
    setup_result: Option<Receiver<Result<ActionResult>>>,
    setup_message: &'static str,
    cancel_result: Option<Receiver<Result<ActionResult>>>,
    latest_command: ActionLane,
    setup_status: Option<(String, bool)>,
    model_cancel: Option<Arc<AtomicBool>>,
    setup_close_guard: bool,
    close_after_setup: bool,
    message: Option<(String, bool)>,
    selected_id: Option<String>,
    selection_initialized: bool,
    show_history: bool,
    forget: Option<Take>,
    confirm_focus: bool,
    credentials: Option<Credentials>,
    diagnosis: Option<String>,
    initial_doctor: bool,
    palette: theme::Palette,
    screenshot: Option<PathBuf>,
    screenshot_started: Option<Instant>,
    frames: u32,
}

impl ActionsApp {
    fn new(cc: &eframe::CreationContext<'_>, screenshot: Option<PathBuf>, doctor: bool) -> Self {
        let palette = theme::load();
        apply_theme(&cc.egui_ctx, palette);
        Self {
            observation: None,
            poll_result: None,
            last_poll: Instant::now() - POLL,
            takes: Vec::new(),
            pending_indices: Vec::new(),
            retained_audio_bytes: None,
            focus_recording: false,
            recording_focus: None,
            recording_scroll: 0.0,
            history_revision: None,
            refresh_history: true,
            action_result: None,
            busy_message: "",
            setup_result: None,
            setup_message: "",
            cancel_result: None,
            latest_command: ActionLane::Command,
            setup_status: None,
            model_cancel: None,
            setup_close_guard: false,
            close_after_setup: false,
            message: None,
            selected_id: None,
            selection_initialized: false,
            show_history: false,
            forget: None,
            confirm_focus: false,
            credentials: None,
            diagnosis: None,
            initial_doctor: doctor,
            palette,
            screenshot,
            screenshot_started: None,
            frames: 0,
        }
    }

    fn takes(&self) -> &[Take] {
        &self.takes
    }

    fn submit(&mut self, action: Action, message: &'static str, ctx: &egui::Context) {
        let lane = match &action {
            Action::Command(Command::Cancel) => ActionLane::Cancel,
            Action::Command(_) => ActionLane::Command,
            _ => ActionLane::Setup,
        };
        let occupied = match lane {
            ActionLane::Command => self.action_result.is_some(),
            ActionLane::Setup => self.setup_result.is_some(),
            ActionLane::Cancel => self.cancel_result.is_some(),
        };
        if occupied {
            return;
        }
        if lane == ActionLane::Setup {
            self.setup_message = message;
            self.setup_status = None;
            self.setup_close_guard =
                matches!(&action, Action::InstallModel { .. } | Action::StartDaemon);
            if let Action::InstallModel { cancel } = &action {
                self.model_cancel = Some(cancel.clone());
            }
        } else {
            self.latest_command = lane;
            self.busy_message = message;
            self.message = None;
        }
        let (tx, rx) = mpsc::channel();
        let context = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(perform(action));
            context.request_repaint();
        });
        match lane {
            ActionLane::Command => self.action_result = Some(rx),
            ActionLane::Setup => self.setup_result = Some(rx),
            ActionLane::Cancel => self.cancel_result = Some(rx),
        }
    }

    fn refresh(&mut self, ctx: &egui::Context) {
        match self.poll_result.as_ref().map(Receiver::try_recv) {
            Some(Ok(mut observation)) => {
                let history_loaded = observation.history.is_some();
                if let Some(history) = observation.history.take() {
                    self.pending_indices = history
                        .iter()
                        .enumerate()
                        .filter_map(|(index, take)| take.unresolved.then_some(index))
                        .collect();
                    self.retained_audio_bytes = observation.retained_audio_bytes;
                    self.takes = history;
                }
                if observation.history_unavailable {
                    self.refresh_history = true;
                } else {
                    self.history_revision = Some(observation.revision.clone());
                }
                self.observation = Some(observation);
                self.poll_result = None;
                if !self.selection_initialized && history_loaded {
                    self.selected_id = self
                        .takes()
                        .iter()
                        .find(|take| take.unresolved)
                        .map(|take| take.id.clone());
                    self.selection_initialized = true;
                    self.focus_recording = self.selected_id.is_some();
                }
                // A changed or deleted take requires a new confirmation, never
                // an action on whichever take now occupies its list position.
                if self
                    .forget
                    .as_ref()
                    .is_some_and(|confirmed| !self.takes().iter().any(|take| take == confirmed))
                {
                    self.forget = None;
                    self.message = Some((
                        "That recording changed. Review it before choosing Forget again."
                            .to_owned(),
                        true,
                    ));
                }
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.poll_result = None;
                self.refresh_history = true;
                self.last_poll = Instant::now();
                if let Some(observation) = &mut self.observation {
                    observation.status = None;
                    observation.history_unavailable = true;
                }
                self.message = Some((
                    "Status refresh stopped unexpectedly. Live state and saved history could not be checked."
                        .to_owned(),
                    true,
                ));
            }
            _ => {}
        }
        for (lane, result) in [
            (
                ActionLane::Command,
                completed_action(self.action_result.as_ref()),
            ),
            (
                ActionLane::Setup,
                completed_action(self.setup_result.as_ref()),
            ),
            (
                ActionLane::Cancel,
                completed_action(self.cancel_result.as_ref()),
            ),
        ] {
            if let Some(result) = result {
                let mut message = match result {
                    // Acceptance is not live state. Only daemon snapshots describe progress.
                    Ok(ActionResult::CommandAccepted) => None,
                    Ok(ActionResult::Message(message)) => Some((message, false)),
                    Ok(ActionResult::Diagnosis(diagnosis)) => {
                        self.diagnosis = Some(diagnosis);
                        None
                    }
                    Err(error) => Some((format!("{error:#}"), true)),
                };
                match lane {
                    ActionLane::Setup => {
                        self.setup_result = None;
                        self.setup_status = message.take();
                        self.model_cancel = None;
                        self.setup_close_guard = false;
                        if self.close_after_setup
                            && !self
                                .setup_status
                                .as_ref()
                                .is_some_and(|(_, failed)| *failed)
                        {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        self.close_after_setup = false;
                    }
                    ActionLane::Command => self.action_result = None,
                    ActionLane::Cancel => self.cancel_result = None,
                }
                if lane == self.latest_command {
                    self.message = message;
                }
                self.last_poll = Instant::now() - POLL;
                self.refresh_history = true;
            }
        }
        if self.last_poll.elapsed() >= POLL && self.poll_result.is_none() {
            self.last_poll = Instant::now();
            self.palette = theme::load();
            apply_theme(ctx, self.palette);
            let (tx, rx) = mpsc::channel();
            let context = ctx.clone();
            let revision = if std::mem::take(&mut self.refresh_history) {
                None
            } else {
                self.history_revision.clone()
            };
            std::thread::spawn(move || {
                let _ = tx.send(observe(revision));
                context.request_repaint();
            });
            self.poll_result = Some(rx);
        }
    }

    fn request_close(&mut self, ctx: &egui::Context) {
        if self.setup_close_guard {
            self.close_after_setup = true;
            if let Some(cancel) = &self.model_cancel {
                cancel.store(true, Ordering::Relaxed);
                self.setup_message =
                    "Stopping model installation and cleaning temporary files before closing…";
            } else {
                self.setup_message = "Finishing the bounded startup check before closing…";
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        } else {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
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
        let current = if self.show_history {
            self.takes
                .iter()
                .position(|take| Some(take.id.as_str()) == self.selected_id.as_deref())
        } else {
            self.pending_indices.iter().position(|index| {
                Some(self.takes[*index].id.as_str()) == self.selected_id.as_deref()
            })
        };
        let row = moved_recording_row(
            current,
            count,
            key,
            (180.0 / row_stride).floor().max(1.0) as usize,
        )?;
        let index = if self.show_history {
            row
        } else {
            self.pending_indices[row]
        };
        self.selected_id = Some(self.takes[index].id.clone());
        self.focus_recording = true;
        Some(row)
    }

    fn main_view(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Cantrip");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Settings").clicked() {
                    if let Err(error) = launch("settings") {
                        self.message = Some((error.to_string(), true));
                    }
                }
            });
        });
        ui.label(egui::RichText::new("Saved speech, deliberate actions.").weak());
        ui.add_space(12.0);
        self.status_view(ui);
        if let Some((message, attention)) = &self.message {
            ui.colored_label(
                color(if *attention {
                    self.palette.attention
                } else {
                    self.palette.foreground
                }),
                message,
            );
        }
        if self.action_result.is_some() {
            ui.label(self.busy_message);
        }
        if self.setup_result.is_some() {
            ui.label(self.setup_message);
        }
        if self.cancel_result.is_some() {
            ui.label("Requesting cancellation…");
        }
        ui.add_space(16.0);
        ui.heading("Recordings");
        ui.label("Only recording details are shown here. Copying replaces your clipboard; nothing is typed into another app.");
        ui.add_space(8.0);
        if ui.button("Refresh recordings").clicked() {
            self.refresh_history = true;
            self.last_poll = Instant::now() - POLL;
        }
        if let Some(bytes) = self.retained_audio_bytes.filter(|bytes| *bytes > 0) {
            ui.label(format!("{:.1} MiB of retained audio. Kept until recovery or explicit Forget; never automatically deleted.", bytes as f64 / 1_048_576.0));
        }
        ui.checkbox(&mut self.show_history, "Include completed history");
        if self.observation.is_none() {
            ui.label("Loading saved recordings…");
        } else if self
            .observation
            .as_ref()
            .is_some_and(|observation| observation.history_unavailable)
        {
            ui.colored_label(
                color(self.palette.attention),
                "Saved history could not be refreshed. Displayed details may be out of date. Refresh recordings or check setup.",
            );
        } else if self.takes.is_empty() {
            ui.label("No saved recordings yet.");
        } else if self.pending_indices.is_empty() && !self.show_history {
            ui.label(
                "Nothing waiting to recover. Include completed history to view saved transcripts.",
            );
        }
        let mut selected = None;
        let mut focused = false;
        let row_count = if self.show_history {
            self.takes.len()
        } else {
            self.pending_indices.len()
        };
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
        let area = scroll.show_rows(ui, row_height, row_count, |ui, range| {
            for row in range {
                let index = if self.show_history {
                    row
                } else {
                    self.pending_indices[row]
                };
                let take = &self.takes[index];
                let label = format!(
                    "{}   {}\n{}; {}{}",
                    take_time(take.created_at_unix_ms),
                    take_duration(take.duration_ms),
                    completeness(take),
                    if take.audio_available {
                        "audio saved"
                    } else {
                        "no audio"
                    },
                    if take.unresolved { "; pending" } else { "" }
                );
                ui.push_id(&take.id, |ui| {
                    let response = ui.selectable_label(
                        self.selected_id.as_deref() == Some(take.id.as_str()),
                        label,
                    );
                    if self.focus_recording && self.selected_id.as_deref() == Some(take.id.as_str())
                    {
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
            self.selected_id = Some(selected);
        }
        if let Some(take) = self
            .takes()
            .iter()
            .find(|take| Some(take.id.as_str()) == self.selected_id.as_deref())
            .cloned()
        {
            ui.add_space(12.0);
            self.take_view(ui, &take);
        } else if self.selected_id.is_some() {
            ui.label("This recording is no longer available. Choose another recording explicitly.");
        }
        ui.add_space(20.0);
        self.setup_view(ui);
        ui.add_space(14.0);
        ui.label(egui::RichText::new("Tab between controls. Arrow keys, Page Up/Down, Home/End browse every recording. Enter or Space chooses; Escape closes without deleting.").weak().small());
    }

    fn status_view(&mut self, ui: &mut egui::Ui) {
        let mut command = None;
        if let Some(status) = self
            .observation
            .as_ref()
            .and_then(|observation| observation.status.as_ref())
        {
            let caption = status.stage.as_ref().map_or_else(
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
            ui.label(egui::RichText::new(caption).size(18.0));
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(
                        status.capabilities.stop && self.action_result.is_none(),
                        egui::Button::new("Stop recording"),
                    )
                    .clicked()
                {
                    command = Some(Command::Stop);
                }
                if ui
                    .add_enabled(
                        status.capabilities.cancel && self.cancel_result.is_none(),
                        egui::Button::new("Cancel — do not deliver"),
                    )
                    .clicked()
                {
                    command = Some(Command::Cancel);
                }
            });
            if let Some(outcome) = &status.outcome {
                if !outcome.dismissed {
                    ui.label(&outcome.message);
                    if let Some(error) = &outcome.error {
                        ui.colored_label(color(self.palette.attention), error);
                    }
                    if ui
                        .add_enabled(
                            status.capabilities.dismiss && self.action_result.is_none(),
                            egui::Button::new("Dismiss outcome"),
                        )
                        .clicked()
                    {
                        command = Some(Command::Dismiss {
                            event_id: Some(outcome.event_id),
                        });
                    }
                }
            }
            if let Some(notice) = &status.notice {
                ui.label(&notice.message);
                if ui
                    .add_enabled(
                        status.capabilities.dismiss && self.action_result.is_none(),
                        egui::Button::new("Dismiss notice"),
                    )
                    .clicked()
                {
                    command = Some(Command::Dismiss {
                        event_id: Some(notice.event_id),
                    });
                }
            }
        } else {
            ui.label(
                egui::RichText::new(if self.observation.is_none() {
                    "Connecting to Cantrip…"
                } else {
                    "Cantrip is unreachable"
                })
                .size(18.0),
            );
            if self.observation.is_some() {
                ui.label("Live recording and delivery status are unknown. Saved metadata below is read from disk; actions need a daemon connection.");
            }
        }
        if let Some(command) = command {
            self.submit(Action::Command(command), "Applying action…", ui.ctx());
        }
    }

    fn take_view(&mut self, ui: &mut egui::Ui, take: &Take) {
        let status = self
            .observation
            .as_ref()
            .and_then(|observation| observation.status.as_ref());
        let copy = status.is_some_and(|status| status.capabilities.copy) && take.text_available;
        let local = status
            .is_some_and(|status| status.capabilities.recover && status.capabilities.local_model)
            && take.audio_available;
        let remote = status.is_some_and(|status| {
            status.capabilities.recover && status.capabilities.remote_configured
        }) && take.audio_available;
        let forget = status.is_some_and(|status| status.state == StateKind::Idle)
            && (take.audio_available || take.unresolved);
        ui.separator();
        ui.label(egui::RichText::new(take_time(take.created_at_unix_ms)).strong());
        ui.label(format!(
            "Duration: {}\nTranscript: {}\nAudio: {}",
            take_duration(take.duration_ms),
            completeness(take),
            if take.audio_available {
                "saved on this machine"
            } else {
                "not retained"
            }
        ));
        ui.label(
            egui::RichText::new(format!("Recording ID: {}", take.id))
                .weak()
                .small(),
        );
        let mut command = None;
        let history_available = self
            .observation
            .as_ref()
            .is_some_and(|observation| !observation.history_unavailable);
        ui.add_enabled_ui(self.action_result.is_none() && history_available, |ui| {
            if ui.add_enabled(copy, egui::Button::new("Copy this transcript")).clicked() { command = Some(Command::Copy { id: take.id.clone() }); }
            if take.partial && take.text_available { ui.label("Saved text is partial. Recover the whole take before replacing text in your document."); }
            if ui.add_enabled(local, egui::Button::new("Recover locally to clipboard")).clicked() {
                command = Some(Command::Recover { id: Some(take.id.clone()), local: true, clipboard: true });
            }
            ui.label(egui::RichText::new("Uses installed Parakeet. Audio stays here; no cloud cleanup.").weak().small());
            if ui.add_enabled(remote, egui::Button::new("Recover with configured provider to clipboard")).clicked() {
                command = Some(Command::Recover { id: Some(take.id.clone()), local: false, clipboard: true });
            }
            ui.label(egui::RichText::new("Sends audio to your configured STT endpoint; configured cleanup may also run. No automatic cloud fallback.").weak().small());
            if !take.audio_available { ui.label("No audio is available to transcribe again."); }
            ui.add_space(6.0);
            if ui.add_enabled(forget, egui::Button::new("Forget retained recording…")).clicked() {
                self.forget = Some(take.clone());
                self.confirm_focus = true;
            }
        });
        if let Some(command) = command {
            self.submit(
                Action::Command(command),
                "Applying recording action…",
                ui.ctx(),
            );
        }
    }

    fn setup_view(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.heading("Setup and checks");
        if let Some((message, attention)) = &self.setup_status {
            ui.colored_label(
                color(if *attention {
                    self.palette.attention
                } else {
                    self.palette.foreground
                }),
                message,
            );
        }
        if let Some(cancel) = &self.model_cancel {
            if ui
                .add_enabled(
                    !cancel.load(Ordering::Relaxed),
                    egui::Button::new("Cancel model installation"),
                )
                .clicked()
            {
                cancel.store(true, Ordering::Relaxed);
                self.setup_message = "Stopping model installation and cleaning temporary files…";
            }
        }
        let local_model = self
            .observation
            .as_ref()
            .is_some_and(|observation| observation.local_model);
        let online = self
            .observation
            .as_ref()
            .is_some_and(|observation| observation.status.is_some());
        if !local_model {
            ui.label("Local recovery needs the Parakeet model. Installing it downloads model files, not your recordings.");
        }
        if self
            .observation
            .as_ref()
            .is_some_and(|observation| !observation.config_ok)
        {
            ui.colored_label(color(self.palette.attention), "Configuration needs attention. Open Settings to correct it; retained recordings are unchanged.");
        }
        let mut action = None;
        ui.add_enabled_ui(self.setup_result.is_none(), |ui| {
            ui.horizontal_wrapped(|ui| {
                if !local_model && ui.button("Install local model").clicked() {
                    action = Some((
                        Action::InstallModel {
                            cancel: Arc::new(AtomicBool::new(false)),
                        },
                        "Downloading and verifying the local model…",
                    ));
                }
                if !online && ui.button("Start Cantrip").clicked() {
                    action = Some((Action::StartDaemon, "Starting Cantrip…"));
                }
                if online
                    && ui
                        .add_enabled(
                            self.action_result.is_none(),
                            egui::Button::new("Reload configuration"),
                        )
                        .clicked()
                {
                    action = Some((Action::Command(Command::Reload), "Reloading configuration…"));
                }
                if ui.button("Check setup").clicked() {
                    action = Some((Action::Doctor, "Checking setup…"));
                }
                if ui.button("API key…").clicked() {
                    self.credentials = Some(Credentials {
                        id: self
                            .observation
                            .as_ref()
                            .and_then(|observation| observation.key_id.clone())
                            .unwrap_or_default(),
                        secret: String::new(),
                        focus: true,
                    });
                }
            });
        });
        if let Some((action, message)) = action {
            self.submit(action, message, ui.ctx());
        }
        if let Some(diagnosis) = &self.diagnosis {
            ui.add_space(8.0);
            ui.label(egui::RichText::new(diagnosis).monospace());
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        if let Some(take) = self.forget.clone() {
            let enabled = self.action_result.is_none()
                && self.observation.as_ref().is_some_and(|observation| {
                    !observation.history_unavailable
                        && observation
                            .status
                            .as_ref()
                            .is_some_and(|status| status.state == StateKind::Idle)
                });
            let mut confirmed = false;
            let mut close = false;
            egui::Window::new("Forget this recording?").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                ui.set_max_width(390.0);
                ui.label(take_time(take.created_at_unix_ms));
                ui.label(format!("{}\nRecording ID: {}", take_duration(take.duration_ms), take.id));
                ui.label("Permanently delete this take’s retained audio and any incomplete transcript, and clear its pending recovery marker. Complete archived transcript text is kept. This cannot be undone.");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let keep = ui.button("Keep recording");
                    if self.confirm_focus { keep.request_focus(); self.confirm_focus = false; }
                    close = keep.clicked();
                    confirmed = ui.add_enabled(enabled, egui::Button::new("Forget this recording")).clicked();
                });
            });
            if close || confirmed {
                self.forget = None;
            }
            if confirmed {
                self.submit(
                    Action::Command(Command::Forget { id: take.id }),
                    "Forgetting this recording…",
                    ctx,
                );
            }
        }
        if let Some(credentials) = &mut self.credentials {
            let mut save = false;
            let mut close = false;
            egui::Window::new("Save an API key").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                ui.set_max_width(390.0);
                ui.label("Store a provider key in the OS keyring. Use the same key ID in Settings. Saving replaces an existing key with this ID.");
                ui.label("Key ID");
                let id = ui.text_edit_singleline(&mut credentials.id);
                if credentials.focus { id.request_focus(); credentials.focus = false; }
                ui.label("API key");
                ui.add(egui::TextEdit::singleline(&mut credentials.secret).password(true).desired_width(f32::INFINITY));
                ui.horizontal(|ui| {
                    close = ui.button("Cancel").clicked();
                    save = ui.add_enabled(!credentials.id.trim().is_empty() && !credentials.secret.is_empty() && self.setup_result.is_none(), egui::Button::new("Save in keyring")).clicked();
                });
            });
            if save {
                let id = std::mem::take(&mut credentials.id);
                let secret = std::mem::take(&mut credentials.secret);
                self.credentials = None;
                self.submit(
                    Action::StoreKey { id, secret },
                    "Saving the key in the OS keyring…",
                    ctx,
                );
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
            self.submit(Action::Doctor, "Checking setup…", ctx);
        }
        if ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
            if self.forget.is_some() {
                self.forget = None;
            } else if self.credentials.is_some() {
                self.credentials = None;
            } else {
                self.request_close(ctx);
            }
        }
        if ctx.input(|input| input.viewport().close_requested()) {
            self.request_close(ctx);
        }
        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(color(self.palette.background))
                    .inner_margin(egui::Margin::same(24.0)),
            )
            .show(ctx, |ui| {
                ui.add_enabled_ui(self.forget.is_none() && self.credentials.is_none(), |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| self.main_view(ui));
                });
            });
        self.dialogs(ctx);
        self.screenshot(ctx);
        ctx.request_repaint_after(POLL);
    }
}

fn completeness(take: &Take) -> &'static str {
    if take.partial && take.text_available {
        "partial text saved"
    } else if take.text_available {
        "complete text saved"
    } else {
        "no transcript saved"
    }
}

/// Human-readable local capture time, shared by the explicit metadata CLI.
pub fn take_time(unix_ms: u64) -> String {
    let Ok(seconds) = libc::time_t::try_from(unix_ms / 1000) else {
        return "Capture time unavailable".to_owned();
    };
    let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
    // localtime_r writes only our stack allocation and uses the system timezone.
    let result = unsafe { libc::localtime_r(&seconds, local.as_mut_ptr()) };
    if result.is_null() {
        return "Capture time unavailable".to_owned();
    }
    let local = unsafe { local.assume_init() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} local",
        local.tm_year + 1900,
        local.tm_mon + 1,
        local.tm_mday,
        local.tm_hour,
        local.tm_min,
        local.tm_sec
    )
}

pub fn take_duration(duration_ms: Option<u64>) -> String {
    match duration_ms {
        Some(ms) if ms < 1000 => format!("{ms} ms"),
        Some(ms) => format!("{}m {:02}s", ms / 60_000, (ms / 1000) % 60),
        None => "duration unknown".to_owned(),
    }
}

pub fn run(screenshot: Option<PathBuf>, doctor: bool) -> Result<()> {
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
