//! The cantrip daemon and its socket-driven state machine.

use crate::capture::{self, InputSignal};
use crate::config::{Config, SttConfig, TelemetryConfig};
use crate::hud;
use crate::inject::{
    self, DeliveryGuard, InjectionFailure, InjectionFailureKind, InjectionMode, InjectionOutcome,
};
use crate::ipc::{
    self, Artifacts, AudioSignal, Capabilities, Cleanup, Command, CommandReply, Completeness,
    Delivery, InteractionNotice, OperationKind, Request, StateKind, StatusSnapshot,
    TerminalOutcome,
};
use crate::models;
use crate::paths;
use crate::pipeline::{self, PostprocStatus, Stage};
use crate::recovery::{self, Take};
use crate::stt;
use crate::telemetry::{self, TelemetryReporter};
use anyhow::{Context, Result};
use serde::Serialize;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const CLIENT_DEADLINE: Duration = Duration::from_secs(2);
const MAX_CLIENTS: usize = 64;
const HUD_SUPERVISE_INTERVAL: Duration = Duration::from_secs(5);
const HUD_SPAWN_COOLDOWN: Duration = Duration::from_secs(30);
/// Only the daemon consumes capture samples; status clients share this cache.
const SIGNAL_SAMPLE_INTERVAL: Duration = Duration::from_millis(100);
const CAPABILITY_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const CANCEL_OPEN: u8 = 0;
const CANCEL_REQUESTED: u8 = 1;
const CANCEL_SEALED: u8 = 2;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

trait RecorderBoundary: Send {
    fn input_signal(&mut self) -> Option<InputSignal>;
    fn request_stop(&mut self) -> Result<()>;
    fn stop(self: Box<Self>) -> Result<PathBuf>;
}

impl RecorderBoundary for capture::Recorder {
    fn input_signal(&mut self) -> Option<InputSignal> {
        capture::Recorder::input_signal(self)
    }

    fn request_stop(&mut self) -> Result<()> {
        capture::Recorder::request_stop(self)
    }

    fn stop(self: Box<Self>) -> Result<PathBuf> {
        capture::Recorder::stop(*self)
    }
}

