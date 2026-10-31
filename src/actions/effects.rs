//! The blocking work behind the Recordings and Recovery windows: observing
//! live status, saved history and local setup, and performing the actions the
//! window's state hands out. Each runs on its own worker thread; [`Workers`]
//! starts them and folds their results back into [`Recordings`].

use super::model::{
    Action, ActionResult, HistoryRevision, Job, Lane, Observation, Observed, Recordings,
};
use anyhow::{anyhow, Context, Result};
use cantrip_engine::config::Config;
use cantrip_engine::ipc::{self, Command};
use cantrip_engine::{keys, models, recovery};
use std::io::Read;
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
#[cfg(target_os = "linux")]
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Read live status, saved history when `previous` no longer matches its
/// revision (always when `previous` is None), retained audio size, and setup.
pub(crate) fn observe(previous: Option<HistoryRevision>) -> Observation {
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

pub(crate) fn perform(action: Action) -> Result<ActionResult> {
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
                "Local model installed and applied.".to_owned()
            } else {
                "Local model installed. Reload configuration after the current take, or start Cantrip below.".to_owned()
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
            result.map_err(|_| anyhow!("Key was not saved. Unlock the OS keyring and use a nonempty key without whitespace."))?;
            "API key saved in the OS keyring. Use this key ID in Settings; the key is not stored in configuration.".to_owned()
        }
        Action::StartDaemon => start_daemon()?,
    };
    Ok(ActionResult::Message(message))
}

/// A finished action, or an error when its worker vanished.
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

/// What [`Workers::tick`] folded into the window's state.
#[derive(Default)]
pub(crate) struct Collected {
    pub(crate) observed: Option<Observed>,
    /// An observation was lost or started, or an action finished.
    pub(crate) changed: bool,
    /// A new observation started.
    pub(crate) polled: bool,
    /// The window asked to close while setup ran, and may close now.
    pub(crate) close: bool,
}

/// The threads one window runs: at most one observation and one action per lane.
#[derive(Default)]
pub(crate) struct Workers {
    observation: Option<Receiver<Observation>>,
    command: Option<Receiver<Result<ActionResult>>>,
    setup: Option<Receiver<Result<ActionResult>>>,
    cancel: Option<Receiver<Result<ActionResult>>>,
}

impl Workers {
    /// Run `job` on its own thread; `wake` asks the window to look again.
    pub(crate) fn start(&mut self, job: Job, wake: impl Fn() + Send + 'static) {
        let (sender, receiver) = mpsc::channel();
        let Job { lane, action } = job;
        std::thread::spawn(move || {
            let _ = sender.send(perform(action));
            wake();
        });
        *self.lane(lane) = Some(receiver);
    }

    fn lane(&mut self, lane: Lane) -> &mut Option<Receiver<Result<ActionResult>>> {
        match lane {
            Lane::Command => &mut self.command,
            Lane::Setup => &mut self.setup,
            Lane::Cancel => &mut self.cancel,
        }
    }

    /// Fold finished work into `recordings`, then start the observation it
    /// asks for. Call on every window tick or frame.
    pub(crate) fn tick(
        &mut self,
        recordings: &mut Recordings,
        now: Instant,
        wake: impl Fn() + Send + 'static,
    ) -> Collected {
        let mut collected = Collected::default();
        match self.observation.as_ref().map(Receiver::try_recv) {
            Some(Ok(observation)) => {
                self.observation = None;
                collected.observed = Some(recordings.observed(observation));
                collected.changed = true;
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.observation = None;
                recordings.observation_lost(now);
                collected.changed = true;
            }
            _ => {}
        }
        for lane in [Lane::Command, Lane::Setup, Lane::Cancel] {
            if let Some(result) = completed_action(self.lane(lane).as_ref()) {
                *self.lane(lane) = None;
                collected.close |= recordings.finished(lane, result);
                collected.changed = true;
            }
        }
        if let Some(revision) = recordings.poll(now) {
            let (sender, receiver) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = sender.send(observe(revision));
                wake();
            });
            self.observation = Some(receiver);
            collected.polled = true;
        }
        collected
    }
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

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
fn property<'a>(properties: &'a str, key: &str) -> Option<&'a str> {
    properties.lines().find_map(|line| {
        line.split_once('=')
            .filter(|(name, _)| *name == key)
            .map(|(_, value)| value)
    })
}

/// Linux: the systemd user service owns startup when it is installed.
#[cfg(target_os = "linux")]
fn start_daemon() -> Result<String> {
    if ipc::status().is_ok() {
        return Ok("Cantrip is already connected.".to_owned());
    }
    if crate::inject::executable_in_path("systemctl") {
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

/// macOS: the menu-bar app is the one startup owner. Its Recordings and
/// Recovery window asks the app to start its engine, and this waits, bounded,
/// for that engine to answer.
#[cfg(target_os = "macos")]
fn start_daemon() -> Result<String> {
    if ipc::status().is_ok() {
        return Ok("Cantrip is already connected.".to_owned());
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
        if ipc::status().is_ok() {
            return Ok("Cantrip is connected.".to_owned());
        }
    }
    anyhow::bail!(
        "Cantrip did not start dictation. Open the Cantrip menu in the menu bar to see why."
    )
}

#[cfg(target_os = "linux")]
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
