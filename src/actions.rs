//! Deliberately opened, metadata-only recovery and setup window.

use crate::config::Config;
use crate::ipc::{self, Command, StateKind, StatusSnapshot};
use crate::pipeline::Stage;
use crate::recovery::{self, Take};
use crate::theme::Tones;
use crate::ui::{self, color, Stamp, Tone};
use crate::{fonts, inject, keys, models, theme};
use anyhow::{anyhow, Context, Result};
use eframe::egui::{self, RichText};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_secs(2);
/// Readable column; the window reflows below it down to its minimum width.
const COLUMN_WIDTH: f32 = 720.0;
/// Uniform recording row height, so virtualised rows and paging stay exact.
const ROW_HEIGHT: f32 = 48.0;
const LIST_HEIGHT: f32 = 300.0;
const DIALOG_WIDTH: f32 = 376.0;
const SCROLL_GUTTER: f32 = 12.0;

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
        ui::setup(&cc.egui_ctx, palette);
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
            let palette = theme::load();
            if palette != self.palette {
                self.palette = palette;
                ui::apply(ctx, palette);
            }
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
            (LIST_HEIGHT / row_stride).floor().max(1.0) as usize,
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

    fn open_settings(&mut self) {
        if let Err(error) = launch("settings") {
            self.message = Some((error.to_string(), true));
        }
    }

    fn open_credentials(&mut self) {
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

    fn main_view(&mut self, ui: &mut egui::Ui) {
        let tones = self.palette.tones();
        ui.horizontal(|ui| {
            ui::wordmark(ui, &tones, "cantrip");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if button(ui, &tones, Tone::Secondary, "Settings", true).clicked() {
                    self.open_settings();
                }
            });
        });
        ui.add_space(12.0);
        self.status_view(ui, &tones);
        if let Some((message, attention)) = &self.message {
            ui.add_space(4.0);
            ui.label(RichText::new(message).color(color(if *attention {
                tones.attention
            } else {
                tones.text
            })));
        }
        ui.add_space(24.0);
        self.recordings_view(ui, &tones);
        if let Some(take) = self
            .takes()
            .iter()
            .find(|take| Some(take.id.as_str()) == self.selected_id.as_deref())
            .cloned()
        {
            ui.add_space(12.0);
            self.take_view(ui, &tones, &take);
        } else if self.selected_id.is_some() {
            ui.add_space(8.0);
            ui.label(ui::muted(
                "This recording is no longer available. Choose another recording explicitly.",
                &tones,
            ));
        }
        ui.add_space(24.0);
        self.setup_view(ui, &tones);
        ui.add_space(16.0);
        ui.label(ui::faint(
            "↑ ↓ browse recordings · Enter or Space chooses · Esc closes without deleting",
            &tones,
        ));
    }

    fn hero(&self) -> Hero {
        let Some(observation) = &self.observation else {
            return Hero {
                stamp: Stamp::Rest,
                title: "Connecting to Cantrip…".to_owned(),
                elapsed: None,
                sentence: "Reading live status and saved recordings.".to_owned(),
            };
        };
        let Some(status) = &observation.status else {
            return Hero {
                stamp: Stamp::Attention,
                title: "Cantrip isn't running".to_owned(),
                elapsed: None,
                sentence: "Live status is unknown. Saved recordings below are read from disk; actions need Cantrip running.".to_owned(),
            };
        };
        if let Some(stage) = &status.stage {
            return Hero {
                stamp: Stamp::Live,
                title: stage_title(stage),
                elapsed: None,
                sentence: "Cantrip is working on the latest recording.".to_owned(),
            };
        }
        match &status.state {
            StateKind::Idle => Hero {
                stamp: Stamp::Idle,
                title: "Ready".to_owned(),
                elapsed: None,
                sentence: match status.pending_recordings {
                    0 => "Nothing is waiting.".to_owned(),
                    1 => "1 recording is waiting for a decision.".to_owned(),
                    count => format!("{count} recordings are waiting for a decision."),
                },
            },
            StateKind::Recording if status.signal.is_none() => Hero {
                stamp: Stamp::Live,
                title: "Starting microphone…".to_owned(),
                elapsed: None,
                sentence: "Recording begins as soon as the microphone answers.".to_owned(),
            },
            StateKind::Recording => Hero {
                stamp: Stamp::Live,
                title: "Recording".to_owned(),
                elapsed: Some(format!(
                    "{}:{:02}",
                    status.elapsed / 60,
                    status.elapsed % 60
                )),
                sentence: "Stop to transcribe and deliver the text.".to_owned(),
            },
            StateKind::Processing => Hero {
                stamp: Stamp::Live,
                title: "Working".to_owned(),
                elapsed: None,
                sentence: "Cantrip is working on the latest recording.".to_owned(),
            },
            StateKind::Unknown(_) => Hero {
                stamp: Stamp::Attention,
                title: "State unknown".to_owned(),
                elapsed: None,
                sentence: "Cantrip reported a state this window doesn't recognise.".to_owned(),
            },
        }
    }

    fn status_view(&mut self, ui: &mut egui::Ui, tones: &Tones) {
        let hero = self.hero();
        let status = self
            .observation
            .as_ref()
            .and_then(|observation| observation.status.as_ref());
        let offline = self.observation.is_some() && status.is_none();
        let capabilities = status.map(|status| status.capabilities).unwrap_or_default();
        let mut command = None;
        let mut start = false;
        ui::card(tones).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.add_space(6.0);
                    ui::stamp(ui, tones, hero.stamp);
                });
                ui.add_space(6.0);
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(ui::hero(hero.title, tones));
                        if let Some(elapsed) = hero.elapsed {
                            ui.label(ui::data(elapsed, tones).size(15.0));
                        }
                    });
                    ui.label(ui::muted(hero.sentence, tones));
                    let stop = capabilities.stop;
                    let cancel = capabilities.cancel;
                    if stop || cancel || offline {
                        ui.add_space(4.0);
                        ui.horizontal_wrapped(|ui| {
                            if offline
                                && button(
                                    ui,
                                    tones,
                                    Tone::Primary,
                                    "Start Cantrip",
                                    self.setup_result.is_none(),
                                )
                                .clicked()
                            {
                                start = true;
                            }
                            if stop
                                && button(
                                    ui,
                                    tones,
                                    Tone::Primary,
                                    "Stop recording",
                                    self.action_result.is_none(),
                                )
                                .clicked()
                            {
                                command = Some(Command::Stop);
                            }
                            if cancel
                                && button(
                                    ui,
                                    tones,
                                    Tone::Danger,
                                    "Cancel without delivering",
                                    self.cancel_result.is_none(),
                                )
                                .clicked()
                            {
                                command = Some(Command::Cancel);
                            }
                        });
                    }
                    for message in [
                        self.action_result.is_some().then_some(self.busy_message),
                        self.cancel_result
                            .is_some()
                            .then_some("Requesting cancellation…"),
                        self.setup_result.is_some().then_some(self.setup_message),
                    ]
                    .into_iter()
                    .flatten()
                    {
                        ui.horizontal(|ui| {
                            ui.add(egui::Spinner::new().size(13.0).color(color(tones.accent)));
                            ui.label(ui::muted(message, tones));
                        });
                    }
                });
            });
        });
        if let Some(status) = status {
            let dismiss = capabilities.dismiss.then_some(self.action_result.is_none());
            if let Some(outcome) = status.outcome.as_ref().filter(|outcome| !outcome.dismissed) {
                ui.add_space(8.0);
                let frame = if outcome.needs_attention() {
                    ui::attention_card(tones)
                } else {
                    ui::card(tones)
                };
                if banner(
                    ui,
                    tones,
                    frame,
                    &outcome.message,
                    outcome.error.as_deref(),
                    dismiss,
                ) {
                    command = Some(Command::Dismiss {
                        event_id: Some(outcome.event_id),
                    });
                }
            }
            if let Some(notice) = &status.notice {
                ui.add_space(8.0);
                if banner(ui, tones, ui::card(tones), &notice.message, None, dismiss) {
                    command = Some(Command::Dismiss {
                        event_id: Some(notice.event_id),
                    });
                }
            }
        }
        if start {
            self.submit(Action::StartDaemon, "Starting Cantrip…", ui.ctx());
        }
        if let Some(command) = command {
            self.submit(Action::Command(command), "Applying action…", ui.ctx());
        }
    }

    fn recordings_view(&mut self, ui: &mut egui::Ui, tones: &Tones) {
        let loaded = self.observation.is_some();
        let (waiting, all) = if loaded {
            (
                format!("Waiting {}", self.pending_indices.len()),
                format!("All {}", self.takes.len()),
            )
        } else {
            ("Waiting".to_owned(), "All".to_owned())
        };
        let unavailable = self
            .observation
            .as_ref()
            .is_some_and(|observation| observation.history_unavailable);
        let mut refresh = false;
        ui.horizontal(|ui| {
            ui.label(ui::heading("Recordings", tones));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui::segmented(
                    ui,
                    tones,
                    &mut self.show_history,
                    &[(false, waiting.as_str()), (true, all.as_str())],
                );
                if loaded && !unavailable {
                    refresh = button(ui, tones, Tone::Quiet, "Refresh", true).clicked();
                }
            });
        });
        if let Some(bytes) = self.retained_audio_bytes.filter(|bytes| *bytes > 0) {
            ui.label(ui::muted(
                format!(
                    "{:.1} MiB of audio kept on this computer. Kept until you forget it.",
                    bytes as f64 / 1_048_576.0
                ),
                tones,
            ));
        }
        ui.label(ui::faint(
            "Only recording details appear here. Copy replaces your clipboard; nothing is typed into another app.",
            tones,
        ));
        ui.add_space(4.0);
        if self.observation.is_none() {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(13.0).color(color(tones.accent)));
                ui.label(ui::muted("Loading saved recordings…", tones));
            });
        } else if unavailable {
            ui::attention_card(tones).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                    refresh = button(ui, tones, Tone::Secondary, "Refresh", true)
                        .clicked();
                    ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                        ui.label(
                            "Saved history could not be refreshed. Details shown may be out of date.",
                        );
                    });
                });
            });
            ui.add_space(4.0);
        } else if self.takes.is_empty() {
            ui.label(ui::muted(
                "No saved recordings yet. Every stopped take appears here.",
                tones,
            ));
        } else if self.pending_indices.is_empty() && !self.show_history {
            ui.label(ui::muted(
                "Nothing is waiting. Choose All to browse saved takes.",
                tones,
            ));
        }
        if refresh {
            self.refresh_history = true;
            self.last_poll = Instant::now() - POLL;
        }
        let row_count = if self.show_history {
            self.takes.len()
        } else {
            self.pending_indices.len()
        };
        if row_count == 0 {
            return;
        }
        let row_stride = ROW_HEIGHT + ui.spacing().item_spacing.y;
        let navigating_to = self.navigate_recordings(ui, row_count, row_stride);
        let days = CalendarDays::now();
        let mut selected = None;
        let mut focused = false;
        let mut recording_focus = self.recording_focus;
        ui::card(tones)
            .inner_margin(egui::Margin::same(6.0))
            .show(ui, |ui| {
                let mut scroll = egui::ScrollArea::vertical()
                    .id_salt("recordings")
                    .auto_shrink([false, true])
                    .max_height(LIST_HEIGHT);
                if let Some(row) = navigating_to {
                    let top = row as f32 * row_stride;
                    let offset = if top < self.recording_scroll {
                        top
                    } else if top + ROW_HEIGHT > self.recording_scroll + LIST_HEIGHT {
                        top + ROW_HEIGHT - LIST_HEIGHT
                    } else {
                        self.recording_scroll
                    };
                    scroll = scroll.vertical_scroll_offset(offset.max(0.0));
                }
                let area = scroll.show_rows(ui, ROW_HEIGHT, row_count, |ui, range| {
                    for row in range {
                        let index = if self.show_history {
                            row
                        } else {
                            self.pending_indices[row]
                        };
                        let take = &self.takes[index];
                        let is_selected = self.selected_id.as_deref() == Some(take.id.as_str());
                        ui.push_id(&take.id, |ui| {
                            let response =
                                take_row(ui, tones, take, is_selected, self.show_history, &days);
                            if self.focus_recording && is_selected {
                                response.request_focus();
                                // Present the moved focus now, not on the next poll.
                                ui.ctx().request_repaint();
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
            });
        self.recording_focus = recording_focus;
        if focused {
            self.focus_recording = false;
        }
        if let Some(selected) = selected {
            self.selected_id = Some(selected);
        }
    }

    fn take_view(&mut self, ui: &mut egui::Ui, tones: &Tones, take: &Take) {
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
        let online = status.is_some();
        let history_available = self
            .observation
            .as_ref()
            .is_some_and(|observation| !observation.history_unavailable);
        let days = CalendarDays::now();
        let mut command = None;
        let mut confirm_forget = false;
        ui::card(tones).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                RichText::new(detail_time(take.created_at_unix_ms, &days))
                    .family(fonts::semibold())
                    .size(16.0)
                    .color(color(tones.text)),
            );
            ui.label(
                ui::data(
                    format!("{} · {}", short_duration(take.duration_ms), take.id),
                    tones,
                )
                .size(12.0)
                .color(color(tones.text_faint)),
            );
            ui.add_space(6.0);
            egui::Grid::new("take-facts")
                .num_columns(2)
                .spacing([28.0, 6.0])
                .min_row_height(20.0)
                .show(ui, |ui| {
                    ui.label(ui::muted("Transcript", tones));
                    ui.label(if take.partial && take.text_available {
                        "Partial"
                    } else if take.text_available {
                        "Complete"
                    } else {
                        "None"
                    });
                    ui.end_row();
                    ui.label(ui::muted("Audio", tones));
                    ui.label(if take.audio_available {
                        "Kept on this computer"
                    } else {
                        "Not kept"
                    });
                    ui.end_row();
                    ui.label(ui::muted("Status", tones));
                    ui.label(if take.unresolved {
                        "Waiting for a decision"
                    } else {
                        "Resolved"
                    });
                    ui.end_row();
                });
            ui.add_space(10.0);
            if !online {
                ui.label(ui::faint(
                    "Copy, recovery and Forget need Cantrip running.",
                    tones,
                ));
            }
            ui.add_enabled_ui(self.action_result.is_none() && history_available, |ui| {
                if copy || local || remote {
                    ui.horizontal_wrapped(|ui| {
                        let mut primary = true;
                        let mut tone = || {
                            if std::mem::take(&mut primary) {
                                Tone::Primary
                            } else {
                                Tone::Secondary
                            }
                        };
                        if copy
                            && button(ui, tones, tone(), "Copy transcript", true)
                                .clicked()
                        {
                            command = Some(Command::Copy {
                                id: take.id.clone(),
                            });
                        }
                        if local
                            && button(ui, tones, tone(), "Recover locally", true)
                                .clicked()
                        {
                            command = Some(Command::Recover {
                                id: Some(take.id.clone()),
                                local: true,
                                clipboard: true,
                            });
                        }
                        if remote
                            && button(ui, tones, Tone::Secondary, "Recover with provider", true)
                                .clicked()
                        {
                            command = Some(Command::Recover {
                                id: Some(take.id.clone()),
                                local: false,
                                clipboard: true,
                            });
                        }
                    });
                }
                if take.partial && take.text_available {
                    ui.label(RichText::new("Saved text is partial. Recover the whole take before replacing text in your document.").color(color(tones.attention)));
                }
                if !take.audio_available {
                    ui.label(ui::muted(
                        "No audio is available to transcribe again.",
                        tones,
                    ));
                }
                if local {
                    ui.label(ui::faint("Recover locally transcribes again with installed Parakeet and copies the result. Audio stays here; no cloud cleanup.", tones));
                }
                if remote {
                    ui.label(ui::faint("Recover with provider sends audio to your configured STT endpoint and copies the result; configured cleanup may also run. No automatic cloud fallback.", tones));
                }
                if forget {
                    ui.add_space(4.0);
                    ui.separator();
                    ui.add_space(2.0);
                    if button(ui, tones, Tone::Danger, "Forget recording…", true)
                        .clicked()
                    {
                        confirm_forget = true;
                    }
                    ui.label(ui::faint(
                        "Deletes retained audio and incomplete text. Complete transcript text is kept.",
                        tones,
                    ));
                }
            });
        });
        if confirm_forget {
            self.forget = Some(take.clone());
            self.confirm_focus = true;
        }
        if let Some(command) = command {
            self.submit(
                Action::Command(command),
                "Applying recording action…",
                ui.ctx(),
            );
        }
    }

    fn setup_view(&mut self, ui: &mut egui::Ui, tones: &Tones) {
        let mut action = None;
        let mut open_settings = false;
        let mut open_credentials = false;
        ui.label(ui::heading("Setup", tones));
        ui.add_space(4.0);
        ui::card(tones).show(ui, |ui| {
            ui.set_width(ui.available_width());
            if let Some((message, attention)) = &self.setup_status {
                ui.label(RichText::new(message).color(color(if *attention {
                    tones.attention
                } else {
                    tones.text
                })));
                ui.separator();
            }
            let idle = self.setup_result.is_none();
            if let Some(observation) = &self.observation {
                let online = observation.status.is_some();
                setup_row(ui, tones, "Local speech model", |ui| {
                    if let Some(cancel) = &self.model_cancel {
                        let cancelled = cancel.load(Ordering::Relaxed);
                        if button(ui, tones, Tone::Secondary, "Cancel installation", !cancelled)
                            .clicked()
                        {
                            cancel.store(true, Ordering::Relaxed);
                            self.setup_message =
                                "Stopping model installation and cleaning temporary files…";
                        }
                        ui.label(ui::muted(
                            if cancelled { "Stopping…" } else { "Installing…" },
                            tones,
                        ));
                        ui.add(egui::Spinner::new().size(13.0).color(color(tones.accent)));
                    } else if observation.local_model {
                        ui.label(ui::muted("Installed", tones));
                    } else {
                        if button(ui, tones, Tone::Primary, "Install local model", idle)
                            .clicked()
                        {
                            action = Some((
                                Action::InstallModel {
                                    cancel: Arc::new(AtomicBool::new(false)),
                                },
                                "Downloading and verifying the local model…",
                            ));
                        }
                        ui.label(ui::muted("Not installed", tones));
                    }
                });
                if !observation.local_model && self.model_cancel.is_none() {
                    ui.label(ui::faint(
                        "Local recovery needs the Parakeet model. Downloads model files, not your recordings.",
                        tones,
                    ));
                }
                ui.separator();
                setup_row(ui, tones, "Cantrip", |ui| {
                    if online {
                        let reload = idle && self.action_result.is_none();
                        if button(ui, tones, Tone::Secondary, "Reload configuration", reload)
                            .clicked()
                        {
                            action = Some((
                                Action::Command(Command::Reload),
                                "Reloading configuration…",
                            ));
                        }
                        ui.label(ui::muted("Running", tones));
                    } else {
                        if button(ui, tones, Tone::Secondary, "Start Cantrip", idle)
                            .clicked()
                        {
                            action = Some((Action::StartDaemon, "Starting Cantrip…"));
                        }
                        ui.label(
                            RichText::new("Not running").color(color(tones.attention)),
                        );
                    }
                });
                ui.separator();
                setup_row(ui, tones, "Configuration", |ui| {
                    if observation.config_ok {
                        ui.label(ui::muted("Valid", tones));
                    } else {
                        open_settings = button(ui, tones, Tone::Secondary, "Open Settings", idle)
                            .clicked();
                        ui.label(
                            RichText::new("Needs attention").color(color(tones.attention)),
                        );
                    }
                });
                if !observation.config_ok {
                    ui.label(ui::faint(
                        "Correct it in Settings; retained recordings are unchanged.",
                        tones,
                    ));
                }
                ui.separator();
                setup_row(ui, tones, "API keys", |ui| {
                    open_credentials = button(ui, tones, Tone::Secondary, "Store API key…", idle)
                        .clicked();
                    if let Some(key_id) = &observation.key_id {
                        ui.label(
                            ui::data(key_id, tones).color(color(tones.text_muted)),
                        );
                    }
                });
                ui.separator();
            } else {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(13.0).color(color(tones.accent)));
                    ui.label(ui::muted("Checking setup…", tones));
                });
                ui.separator();
            }
            setup_row(ui, tones, "Diagnosis", |ui| {
                if button(ui, tones, Tone::Secondary, "Check setup", idle)
                    .clicked()
                {
                    action = Some((Action::Doctor, "Checking setup…"));
                }
            });
            if let Some(diagnosis) = &self.diagnosis {
                ui.add_space(4.0);
                ui::well(tones).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(ui::data(diagnosis.trim_end(), tones));
                });
            }
        });
        if open_settings {
            self.open_settings();
        }
        if open_credentials {
            self.open_credentials();
        }
        if let Some((action, message)) = action {
            self.submit(action, message, ui.ctx());
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let tones = self.palette.tones();
        if self.forget.is_some() || self.credentials.is_some() {
            ui::scrim(ctx, &tones);
        }
        if let Some(take) = self.forget.clone() {
            let enabled = self.action_result.is_none()
                && self.observation.as_ref().is_some_and(|observation| {
                    !observation.history_unavailable
                        && observation
                            .status
                            .as_ref()
                            .is_some_and(|status| status.state == StateKind::Idle)
                });
            let days = CalendarDays::now();
            let mut confirmed = false;
            let mut close = false;
            dialog(ctx, &tones, "Forget this recording?", |ui| {
                ui.label(ui::hero("Forget this recording?", &tones));
                ui.add_space(2.0);
                ui.label(ui::data(
                    format!(
                        "{} · {}",
                        row_time(take.created_at_unix_ms, &days),
                        short_duration(take.duration_ms)
                    ),
                    &tones,
                ));
                ui.label(
                    ui::data(&take.id, &tones)
                        .size(12.0)
                        .color(color(tones.text_faint)),
                );
                ui.add_space(8.0);
                for line in [
                    "Deletes its retained audio and any incomplete transcript, and clears it from Waiting.",
                    "Keeps complete archived transcript text.",
                    "This can't be undone.",
                ] {
                    ui.label(line);
                }
                ui.add_space(12.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                    confirmed = button(ui, &tones, Tone::DangerFilled, "Forget recording", enabled)
                        .clicked();
                    let keep = button(ui, &tones, Tone::Secondary, "Keep recording", true);
                    if self.confirm_focus {
                        keep.request_focus();
                        ui.ctx().request_repaint();
                        self.confirm_focus = false;
                    }
                    close = keep.clicked();
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
            let setup_idle = self.setup_result.is_none();
            dialog(ctx, &tones, "Store an API key", |ui| {
                ui.label(ui::hero("Store an API key", &tones));
                ui.add_space(2.0);
                ui.label(ui::muted("The key is saved in the OS keyring, never in configuration. Use the same key ID in Settings; saving replaces any key with this ID.", &tones));
                ui.add_space(10.0);
                ui.label(RichText::new("Key ID").family(fonts::medium()));
                let id = ui.add(
                    egui::TextEdit::singleline(&mut credentials.id)
                        .font(egui::TextStyle::Monospace)
                        .hint_text("Name used in Settings")
                        .margin(egui::vec2(8.0, 6.0))
                        .desired_width(f32::INFINITY),
                );
                if credentials.focus {
                    id.request_focus();
                    ui.ctx().request_repaint();
                    credentials.focus = false;
                }
                ui.add_space(4.0);
                ui.label(RichText::new("API key").family(fonts::medium()));
                ui.add(
                    egui::TextEdit::singleline(&mut credentials.secret)
                        .password(true)
                        .margin(egui::vec2(8.0, 6.0))
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(12.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                    save = button(
                        ui,
                        &tones,
                        Tone::Primary,
                        "Save in keyring",
                        !credentials.id.trim().is_empty()
                            && !credentials.secret.is_empty()
                            && setup_idle,
                    )
                    .clicked();
                    close = button(ui, &tones, Tone::Secondary, "Cancel", true).clicked();
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
        let tones = self.palette.tones();
        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(color(tones.canvas))
                    .inner_margin(egui::Margin {
                        left: 20.0,
                        right: 20.0 - SCROLL_GUTTER,
                        top: 20.0,
                        bottom: 20.0,
                    }),
            )
            .show(ctx, |ui| {
                ui.add_enabled_ui(self.forget.is_none() && self.credentials.is_none(), |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink(false)
                        .show(ui, |ui| {
                            // The floating scroll bar sits in a gutter, never over cards.
                            let full = ui.available_width() - SCROLL_GUTTER;
                            let width = full.min(COLUMN_WIDTH);
                            let side = ((full - width) / 2.0).max(0.0);
                            egui::Frame::none()
                                .inner_margin(egui::Margin {
                                    left: side,
                                    right: side + SCROLL_GUTTER,
                                    top: 0.0,
                                    bottom: 8.0,
                                })
                                .show(ui, |ui| {
                                    ui.set_width(width);
                                    self.main_view(ui);
                                });
                        });
                });
            });
        self.dialogs(ctx);
        self.screenshot(ctx);
        ctx.request_repaint_after(POLL);
    }
}

struct Hero {
    stamp: Stamp,
    title: String,
    elapsed: Option<String>,
    sentence: String,
}

fn stage_title(stage: &Stage) -> String {
    match stage {
        Stage::FinalizingAudio => "Finishing the recording".to_owned(),
        Stage::Transcribing { completed, total } if *total > 1 => {
            format!("Transcribing · {completed} of {total}")
        }
        Stage::Transcribing { .. } => "Transcribing".to_owned(),
        Stage::CleaningUp => "Cleaning up the text".to_owned(),
        Stage::Delivering => "Delivering text".to_owned(),
        Stage::Cancelling => "Cancelling…".to_owned(),
        Stage::RemovingRecording => "Removing saved audio…".to_owned(),
        Stage::Unknown(stage) => stage.clone(),
    }
}

/// An outcome or notice: message with optional error class, Dismiss on the right.
fn banner(
    ui: &mut egui::Ui,
    tones: &Tones,
    frame: egui::Frame,
    message: &str,
    error: Option<&str>,
    dismiss: Option<bool>,
) -> bool {
    let mut clicked = false;
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
            if let Some(enabled) = dismiss {
                clicked = button(ui, tones, Tone::Secondary, "Dismiss", enabled).clicked();
            }
            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.label(message);
                if let Some(error) = error {
                    ui.label(
                        ui::data(error, tones)
                            .size(11.5)
                            .color(color(tones.text_muted)),
                    );
                }
            });
        });
    });
    clicked
}

