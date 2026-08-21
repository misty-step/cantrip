//! The cantrip daemon and its socket-driven state machine.

use crate::capture::{self, InputSignal};
use crate::config::{Config, PostprocConfig, SttConfig};
use crate::hud;
use crate::inject::{self, InjectionMode, InjectionOutcome};
use crate::ipc::{AudioSignal, Command, Request, TerminalOutcome, WireReply};
use crate::models;
use crate::paths;
use crate::pipeline::{self, PostprocStatus};
use crate::stt;
use anyhow::{Context, Result};
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CLIENT_LINE_LIMIT: usize = 256;
const CLIENT_READ_DEADLINE: Duration = Duration::from_secs(2);
/// How often the daemon checks that a HUD is alive and holding its lock.
const HUD_SUPERVISE_INTERVAL: Duration = Duration::from_secs(5);
/// Minimum gap between HUD spawn attempts, so a broken HUD (for example a
/// headless session with no Wayland) cannot cause a respawn hot loop.
const HUD_SPAWN_COOLDOWN: Duration = Duration::from_secs(30);
/// Fixed daemon-owned sampling window. Status clients only read the cached
/// result, so HUD and settings polling cannot consume each other's samples.
const SIGNAL_SAMPLE_INTERVAL: Duration = Duration::from_millis(200);

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// The recorder operations the daemon state machine needs. Production uses
/// `capture::Recorder`; tests substitute a fake so Idle/Recording/Processing
/// transitions can be proven without PipeWire hardware.
trait RecorderBoundary: Send {
    fn input_signal(&mut self) -> Option<InputSignal>;
    fn stop(self: Box<Self>) -> Result<PathBuf>;
    fn cancel(self: Box<Self>) -> Result<()>;
}

impl RecorderBoundary for capture::Recorder {
    fn input_signal(&mut self) -> Option<InputSignal> {
        capture::Recorder::input_signal(self)
    }

    fn stop(self: Box<Self>) -> Result<PathBuf> {
        capture::Recorder::stop(*self)
    }

    fn cancel(self: Box<Self>) -> Result<()> {
        capture::Recorder::cancel(*self)
    }
}

extern "C" fn signal_handler(_signal: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

enum State {
    Idle,
    Recording {
        recorder: Box<dyn RecorderBoundary>,
        started: Instant,
        signal: Option<InputSignal>,
        next_signal_sample: Instant,
        /// Per-capture post-processing override (Some(true)=clean,
        /// Some(false)=raw, None=follow config). Applied when this capture
        /// is stopped and dispatched to the worker.
        postproc: Option<bool>,
    },
    Processing {
        started: Instant,
        stage: pipeline::Stage,
    },
}

impl State {
    fn name(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Recording { .. } => "recording",
            Self::Processing { .. } => "processing",
        }
    }
}

struct Job {
    wav: PathBuf,
    stt: SttConfig,
    vocabulary: Vec<String>,
    postproc: PostprocConfig,
    source: pipeline::Source,
}

struct WorkerResult {
    result: std::result::Result<String, String>,
    stt_elapsed: Duration,
    postproc: PostprocStatus,
    partial: bool,
}

/// The daemon's most recent terminal outcome, surfaced on status replies so
/// the HUD pill can flash the true result instead of a fake success.
#[derive(Debug, Clone, Default)]
struct LastOutcome {
    message: Option<String>,
    /// Whether the dictation was delivered (typed or copied).
    ok: Option<bool>,
}

impl LastOutcome {
    fn success(message: impl Into<String>) -> Self {
        Self {
            message: Some(message.into()),
            ok: Some(true),
        }
    }

    fn notice(message: impl Into<String>) -> Self {
        Self {
            message: Some(message.into()),
            ok: Some(false),
        }
    }

    fn to_ipc(&self) -> Option<TerminalOutcome> {
        self.message.clone().map(|message| TerminalOutcome {
            message,
            ok: self.ok.unwrap_or(false),
        })
    }
}

struct SocketCleanup(PathBuf);

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.0) {
            if error.kind() != ErrorKind::NotFound {
                tracing::warn!("[Daemon] could not remove socket: {}", error);
            }
        }
    }
}

struct RecordingCleanup(PathBuf);