extern "C" fn signal_handler(_signal: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

#[derive(Debug, PartialEq, Eq)]
struct Identity {
    epoch: Arc<str>,
    operation_id: String,
    take_id: String,
    kind: Option<OperationKind>,
}

#[derive(Clone)]
struct Operation {
    identity: Arc<Identity>,
    cancel: Arc<AtomicBool>,
    lifecycle: Arc<AtomicU8>,
    hud: crate::config::HudConfig,
    started: Instant,
}

impl Operation {
    fn request_cancel(&self) -> bool {
        if self
            .lifecycle
            .compare_exchange(
                CANCEL_OPEN,
                CANCEL_REQUESTED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            self.cancel.store(true, Ordering::Release);
            true
        } else {
            false
        }
    }

    /// Cancellation and successful settlement have one atomic ordering. Once
    /// settlement commits, a later Cancel is rejected rather than acknowledged.
    fn seal(&self) -> bool {
        self.lifecycle
            .compare_exchange(
                CANCEL_OPEN,
                CANCEL_SEALED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
}

enum State {
    Idle,
    Recording {
        operation: Operation,
        recorder: Box<dyn RecorderBoundary>,
        wav: PathBuf,
        config: Box<Config>,
        started: Instant,
        signal: Option<InputSignal>,
        next_signal_sample: Instant,
    },
    Processing {
        operation: Operation,
        phase_started: Instant,
        stage: Stage,
        kind: WorkKind,
    },
}

impl State {
    fn kind(&self) -> StateKind {
        match self {
            Self::Idle => StateKind::Idle,
            Self::Recording { .. } => StateKind::Recording,
            Self::Processing { .. } => StateKind::Processing,
        }
    }

    fn operation(&self) -> Option<&Operation> {
        match self {
            Self::Idle => None,
            Self::Recording { operation, .. } | Self::Processing { operation, .. } => {
                Some(operation)
            }
        }
    }

    fn stage(&self) -> Option<&Stage> {
        match self {
            Self::Processing { stage, .. } => Some(stage),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WorkKind {
    Transcription,
    Delivery,
    Forget,
    Retain,
}

enum Work {
    Capture {
        recorder: Box<dyn RecorderBoundary>,
        wav: PathBuf,
        duration_ms: u64,
    },
    Recover,
    Text,
    Forget,
}

struct Job {
    operation: Operation,
    config: Box<Config>,
    guard: Option<DeliveryGuard>,
    work: Work,
}

struct StageEvent {
    identity: Arc<Identity>,
    stage: Stage,
}

struct WorkerResult {
    identity: Arc<Identity>,
    outcome: TerminalOutcome,
    recordings: Option<Vec<Take>>,
    telemetry: Option<(TelemetryConfig, telemetry::JobTelemetry)>,
}

struct Daemon {
    config: Config,
    state: State,
    epoch: Arc<str>,
    event_sequence: u64,
    operation_sequence: u64,
    outcome: Option<TerminalOutcome>,
    notice: Option<InteractionNotice>,
    recordings: Vec<Take>,
    pending_recordings: usize,
    has_audio: bool,
    has_text: bool,
    history_revision: u64,
    local_model: bool,
    worker_available: bool,
    retainer: Option<JoinHandle<WorkerResult>>,
}

impl Daemon {
    fn new(config: Config, recordings: Vec<Take>, local_model: bool) -> Self {
        Self {
            config,
            state: State::Idle,
            epoch: Arc::from(recovery::new_id()),
            event_sequence: 0,
            operation_sequence: 0,
            outcome: None,
            notice: None,
            pending_recordings: recordings.iter().filter(|take| take.unresolved).count(),
            has_audio: recordings.iter().any(|take| take.audio_available),
            has_text: recordings.iter().any(|take| take.text_available),
            history_revision: 0,
            recordings,
            local_model,
            worker_available: true,
            retainer: None,
        }
    }

    fn replace_recordings(&mut self, recordings: Vec<Take>) {
        self.pending_recordings = recordings.iter().filter(|take| take.unresolved).count();
        self.has_audio = recordings.iter().any(|take| take.audio_available);
        self.has_text = recordings.iter().any(|take| take.text_available);
        self.recordings = recordings;
        self.history_revision += 1;
    }

    fn operation(&mut self, take_id: String, kind: Option<OperationKind>) -> Operation {
        self.operation_sequence += 1;
        Operation {
            identity: Arc::new(Identity {
                epoch: self.epoch.clone(),
                operation_id: format!("{}-{}", self.epoch, self.operation_sequence),
                take_id,
                kind,
            }),
            cancel: Arc::new(AtomicBool::new(false)),
            lifecycle: Arc::new(AtomicU8::new(CANCEL_OPEN)),
            hud: self.config.hud,
            started: Instant::now(),
        }
    }

    fn event_id(&mut self) -> u64 {
        self.event_sequence += 1;
        self.event_sequence
    }

    fn notice(&mut self, message: impl Into<String>, error: Option<&str>) {
        self.notice = Some(InteractionNotice {
            event_id: self.event_id(),
            message: message.into(),
            error: error.map(str::to_owned),
        });
    }

    fn reply(&self, ok: bool, message: impl Into<String>, error: Option<&str>) -> CommandReply {
        CommandReply {
            ok,
            state: self.state.kind(),
            message: Some(message.into()),
            stage: self.state.stage().cloned(),
            outcome: self.outcome.clone(),
            error: error.map(str::to_owned),
        }
    }

    fn reject(&mut self, message: &str, error: &str) -> CommandReply {
        self.notice(message, Some(error));
        self.reply(false, message, Some(error))
    }

    fn busy(&mut self) -> CommandReply {
        self.reject(
            "Another operation is active; nothing new was started.",
            "busy",
        )
    }

    fn publish(&mut self, mut outcome: TerminalOutcome) {
        outcome.event_id = self.event_id();
        self.outcome = Some(outcome);
    }

    fn begin(&mut self, operation: Operation, stage: Stage, kind: WorkKind) {
        self.outcome = None;
        self.notice = None;
        self.state = State::Processing {
            operation,
            phase_started: Instant::now(),
            stage,
            kind,
        };
    }

    fn snapshot(&self) -> StatusSnapshot {
        let (elapsed, signal) = match &self.state {
            State::Recording {
                started, signal, ..
            } => (
                started.elapsed().as_secs(),
                signal.map(|signal| AudioSignal {
                    level: signal.level,
                    silent: signal.silent,
                    waveform: signal.waveform,
                }),
            ),
            State::Processing { phase_started, .. } => (phase_started.elapsed().as_secs(), None),
            State::Idle => (0, None),
        };
        let idle = matches!(self.state, State::Idle) && self.worker_available;
        let cancellable = match &self.state {
            State::Recording { .. } => true,
            State::Processing {
                operation, kind, ..
            } => {
                matches!(kind, WorkKind::Transcription | WorkKind::Delivery)
                    && operation.lifecycle.load(Ordering::Acquire) == CANCEL_OPEN
            }
            State::Idle => false,
        };
        let hud = self
            .state
            .operation()
            .map_or(self.config.hud, |operation| operation.hud);
        StatusSnapshot {
            epoch: self.epoch.to_string(),
            operation_id: self
                .state
                .operation()
                .map(|operation| operation.identity.operation_id.clone()),
            operation_kind: self
                .state
                .operation()
                .and_then(|operation| operation.identity.kind),
            state: self.state.kind(),
            elapsed,
            signal,
            stage: self.state.stage().cloned(),
            outcome: self.outcome.clone(),
            notice: self.notice.clone(),
            pending_recordings: self.pending_recordings,
            capabilities: Capabilities {
                stop: matches!(self.state, State::Recording { .. }),
                cancel: cancellable,
                recover: idle && self.has_audio,
                copy: idle && self.has_text,
                dismiss: self.notice.is_some()
                    || self
                        .outcome
                        .as_ref()
                        .is_some_and(|outcome| !outcome.dismissed),
                local_model: self.local_model,
                remote_configured: self.config.stt.endpoint.is_some(),
            },
            hud,
        }
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

/// Run the cantrip daemon until it receives SIGINT, SIGTERM, or a fatal error.
pub fn run(config: Config, preload: bool) -> Result<()> {
    tracing::info!("[Daemon] starting cantrip {}", env!("CARGO_PKG_VERSION"));
    SHUTDOWN.store(false, Ordering::SeqCst);
    install_signal_handlers();

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

    let import_failed = recovery::import_legacy().is_err();
    let runtime_import_failed = recovery::import_runtime(&runtime_dir).is_err();
    let recordings = recovery::list();
    let list_failed = recordings.is_err();
    let local_model = local_model_ready(&SttConfig::default());
    let mut daemon = Daemon::new(config, recordings.unwrap_or_default(), local_model);
    if import_failed || runtime_import_failed || list_failed {
        tracing::warn!("[Daemon] recovery discovery incomplete class=storage-failed");
        daemon.notice(
            "Saved recordings could not all be loaded; check storage and refresh.",
            Some("storage-failed"),
        );
    }
    DeliveryGuard::prepare();
    let warm = preload || daemon.config.keep_warm;
    let WorkerChannels {
        jobs: job_tx,
        results: result_rx,
        stages: stage_rx,
        handle: worker,
    } = spawn_worker(warm, daemon.config.stt.clone());

    tracing::info!("[Daemon] listening");
    start_hud_supervisor(runtime_dir.clone());
    let telemetry_reporter = TelemetryReporter::spawn();
    let loop_result = serve(
        &listener,
        &runtime_dir,
        &mut daemon,
        &job_tx,
        &result_rx,
        &stage_rx,
        &telemetry_reporter,
    );
    if loop_result.is_err() {
        finish_retention(&mut daemon, &telemetry_reporter);
        shutdown_state(&mut daemon.state);
    }
    drop(job_tx);
    if worker.join().is_err() {
        tracing::warn!("[STT] worker thread exited unexpectedly");
    }
    telemetry_reporter.shutdown();
    loop_result
}

fn install_signal_handlers() {
    let handler = signal_handler as extern "C" fn(libc::c_int);
    unsafe {
        libc::signal(libc::SIGINT, handler as usize);
        libc::signal(libc::SIGTERM, handler as usize);
    }
}

fn start_hud_supervisor(runtime_dir: PathBuf) {
    thread::spawn(move || {
        let mut last_spawn = Instant::now()
            .checked_sub(HUD_SPAWN_COOLDOWN)
            .unwrap_or_else(Instant::now);
        loop {
            if last_spawn.elapsed() >= HUD_SPAWN_COOLDOWN {
                match hud::acquire_instance_lock() {
                    Ok(Some(lock)) => {
                        drop(lock);
                        last_spawn = Instant::now();
                        match spawn_hud(&runtime_dir) {
                            Ok(()) => tracing::info!("[Daemon] HUD not running; spawned it"),
                            Err(error) => tracing::warn!("[Daemon] spawning HUD failed: {error:#}"),
                        }
                    }
                    Ok(None) => {}
                    Err(error) => tracing::warn!("[Daemon] HUD lock check failed: {error:#}"),
                }
            }
            thread::sleep(HUD_SUPERVISE_INTERVAL);
        }
    });
}

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

struct WorkerChannels {
    jobs: Sender<Job>,
    results: Receiver<WorkerResult>,
    stages: Receiver<StageEvent>,
    handle: JoinHandle<()>,
}

fn spawn_worker(warm: bool, warm_stt: SttConfig) -> WorkerChannels {
    let (job_tx, job_rx) = mpsc::channel::<Job>();
    let (result_tx, result_rx) = mpsc::channel::<WorkerResult>();
    let (stage_tx, stage_rx) = mpsc::channel::<StageEvent>();
    let worker = thread::spawn(move || {
        let mut transcriber: pipeline::TranscriberCache = None;
        if warm {
            let model = if warm_stt.endpoint.is_some() {
                models::PARAKEET_V3_INT8.dir_name
            } else {
                &warm_stt.model
            };
            match pipeline::load_transcriber(model) {
                Ok(loaded) => transcriber = Some(loaded),
                Err(error) => tracing::warn!(
                    "[Models] warm load skipped class=local-model-unavailable error={error:#}"
                ),
            }
        }
        while let Ok(job) = job_rx.recv() {
            let result = run_job(job, &mut transcriber, &stage_tx);
            let chars = result.telemetry.as_ref().map_or(0, |(_, job)| job.chars);
            if result_tx.send(result).is_err() {
                tracing::warn!("[Daemon] transcription result dropped chars={chars}");
            }
        }
    });
    WorkerChannels {
        jobs: job_tx,
        results: result_rx,
        stages: stage_rx,
        handle: worker,
    }
}

fn serve(
    listener: &UnixListener,
    runtime_dir: &Path,
    daemon: &mut Daemon,
    job_tx: &Sender<Job>,
    result_rx: &Receiver<WorkerResult>,
    stage_rx: &Receiver<StageEvent>,
    telemetry_reporter: &TelemetryReporter,
) -> Result<()> {
    let mut clients: Vec<PendingClient> = Vec::new();
    let mut history = HistoryReader::spawn()?;
    let mut next_capability_refresh = Instant::now();
    loop {
        drain_stage(daemon, stage_rx);
        drain_worker_results(daemon, result_rx, telemetry_reporter);
        if daemon
            .retainer
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
        {
            finish_retention(daemon, telemetry_reporter);
        }
        refresh_recording_signal(&mut daemon.state);
        if Instant::now() >= next_capability_refresh {
            daemon.local_model = local_model_ready(&SttConfig::default());
            next_capability_refresh = Instant::now() + CAPABILITY_REFRESH_INTERVAL;
        }
        if let Ok((revision, refreshed)) = history.results.try_recv() {
            history.active = false;
            if revision != daemon.history_revision {
                if clients.iter().any(|client| client.history_wait)
                    && history.request(daemon.history_revision).is_err()
                {
                    let payload = encode_reply(&daemon.reply(
                        false,
                        "Saved recordings could not be refreshed.",
                        Some("storage-failed"),
                    ))?;
                    for client in &mut clients {
                        if client.history_wait {
                            client.respond(payload.clone());
                        }
                    }
                }
            } else {
                let payload = match refreshed {
                    Ok((recordings, payload)) => {
                        daemon.replace_recordings(recordings);
                        payload
                    }
                    Err(()) => encode_reply(&daemon.reply(
                        false,
                        "Saved recordings could not be refreshed.",
                        Some("storage-failed"),
                    ))?,
                };
                for client in &mut clients {
                    if client.history_wait {
                        client.respond(payload.clone());
                    }
                }
            }
        }
        if SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }
        // A stream of connecting clients cannot starve cancellation or results.
        for _ in 0..16 {
            match listener.accept() {
                Ok((stream, _)) if clients.len() < MAX_CLIENTS => {
                    if let Some(client) = accept_client(stream) {
                        clients.push(client);
                    }
                }
                Ok((mut stream, _)) => {
                    let _ = stream.set_nonblocking(true);
                    if let Ok(payload) =
                        encode_reply(&daemon.reply(false, "Too many daemon clients.", Some("busy")))
                    {
                        let _ = stream.write(&payload);
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(error).context("accepting daemon connection"),
            }
        }
        let mut index = 0;
        while index < clients.len() {
            match poll_client(&mut clients[index]) {
                ClientPoll::Pending => index += 1,
                ClientPoll::Ready(request) => {
                    let client = &mut clients[index];
                    let reply = match request {
                        Ok(Request::Command(command)) => Some(encode_reply(&execute(
                            command,
                            daemon,
                            runtime_dir,
                            job_tx,
                        ))?),
                        Ok(Request::Status) => Some(encode_reply(&daemon.snapshot())?),
                        Ok(Request::Recordings) => {
                            client.history_wait = true;
                            client.deadline = Instant::now() + Duration::from_secs(8);
                            if let Err(error) = history.request(daemon.history_revision) {
                                tracing::warn!(
                                    "[Daemon] history reader unavailable class=storage-failed"
                                );
                                client.history_wait = false;
                                let _ = error;
                                Some(encode_reply(&daemon.reply(
                                    false,
                                    "Saved recordings could not be refreshed.",
                                    Some("storage-failed"),
                                ))?)
                            } else {
                                None
                            }
                        }
                        Err(message) => Some(encode_reply(&daemon.reply(
                            false,
                            message,
                            Some("protocol"),
                        ))?),
                    };
                    if let Some(reply) = reply {
                        client.respond(reply);
                    }
                    index += 1;
                }
                ClientPoll::Closed => {
                    clients.swap_remove(index);
                }
            }
        }
        thread::sleep(Duration::from_millis(5));
    }

    finish_retention(daemon, telemetry_reporter);
    if let State::Processing { operation, .. } = &daemon.state {
        operation.request_cancel();
        match result_rx.recv_timeout(Duration::from_secs(30)) {
            Ok(result) => apply_worker_result(daemon, result, telemetry_reporter),
            Err(_) => tracing::warn!("[Daemon] worker has not settled during shutdown"),
        }
    }
    shutdown_state(&mut daemon.state);
    tracing::info!("[Daemon] shutting down");
    Ok(())
}

type HistoryRead = (u64, std::result::Result<(Vec<Take>, Arc<Vec<u8>>), ()>);

/// One bounded read worker, independent of inference and delivery. Deliberate
/// history reads refresh disk; high-cadence status never scans or serializes it.
struct HistoryReader {
    requests: mpsc::SyncSender<u64>,
    results: Receiver<HistoryRead>,
    active: bool,
}

impl HistoryReader {
    fn spawn() -> Result<Self> {
        let (requests, receiver) = mpsc::sync_channel(1);
        let (sender, results) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("history-reader".to_owned())
            .spawn(move || {
                while let Ok(revision) = receiver.recv() {
                    let read = recovery::list()
                        .and_then(|takes| encode_reply(&takes).map(|payload| (takes, payload)))
                        .map_err(|_| ());
                    if sender.send((revision, read)).is_err() {
                        break;
                    }
                }
            })
            .context("starting history reader")?;
        Ok(Self {
            requests,
            results,
            active: false,
        })
    }

    fn request(&mut self, revision: u64) -> Result<()> {
        if !self.active {
            self.requests
                .try_send(revision)
                .context("requesting history refresh")?;
            self.active = true;
        }
        Ok(())
    }
}

struct PendingClient {
    stream: UnixStream,
    buffer: Vec<u8>,
    response: Option<Arc<Vec<u8>>>,
    written: usize,
    history_wait: bool,
    deadline: Instant,
}

impl PendingClient {
    fn respond(&mut self, payload: Arc<Vec<u8>>) {
        self.response = Some(payload);
        self.history_wait = false;
        self.deadline = Instant::now() + Duration::from_secs(8);
    }
}

enum ClientPoll {
    Pending,
    Ready(Result<Request, &'static str>),
    Closed,
}

fn accept_client(stream: UnixStream) -> Option<PendingClient> {
    stream.set_nonblocking(true).ok()?;
    Some(PendingClient {
        stream,
        buffer: Vec::new(),
        response: None,
        written: 0,
        history_wait: false,
        deadline: Instant::now() + CLIENT_DEADLINE,
    })
}

fn poll_client(client: &mut PendingClient) -> ClientPoll {
    if Instant::now() >= client.deadline {
        return if client.response.is_some() {
            ClientPoll::Closed
        } else {
            ClientPoll::Ready(Err("daemon request timed out"))
        };
    }
    if let Some(response) = &client.response {
        let end = (client.written + 65_536).min(response.len());
        match client.stream.write(&response[client.written..end]) {
            Ok(0) => return ClientPoll::Closed,
            Ok(written) => client.written += written,
            Err(error)
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {}
            Err(_) => return ClientPoll::Closed,
        }
        return if client.written == response.len() {
            ClientPoll::Closed
        } else {
            ClientPoll::Pending
        };
    }
    if client.history_wait {
        return ClientPoll::Pending;
    }
    let mut buffer = [0_u8; 512];
    loop {
        match client.stream.read(&mut buffer) {
            Ok(0) => {
                return if client.buffer.is_empty() {
                    ClientPoll::Closed
                } else {
                    ClientPoll::Ready(Err("incomplete daemon request"))
                }
            }
            Ok(bytes) => {
                let newline = buffer[..bytes].iter().position(|byte| *byte == b'\n');
                let end = newline.unwrap_or(bytes);
                if client.buffer.len() + end >= ipc::REQUEST_LIMIT {
                    return ClientPoll::Ready(Err("daemon request exceeds size limit"));
                }
                client.buffer.extend_from_slice(&buffer[..end]);
                if newline.is_some() {
                    return ClientPoll::Ready(decode_request(&client.buffer));
                }
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => return ClientPoll::Pending,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return ClientPoll::Closed,
        }
    }
}

fn decode_request(bytes: &[u8]) -> Result<Request, &'static str> {
    let line = std::str::from_utf8(bytes).map_err(|_| "invalid daemon request")?;
    Request::parse(line).ok_or("unknown daemon command")
}

struct ReplyBuffer(Vec<u8>);

impl Write for ReplyBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0.len() + bytes.len() >= ipc::REPLY_LIMIT {
            return Err(std::io::Error::other("daemon reply exceeds size limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn encode_reply<T: Serialize + ?Sized>(value: &T) -> Result<Arc<Vec<u8>>> {
    let mut buffer = ReplyBuffer(Vec::new());
    serde_json::to_writer(&mut buffer, value).context("serializing daemon reply")?;
    buffer.0.push(b'\n');
    Ok(Arc::new(buffer.0))
}

fn execute(
    command: Command,
    daemon: &mut Daemon,
    runtime_dir: &Path,
    job_tx: &Sender<Job>,
) -> CommandReply {
    match command {
        Command::Toggle { postproc } => match &daemon.state {
            State::Idle => start_recording(daemon, runtime_dir, postproc),
            State::Recording { .. } => stop_recording(daemon, job_tx, false),
            State::Processing { .. } => daemon.busy(),
        },
        Command::Start { postproc } => match &daemon.state {
            State::Idle => start_recording(daemon, runtime_dir, postproc),
            _ => daemon.busy(),
        },
        Command::Stop => match &daemon.state {
            State::Recording { .. } => stop_recording(daemon, job_tx, false),
            State::Idle => daemon.reject("Nothing is recording.", "not-recording"),
            State::Processing { .. } => daemon.busy(),
        },
        Command::Cancel => cancel_command(daemon, job_tx),
        Command::Last => replay_latest(daemon, job_tx),
        Command::Recover {
            id,
            local,
            clipboard,
        } => recover_take(daemon, job_tx, id, local, clipboard),
        Command::Copy { id } => copy_take(daemon, job_tx, id),
        Command::Dismiss { event_id } => dismiss(daemon, event_id),
        Command::Forget { id } => forget_take(daemon, job_tx, id),
        Command::Ping => daemon.reply(true, "pong", None),
        Command::Reload => match Config::load() {
            Ok(config) => {
                daemon.config = config;
                daemon.local_model = local_model_ready(&SttConfig::default());
                tracing::info!("[Daemon] config reloaded");
                daemon.reply(true, "reloaded", None)
            }
            Err(_) => daemon.reject(
                "Configuration could not be reloaded; the active configuration is unchanged.",
                "reload-failed",
            ),
        },
    }
}

fn start_recording(
    daemon: &mut Daemon,
    runtime_dir: &Path,
    postproc: Option<bool>,
) -> CommandReply {
    if !daemon.worker_available {
        return daemon.reject("Transcription worker unavailable.", "stt-failed");
    }
    if postproc == Some(true) && daemon.config.postproc.model.trim().is_empty() {
        let mut outcome = setup_failure(
            "Post-processing requested but [postproc].model is not set.",
            "postproc-model-unset",
        );
        outcome.event_id = daemon.event_id();
        daemon.outcome = Some(outcome);
        return daemon.reply(
            false,
            "Post-processing requested but [postproc].model is not set.",
            Some("postproc-model-unset"),
        );
    }
    if daemon.config.stt.endpoint.is_none() && !local_model_ready(&daemon.config.stt) {
        let mut outcome = setup_failure(
            "Local model unavailable — run: cantrip models pull",
            "local-model-unavailable",
        );
        outcome.event_id = daemon.event_id();
        daemon.outcome = Some(outcome);
        return daemon.reply(
            false,
            "Local model unavailable — run: cantrip models pull",
            Some("local-model-unavailable"),
        );
    }
    let take_id = recovery::new_id();
    let wav = runtime_dir.join(format!("rec-{take_id}.wav"));
    match capture::Recorder::start(&wav, daemon.config.audio_source.as_deref()) {
        Ok(recorder) => {
            let operation = daemon.operation(take_id, Some(OperationKind::Dictation));
            let mut config = daemon.config.clone();
            if let Some(enabled) = postproc {
                config.postproc.enabled = enabled;
            }
            let started = Instant::now();
            daemon.outcome = None;
            daemon.notice = None;
            daemon.state = State::Recording {
                operation,
                recorder: Box::new(recorder),
                wav,
                config: Box::new(config),
                started,
                signal: None,
                next_signal_sample: started,
            };
            tracing::info!("[Daemon] state idle -> recording");
            daemon.reply(true, "recording", None)
        }
        Err(_) => {
            tracing::warn!("[Capture] starting recording failed class=capture-failed");
            let mut outcome = setup_failure("Starting recording failed", "capture-failed");
            outcome.event_id = daemon.event_id();
            daemon.outcome = Some(outcome);
            daemon.reply(false, "Starting recording failed", Some("capture-failed"))
        }
    }
}

fn stop_recording(daemon: &mut Daemon, job_tx: &Sender<Job>, cancel: bool) -> CommandReply {
    let State::Recording {
        operation,
        mut recorder,
        wav,
        config,
        started,
        ..
    } = std::mem::replace(&mut daemon.state, State::Idle)
    else {
        unreachable!("stop_recording called outside recording");
    };
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    if cancel {
        operation.request_cancel();
    }
    let guard = (!cancel).then(DeliveryGuard::capture);
    if recorder.request_stop().is_err() {
        tracing::warn!(
            "[Capture] immediate stop request failed; finalization will retry class=capture-failed"
        );
    }
    let job = Job {
        operation: operation.clone(),
        config,
        guard,
        work: Work::Capture {
            recorder,
            wav,
            duration_ms,
        },
    };
    let stage = if cancel {
        Stage::Cancelling
    } else {
        Stage::FinalizingAudio
    };
    if let Err(mpsc::SendError(job)) = job_tx.send(job) {
        daemon.worker_available = false;
        daemon.begin(operation, stage, WorkKind::Retain);
        daemon.retainer = Some(thread::spawn(move || {
            let identity = job.operation.identity.clone();
            let mut outcome = retain_rejected_capture(job);
            let recordings = recovery::list().ok();
            storage_warning(&mut outcome, recordings.is_none());
            WorkerResult {
                identity,
                outcome,
                recordings,
                telemetry: None,
            }
        }));
        return daemon.reply(
            false,
            "Worker unavailable; recording finalization is running without transcription.",
            Some("worker-failed"),
        );
    }
    daemon.begin(operation, stage, WorkKind::Transcription);
    tracing::info!("[Daemon] state recording -> processing");
    if cancel {
        daemon.reply(true, "cancelling", None)
    } else {
        daemon.reply(true, "processing", None)
    }
}

fn cancel_command(daemon: &mut Daemon, job_tx: &Sender<Job>) -> CommandReply {
    match &daemon.state {
        State::Idle => daemon.reject("Nothing to cancel.", "nothing-to-cancel"),
        State::Recording { .. } => stop_recording(daemon, job_tx, true),
        State::Processing { kind, .. }
            if !matches!(kind, WorkKind::Transcription | WorkKind::Delivery) =>
        {
            daemon.busy()
        }
        State::Processing { operation, .. } => {
            if !operation.request_cancel() {
                return daemon.reject(
                    "The operation is already cancelling or settling.",
                    "already-settling",
                );
            }
            if let State::Processing {
                stage,
                phase_started,
                ..
            } = &mut daemon.state
            {
                *stage = Stage::Cancelling;
                *phase_started = Instant::now();
            }
            daemon.reply(true, "cancelling", None)
        }
    }
}

fn replay_latest(daemon: &mut Daemon, job_tx: &Sender<Job>) -> CommandReply {
    if !matches!(daemon.state, State::Idle) {
        return daemon.busy();
    }
    let Some(take) = daemon
        .recordings
        .iter()
        .find(|take| take.text_available)
        .cloned()
    else {
        return daemon.reject("No saved transcript.", "no-transcript");
    };
    dispatch_text(daemon, job_tx, take, daemon.config.injection)
}

fn recover_take(
    daemon: &mut Daemon,
    job_tx: &Sender<Job>,
    id: Option<String>,
    local: bool,
    clipboard: bool,
) -> CommandReply {
    if !matches!(daemon.state, State::Idle) {
        return daemon.busy();
    }
    let Some(take) = select_take(&daemon.recordings, id.as_deref(), |take| {
        take.audio_available && (id.is_some() || take.unresolved || take.partial)
    }) else {
        let message = if id.is_some() {
            "That recording is not available."
        } else {
            "No failed recording to recover."
        };
        return daemon.reject(message, "no-recording");
    };
    let mut config = daemon.config.clone();
    if local {
        config.stt = SttConfig::default();
        config.postproc.enabled = false;
        if !local_model_ready(&config.stt) {
            let mut outcome = setup_failure(
                "Local model unavailable — run: cantrip models pull",
                "local-model-unavailable",
            );
            outcome.artifacts.take_id = Some(take.id.clone());
            outcome.artifacts.audio = take.audio_available;
            outcome.artifacts.text = take.text_available;
            outcome.event_id = daemon.event_id();
            daemon.outcome = Some(outcome);
            return daemon.reply(
                false,
                "Local model unavailable — run: cantrip models pull",
                Some("local-model-unavailable"),
            );
        }
    }
    if clipboard {
        config.injection = InjectionMode::Clipboard;
    }
    let operation = daemon.operation(take.id, Some(OperationKind::Recovery));
    let job = Job {
        operation: operation.clone(),
        config: Box::new(config),
        guard: Some(DeliveryGuard::capture()),
        work: Work::Recover,
    };
    if job_tx.send(job).is_err() {
        daemon.worker_available = false;
        return daemon.reject("Transcription worker unavailable.", "stt-failed");
    }
    daemon.begin(
        operation,
        Stage::Transcribing {
            completed: 0,
            total: 1,
        },
        WorkKind::Transcription,
    );
    daemon.reply(true, "recovering", None)
}

fn copy_take(daemon: &mut Daemon, job_tx: &Sender<Job>, id: String) -> CommandReply {
    if !matches!(daemon.state, State::Idle) {
        return daemon.busy();
    }
    let Some(take) = select_take(&daemon.recordings, Some(&id), |take| take.text_available) else {
        return daemon.reject("That recording is not available.", "no-transcript");
    };
    dispatch_text(daemon, job_tx, take, InjectionMode::Clipboard)
}

fn forget_take(daemon: &mut Daemon, job_tx: &Sender<Job>, id: String) -> CommandReply {
    if !matches!(daemon.state, State::Idle) {
        return daemon.busy();
    }
    if select_take(&daemon.recordings, Some(&id), |_| true).is_none() {
        return daemon.reject("That recording is not available.", "no-recording");
    }
    let operation = daemon.operation(id, Some(OperationKind::Forget));
    let job = Job {
        operation: operation.clone(),
        config: Box::new(daemon.config.clone()),
        guard: None,
        work: Work::Forget,
    };
    if job_tx.send(job).is_err() {
        daemon.worker_available = false;
        return daemon.reject("Transcription worker unavailable.", "stt-failed");
    }
    daemon.begin(operation, Stage::RemovingRecording, WorkKind::Forget);
    daemon.reply(true, "forgetting", None)
}

fn dispatch_text(
    daemon: &mut Daemon,
    job_tx: &Sender<Job>,
    take: Take,
    mode: InjectionMode,
) -> CommandReply {
    let mut config = daemon.config.clone();
    config.injection = mode;
    let operation = daemon.operation(take.id, Some(OperationKind::Replay));
    let job = Job {
        operation: operation.clone(),
        config: Box::new(config),
        guard: Some(DeliveryGuard::capture()),
        work: Work::Text,
    };
    if job_tx.send(job).is_err() {
        daemon.worker_available = false;
        return daemon.reject("Transcription worker unavailable.", "stt-failed");
    }
    daemon.begin(operation, Stage::Delivering, WorkKind::Delivery);
    daemon.reply(true, "delivering", None)
}

fn dismiss(daemon: &mut Daemon, event_id: Option<u64>) -> CommandReply {
    let mut changed = false;
    if let Some(notice) = &daemon.notice {
        if event_id.is_none_or(|id| id == notice.event_id) {
            daemon.notice = None;
            changed = true;
        }
    }
    if let Some(outcome) = &mut daemon.outcome {
        if event_id.is_none_or(|id| id == outcome.event_id) {
            outcome.dismissed = true;
            changed = true;
        }
    }
    if changed {
        daemon.reply(true, "dismissed", None)
    } else {
        daemon.reject("Nothing to dismiss.", "nothing-to-dismiss")
    }
}

fn select_take(
    recordings: &[Take],
    id: Option<&str>,
    predicate: impl Fn(&Take) -> bool,
) -> Option<Take> {
    match id {
        Some(id) => recordings
            .iter()
            .find(|take| take.id == id && predicate(take))
            .cloned(),
        None => recordings.iter().find(|take| predicate(take)).cloned(),
    }
}

fn local_model_ready(stt: &SttConfig) -> bool {
    models::require(&stt.model)
        .ok()
        .and_then(|spec| models::installed(spec).ok().flatten())
        .is_some()
}

fn setup_failure(message: &str, error: &str) -> TerminalOutcome {
    TerminalOutcome {
        event_id: 0,
        operation_id: None,
        message: message.to_owned(),
        completeness: Completeness::Failed,
        delivery: Delivery::None,
        cleanup: Cleanup::Off,
        error: Some(error.to_owned()),
        artifacts: Artifacts::default(),
        dismissed: false,
    }
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

fn drain_stage(daemon: &mut Daemon, stage_rx: &Receiver<StageEvent>) {
    while let Ok(event) = stage_rx.try_recv() {
        if let State::Processing {
            operation,
            stage,
            phase_started,
            ..
        } = &mut daemon.state
        {
            if event.identity.as_ref() == operation.identity.as_ref() && *stage != Stage::Cancelling
            {
                if std::mem::discriminant(stage) != std::mem::discriminant(&event.stage) {
                    *phase_started = Instant::now();
                }
                *stage = event.stage;
            }
        }
    }
}

fn finish_retention(daemon: &mut Daemon, telemetry_reporter: &TelemetryReporter) {
    let Some(retainer) = daemon.retainer.take() else {
        return;
    };
    match retainer.join() {
        Ok(result) => apply_worker_result(daemon, result, telemetry_reporter),
        Err(_) => {
            let mut outcome = setup_failure(
                "Recording finalization failed; check runtime storage.",
                "worker-failed",
            );
            if let Some(operation) = daemon.state.operation() {
                outcome.operation_id = Some(operation.identity.operation_id.clone());
                outcome.artifacts = facts(&operation.identity.take_id);
                operation.lifecycle.store(CANCEL_SEALED, Ordering::Release);
            }
            daemon.state = State::Idle;
            daemon.publish(outcome);
        }
    }
}

fn drain_worker_results(
    daemon: &mut Daemon,
    result_rx: &Receiver<WorkerResult>,
    telemetry_reporter: &TelemetryReporter,
) {
    if !daemon.worker_available {
        return;
    }
    loop {
        match result_rx.try_recv() {
            Ok(result) => apply_worker_result(daemon, result, telemetry_reporter),
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                daemon.worker_available = false;
                if let State::Processing { operation, .. } = &daemon.state {
                    operation.cancel.store(true, Ordering::Release);
                    operation.lifecycle.store(CANCEL_SEALED, Ordering::Release);
                    let mut outcome = setup_failure(
                        "Worker failed; no further delivery will be attempted.",
                        "worker-failed",
                    );
                    outcome.operation_id = Some(operation.identity.operation_id.clone());
                    outcome.artifacts = facts(&operation.identity.take_id);
                    describe_artifacts(&mut outcome);
                    daemon.state = State::Idle;
                    daemon.publish(outcome);
                    if let Ok(recordings) = recovery::list() {
                        daemon.replace_recordings(recordings);
                    }
                } else {
                    daemon.notice(
                        "Worker unavailable; restart Cantrip before recording.",
                        Some("worker-failed"),
                    );
                }
                return;
            }
        }
    }
}

fn apply_worker_result(
    daemon: &mut Daemon,
    result: WorkerResult,
    telemetry_reporter: &TelemetryReporter,
) {
    let current = daemon
        .state
        .operation()
        .map(|operation| operation.identity.clone());
    if current.as_deref() != Some(result.identity.as_ref()) {
        tracing::warn!("[Daemon] ignored stale worker result");
        return;
    }
    if let Some(recordings) = result.recordings {
        daemon.replace_recordings(recordings);
    }
    // A reload never changes credentials/config inside a running operation,
    // but a live telemetry opt-out still vetoes its not-yet-queued report.
    if daemon.config.telemetry.enabled {
        if let Some((config, job)) = result.telemetry {
            telemetry_reporter.report(&config, job);
        }
    }
    if let Some(operation) = daemon.state.operation() {
        operation.lifecycle.store(CANCEL_SEALED, Ordering::Release);
    }
    daemon.state = State::Idle;
    daemon.publish(result.outcome);
}

fn shutdown_state(state: &mut State) {
    match std::mem::replace(state, State::Idle) {
        State::Recording {
            operation,
            recorder,
            wav,
            started,
            ..
        } => {
            let duration_ms = milliseconds(started.elapsed().as_millis());
            let wav = recorder.stop().unwrap_or(wav);
            let saved = persist_attempt(
                &operation.identity.take_id,
                duration_ms,
                None,
                Some(&wav),
                false,
                true,
            );
            if saved.warning {
                tracing::warn!("[Capture] shutdown retention incomplete class=storage-failed");
            }
            if saved.durable_audio && capture::remove_recording(&wav).is_err() {
                tracing::warn!("[Capture] shutdown runtime cleanup failed class=storage-failed");
            }
        }
        State::Processing { operation, .. } => {
            operation.request_cancel();
        }
        State::Idle => {}
    }
}

fn retain_rejected_capture(job: Job) -> TerminalOutcome {
    let mut outcome = setup_failure(
        "Worker unavailable; transcription did not start.",
        "worker-failed",
    );
    outcome.operation_id = Some(job.operation.identity.operation_id.clone());
    if let Work::Capture {
        recorder,
        wav,
        duration_ms,
    } = job.work
    {
        let wav = recorder.stop().unwrap_or(wav);
        let saved = persist_attempt(
            &job.operation.identity.take_id,
            duration_ms,
            None,
            Some(&wav),
            false,
            true,
        );
        if job.operation.cancel.load(Ordering::Acquire) {
            outcome.completeness = Completeness::Cancelled;
            outcome.delivery = Delivery::Cancelled;
            outcome.message = "Cancelled; transcription did not start.".to_owned();
            outcome.error = None;
        }
        outcome.artifacts = saved.artifacts;
        describe_artifacts(&mut outcome);
        storage_warning(&mut outcome, saved.warning);
        if saved.durable_audio {
            remove_runtime(&wav, &mut outcome);
        }
    }
    job.operation
        .lifecycle
        .store(CANCEL_SEALED, Ordering::Release);
    outcome
}

struct WorkerContext<'a> {
    operation: &'a Operation,
    config: &'a Config,
    guard: Option<&'a DeliveryGuard>,
    stages: &'a Sender<StageEvent>,
}

impl WorkerContext<'_> {
    fn id(&self) -> &str {
        &self.operation.identity.take_id
    }

    fn cancelled(&self) -> bool {
        self.operation.cancel.load(Ordering::Acquire)
            || self.operation.lifecycle.load(Ordering::Acquire) == CANCEL_REQUESTED
    }

    fn stage(&self, stage: Stage) {
        let _ = self.stages.send(StageEvent {
            identity: self.operation.identity.clone(),
            stage,
        });
    }

    fn outcome(&self, completeness: Completeness, delivery: Delivery) -> TerminalOutcome {
        TerminalOutcome {
            event_id: 0,
            operation_id: Some(self.operation.identity.operation_id.clone()),
            message: match completeness {
                Completeness::Cancelled => "Cancelled.",
                Completeness::Empty => "No text returned.",
                Completeness::Failed => "The operation failed.",
                Completeness::Partial => "Partial transcription.",
                Completeness::Complete => "Transcription complete.",
            }
            .to_owned(),
            completeness,
            delivery,
            cleanup: Cleanup::Off,
            error: None,
            artifacts: facts(self.id()),
            dismissed: false,
        }
    }

    fn deliver(&self, text: &str, partial: bool) -> DeliveryReport {
        self.stage(Stage::Delivering);
        deliver_with(
            text,
            self.config.injection,
            partial,
            &self.operation.cancel,
            |text, mode| match self.guard {
                Some(guard) => inject::inject(text, mode, guard, &self.operation.cancel),
                None => Err(InjectionFailure {
                    kind: InjectionFailureKind::Deferred,
                    message: "The intended destination is unknown.".to_owned(),
                }),
            },
        )
    }
}

fn run_job(
    job: Job,
    transcriber: &mut pipeline::TranscriberCache,
    stages: &Sender<StageEvent>,
) -> WorkerResult {
    let Job {
        operation,
        config,
        guard,
        work,
    } = job;
    let context = WorkerContext {
        operation: &operation,
        config: &config,
        guard: guard.as_ref(),
        stages,
    };
    let (mut outcome, telemetry) = match work {
        Work::Capture {
            recorder,
            wav,
            duration_ms,
        } => match recorder.stop() {
            Ok(wav) => process_audio(
                &context,
                transcriber,
                wav,
                duration_ms,
                pipeline::Source::Dictation,
            ),
            Err(_) => {
                // Finalization failure never authorizes deleting available audio.
                let saved =
                    persist_attempt(context.id(), duration_ms, None, Some(&wav), false, true);
                let mut outcome = context.outcome(Completeness::Failed, Delivery::None);
                outcome.message = "Recording could not be finalized.".to_owned();
                outcome.error = Some("capture-failed".to_owned());
                outcome.artifacts = saved.artifacts;
                describe_artifacts(&mut outcome);
                storage_warning(&mut outcome, saved.warning);
                if saved.durable_audio {
                    remove_runtime(&wav, &mut outcome);
                }
                (outcome, None)
            }
        },
        Work::Recover => match recovery::audio_path(context.id()) {
            Ok(wav) => {
                let duration = stt::wav_duration_ms(&wav).unwrap_or(0);
                process_audio(
                    &context,
                    transcriber,
                    wav,
                    duration,
                    pipeline::Source::Recover,
                )
            }
            Err(_) => {
                let mut outcome = context.outcome(Completeness::Failed, Delivery::None);
                outcome.message = "That recording's audio is no longer available.".to_owned();
                outcome.error = Some("no-recording".to_owned());
                (outcome, None)
            }
        },
        Work::Text => replay_text(&context),
        Work::Forget => {
            let removed = recovery::forget(context.id());
            let mut outcome = context.outcome(Completeness::Complete, Delivery::None);
            outcome.message = "Retained recording removed.".to_owned();
            if removed.is_err() {
                outcome.completeness = Completeness::Failed;
                outcome.message = "Recording removal did not complete.".to_owned();
                outcome.error = Some("forget-failed".to_owned());
            }
            // get, including after unlink/fsync failure, reports actual files.
            outcome.artifacts = facts(context.id());
            if removed.is_ok() && outcome.artifacts.text {
                outcome.message.push_str(" Archived text is unchanged.");
            }
            (outcome, None)
        }
    };
    operation.lifecycle.store(CANCEL_SEALED, Ordering::Release);
    let recordings = recovery::list().ok();
    if recordings.is_none() {
        storage_warning(&mut outcome, true);
    }
    tracing::info!(
        "[Daemon] operation settled completeness={:?} delivery={:?} cleanup={:?}",
        outcome.completeness,
        outcome.delivery,
        outcome.cleanup
    );
    WorkerResult {
        identity: operation.identity.clone(),
        outcome,
        recordings,
        telemetry: telemetry.map(|job| (config.telemetry.clone(), job)),
    }
}

/// File presence and permission to discard the source are separate facts.
struct SavedArtifacts {
    artifacts: Artifacts,
    durable_audio: bool,
    durable_text: bool,
    audio_conflict: bool,
    warning: bool,
}

fn persist_attempt(
    take_id: &str,
    duration_ms: u64,
    text: Option<&str>,
    wav: Option<&Path>,
    partial: bool,
    unresolved: bool,
) -> SavedArtifacts {
    let saved = recovery::persist(take_id, duration_ms, text, wav, partial, unresolved);
    let (take, durable, mut audio_conflict) = match saved {
        Ok(take) => {
            let artifacts = artifacts_for(take_id, Some(&take));
            return SavedArtifacts {
                durable_audio: artifacts.audio,
                // A successful complete persist publishes exactly this text.
                durable_text: !partial
                    && text.is_some_and(|text| !text.trim().is_empty())
                    && artifacts.text,
                audio_conflict: false,
                warning: wav.is_some() && !artifacts.audio,
                artifacts,
            };
        }
        Err(error) => {
            let audio_conflict = error.is::<recovery::AudioMismatch>();
            tracing::warn!("[Daemon] artifact persistence failed class=storage-failed");
            // Read attempted writes even if the subsequent confirmation fails.
            let present = recovery::get(take_id).ok();
            match recovery::confirm(take_id) {
                Ok(take) => (Some(take), true, audio_conflict),
                Err(_) => (present, false, audio_conflict),
            }
        }
    };
    let mut saved = saved_artifacts(
        take_id,
        take.as_ref(),
        durable,
        text,
        wav.is_some(),
        partial,
    );
    if let Some(wav) = wav {
        match recovery::confirm_audio(take_id, wav) {
            Ok(take) => {
                saved.durable_audio = take.audio_available;
                saved.warning |= !take.audio_available;
            }
            Err(error) => {
                audio_conflict |= error.is::<recovery::AudioMismatch>();
                saved.durable_audio = false;
                saved.warning = true;
            }
        }
    }
    saved.audio_conflict = audio_conflict;
    saved.durable_audio &= !audio_conflict;
    saved.durable_text &= !audio_conflict;
    saved.warning |= audio_conflict;
    saved
}

fn saved_artifacts(
    take_id: &str,
    take: Option<&Take>,
    durable: bool,
    text: Option<&str>,
    wants_audio: bool,
    partial: bool,
) -> SavedArtifacts {
    let artifacts = artifacts_for(take_id, take);
    let matching_text = !partial
        && text
            .filter(|text| !text.trim().is_empty())
            .is_some_and(|text| recovery::read_text(take_id).is_ok_and(|saved| saved == text));
    SavedArtifacts {
        durable_audio: durable && artifacts.audio,
        durable_text: durable && matching_text,
        audio_conflict: false,
        warning: !durable
            || (wants_audio && !artifacts.audio)
            || (!partial && text.is_some_and(|text| !text.trim().is_empty()) && !matching_text),
        artifacts,
    }
}

fn artifacts_for(take_id: &str, take: Option<&Take>) -> Artifacts {
    Artifacts {
        take_id: Some(take_id.to_owned()),
        audio: take.is_some_and(|take| take.audio_available),
        text: take.is_some_and(|take| take.text_available),
    }
}

fn facts(take_id: &str) -> Artifacts {
    artifacts_for(take_id, recovery::get(take_id).ok().as_ref())
}

fn process_audio(
    context: &WorkerContext<'_>,
    transcriber: &mut pipeline::TranscriberCache,
    wav: PathBuf,
    duration_ms: u64,
    source: pipeline::Source,
) -> (TerminalOutcome, Option<telemetry::JobTelemetry>) {
    let runtime_source = source == pipeline::Source::Dictation;
    // Secure the full take before entering blocking inference. Recovery uses
    // the same immutable sidecar; no retry changes another recording's files.
    let initial = persist_attempt(context.id(), duration_ms, None, Some(&wav), false, true);
    if initial.audio_conflict {
        let mut outcome = context.outcome(Completeness::Failed, Delivery::None);
        outcome.message =
            "Recording identity does not match its retained audio; transcription did not start."
                .to_owned();
        outcome.artifacts = initial.artifacts;
        describe_artifacts(&mut outcome);
        storage_warning(&mut outcome, true);
        return (outcome, None);
    }
    if context.cancelled() {
        let mut outcome = context.outcome(Completeness::Cancelled, Delivery::Cancelled);
        outcome.artifacts = initial.artifacts;
        describe_artifacts(&mut outcome);
        storage_warning(&mut outcome, initial.warning);
        if runtime_source && initial.durable_audio {
            remove_runtime(&wav, &mut outcome);
        }
        return (outcome, None);
    }
    let pipeline = pipeline::run(
        transcriber,
        &wav,
        &context.config.stt,
        &context.config.vocabulary,
        &context.config.postproc,
        pipeline::RunContext {
            source,
            take_id: Some(context.id()),
            cancel: Some(&context.operation.cancel),
        },
        |stage| context.stage(stage),
    );
    finish_audio(context, &wav, duration_ms, source, pipeline)
}

fn finish_audio(
    context: &WorkerContext<'_>,
    wav: &Path,
    duration_ms: u64,
    source: pipeline::Source,
    pipeline: pipeline::Outcome,
) -> (TerminalOutcome, Option<telemetry::JobTelemetry>) {
    let text = pipeline
        .text
        .as_ref()
        .ok()
        .map(String::as_str)
        .filter(|text| !text.trim().is_empty());
    let cancelled = pipeline.cancelled || context.cancelled();
    let completeness = if cancelled {
        Completeness::Cancelled
    } else if pipeline.text.is_err() {
        Completeness::Failed
    } else if pipeline.partial {
        Completeness::Partial
    } else if text.is_none() {
        Completeness::Empty
    } else {
        Completeness::Complete
    };
    let mut outcome = context.outcome(completeness, Delivery::None);
    outcome.cleanup = cleanup_from(&pipeline.postproc);
    let mut saved = persist_attempt(
        context.id(),
        duration_ms,
        text,
        Some(wav),
        pipeline.partial || cancelled,
        true,
    );
    let mut report = DeliveryReport::none();
    if completeness == Completeness::Cancelled {
        report.delivery = Delivery::Cancelled;
    } else if let Some(text) = text {
        report = context.deliver(text, pipeline.partial);
        outcome.delivery = report.delivery;
        if report.delivery == Delivery::Cancelled {
            outcome.completeness = Completeness::Cancelled;
            // Cancellation after successful STT is still an incomplete attempt.
            saved = persist_attempt(context.id(), duration_ms, Some(text), Some(wav), true, true);
        } else if completeness == Completeness::Complete
            && report.delivered()
            && saved.durable_text
            && context.operation.seal()
        {
            let resolved = recovery::resolve(context.id()).is_ok();
            // Re-confirm durability after updating the take's resolution.
            let confirmed = recovery::confirm(context.id());
            saved = saved_artifacts(
                context.id(),
                confirmed.as_ref().ok(),
                confirmed.is_ok(),
                Some(text),
                false,
                false,
            );
            if confirmed.is_err() {
                saved.artifacts = facts(context.id());
            }
            saved.warning |= !resolved;
        }
    }
    if context.cancelled() && !report.delivered() && report.delivery != Delivery::Uncertain {
        outcome.completeness = Completeness::Cancelled;
        report.delivery = Delivery::Cancelled;
        report.error = None;
    }
    outcome.delivery = report.delivery;
    outcome.artifacts = saved.artifacts;
    outcome.error = report
        .error
        .map(str::to_owned)
        .or_else(|| match outcome.completeness {
            Completeness::Failed => Some("stt-failed".to_owned()),
            Completeness::Partial => Some("stt-partial".to_owned()),
            _ if outcome.cleanup == Cleanup::Failed => Some("cleanup-failed".to_owned()),
            _ => None,
        });
    outcome.message = describe_delivery(&outcome, context.config.injection);
    if let Err(error) = &pipeline.text {
        if outcome.completeness != Completeness::Cancelled {
            let notice = stt::classify_failure(error);
            tracing::warn!("[STT] transcription failed notice={notice}");
            outcome.message = format!("{notice}.");
        }
    }
    if context.cancelled() && report.delivered() {
        outcome
            .message
            .push_str(" Cancellation arrived after delivery.");
    }
    if !report.delivered() || outcome.completeness == Completeness::Partial {
        describe_artifacts(&mut outcome);
    }
    if pipeline.local_fallback {
        outcome
            .message
            .push_str(" Used local transcription after cloud failure.");
    }
    if outcome.cleanup == Cleanup::Failed {
        outcome
            .message
            .push_str(" Cleanup was unavailable; original text was used.");
    }
    let archive_failed = matches!(pipeline.archive, pipeline::ArchiveStatus::Failed(_));
    storage_warning(&mut outcome, saved.warning);
    if archive_failed {
        if outcome
            .error
            .as_deref()
            .is_none_or(|error| error == "cleanup-failed")
        {
            outcome.error = Some("history-incomplete".to_owned());
        }
        outcome
            .message
            .push_str(" Detailed transcript history could not be saved.");
    }
    if source == pipeline::Source::Dictation && saved.durable_audio {
        remove_runtime(wav, &mut outcome);
    }
    let metadata = context.config.telemetry.enabled.then(|| {
        let capture_ms = if source == pipeline::Source::Dictation {
            duration_ms
        } else {
            0
        };
        let cleanup_ms = match pipeline.postproc {
            PostprocStatus::Applied { ms } | PostprocStatus::Failed { ms } => {
                Some(milliseconds(ms))
            }
            _ => None,
        };
        telemetry::JobTelemetry {
            source: source.as_str(),
            capture_ms,
            stt_ms: milliseconds(pipeline.stt_elapsed.as_millis()),
            stt_model: if pipeline.local_fallback {
                models::PARAKEET_V3_INT8.dir_name.to_owned()
            } else {
                context.config.stt.model.clone()
            },
            stt_remote: context.config.stt.endpoint.is_some() && !pipeline.local_fallback,
            chars: text.map_or(0, |text| text.chars().count()),
            partial: pipeline.partial,
            cleanup_state: match outcome.cleanup {
                Cleanup::Applied => "applied",
                Cleanup::Failed => "failed",
                Cleanup::Skipped => "skipped_short",
                Cleanup::Off => "off",
            },
            cleanup_ms,
            cleanup_model: cleanup_ms.map(|_| context.config.postproc.model.clone()),
            tokens_in: pipeline
                .postproc_usage
                .as_ref()
                .map(|usage| usage.prompt_tokens),
            tokens_out: pipeline
                .postproc_usage
                .as_ref()
                .map(|usage| usage.completion_tokens),
            tokens_total: pipeline
                .postproc_usage
                .as_ref()
                .map(|usage| usage.total_tokens),
            inject_ms: report.elapsed_ms,
            delivered: report.backend,
            error_class: outcome.error.clone(),
            total_ms: milliseconds(context.operation.started.elapsed().as_millis()),
        }
    });
    (outcome, metadata)
}

fn replay_text(context: &WorkerContext<'_>) -> (TerminalOutcome, Option<telemetry::JobTelemetry>) {
    let take = recovery::get(context.id());
    let text = recovery::read_text(context.id());
    let (Ok(take), Ok(text)) = (take, text) else {
        let mut outcome = context.outcome(Completeness::Failed, Delivery::None);
        outcome.message = "That recording's transcript is no longer available.".to_owned();
        outcome.error = Some("no-transcript".to_owned());
        return (outcome, None);
    };
    let report = context.deliver(&text, take.partial);
    let completeness = if report.delivery == Delivery::Cancelled {
        Completeness::Cancelled
    } else if take.partial {
        Completeness::Partial
    } else {
        Completeness::Complete
    };
    let mut outcome = context.outcome(completeness, report.delivery);
    outcome.error = report
        .error
        .map(str::to_owned)
        .or_else(|| (completeness == Completeness::Partial).then(|| "stt-partial".to_owned()));
    outcome.message = describe_delivery(&outcome, context.config.injection);
    if !report.delivered() || take.partial {
        describe_artifacts(&mut outcome);
    }
    // Replaying an earlier transcript is not a successful retry of its audio.
    let metadata = context
        .config
        .telemetry
        .enabled
        .then(|| telemetry::JobTelemetry {
            source: pipeline::Source::Replay.as_str(),
            chars: text.chars().count(),
            partial: take.partial,
            cleanup_state: "off",
            inject_ms: report.elapsed_ms,
            delivered: report.backend,
            error_class: outcome.error.clone(),
            total_ms: milliseconds(context.operation.started.elapsed().as_millis()),
            ..telemetry::JobTelemetry::default()
        });
    (outcome, metadata)
}

#[derive(Debug)]
struct DeliveryReport {
    delivery: Delivery,
    error: Option<&'static str>,
    elapsed_ms: Option<u64>,
    backend: Option<&'static str>,
}

impl DeliveryReport {
    fn none() -> Self {
        Self {
            delivery: Delivery::None,
            error: None,
            elapsed_ms: None,
            backend: None,
        }
    }

    fn delivered(&self) -> bool {
        matches!(
            self.delivery,
            Delivery::Typed | Delivery::Pasted | Delivery::Copied
        )
    }
}

fn deliver_with(
    text: &str,
    requested: InjectionMode,
    partial: bool,
    cancel: &AtomicBool,
    mut inject: impl FnMut(
        &str,
        InjectionMode,
    ) -> std::result::Result<InjectionOutcome, InjectionFailure>,
) -> DeliveryReport {
    if cancel.load(Ordering::Acquire) {
        return DeliveryReport {
            delivery: Delivery::Cancelled,
            ..DeliveryReport::none()
        };
    }
    if partial && requested == InjectionMode::Type {
        return DeliveryReport {
            delivery: Delivery::Deferred,
            error: Some("stt-partial"),
            ..DeliveryReport::none()
        };
    }
    let mode = if partial {
        InjectionMode::Clipboard
    } else {
        requested
    };
    let started = Instant::now();
    let mut result = inject(text, mode);
    if matches!(&result, Err(error) if error.kind == InjectionFailureKind::Deferred)
        && matches!(mode, InjectionMode::Auto | InjectionMode::Paste)
        && !cancel.load(Ordering::Acquire)
    {
        // Deferred guarantees no keys were initiated. Uncertain is never retried.
        result = inject(text, InjectionMode::Clipboard);
    }
    if cancel.load(Ordering::Acquire)
        && matches!(&result, Err(error) if matches!(error.kind, InjectionFailureKind::Deferred | InjectionFailureKind::Failed))
    {
        return DeliveryReport {
            delivery: Delivery::Cancelled,
            elapsed_ms: Some(milliseconds(started.elapsed().as_millis())),
            ..DeliveryReport::none()
        };
    }
    let (delivery, backend, error) = match result {
        Ok(InjectionOutcome::Typed(_)) => (Delivery::Typed, Some("typed-wayland"), None),
        Ok(InjectionOutcome::Pasted) => (Delivery::Pasted, Some("pasted"), None),
        Ok(InjectionOutcome::Clipboard) => (Delivery::Copied, Some("clipboard"), None),
        Err(error) => match error.kind {
            InjectionFailureKind::Cancelled => (Delivery::Cancelled, None, None),
            InjectionFailureKind::Deferred => {
                (Delivery::Deferred, None, Some("injection-deferred"))
            }
            InjectionFailureKind::Failed => (Delivery::Failed, None, Some("injection-failed")),
            InjectionFailureKind::Uncertain => {
                (Delivery::Uncertain, None, Some("injection-uncertain"))
            }
        },
    };
    DeliveryReport {
        delivery,
        backend,
        error,
        elapsed_ms: Some(milliseconds(started.elapsed().as_millis())),
    }
}

fn describe_delivery(outcome: &TerminalOutcome, requested: InjectionMode) -> String {
    match (outcome.completeness, outcome.delivery) {
        (Completeness::Cancelled, _) => "Cancelled.".to_owned(),
        (Completeness::Empty, _) => "No text returned.".to_owned(),
        (Completeness::Failed, _) => "Transcription failed.".to_owned(),
        (Completeness::Partial, Delivery::Copied) => {
            "Partial text copied. Review before pasting.".to_owned()
        }
        (Completeness::Partial, Delivery::Uncertain) => {
            "Partial text copy is uncertain.".to_owned()
        }
        (Completeness::Partial, _) => "Partial text was not delivered.".to_owned(),
        (_, Delivery::Typed) => "Typed.".to_owned(),
        (_, Delivery::Pasted) => "Pasted.".to_owned(),
        (_, Delivery::Copied) if requested == InjectionMode::Clipboard => {
            "Copied. Paste when ready.".to_owned()
        }
        (_, Delivery::Copied) => "Copied instead. Paste when ready.".to_owned(),
        (_, Delivery::Uncertain) => {
            "Delivery is uncertain; check the destination before trying again.".to_owned()
        }
        (_, Delivery::Deferred) => {
            "Not delivered to the changed or unverified destination.".to_owned()
        }
        (_, Delivery::Failed) => "Delivery failed.".to_owned(),
        _ => "No text delivered.".to_owned(),
    }
}

fn describe_artifacts(outcome: &mut TerminalOutcome) {
    match (outcome.artifacts.text, outcome.artifacts.audio) {
        (true, true) => outcome
            .message
            .push_str(" Saved text and audio are available."),
        (true, false) => outcome.message.push_str(" Saved text is available."),
        (false, true) => outcome.message.push_str(" Saved audio is available."),
        (false, false) => {}
    }
}

fn storage_warning(outcome: &mut TerminalOutcome, warning: bool) {
    if warning && outcome.error.as_deref() != Some("storage-failed") {
        outcome.error = Some("storage-failed".to_owned());
        outcome
            .message
            .push_str(" Recording storage is incomplete; check storage before recording again.");
    }
}

fn remove_runtime(path: &Path, outcome: &mut TerminalOutcome) {
    if capture::remove_recording(path).is_err() {
        storage_warning(outcome, true);
    }
}

fn cleanup_from(status: &PostprocStatus) -> Cleanup {
    match status {
        PostprocStatus::Off => Cleanup::Off,
        PostprocStatus::Applied { .. } => Cleanup::Applied,
        PostprocStatus::SkippedShort { .. } => Cleanup::Skipped,
        PostprocStatus::Failed { .. } => Cleanup::Failed,
    }
}

fn milliseconds(ms: u128) -> u64 {
    u64::try_from(ms).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn take(id: &str, audio: bool, text: bool) -> Take {
        Take {
            id: id.to_owned(),
            created_at_unix_ms: 1,
            duration_ms: Some(4_000),
            text_available: text,
            audio_available: audio,
            partial: false,
            unresolved: audio,
        }
    }

    fn idle_daemon() -> Daemon {
        Daemon::new(Config::default(), vec![take("keep-me", true, true)], false)
    }

    fn processing(kind: WorkKind) -> (Daemon, Operation) {
        let mut daemon = idle_daemon();
        let operation = daemon.operation("keep-me".to_owned(), Some(OperationKind::Dictation));
        daemon.begin(
            operation.clone(),
            Stage::Transcribing {
                completed: 0,
                total: 2,
            },
            kind,
        );
        (daemon, operation)
    }

    #[test]
    fn status_size_is_independent_of_history_size() {
        let mut daemon = idle_daemon();
        let baseline = encode_reply(&daemon.snapshot()).unwrap().len();
        daemon.replace_recordings(
            (0..10_000)
                .map(|id| take(&format!("take-{id}"), true, true))
                .collect(),
        );
        let snapshot = daemon.snapshot();
        assert_eq!(snapshot.pending_recordings, 10_000);
        assert!(encode_reply(&snapshot).unwrap().len() < baseline + 10);
    }

    #[test]
    fn capture_signal_none_is_starting_not_proved_listening() {
        let mut daemon = idle_daemon();
        let operation = daemon.operation("new".to_owned(), Some(OperationKind::Dictation));
        daemon.state = State::Recording {
            operation,
            recorder: Box::new(FakeRecorder::default()),
            wav: PathBuf::from("/tmp/unused.wav"),
            config: Box::new(Config::default()),
            started: Instant::now(),
            signal: None,
            next_signal_sample: Instant::now() + Duration::from_secs(30),
        };
        assert!(daemon.snapshot().signal.is_none());
        assert_eq!(
            daemon.snapshot().operation_kind,
            Some(OperationKind::Dictation)
        );
    }

    #[test]
    fn busy_notice_does_not_replace_active_or_outcome() {
        let (mut daemon, _) = processing(WorkKind::Transcription);
        let (job_tx, job_rx) = mpsc::channel();
        let reply = execute(
            Command::Start { postproc: None },
            &mut daemon,
            Path::new("/tmp"),
            &job_tx,
        );
        assert!(!reply.ok);
        assert_eq!(reply.error.as_deref(), Some("busy"));
        assert!(matches!(daemon.state, State::Processing { .. }));
        assert!(daemon.outcome.is_none());
        assert_eq!(
            daemon.notice.as_ref().unwrap().error.as_deref(),
            Some("busy")
        );
        assert!(job_rx.try_recv().is_err());
    }

    #[test]
    fn processing_cancel_acknowledges_without_settling() {
        let (mut daemon, operation) = processing(WorkKind::Transcription);
        let (job_tx, job_rx) = mpsc::channel();
        let reply = execute(Command::Cancel, &mut daemon, Path::new("/tmp"), &job_tx);
        assert!(reply.ok);
        assert_eq!(reply.stage, Some(Stage::Cancelling));
        assert!(operation.cancel.load(Ordering::SeqCst));
        assert!(matches!(
            daemon.state,
            State::Processing {
                stage: Stage::Cancelling,
                ..
            }
        ));
        assert!(job_rx.try_recv().is_err());
    }

    #[test]
    fn cancelling_capture_revokes_delivery_before_finalization() {
        let mut daemon = idle_daemon();
        let operation =
            daemon.operation("cancelled-take".to_owned(), Some(OperationKind::Dictation));
        daemon.state = State::Recording {
            operation,
            recorder: Box::new(FakeRecorder::default()),
            wav: PathBuf::from("/tmp/unused.wav"),
            config: Box::new(Config::default()),
            started: Instant::now(),
            signal: None,
            next_signal_sample: Instant::now(),
        };
        let (sender, receiver) = mpsc::channel();
        assert!(cancel_command(&mut daemon, &sender).ok);
        let job = receiver.try_recv().unwrap();
        let report = deliver_with(
            "must remain saved",
            InjectionMode::Clipboard,
            false,
            &job.operation.cancel,
            |_, _| panic!("cancelled capture must not reach desktop delivery"),
        );
        assert_eq!(report.delivery, Delivery::Cancelled);
        assert_eq!(daemon.snapshot().stage, Some(Stage::Cancelling));
    }

    #[test]
    fn stale_worker_result_cannot_replace_a_newer_operation() {
        let (mut daemon, old) = processing(WorkKind::Transcription);
        let reporter = TelemetryReporter::spawn();
        let newer = daemon.operation("keep-me".to_owned(), Some(OperationKind::Replay));
        daemon.begin(newer.clone(), Stage::Delivering, WorkKind::Delivery);
        apply_worker_result(
            &mut daemon,
            WorkerResult {
                identity: old.identity,
                outcome: TerminalOutcome {
                    event_id: 0,
                    operation_id: Some("old".to_owned()),
                    message: "should not appear".to_owned(),
                    completeness: Completeness::Complete,
                    delivery: Delivery::Pasted,
                    cleanup: Cleanup::Off,
                    error: None,
                    artifacts: Artifacts::default(),
                    dismissed: false,
                },
                recordings: Some(Vec::new()),
                telemetry: None,
            },
            &reporter,
        );
        assert!(matches!(daemon.state, State::Processing { .. }));
        assert!(daemon.outcome.is_none());
        assert_eq!(
            daemon.state.operation().unwrap().identity.operation_id,
            newer.identity.operation_id
        );
        assert_eq!(daemon.snapshot().pending_recordings, 1);
        reporter.shutdown();
    }

    #[test]
    fn targeted_dismiss_does_not_hide_an_independent_notice_or_delete_artifacts() {
        let mut daemon = idle_daemon();
        daemon.notice("busy", Some("busy"));
        daemon.publish(TerminalOutcome {
            event_id: 0,
            operation_id: Some("op".to_owned()),
            message: "Partial text copied (4 chars).".to_owned(),
            completeness: Completeness::Partial,
            delivery: Delivery::Copied,
            cleanup: Cleanup::Off,
            error: Some("stt-partial".to_owned()),
            artifacts: Artifacts {
                take_id: Some("keep-me".to_owned()),
                audio: true,
                text: true,
            },
            dismissed: false,
        });
        let event = daemon.outcome.as_ref().unwrap().event_id;
        let (job_tx, _) = mpsc::channel();
        let reply = execute(
            Command::Dismiss {
                event_id: Some(event),
            },
            &mut daemon,
            Path::new("/tmp"),
            &job_tx,
        );
        assert!(reply.ok);
        assert!(daemon.notice.is_some());
        let outcome = daemon.outcome.unwrap();
        assert!(outcome.dismissed);
        assert!(outcome.artifacts.audio);
        assert_eq!(outcome.artifacts.take_id.as_deref(), Some("keep-me"));
    }

    #[test]
    fn recover_rejects_unknown_identity_instead_of_retargeting() {
        let mut daemon = idle_daemon();
        let (job_tx, job_rx) = mpsc::channel();
        let reply = execute(
            Command::Recover {
                id: Some("missing".to_owned()),
                local: false,
                clipboard: true,
            },
            &mut daemon,
            Path::new("/tmp"),
            &job_tx,
        );
        assert!(!reply.ok);
        assert_eq!(reply.error.as_deref(), Some("no-recording"));
        assert!(job_rx.try_recv().is_err());
        assert!(matches!(daemon.state, State::Idle));
    }

    #[test]
    fn cancellation_prevents_every_delivery_mode() {
        for mode in [
            InjectionMode::Auto,
            InjectionMode::Type,
            InjectionMode::Paste,
            InjectionMode::Clipboard,
        ] {
            let report = deliver_with(
                "never dispatched",
                mode,
                false,
                &AtomicBool::new(true),
                |_, _| {
                    panic!("cancelled work must not reach the delivery boundary");
                },
            );
            assert_eq!(report.delivery, Delivery::Cancelled);
        }
    }

    #[test]
    fn partial_text_never_reaches_keyboard_or_paste_delivery() {
        let cancel = AtomicBool::new(false);
        let strict = deliver_with("incomplete", InjectionMode::Type, true, &cancel, |_, _| {
            panic!("strict typing cannot fall back to clipboard");
        });
        assert_eq!(strict.delivery, Delivery::Deferred);
        for requested in [
            InjectionMode::Auto,
            InjectionMode::Paste,
            InjectionMode::Clipboard,
        ] {
            let report = deliver_with("incomplete", requested, true, &cancel, |_, actual| {
                assert_eq!(actual, InjectionMode::Clipboard);
                Ok(InjectionOutcome::Clipboard)
            });
            assert_eq!(report.delivery, Delivery::Copied);
        }
    }

    #[test]
    fn uncertain_delivery_never_starts_a_second_backend() {
        let mut calls = 0;
        let report = deliver_with(
            "do not duplicate",
            InjectionMode::Auto,
            false,
            &AtomicBool::new(false),
            |_, _| {
                calls += 1;
                Err(InjectionFailure {
                    kind: InjectionFailureKind::Uncertain,
                    message: "not payload".to_owned(),
                })
            },
        );
        assert_eq!(calls, 1);
        assert_eq!(report.delivery, Delivery::Uncertain);
        assert_eq!(report.error, Some("injection-uncertain"));
    }

    #[test]
    fn focus_deferral_copies_only_when_policy_allows_and_not_after_cancel() {
        let cancel = AtomicBool::new(false);
        let mut modes = Vec::new();
        let report = deliver_with(
            "recoverable",
            InjectionMode::Paste,
            false,
            &cancel,
            |_, mode| {
                modes.push(mode);
                if mode == InjectionMode::Clipboard {
                    Ok(InjectionOutcome::Clipboard)
                } else {
                    Err(InjectionFailure {
                        kind: InjectionFailureKind::Deferred,
                        message: String::new(),
                    })
                }
            },
        );
        assert_eq!(report.delivery, Delivery::Copied);
        assert_eq!(modes, [InjectionMode::Paste, InjectionMode::Clipboard]);
        let mut calls = 0;
        let interrupted = deliver_with(
            "recoverable",
            InjectionMode::Auto,
            false,
            &cancel,
            |_, _| {
                calls += 1;
                cancel.store(true, Ordering::Release);
                Err(InjectionFailure {
                    kind: InjectionFailureKind::Deferred,
                    message: String::new(),
                })
            },
        );
        assert_eq!(calls, 1);
        assert_eq!(interrupted.delivery, Delivery::Cancelled);
    }

    #[test]
    fn completed_delivery_is_not_relabelled_as_cancellation() {
        let cancel = AtomicBool::new(false);
        let report = deliver_with(
            "already sent",
            InjectionMode::Type,
            false,
            &cancel,
            |_, _| {
                cancel.store(true, Ordering::Release);
                Ok(InjectionOutcome::Typed("wayland"))
            },
        );
        assert_eq!(report.delivery, Delivery::Typed);
    }

    #[test]
    fn stage_updates_match_epoch_and_preserve_elapsed_chunk_phase() {
        let (mut daemon, operation) = processing(WorkKind::Transcription);
        if let State::Processing { phase_started, .. } = &mut daemon.state {
            *phase_started = Instant::now() - Duration::from_secs(60);
        }
        let (sender, receiver) = mpsc::channel();
        sender
            .send(StageEvent {
                identity: operation.identity.clone(),
                stage: Stage::Transcribing {
                    completed: 1,
                    total: 2,
                },
            })
            .unwrap();
        let foreign = Arc::new(Identity {
            epoch: Arc::from("other-epoch"),
            operation_id: operation.identity.operation_id.clone(),
            take_id: operation.identity.take_id.clone(),
            kind: operation.identity.kind,
        });
        sender
            .send(StageEvent {
                identity: foreign,
                stage: Stage::Delivering,
            })
            .unwrap();
        drain_stage(&mut daemon, &receiver);
        assert_eq!(
            daemon.snapshot().stage,
            Some(Stage::Transcribing {
                completed: 1,
                total: 2
            })
        );
        assert!(daemon.snapshot().elapsed >= 60);
        sender
            .send(StageEvent {
                identity: operation.identity.clone(),
                stage: Stage::CleaningUp,
            })
            .unwrap();
        drain_stage(&mut daemon, &receiver);
        assert!(daemon.snapshot().elapsed < 10);
        let (jobs, _) = mpsc::channel();
        assert!(cancel_command(&mut daemon, &jobs).ok);
        sender
            .send(StageEvent {
                identity: operation.identity,
                stage: Stage::Delivering,
            })
            .unwrap();
        drain_stage(&mut daemon, &receiver);
        assert_eq!(daemon.snapshot().stage, Some(Stage::Cancelling));
    }

    #[test]
    fn cancellation_after_settlement_is_rejected() {
        let (mut daemon, operation) = processing(WorkKind::Delivery);
        assert!(operation.seal());
        let (sender, _) = mpsc::channel();
        assert!(!cancel_command(&mut daemon, &sender).ok);
        assert!(!operation.cancel.load(Ordering::Acquire));
    }

    #[test]
    fn accepted_cancellation_prevents_successful_settlement() {
        let (mut daemon, operation) = processing(WorkKind::Transcription);
        let (sender, _) = mpsc::channel();
        assert!(cancel_command(&mut daemon, &sender).ok);
        assert!(!operation.seal());
        assert!(!operation.request_cancel());
        assert_eq!(daemon.snapshot().stage, Some(Stage::Cancelling));
    }

    #[test]
    fn present_audio_without_durability_cannot_authorize_source_deletion() {
        let take = take("present-not-synced", true, false);
        let saved = saved_artifacts(&take.id, Some(&take), false, None, true, false);
        assert!(saved.artifacts.audio);
        assert!(!saved.durable_audio);
        assert!(!saved.durable_text);
        assert!(saved.warning);
    }

    #[test]
    fn shutdown_revokes_processing_delivery() {
        let (mut daemon, operation) = processing(WorkKind::Transcription);
        shutdown_state(&mut daemon.state);
        let report = deliver_with(
            "must stay saved",
            InjectionMode::Auto,
            false,
            &operation.cancel,
            |_, _| {
                panic!("shutdown must revoke delivery");
            },
        );
        assert_eq!(report.delivery, Delivery::Cancelled);
    }

    #[test]
    fn capture_policy_survives_live_configuration_change() {
        let mut daemon = idle_daemon();
        daemon.config.injection = InjectionMode::Type;
        daemon.config.hud.labels = true;
        let operation = daemon.operation("stable-take".to_owned(), Some(OperationKind::Dictation));
        let recorder = FakeRecorder::default();
        let stop_requested = recorder.stop_requested.clone();
        daemon.state = State::Recording {
            operation,
            recorder: Box::new(recorder),
            wav: PathBuf::from("/unused-test-capture.wav"),
            config: Box::new(daemon.config.clone()),
            started: Instant::now(),
            signal: None,
            next_signal_sample: Instant::now(),
        };
        daemon.config.injection = InjectionMode::Clipboard;
        daemon.config.hud.labels = false;
        let (sender, receiver) = mpsc::channel();
        assert!(stop_recording(&mut daemon, &sender, false).ok);
        assert!(stop_requested.load(Ordering::Acquire));
        assert!(daemon.snapshot().hud.labels);
        let job = receiver.try_recv().unwrap();
        let report = deliver_with(
            "partial",
            job.config.injection,
            true,
            &job.operation.cancel,
            |_, _| {
                panic!("the reloaded clipboard policy must not apply to the earlier capture");
            },
        );
        assert_eq!(report.delivery, Delivery::Deferred);
    }

    #[test]
    fn blocked_reply_does_not_block_another_status_client() {
        let (server, _slow_reader) = UnixStream::pair().unwrap();
        let mut slow = accept_client(server).unwrap();
        slow.respond(Arc::new(vec![b'x'; 2_000_000]));
        for _ in 0..64 {
            assert!(matches!(poll_client(&mut slow), ClientPoll::Pending));
        }
        let (server, mut reader) = UnixStream::pair().unwrap();
        let mut client = accept_client(server).unwrap();
        reader.write_all(b"status\n").unwrap();
        assert!(matches!(
            poll_client(&mut client),
            ClientPoll::Ready(Ok(Request::Status))
        ));
        let (daemon, _) = processing(WorkKind::Delivery);
        client.respond(encode_reply(&daemon.snapshot()).unwrap());
        assert!(matches!(poll_client(&mut client), ClientPoll::Closed));
        drop(client);
        let snapshot: StatusSnapshot = serde_json::from_reader(reader).unwrap();
        assert_eq!(snapshot.epoch, daemon.epoch.as_ref());
        assert_eq!(snapshot.state, StateKind::Processing);
        assert!(snapshot.capabilities.cancel);
    }

    #[test]
    fn unterminated_and_invalid_utf8_requests_are_rejected() {
        let (server, mut client) = UnixStream::pair().unwrap();
        let mut pending = accept_client(server).unwrap();
        client.write_all(b"status").unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        assert!(matches!(
            poll_client(&mut pending),
            ClientPoll::Ready(Err(_))
        ));
        assert!(decode_request(&[0xff]).is_err());
    }

    struct FakeRecorder {
        signal: Option<InputSignal>,
        stop_requested: Arc<AtomicBool>,
        stop_result: Result<PathBuf>,
    }

    impl Default for FakeRecorder {
        fn default() -> Self {
            Self {
                signal: None,
                stop_requested: Arc::new(AtomicBool::new(false)),
                stop_result: Ok(PathBuf::from("/tmp/unused.wav")),
            }
        }
    }

    impl RecorderBoundary for FakeRecorder {
        fn input_signal(&mut self) -> Option<InputSignal> {
            self.signal
        }
        fn request_stop(&mut self) -> Result<()> {
            self.stop_requested.store(true, Ordering::Release);
            Ok(())
        }
        fn stop(self: Box<Self>) -> Result<PathBuf> {
            self.stop_result
        }
    }
}