/// A Lantern button, enabled or not; `ui::button` owns hover, press and focus.
fn button(
    ui: &mut egui::Ui,
    tones: &Tones,
    tone: Tone,
    text: &str,
    enabled: bool,
) -> egui::Response {
    ui.add_enabled(enabled, ui::button(tones, tone, text))
}

/// A setup fact: label on the left; state and controls, added right to left.
fn setup_row(ui: &mut egui::Ui, tones: &Tones, label: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.set_min_height(30.0);
        ui.label(
            RichText::new(label)
                .family(fonts::medium())
                .color(color(tones.text)),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), add);
    });
}

fn dialog(ctx: &egui::Context, tones: &Tones, id: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Window::new(id)
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .frame(ui::dialog_frame(ctx, tones))
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.set_width(DIALOG_WIDTH);
            add(ui);
        });
}

fn take_stamp(take: &Take) -> Stamp {
    if take.text_available && take.partial {
        Stamp::Partial
    } else if take.text_available {
        Stamp::Complete
    } else if take.audio_available && take.unresolved {
        Stamp::Attention
    } else {
        Stamp::Rest
    }
}

fn take_facts(take: &Take) -> String {
    format!(
        "{} · {}",
        if take.partial && take.text_available {
            "Partial text"
        } else if take.text_available {
            "Complete text"
        } else {
            "No transcript"
        },
        if take.audio_available {
            "audio kept"
        } else {
            "audio not kept"
        }
    )
}