impl Drop for RecordingCleanup {
    fn drop(&mut self) {
        if let Err(error) = capture::remove_recording(&self.0) {
            tracing::warn!("[Capture] recording cleanup failed: {error:#}");
        }
    }
}
/// Run the cantrip daemon until it receives SIGINT, SIGTERM, or a fatal error.
pub fn run(config: Config, preload: bool) -> Result<()> {
    tracing::info!("[Daemon] starting cantrip {}", env!("CARGO_PKG_VERSION"));
    SHUTDOWN.store(false, Ordering::SeqCst);
    install_signal_handlers();

    if config.stt.endpoint.is_none() {
        let spec = models::require(&config.stt.model)?;
        models::installed(spec)?.context("model not installed — run: cantrip models pull")?;
        tracing::info!("[Models] model is installed");
    }

    let runtime_dir =
        paths::ensure_dir(paths::runtime_dir()?).context("creating runtime directory")?;
    let runtime_metadata = fs::symlink_metadata(&runtime_dir)
        .with_context(|| format!("checking runtime directory {}", runtime_dir.display()))?;
    if runtime_metadata.file_type().is_symlink() {
        anyhow::bail!("runtime directory {} is a symlink", runtime_dir.display());
    }
    if !runtime_metadata.is_dir() {
        anyhow::bail!("runtime path {} is not a directory", runtime_dir.display());
    }
    if runtime_metadata.uid() != unsafe { libc::getuid() } {
        anyhow::bail!(
            "runtime directory {} is not owned by the current user",
            runtime_dir.display()
        );
    }
    if runtime_metadata.permissions().mode() & 0o777 != 0o700 {
        fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting permissions on {}", runtime_dir.display()))?;
    }
    let socket_path = paths::socket_path()?;
    remove_stale_socket(&socket_path)?;
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("binding daemon socket at {}", socket_path.display()))?;
    let _socket_cleanup = SocketCleanup(socket_path);
    listener
        .set_nonblocking(true)
        .context("enabling non-blocking daemon socket")?;

    let warm = preload || config.keep_warm;
    let WorkerChannels {
        jobs: job_tx,
        results: result_rx,
        ready: ready_rx,
        stages: stage_rx,
        handle: worker,
    } = spawn_worker(&config, warm);
    match ready_rx
        .recv()
        .context("waiting for transcription worker")?
    {
        Ok(()) => {}
        Err(error) => {
            drop(job_tx);
            let _ = worker.join();
            return Err(anyhow::anyhow!("{}", error));
        }
    }

    tracing::info!("[Daemon] listening");
    start_hud_supervisor(runtime_dir.clone());
    let loop_result = serve(
        &listener,
        config,
        &runtime_dir,
        &job_tx,
        &result_rx,
        &stage_rx,
    );
    drop(job_tx);
    if worker.join().is_err() {
        tracing::warn!("[STT] worker thread exited unexpectedly");
    }
    loop_result
}

fn install_signal_handlers() {
    let handler = signal_handler as extern "C" fn(libc::c_int);
    unsafe {
        libc::signal(libc::SIGINT, handler as usize);
        libc::signal(libc::SIGTERM, handler as usize);
    }
}

/// Own the HUD lifecycle: check every few seconds that a HUD is alive (it
/// holds an exclusive flock on `hud.lock`); when the lock is free, spawn a
/// detached HUD. The user never has to start or restart the pill by hand.
fn start_hud_supervisor(runtime_dir: PathBuf) {
    thread::spawn(move || {
        // Start in the past so the first check can spawn immediately.
        // checked_sub: on a machine with less than 30s of monotonic uptime a
        // plain subtraction would panic.
        let mut last_spawn = Instant::now()
            .checked_sub(HUD_SPAWN_COOLDOWN)
            .unwrap_or_else(Instant::now);
        loop {
            if last_spawn.elapsed() >= HUD_SPAWN_COOLDOWN {
                match hud::acquire_instance_lock() {
                    Ok(Some(_lock)) => {
                        // Lock free: no HUD is running. Release it and spawn
                        // one; the child takes the lock itself (or exits if
                        // another instance won the race).
                        drop(_lock);
                        last_spawn = Instant::now();
                        match spawn_hud(&runtime_dir) {
                            Ok(()) => tracing::info!("[Daemon] HUD not running; spawned it"),
                            Err(error) => tracing::warn!("[Daemon] spawning HUD failed: {error:#}"),
                        }
                    }
                    Ok(None) => {} // a HUD holds the lock and is alive
                    Err(error) => tracing::warn!("[Daemon] HUD lock check failed: {error:#}"),
                }
            }
            thread::sleep(HUD_SUPERVISE_INTERVAL);
        }
    });
}

/// Launch the HUD as a detached child of this process. Its output goes to
/// `hud.log` in the runtime directory; it leaves the daemon's process group
/// so terminal signals to the daemon do not kill the pill.
fn spawn_hud(runtime_dir: &Path) -> Result<()> {
    let executable = std::env::current_exe().context("locating the cantrip binary")?;
    let log_path = runtime_dir.join("hud.log");
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening HUD log {}", log_path.display()))?;
    let mut command = ProcessCommand::new(executable);
    command
        .arg("hud")
        .stdin(Stdio::null())
        .stdout(Stdio::from(
            log.try_clone().context("cloning HUD log handle")?,
        ))
        .stderr(Stdio::from(log));
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    command.spawn().context("spawning the HUD")?;
    Ok(())
}

fn remove_stale_socket(path: &Path) -> Result<()> {
    match UnixStream::connect(path) {
        Ok(_) => anyhow::bail!("cantrip daemon already running"),
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::ConnectionRefused | ErrorKind::NotFound
            ) =>
        {
            match fs::remove_file(path) {
                Ok(()) => tracing::info!("[Daemon] removed stale socket"),
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("removing stale socket {}", path.display()));
                }
            }
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("checking daemon socket {}", path.display()));
        }
    }
    Ok(())
}

/// Channels wiring the daemon loop to its transcription worker thread.
struct WorkerChannels {
    jobs: Sender<Job>,
    results: Receiver<WorkerResult>,
    ready: Receiver<std::result::Result<(), String>>,
    stages: Receiver<pipeline::Stage>,
    handle: JoinHandle<()>,
}