/// One uniform-height recording row, painted directly but focusable and
/// announced as a selectable item.
fn take_row(
    ui: &mut egui::Ui,
    tones: &Tones,
    take: &Take,
    selected: bool,
    show_waiting: bool,
    days: &CalendarDays,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), ROW_HEIGHT),
        egui::Sense::click(),
    );
    let time = row_time(take.created_at_unix_ms, days);
    let facts = take_facts(take);
    let duration = short_duration(take.duration_ms);
    let waiting = show_waiting && take.unresolved;
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::SelectableLabel,
            ui.is_enabled(),
            selected,
            format!(
                "{time}, {facts}, {duration}{}",
                if waiting { ", waiting" } else { "" }
            ),
        )
    });
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let rounding = egui::Rounding::same(ui::CONTROL_RADIUS);
    let painter = ui.painter_at(rect.expand(1.0));
    if selected {
        painter.rect_filled(rect, rounding, color(tones.accent_soft));
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(rect.left(), rect.top() + 8.0),
                egui::pos2(rect.left() + 3.0, rect.bottom() - 8.0),
            ),
            egui::Rounding::same(1.5),
            color(tones.accent),
        );
    } else if response.hovered() {
        painter.rect_filled(rect, rounding, color(tones.raised));
    }
    if response.has_focus() {
        painter.rect_stroke(
            rect.shrink(1.0),
            rounding,
            egui::Stroke::new(2.0_f32, color(tones.accent)),
        );
    }
    let mut stamp_ui = ui.new_child(egui::UiBuilder::new().max_rect(egui::Rect::from_min_size(
        egui::pos2(rect.left() + 16.0, rect.center().y - 7.0),
        egui::vec2(64.0, 14.0),
    )));
    ui::stamp(&mut stamp_ui, tones, take_stamp(take));
    let mono = egui::FontId::new(12.5, egui::FontFamily::Monospace);
    let left = rect.left() + 92.0;
    let right = rect.right() - 14.0;
    let first = rect.top() + 7.0;
    let second = rect.top() + 26.0;
    painter.text(
        egui::pos2(left, first),
        egui::Align2::LEFT_TOP,
        time,
        mono.clone(),
        color(tones.text),
    );
    painter.text(
        egui::pos2(right, first),
        egui::Align2::RIGHT_TOP,
        duration,
        mono,
        color(tones.text_muted),
    );
    painter.text(
        egui::pos2(left, second),
        egui::Align2::LEFT_TOP,
        facts,
        egui::FontId::new(13.0, egui::FontFamily::Proportional),
        color(tones.text_muted),
    );
    if waiting {
        let galley = painter.layout_no_wrap(
            "Waiting".to_owned(),
            egui::FontId::new(11.5, fonts::medium()),
            color(tones.text),
        );
        let pill = egui::Rect::from_min_size(
            egui::pos2(right - galley.size().x - 16.0, second - 1.0),
            galley.size() + egui::vec2(16.0, 4.0),
        );
        painter.rect(
            pill,
            egui::Rounding::same(pill.height() / 2.0),
            color(tones.attention_soft),
            egui::Stroke::new(1.0_f32, color(tones.attention_line)),
        );
        painter.galley(pill.min + egui::vec2(8.0, 2.0), galley, color(tones.text));
    }
    response
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LocalTime {
    year: i32,
    year_day: i32,
    week_day: i32,
    month: i32,
    day: i32,
    hour: i32,
    minute: i32,
    second: i32,
}

fn local_tm(seconds: libc::time_t) -> Option<libc::tm> {
    let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
    // localtime_r writes only our stack allocation and uses the system timezone.
    let result = unsafe { libc::localtime_r(&seconds, local.as_mut_ptr()) };
    if result.is_null() {
        return None;
    }
    Some(unsafe { local.assume_init() })
}

fn local_time(unix_ms: u64) -> Option<LocalTime> {
    let local = local_tm(libc::time_t::try_from(unix_ms / 1000).ok()?)?;
    Some(LocalTime {
        year: local.tm_year + 1900,
        year_day: local.tm_yday,
        week_day: local.tm_wday,
        month: local.tm_mon,
        day: local.tm_mday,
        hour: local.tm_hour,
        minute: local.tm_min,
        second: local.tm_sec,
    })
}

/// Today's and yesterday's local calendar days as (year, day of year).
struct CalendarDays {
    today: Option<(i32, i32)>,
    yesterday: Option<(i32, i32)>,
}

impl CalendarDays {
    fn now() -> Self {
        let days = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|now| libc::time_t::try_from(now.as_secs()).ok())
            .and_then(local_tm)
            .and_then(|mut noon| {
                // Step back from local noon so DST changes cannot skip or repeat a day.
                noon.tm_hour = 12;
                noon.tm_min = 0;
                noon.tm_sec = 0;
                noon.tm_isdst = -1;
                let noon = unsafe { libc::mktime(&mut noon) };
                (noon != -1).then_some(noon)
            })
            .map(|noon| {
                let day = |seconds| local_tm(seconds).map(|tm| (tm.tm_year + 1900, tm.tm_yday));
                (day(noon), day(noon - 86_400))
            });
        let (today, yesterday) = days.unwrap_or((None, None));
        Self { today, yesterday }
    }

    fn label(&self, time: &LocalTime) -> String {
        const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
        const MONTHS: [&str; 12] = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        let day = Some((time.year, time.year_day));
        if day == self.today {
            return "Today".to_owned();
        }
        if day == self.yesterday {
            return "Yesterday".to_owned();
        }
        let mut label = format!(
            "{} {} {}",
            WEEKDAYS.get(time.week_day as usize).copied().unwrap_or("?"),
            time.day,
            MONTHS.get(time.month as usize).copied().unwrap_or("?")
        );
        if self.today.is_some_and(|(year, _)| year != time.year) {
            label.push_str(&format!(" {}", time.year));
        }
        label
    }
}