fn spawn_worker(config: &Config, warm: bool) -> WorkerChannels {
    let (job_tx, job_rx) = mpsc::channel::<Job>();
    let (result_tx, result_rx) = mpsc::channel::<WorkerResult>();
    let (ready_tx, ready_rx) = mpsc::channel::<std::result::Result<(), String>>();
    let (stage_tx, stage_rx) = mpsc::channel::<pipeline::Stage>();
    let report_stage = move |stage: pipeline::Stage| {
        let _ = stage_tx.send(stage);
    };
    let warm_stt = config.stt.clone();
    let worker = thread::spawn(move || {
        let mut transcriber: pipeline::TranscriberCache = None;
        if warm && warm_stt.endpoint.is_none() {
            match pipeline::load_transcriber(&warm_stt.model) {
                Ok(loaded) => transcriber = Some(loaded),
                Err(error) => {
                    let _ = ready_tx.send(Err(format!("loading transcription model: {error:#}")));
                    return;
                }
            }
        }
        let _ = ready_tx.send(Ok(()));

        while let Ok(job) = job_rx.recv() {
            let Job {
                wav,
                stt,
                vocabulary,
                postproc,
                source,
            } = job;
            let wav = RecordingCleanup(wav);
            let outcome = pipeline::run(
                &mut transcriber,
                &wav.0,
                &stt,
                &vocabulary,
                &postproc,
                source,
                &report_stage,
            );
            match &outcome.archive {
                pipeline::ArchiveStatus::Saved(path) => {
                    tracing::info!("[Daemon] archived transcript path={}", path.display());
                }
                pipeline::ArchiveStatus::Failed(error) => {
                    tracing::warn!("[Daemon] transcript archive failed error={error}");
                }
                pipeline::ArchiveStatus::NotApplicable => {}
            }
            if outcome.keep_wav {
                if let Err(error) = persist_failed_wav(&wav.0) {
                    tracing::warn!("[Daemon] could not keep failed WAV: {error:#}");
                }
            } else {
                clear_failed_wav();
            }
            if let Ok(text) = outcome.text.as_ref() {
                if !text.trim().is_empty() {
                    if let Err(error) = persist_last_transcript(text) {
                        tracing::warn!("[Daemon] could not save last transcript: {error:#}");
                    }
                }
            }
            let worker_result = WorkerResult {
                result: outcome.text,
                stt_elapsed: outcome.stt_elapsed,
                postproc: outcome.postproc,
                partial: outcome.partial,
            };
            let chars = worker_result
                .result
                .as_ref()
                .map_or(0, |text| text.chars().count());
            if result_tx.send(worker_result).is_err() {
                tracing::warn!("[Daemon] transcription result dropped chars={chars}");
            }
        }
    });
    WorkerChannels {
        jobs: job_tx,
        results: result_rx,
        ready: ready_rx,
        stages: stage_rx,
        handle: worker,
    }
}

fn serve(
    listener: &UnixListener,
    mut config: Config,
    runtime_dir: &Path,
    job_tx: &Sender<Job>,
    result_rx: &Receiver<WorkerResult>,
    stage_rx: &Receiver<pipeline::Stage>,
) -> Result<()> {
    let mut state = State::Idle;
    let mut last_outcome = LastOutcome::default();
    loop {
        drain_stage(&mut state, stage_rx);
        drain_worker_results(&mut state, &config, result_rx, &mut last_outcome)?;
        refresh_recording_signal(&mut state);
        if SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }

        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    if let Err(error) = handle_connection(
                        stream,
                        &mut state,
                        &mut config,
                        runtime_dir,
                        job_tx,
                        &mut last_outcome,
                    ) {
                        tracing::warn!("[Daemon] client request failed: {error:#}");
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(error).context("accepting daemon connection"),
            }
        }
        thread::sleep(Duration::from_millis(50));
    }

    if matches!(&state, State::Processing { .. }) {
        match result_rx.recv_timeout(Duration::from_secs(30)) {
            Ok(result) => handle_worker_result(&mut state, &config, result, &mut last_outcome)?,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                tracing::warn!("[STT] transcription result timed out during shutdown");
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("transcription worker died");
            }
        }
    }
    shutdown_state(&mut state);
    tracing::info!("[Daemon] shutting down");
    Ok(())
}
fn handle_connection(
    mut stream: UnixStream,
    state: &mut State,
    config: &mut Config,
    runtime_dir: &Path,
    job_tx: &Sender<Job>,
    last_outcome: &mut LastOutcome,
) -> Result<()> {
    let deadline = Instant::now() + CLIENT_READ_DEADLINE;
    let mut command_line = Vec::with_capacity(CLIENT_LINE_LIMIT);
    let mut byte = [0_u8; 1];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            write_client_error(&mut stream, state, "client request timed out")?;
            return Ok(());
        }
        stream
            .set_read_timeout(Some(remaining))
            .context("setting client read timeout")?;
        match stream.read(&mut byte) {
            Ok(0) => {
                if command_line.is_empty() {
                    return Ok(());
                }
                break;
            }
            Ok(1) => {
                if byte[0] == b'\n' {
                    break;
                }
                command_line.push(byte[0]);
                if command_line.len() >= CLIENT_LINE_LIMIT {
                    write_client_error(&mut stream, state, "client request exceeds 256 bytes")?;
                    return Ok(());
                }
            }
            Ok(_) => unreachable!("single-byte client read returned multiple bytes"),
            Err(error) if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {
                write_client_error(&mut stream, state, "client request timed out")?;
                return Ok(());
            }
            Err(error) => return Err(error).context("reading client command"),
        }
    }

    let command_line = String::from_utf8_lossy(&command_line);
    let reply = match Request::parse(&command_line) {
        Some(Request::Command(command)) => {
            execute(command, state, config, runtime_dir, job_tx, last_outcome)
        }
        Some(Request::Status) => status_reply(state, last_outcome),
        None => WireReply::command(false, state.name(), Some("unknown command".to_owned())),
    };
    let json = serde_json::to_string(&reply).context("serializing daemon reply")?;
    writeln!(stream, "{json}").context("writing daemon reply")?;
    Ok(())
}