/// "Today 10:42:07", "Yesterday 17:31:12", "Tue 22 Sep 09:30:00".
fn row_time(unix_ms: u64, days: &CalendarDays) -> String {
    local_time(unix_ms).map_or_else(
        || "Time unavailable".to_owned(),
        |time| {
            format!(
                "{} {:02}:{:02}:{:02}",
                days.label(&time),
                time.hour,
                time.minute,
                time.second
            )
        },
    )
}

/// "Today at 10:42:07".
fn detail_time(unix_ms: u64, days: &CalendarDays) -> String {
    local_time(unix_ms).map_or_else(
        || "Capture time unavailable".to_owned(),
        |time| {
            format!(
                "{} at {:02}:{:02}:{:02}",
                days.label(&time),
                time.hour,
                time.minute,
                time.second
            )
        },
    )
}

/// "1:12", "0:38" or "740 ms".
fn short_duration(duration_ms: Option<u64>) -> String {
    match duration_ms {
        Some(ms) if ms < 1000 => format!("{ms} ms"),
        Some(ms) => format!("{}:{:02}", ms / 60_000, (ms / 1000) % 60),
        None => "—".to_owned(),
    }
}

/// Human-readable local capture time, shared by the explicit metadata CLI.
pub fn take_time(unix_ms: u64) -> String {
    let Some(local) = local_time(unix_ms) else {
        return "Capture time unavailable".to_owned();
    };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} local",
        local.year,
        local.month + 1,
        local.day,
        local.hour,
        local.minute,
        local.second
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
            .with_inner_size([680.0, 820.0])
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

    #[test]
    fn capture_days_are_relative_across_a_year_boundary() {
        let days = CalendarDays {
            today: Some((2027, 0)),
            yesterday: Some((2026, 364)),
        };
        let at = |year, year_day, week_day, month, day| LocalTime {
            year,
            year_day,
            week_day,
            month,
            day,
            hour: 9,
            minute: 30,
            second: 0,
        };
        assert_eq!(days.label(&at(2027, 0, 5, 0, 1)), "Today");
        assert_eq!(days.label(&at(2026, 364, 4, 11, 31)), "Yesterday");
        // Same day of year in another year is neither today nor yesterday.
        assert_eq!(days.label(&at(2026, 0, 4, 0, 1)), "Thu 1 Jan 2026");
        assert_eq!(days.label(&at(2026, 363, 3, 11, 30)), "Wed 30 Dec 2026");
    }
}