fn status_reply(state: &State, last_outcome: &LastOutcome) -> WireReply {
    let (elapsed, signal, stage) = match state {
        State::Recording {
            started, signal, ..
        } => (
            Some(started.elapsed().as_secs()),
            signal.map(|value| AudioSignal {
                level: value.level,
                silent: value.silent,
                waveform: value.waveform,
            }),
            None,
        ),
        State::Processing { stage, .. } => (None, None, Some(stage)),
        State::Idle => (None, None, None),
    };
    WireReply::status(state.name(), elapsed, signal, stage, last_outcome.to_ipc())
}

fn refresh_recording_signal(state: &mut State) {
    let State::Recording {
        recorder,
        signal,
        next_signal_sample,
        ..
    } = state
    else {
        return;
    };
    let now = Instant::now();
    if now < *next_signal_sample {
        return;
    }
    *signal = recorder.input_signal();
    *next_signal_sample = now + SIGNAL_SAMPLE_INTERVAL;
}

fn write_client_error(stream: &mut UnixStream, state: &State, message: &str) -> Result<()> {
    let reply = WireReply::command(false, state.name(), Some(message.to_owned()));
    let json = serde_json::to_string(&reply).context("serializing client error reply")?;
    writeln!(stream, "{json}").context("writing client error reply")?;
    Ok(())
}

fn execute(
    command: Command,
    state: &mut State,
    config: &mut Config,
    runtime_dir: &Path,
    job_tx: &Sender<Job>,
    last_outcome: &mut LastOutcome,
) -> WireReply {
    match command {
        Command::Toggle { postproc } => match state {
            State::Idle => start_recording(state, config, runtime_dir, last_outcome, postproc),
            State::Recording { .. } => stop_recording(state, config, job_tx, last_outcome),
            State::Processing { .. } => busy_reply(state),
        },
        Command::Start { postproc } => match state {
            State::Idle => start_recording(state, config, runtime_dir, last_outcome, postproc),
            State::Processing { .. } => busy_reply(state),
            State::Recording { .. } => {
                WireReply::command(false, state.name(), Some("busy".to_owned()))
            }
        },
        Command::Stop => match state {
            State::Recording { .. } => stop_recording(state, config, job_tx, last_outcome),
            State::Idle => {
                WireReply::command(false, state.name(), Some("not recording".to_owned()))
            }
            State::Processing { .. } => busy_reply(state),
        },
        Command::Cancel => cancel_recording(state, last_outcome),
        Command::Last => replay_last(state, config, last_outcome),
        Command::Recover => recover_failed(state, config, job_tx, last_outcome),
        Command::Ping => WireReply::command(true, state.name(), Some("pong".to_owned())),
        Command::Reload => match Config::load() {
            Ok(new_config) => {
                *config = new_config;
                tracing::info!("[Daemon] config reloaded");
                WireReply::command(true, state.name(), Some("reloaded".to_owned()))
            }
            Err(error) => WireReply::command(false, state.name(), Some(format!("{error:#}"))),
        },
    }
}

fn start_recording(
    state: &mut State,
    config: &Config,
    runtime_dir: &Path,
    last_outcome: &mut LastOutcome,
    postproc_override: Option<bool>,
) -> WireReply {
    start_recording_with(
        state,
        config,
        runtime_dir,
        last_outcome,
        postproc_override,
        |wav, source| {
            capture::Recorder::start(wav, source)
                .map(|recorder| Box::new(recorder) as Box<dyn RecorderBoundary>)
        },
    )
}

fn start_recording_with(
    state: &mut State,
    config: &Config,
    runtime_dir: &Path,
    last_outcome: &mut LastOutcome,
    postproc_override: Option<bool>,
    start: impl FnOnce(&Path, Option<&str>) -> Result<Box<dyn RecorderBoundary>>,
) -> WireReply {
    // Forcing cleanup when no model is configured would silently degrade to
    // "cleanup failed — raw text", so reject it up front with a clear error.
    if postproc_override == Some(true) && config.postproc.model.trim().is_empty() {
        *last_outcome = LastOutcome::notice("Post-processing requested but no model set");
        return WireReply::command(false, state.name(), Some(
            "post-processing requested but [postproc].model is not set — add a model or drop --postproc clean".to_owned(),
        ));
    }
    let wav = runtime_dir.join(format!("rec-{}.wav", unix_millis()));
    match start(&wav, config.audio_source.as_deref()) {
        Ok(recorder) => {
            let started = Instant::now();
            *state = State::Recording {
                recorder,
                started,
                signal: None,
                next_signal_sample: started,
                postproc: postproc_override,
            };
            *last_outcome = LastOutcome::default();
            tracing::info!("[Daemon] state idle -> recording");
            WireReply::command(true, state.name(), Some("recording".to_owned()))
        }
        Err(error) => {
            *last_outcome = LastOutcome::notice("Starting recording failed");
            WireReply::command(
                false,
                state.name(),
                Some(format!("starting recording failed: {error:#}")),
            )
        }
    }
}

fn stop_recording(
    state: &mut State,
    config: &Config,
    job_tx: &Sender<Job>,
    last_outcome: &mut LastOutcome,
) -> WireReply {
    let State::Recording {
        recorder,
        started,
        postproc,
        ..
    } = std::mem::replace(state, State::Idle)
    else {
        unreachable!("stop_recording called outside recording state");
    };

    let record_secs = started.elapsed().as_secs_f64();
    let wav = match recorder.stop() {
        Ok(wav) => wav,
        Err(error) => {
            *last_outcome = LastOutcome::notice("Recording failed");
            return WireReply::command(
                false,
                state.name(),
                Some(format!("stopping recording failed: {error:#}")),
            );
        }
    };
    // Apply this capture's override (None = follow config) to the worker job.
    let mut postproc_cfg = config.postproc.clone();
    if let Some(force) = postproc {
        postproc_cfg.enabled = force;
    }
    let job = Job {
        wav: wav.clone(),
        stt: config.stt.clone(),
        vocabulary: config.vocabulary.clone(),
        postproc: postproc_cfg,
        source: pipeline::Source::Dictation,
    };
    if job_tx.send(job).is_err() {
        if let Err(error) = capture::remove_recording(&wav) {
            tracing::warn!("[Capture] recording cleanup failed: {error:#}");
        }
        *last_outcome = LastOutcome::notice("Transcription failed");
        return WireReply::command(
            false,
            state.name(),
            Some("transcription worker unavailable".to_owned()),
        );
    }
    *state = State::Processing {
        started: Instant::now(),
        stage: pipeline::Stage::Transcribing { chunk: 1, total: 1 },
    };
    tracing::info!("[Daemon] state recording -> processing record_secs={record_secs:.3}");
    WireReply::command(true, state.name(), Some("processing".to_owned()))
}

fn cancel_recording(state: &mut State, last_outcome: &mut LastOutcome) -> WireReply {
    let previous = std::mem::replace(state, State::Idle);
    match previous {
        State::Recording { recorder, .. } => match recorder.cancel() {
            Ok(()) => {
                tracing::info!("[Daemon] state recording -> idle (cancelled)");
                *last_outcome = LastOutcome::notice("Cancelled");
                WireReply::command(true, state.name(), Some("cancelled".to_owned()))
            }
            Err(error) => {
                *last_outcome = LastOutcome::notice("Cancelling failed");
                WireReply::command(
                    false,
                    state.name(),
                    Some(format!("cancelling recording failed: {error:#}")),
                )
            }
        },
        other => {
            *state = other;
            if matches!(state, State::Processing { .. }) {
                busy_reply(state)
            } else {
                WireReply::command(false, state.name(), Some("nothing to cancel".to_owned()))
            }
        }
    }
}

fn busy_reply(state: &State) -> WireReply {
    WireReply::command(false, state.name(), Some("busy: processing".to_owned()))
}

fn drain_stage(state: &mut State, stage_rx: &Receiver<pipeline::Stage>) {
    let mut latest = None;
    while let Ok(event) = stage_rx.try_recv() {
        latest = Some(event);
    }
    // Draining always: stale events from a finished job are discarded
    // here, before a next job's own Transcribing event arrives. Only the
    // most recent stage is applied, and only while processing.
    if let (Some(event), State::Processing { stage, .. }) = (latest, state) {
        *stage = event;
    }
}

fn drain_worker_results(
    state: &mut State,
    config: &Config,
    result_rx: &Receiver<WorkerResult>,
    last_outcome: &mut LastOutcome,
) -> Result<()> {
    loop {
        let result = match result_rx.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return Ok(()),
            Err(TryRecvError::Disconnected) => {
                *last_outcome = LastOutcome::notice("Transcription failed");
                anyhow::bail!("transcription worker died");
            }
        };
        handle_worker_result(state, config, result, last_outcome)?;
    }
}
fn handle_worker_result(
    state: &mut State,
    config: &Config,
    result: WorkerResult,
    last_outcome: &mut LastOutcome,
) -> Result<()> {
    let processing_started = match state {
        State::Processing { started, .. } => *started,
        _ => {
            tracing::warn!("[Daemon] ignored transcription result outside processing state");
            return Ok(());
        }
    };
    let WorkerResult {
        result: transcript,
        stt_elapsed,
        postproc,
        partial,
    } = result;
    let postproc_failed = matches!(&postproc, PostprocStatus::Failed { .. });
    let postproc_ms = match postproc {
        PostprocStatus::Applied { ms } | PostprocStatus::Failed { ms } => Some(ms),
        PostprocStatus::Off | PostprocStatus::SkippedShort { .. } => None,
    };

    match transcript {
        Ok(text) if text.trim().is_empty() => {
            tracing::info!(
                "[Daemon] state processing -> idle stt_ms={} chars=0 (no speech detected)",
                stt_elapsed.as_millis()
            );
            *last_outcome = LastOutcome::notice("Heard nothing");
        }
        Ok(text) => {
            let chars = text.chars().count();
            let inject_started = Instant::now();
            let cleanup_suffix = if postproc_failed {
                " (cleanup failed — raw text)"
            } else {
                ""
            };
            let partial_suffix = if partial {
                " (partial — later audio failed)"
            } else {
                ""
            };
            match inject::inject(&text, config.injection) {
                Ok(outcome) => {
                    let inject_ms = inject_started.elapsed().as_millis();
                    let total_ms = processing_started.elapsed().as_millis();
                    let message = match outcome {
                        InjectionOutcome::Typed(tool) => {
                            format!("Typed {chars} chars ({tool}){cleanup_suffix}{partial_suffix}")
                        }
                        InjectionOutcome::Pasted => {
                            format!(
                                "Pasted {chars} chars (clipboard + Ctrl+Shift+V){cleanup_suffix}{partial_suffix}"
                            )
                        }
                        InjectionOutcome::Clipboard => {
                            format!(
                                "Copied to clipboard — press Ctrl+Shift+V ({chars} chars){cleanup_suffix}{partial_suffix}"
                            )
                        }
                    };
                    log_processing_idle(stt_elapsed, inject_ms, chars, postproc_ms);
                    tracing::info!("[Inject] injected chars={chars} total_ms={total_ms}");
                    *last_outcome = LastOutcome::success(message);
                }
                Err(error) if inject::allows_clipboard_fallback(config.injection) => {
                    // Auto only: Type must never touch the clipboard; Paste is strict.
                    tracing::warn!(
                        "[Inject] injection failed chars={chars} stt_ms={} error={error:#}",
                        stt_elapsed.as_millis()
                    );
                    match inject::inject(&text, InjectionMode::Clipboard) {
                        Ok(_) => {
                            let inject_ms = inject_started.elapsed().as_millis();
                            let total_ms = processing_started.elapsed().as_millis();
                            log_processing_idle(stt_elapsed, inject_ms, chars, postproc_ms);
                            tracing::info!(
                                "[Inject] clipboard fallback chars={chars} total_ms={total_ms}"
                            );
                            *last_outcome = LastOutcome::success(format!(
                                "Copied to clipboard ({chars} chars){cleanup_suffix}{partial_suffix}"
                            ));
                        }
                        Err(fallback_error) => {
                            tracing::warn!(
                                "[Inject] clipboard fallback failed chars={chars} stt_ms={} error={fallback_error:#}",
                                stt_elapsed.as_millis()
                            );
                            *last_outcome = LastOutcome::notice("Saved — run: cantrip last");
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        "[Inject] injection failed chars={chars} stt_ms={} error={error:#}",
                        stt_elapsed.as_millis()
                    );
                    *last_outcome = LastOutcome::notice("Saved — run: cantrip last");
                }
            }
        }
        Err(error) => {
            let notice = stt::classify_failure(&error);
            tracing::warn!(
                "[STT] transcription failed stt_ms={} notice={notice} error={error}",
                stt_elapsed.as_millis()
            );
            *last_outcome = LastOutcome::notice(format!("{notice} — run: cantrip recover"));
        }
    }
    *state = State::Idle;
    Ok(())
}

fn log_processing_idle(
    stt_elapsed: Duration,
    inject_ms: u128,
    chars: usize,
    postproc_ms: Option<u128>,
) {
    if let Some(postproc_ms) = postproc_ms {
        tracing::info!(
            "[Daemon] state processing -> idle stt_ms={} inject_ms={} postproc_ms={postproc_ms} chars={chars}",
            stt_elapsed.as_millis(),
            inject_ms
        );
    } else {
        tracing::info!(
            "[Daemon] state processing -> idle stt_ms={} inject_ms={} chars={chars}",
            stt_elapsed.as_millis(),
            inject_ms
        );
    }
}

fn shutdown_state(state: &mut State) {
    let previous = std::mem::replace(state, State::Idle);
    if let State::Recording { recorder, .. } = previous {
        if let Err(error) = recorder.cancel() {
            tracing::warn!("[Capture] shutdown cancellation failed: {error:#}");
        }
    }
}

fn replay_last(state: &State, config: &Config, last_outcome: &mut LastOutcome) -> WireReply {
    if !matches!(state, State::Idle) {
        return busy_reply(state);
    }
    match read_last_transcript() {
        Ok(text) if text.trim().is_empty() => {
            *last_outcome = LastOutcome::notice("No saved transcript");
            WireReply::command(false, state.name(), Some("no saved transcript".to_owned()))
                .with_outcome(last_outcome.to_ipc())
        }
        Ok(text) => {
            let chars = text.chars().count();
            match inject::inject(&text, config.injection) {
                Ok(outcome) => {
                    let message = match outcome {
                        InjectionOutcome::Typed(tool) => format!("Replayed {chars} chars ({tool})"),
                        InjectionOutcome::Pasted => {
                            format!("Replayed {chars} chars (clipboard + Ctrl+Shift+V)")
                        }
                        InjectionOutcome::Clipboard => {
                            format!("Replayed to clipboard — press Ctrl+Shift+V ({chars} chars)")
                        }
                    };
                    tracing::info!("[Inject] replayed last transcript chars={chars}");
                    *last_outcome = LastOutcome::success(message.clone());
                    WireReply::command(true, state.name(), Some(message))
                        .with_outcome(last_outcome.to_ipc())
                }
                Err(error) => {
                    tracing::warn!("[Inject] replay failed chars={chars} error={error:#}");
                    *last_outcome = LastOutcome::notice("Replay failed");
                    WireReply::command(
                        false,
                        state.name(),
                        Some(format!("replay failed: {error:#}")),
                    )
                    .with_outcome(last_outcome.to_ipc())
                }
            }
        }
        Err(error) => {
            *last_outcome = LastOutcome::notice("No saved transcript");
            WireReply::command(
                false,
                state.name(),
                Some(format!("no saved transcript: {error:#}")),
            )
            .with_outcome(last_outcome.to_ipc())
        }
    }
}

fn recover_failed(
    state: &mut State,
    config: &Config,
    job_tx: &Sender<Job>,
    last_outcome: &mut LastOutcome,
) -> WireReply {
    if !matches!(state, State::Idle) {
        return busy_reply(state);
    }
    let path = match paths::last_failed_wav_path() {
        Ok(path) if path.is_file() => path,
        Ok(_) => {
            *last_outcome = LastOutcome::notice("No failed recording to recover");
            return WireReply::command(
                false,
                state.name(),
                Some("no failed recording to recover".to_owned()),
            )
            .with_outcome(last_outcome.to_ipc());
        }
        Err(error) => {
            *last_outcome = LastOutcome::notice("No failed recording to recover");
            return WireReply::command(
                false,
                state.name(),
                Some(format!("no failed recording: {error:#}")),
            )
            .with_outcome(last_outcome.to_ipc());
        }
    };
    // Copy into a fresh runtime WAV so the worker's cleanup still applies.
    let runtime = match paths::runtime_dir().and_then(paths::ensure_dir) {
        Ok(dir) => dir,
        Err(error) => {
            return WireReply::command(
                false,
                state.name(),
                Some(format!("runtime dir unavailable: {error:#}")),
            );
        }
    };
    let wav = runtime.join(format!("recover-{}.wav", unix_millis()));
    if let Err(error) = fs::copy(&path, &wav) {
        return WireReply::command(
            false,
            state.name(),
            Some(format!("copying failed WAV failed: {error}")),
        );
    }
    let job = Job {
        wav: wav.clone(),
        stt: config.stt.clone(),
        vocabulary: config.vocabulary.clone(),
        postproc: config.postproc.clone(),
        source: pipeline::Source::Recover,
    };
    if job_tx.send(job).is_err() {
        let _ = capture::remove_recording(&wav);
        *last_outcome = LastOutcome::notice("Transcription failed");
        return WireReply::command(
            false,
            state.name(),
            Some("transcription worker unavailable".to_owned()),
        );
    }
    let stage = pipeline::Stage::Transcribing { chunk: 1, total: 1 };
    let reply =
        WireReply::command(true, "processing", Some("recovering".to_owned())).with_stage(&stage);
    *state = State::Processing {
        started: Instant::now(),
        stage,
    };
    tracing::info!("[Daemon] state idle -> processing (recover failed WAV)");
    reply
}

fn persist_last_transcript(text: &str) -> Result<()> {
    let path = paths::last_transcript_path()?;
    write_owner_file(&path, text.as_bytes())
}

fn persist_failed_wav(src: &Path) -> Result<()> {
    let path = paths::last_failed_wav_path()?;
    if let Some(parent) = path.parent() {
        paths::ensure_dir(parent.to_path_buf())?;
    }
    fs::copy(src, &path)
        .with_context(|| format!("copying failed WAV {} -> {}", src.display(), path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("setting permissions on {}", path.display()))?;
    tracing::info!("[Daemon] kept failed WAV for recover");
    Ok(())
}

fn clear_failed_wav() {
    if let Ok(path) = paths::last_failed_wav_path() {
        let _ = fs::remove_file(path);
    }
}

fn write_owner_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        paths::ensure_dir(parent.to_path_buf())?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("setting permissions on {}", path.display()))?;
    Ok(())
}

fn read_last_transcript() -> Result<String> {
    let path = paths::last_transcript_path()?;
    fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inject::{allows_clipboard_fallback, InjectionMode};

    struct FakeRecorder {
        signal: Option<InputSignal>,
        stop_result: Result<PathBuf>,
        cancel_result: Result<()>,
    }

    impl RecorderBoundary for FakeRecorder {
        fn input_signal(&mut self) -> Option<InputSignal> {
            self.signal
        }

        fn stop(self: Box<Self>) -> Result<PathBuf> {
            let this = *self;
            this.stop_result
        }

        fn cancel(self: Box<Self>) -> Result<()> {
            let this = *self;
            this.cancel_result
        }
    }

    fn fake_signal() -> InputSignal {
        InputSignal {
            level: 50,
            silent: false,
            waveform: [[0, 0]; crate::capture::AUDIO_WAVEFORM_BINS],
        }
    }

    fn fake_recorder() -> FakeRecorder {
        FakeRecorder {
            signal: Some(fake_signal()),
            stop_result: Ok(PathBuf::from("/tmp/cantrip-fake-recording.wav")),
            cancel_result: Ok(()),
        }
    }

    fn recording_state(recorder: FakeRecorder) -> State {
        State::Recording {
            recorder: Box::new(recorder),
            started: Instant::now(),
            signal: None,
            next_signal_sample: Instant::now(),
            postproc: None,
        }
    }

    fn processing_state() -> State {
        State::Processing {
            started: Instant::now(),
            stage: pipeline::Stage::Transcribing { chunk: 1, total: 1 },
        }
    }

    fn wire(reply: WireReply) -> serde_json::Value {
        serde_json::to_value(reply).expect("reply should serialize")
    }

    #[test]
    fn start_recording_with_fake_recorder_enters_recording() {
        let mut state = State::Idle;
        let config = Config::default();
        let mut last_outcome = LastOutcome::success("stale");
        let recorder = Box::new(fake_recorder());
        let reply = start_recording_with(
            &mut state,
            &config,
            Path::new("/tmp"),
            &mut last_outcome,
            None,
            |_wav, _source| Ok(recorder),
        );
        let json = wire(reply);
        assert_eq!(json["ok"], true);
        assert_eq!(json["state"], "recording");
        assert!(matches!(state, State::Recording { .. }));
        assert_eq!(last_outcome.message, None);
        assert_eq!(last_outcome.ok, None);
    }

    #[test]
    fn start_recording_with_failing_recorder_stays_idle() {
        let mut state = State::Idle;
        let config = Config::default();
        let mut last_outcome = LastOutcome::default();
        let reply = start_recording_with(
            &mut state,
            &config,
            Path::new("/tmp"),
            &mut last_outcome,
            None,
            |_wav, _source| Err(anyhow::anyhow!("no PipeWire")),
        );
        let json = wire(reply);
        assert_eq!(json["ok"], false);
        assert_eq!(json["state"], "idle");
        assert!(matches!(state, State::Idle));
        assert_eq!(
            last_outcome.message.as_deref(),
            Some("Starting recording failed")
        );
    }

    #[test]
    fn stop_recording_dispatches_job_and_enters_processing() {
        let (job_tx, job_rx) = mpsc::channel::<Job>();
        let config = Config::default();
        let wav = PathBuf::from("/tmp/cantrip-fake-recording.wav");
        let mut state = recording_state(FakeRecorder {
            stop_result: Ok(wav.clone()),
            ..fake_recorder()
        });
        let mut last_outcome = LastOutcome::default();

        let reply = stop_recording(&mut state, &config, &job_tx, &mut last_outcome);

        let json = wire(reply);
        assert_eq!(json["ok"], true);
        assert_eq!(json["state"], "processing");
        assert!(matches!(state, State::Processing { .. }));
        let job = job_rx.try_recv().expect("job should be dispatched");
        assert_eq!(job.wav, wav);
        assert_eq!(job.source, pipeline::Source::Dictation);
        assert_eq!(last_outcome.message, None);
    }

    #[test]
    fn stop_recording_failure_returns_to_idle() {
        let (job_tx, job_rx) = mpsc::channel::<Job>();
        let config = Config::default();
        let mut state = recording_state(FakeRecorder {
            stop_result: Err(anyhow::anyhow!("pw-record died")),
            ..fake_recorder()
        });
        let mut last_outcome = LastOutcome::default();

        let reply = stop_recording(&mut state, &config, &job_tx, &mut last_outcome);

        let json = wire(reply);
        assert_eq!(json["ok"], false);
        assert_eq!(json["state"], "idle");
        assert!(matches!(state, State::Idle));
        assert!(job_rx.try_recv().is_err(), "no job on recorder failure");
        assert_eq!(last_outcome.message.as_deref(), Some("Recording failed"));
    }

    #[test]
    fn cancel_recording_stops_fake_recorder_and_returns_idle() {
        let mut state = recording_state(fake_recorder());
        let mut last_outcome = LastOutcome::default();

        let reply = cancel_recording(&mut state, &mut last_outcome);

        let json = wire(reply);
        assert_eq!(json["ok"], true);
        assert_eq!(json["state"], "idle");
        assert!(matches!(state, State::Idle));
        assert_eq!(last_outcome.message.as_deref(), Some("Cancelled"));
    }

    #[test]
    fn cancel_recording_failure_still_returns_idle() {
        let mut state = recording_state(FakeRecorder {
            cancel_result: Err(anyhow::anyhow!("pw-record stuck")),
            ..fake_recorder()
        });
        let mut last_outcome = LastOutcome::default();

        let reply = cancel_recording(&mut state, &mut last_outcome);

        let json = wire(reply);
        assert_eq!(json["ok"], false);
        assert_eq!(json["state"], "idle");
        assert!(matches!(state, State::Idle));
        assert_eq!(last_outcome.message.as_deref(), Some("Cancelling failed"));
    }

    #[test]
    fn refresh_recording_signal_surfaces_fake_signal() {
        let signal = fake_signal();
        let mut state = recording_state(FakeRecorder {
            signal: Some(signal),
            ..fake_recorder()
        });

        refresh_recording_signal(&mut state);

        match state {
            State::Recording { signal: actual, .. } => assert_eq!(actual, Some(signal)),
            State::Processing { .. } => panic!("state should remain recording, got processing"),
            State::Idle => panic!("state should remain recording, got idle"),
        }
    }

    #[test]
    fn processing_rejects_busy_commands_with_stable_message() {
        let commands = [
            Command::Toggle { postproc: None },
            Command::Start { postproc: None },
            Command::Stop,
            Command::Cancel,
            Command::Last,
            Command::Recover,
        ];
        let (job_tx, _job_rx) = mpsc::channel::<Job>();

        for command in commands {
            let mut state = processing_state();
            let mut config = Config::default();
            let mut last_outcome = LastOutcome::default();

            let reply = execute(
                command,
                &mut state,
                &mut config,
                Path::new("/tmp"),
                &job_tx,
                &mut last_outcome,
            );

            let json = wire(reply);
            assert_eq!(json["ok"], false);
            assert_eq!(json["state"], "processing");
            assert_eq!(json["message"], "busy: processing");
        }
    }

    #[test]
    fn last_outcome_success_and_notice_flags() {
        let ok = LastOutcome::success("Pasted 3 chars");
        assert_eq!(ok.ok, Some(true));
        assert_eq!(ok.message.as_deref(), Some("Pasted 3 chars"));

        let notice = LastOutcome::notice("Heard nothing");
        assert_eq!(notice.ok, Some(false));
        assert_eq!(notice.message.as_deref(), Some("Heard nothing"));
    }

    #[test]
    fn busy_reply_rejects_with_processing_message() {
        let reply = busy_reply(&State::Processing {
            started: Instant::now(),
            stage: pipeline::Stage::CleaningUp,
        });
        let json = serde_json::to_value(reply).expect("busy reply should serialize");
        assert_eq!(json["ok"], false);
        assert_eq!(json["state"], "processing");
        assert_eq!(json["message"], "busy: processing");
    }

    #[test]
    fn injection_clipboard_fallback_matches_product_contract() {
        // Daemon must only degrade Auto (see handle_worker_result).
        assert!(allows_clipboard_fallback(InjectionMode::Auto));
        assert!(!allows_clipboard_fallback(InjectionMode::Type));
        assert!(!allows_clipboard_fallback(InjectionMode::Paste));
    }

    #[test]
    fn write_owner_file_creates_missing_parent_with_0600() {
        use std::os::unix::fs::PermissionsExt;
        let unique = format!(
            "cantrip-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        // Parent directory does not exist beforehand.
        let root = std::env::temp_dir().join(unique).join("state");
        let path = root.join("last-transcript.txt");

        write_owner_file(&path, b"hello").expect("write_owner_file should create missing parent");

        assert!(path.exists(), "file should exist");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "file should be owner-only");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(root);
    }
}
