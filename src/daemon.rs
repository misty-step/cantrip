//! The cantrip daemon and its socket-driven state machine.

use crate::capture::{self, Recorder};
use crate::config::{Config, PostprocConfig, SttConfig};
use crate::inject::{self, InjectionMode, InjectionOutcome};
use crate::ipc::{Command, Reply};
use crate::models;
use crate::paths;
use crate::pipeline::{self, PostprocStatus};
use anyhow::{Context, Result};
use notify_rust::{Notification, NotificationHandle, Timeout};
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CLIENT_LINE_LIMIT: usize = 256;
const CLIENT_READ_DEADLINE: Duration = Duration::from_secs(2);

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn signal_handler(_signal: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

enum State {
    Idle,
    Recording {
        recorder: Recorder,
        started: Instant,
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

#[derive(Default)]
struct StatusUi {
    handle: Option<NotificationHandle>,
    last_shown_seconds: u64,
}

impl StatusUi {
    fn recording(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.close();
        }
        self.last_shown_seconds = 0;
        match Notification::new()
            .summary("Cantrip")
            .body("Listening… 0s — press your hotkey to stop")
            .timeout(Timeout::Never)
            .show()
        {
            Ok(handle) => self.handle = Some(handle),
            Err(error) => tracing::debug!("[Daemon] notification unavailable: {}", error),
        }
    }

    fn tick(&mut self, elapsed: Duration) {
        let Some(handle) = self.handle.as_mut() else {
            return;
        };
        let seconds = elapsed.as_secs();
        if seconds <= self.last_shown_seconds {
            return;
        }
        self.last_shown_seconds = seconds;
        let body = format!("Listening… {seconds}s — press your hotkey to stop");
        handle.body(&body);
        if let Err(error) = handle.update() {
            tracing::debug!("[Daemon] notification unavailable: {}", error);
        }
    }

    fn processing(&mut self) {
        let Some(handle) = self.handle.as_mut() else {
            return;
        };
        handle.body("Transcribing…");
        if let Err(error) = handle.update() {
            tracing::debug!("[Daemon] notification unavailable: {}", error);
        }
    }

    fn finish(&mut self, body: &str) {
        if let Some(handle) = self.handle.take() {
            handle.close();
        }
        if let Err(error) = Notification::new().summary("Cantrip").body(body).show() {
            tracing::debug!("[Daemon] notification unavailable: {}", error);
        }
    }
}

impl Drop for StatusUi {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.close();
        }
    }
}

struct Job {
    wav: PathBuf,
    stt: SttConfig,
    vocabulary: Vec<String>,
    postproc: PostprocConfig,
}

struct WorkerResult {
    result: std::result::Result<String, String>,
    stt_elapsed: Duration,
    postproc: PostprocStatus,
}

/// The daemon's most recent terminal outcome, surfaced on `status` so the
/// HUD can flash the true result instead of a fake success.
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
            } = job;
            let wav = RecordingCleanup(wav);
            let outcome = pipeline::run(
                &mut transcriber,
                &wav.0,
                &stt,
                &vocabulary,
                &postproc,
                &report_stage,
            );
            let worker_result = WorkerResult {
                result: outcome.text,
                stt_elapsed: outcome.stt_elapsed,
                postproc: outcome.postproc,
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
    let mut status = StatusUi::default();
    let mut last_outcome = LastOutcome::default();
    loop {
        drain_stage(&mut state, stage_rx);
        drain_worker_results(
            &mut state,
            &config,
            result_rx,
            &mut status,
            &mut last_outcome,
        )?;
        if SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }
        if let State::Recording { started, .. } = &state {
            status.tick(started.elapsed());
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
                        &mut status,
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
            Ok(result) => {
                handle_worker_result(&mut state, &config, result, &mut status, &mut last_outcome)?
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                tracing::warn!("[STT] transcription result timed out during shutdown");
                status.finish("Transcription failed: worker timed out");
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                status.finish("Transcription failed: worker died");
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
    status: &mut StatusUi,
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
    let reply = match Command::parse(&command_line) {
        Some(command) => execute(
            command,
            state,
            config,
            runtime_dir,
            job_tx,
            status,
            last_outcome,
        ),
        None => Reply {
            ok: false,
            state: state.name().to_owned(),
            message: Some("unknown command".to_owned()),
            elapsed: None,
            stage: None,
            last: None,
            last_ok: None,
        },
    };
    let json = serde_json::to_string(&reply).context("serializing daemon reply")?;
    writeln!(stream, "{json}").context("writing daemon reply")?;
    Ok(())
}

fn status_reply(state: &State, last_outcome: &LastOutcome) -> Reply {
    let (elapsed, stage) = match state {
        State::Recording { started, .. } => (Some(started.elapsed().as_secs()), None),
        State::Processing { stage, .. } => (None, Some(stage.as_str().to_owned())),
        State::Idle => (None, None),
    };
    Reply {
        ok: true,
        state: state.name().to_owned(),
        message: None,
        elapsed,
        stage,
        last: last_outcome.message.clone(),
        last_ok: last_outcome.ok,
    }
}

fn write_client_error(stream: &mut UnixStream, state: &State, message: &str) -> Result<()> {
    let reply = Reply {
        ok: false,
        state: state.name().to_owned(),
        message: Some(message.to_owned()),
        elapsed: None,
        stage: None,
        last: None,
        last_ok: None,
    };
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
    status: &mut StatusUi,
    last_outcome: &mut LastOutcome,
) -> Reply {
    match command {
        Command::Toggle => match state {
            State::Idle => start_recording(state, config, runtime_dir, status, last_outcome),
            State::Recording { .. } => stop_recording(state, config, job_tx, status, last_outcome),
            State::Processing { .. } => busy_reply(state),
        },
        Command::Start => match state {
            State::Idle => start_recording(state, config, runtime_dir, status, last_outcome),
            _ => Reply {
                ok: false,
                state: state.name().to_owned(),
                message: Some("busy".to_owned()),
                elapsed: None,
                stage: None,
                last: None,
                last_ok: None,
            },
        },
        Command::Stop => match state {
            State::Recording { .. } => stop_recording(state, config, job_tx, status, last_outcome),
            State::Idle => Reply {
                ok: false,
                state: state.name().to_owned(),
                message: Some("not recording".to_owned()),
                elapsed: None,
                stage: None,
                last: None,
                last_ok: None,
            },
            State::Processing { .. } => busy_reply(state),
        },
        Command::Cancel => cancel_recording(state, status, last_outcome),
        Command::Status => status_reply(state, last_outcome),
        Command::Ping => Reply {
            ok: true,
            state: state.name().to_owned(),
            message: Some("pong".to_owned()),
            elapsed: None,
            stage: None,
            last: None,
            last_ok: None,
        },
        Command::Reload => match Config::load() {
            Ok(new_config) => {
                *config = new_config;
                tracing::info!("[Daemon] config reloaded");
                Reply {
                    ok: true,
                    state: state.name().to_owned(),
                    message: Some("reloaded".to_owned()),
                    elapsed: None,
                    stage: None,
                    last: None,
                    last_ok: None,
                }
            }
            Err(error) => Reply {
                ok: false,
                state: state.name().to_owned(),
                message: Some(format!("{error:#}")),
                elapsed: None,
                stage: None,
                last: None,
                last_ok: None,
            },
        },
    }
}

fn start_recording(
    state: &mut State,
    config: &Config,
    runtime_dir: &Path,
    status: &mut StatusUi,
    last_outcome: &mut LastOutcome,
) -> Reply {
    let wav = runtime_dir.join(format!("rec-{}.wav", unix_millis()));
    match Recorder::start(&wav, config.audio_source.as_deref()) {
        Ok(recorder) => {
            *state = State::Recording {
                recorder,
                started: Instant::now(),
            };
            *last_outcome = LastOutcome::default();
            tracing::info!("[Daemon] state idle -> recording");
            status.recording();
            Reply {
                ok: true,
                state: state.name().to_owned(),
                message: Some("recording".to_owned()),
                elapsed: None,
                stage: None,
                last: None,
                last_ok: None,
            }
        }
        Err(error) => {
            *last_outcome = LastOutcome::notice("Starting recording failed");
            Reply {
                ok: false,
                state: state.name().to_owned(),
                message: Some(format!("starting recording failed: {error:#}")),
                elapsed: None,
                stage: None,
                last: None,
                last_ok: None,
            }
        }
    }
}

fn stop_recording(
    state: &mut State,
    config: &Config,
    job_tx: &Sender<Job>,
    status: &mut StatusUi,
    last_outcome: &mut LastOutcome,
) -> Reply {
    let State::Recording { recorder, started } = std::mem::replace(state, State::Idle) else {
        unreachable!("stop_recording called outside recording state");
    };

    let record_secs = started.elapsed().as_secs_f64();
    let wav = match recorder.stop() {
        Ok(wav) => wav,
        Err(error) => {
            status.finish(&format!("Recording failed: {error:#}"));
            *last_outcome = LastOutcome::notice("Recording failed");
            return Reply {
                ok: false,
                state: state.name().to_owned(),
                message: Some(format!("stopping recording failed: {error:#}")),
                elapsed: None,
                stage: None,
                last: None,
                last_ok: None,
            };
        }
    };
    let job = Job {
        wav: wav.clone(),
        stt: config.stt.clone(),
        vocabulary: config.vocabulary.clone(),
        postproc: config.postproc.clone(),
    };
    if job_tx.send(job).is_err() {
        if let Err(error) = capture::remove_recording(&wav) {
            tracing::warn!("[Capture] recording cleanup failed: {error:#}");
        }
        status.finish("Transcription failed: worker unavailable");
        *last_outcome = LastOutcome::notice("Transcription failed");
        return Reply {
            ok: false,
            state: state.name().to_owned(),
            message: Some("transcription worker unavailable".to_owned()),
            elapsed: None,
            stage: None,
            last: None,
            last_ok: None,
        };
    }
    *state = State::Processing {
        started: Instant::now(),
        stage: pipeline::Stage::Transcribing,
    };
    tracing::info!("[Daemon] state recording -> processing record_secs={record_secs:.3}");
    status.processing();
    Reply {
        ok: true,
        state: state.name().to_owned(),
        message: Some("processing".to_owned()),
        elapsed: None,
        stage: None,
        last: None,
        last_ok: None,
    }
}

fn cancel_recording(
    state: &mut State,
    status: &mut StatusUi,
    last_outcome: &mut LastOutcome,
) -> Reply {
    let previous = std::mem::replace(state, State::Idle);
    match previous {
        State::Recording { recorder, .. } => match recorder.cancel() {
            Ok(()) => {
                tracing::info!("[Daemon] state recording -> idle (cancelled)");
                status.finish("Cancelled");
                *last_outcome = LastOutcome::notice("Cancelled");
                Reply {
                    ok: true,
                    state: state.name().to_owned(),
                    message: Some("cancelled".to_owned()),
                    elapsed: None,
                    stage: None,
                    last: None,
                    last_ok: None,
                }
            }
            Err(error) => {
                status.finish(&format!("Cancelling recording failed: {error:#}"));
                *last_outcome = LastOutcome::notice("Cancelling failed");
                Reply {
                    ok: false,
                    state: state.name().to_owned(),
                    message: Some(format!("cancelling recording failed: {error:#}")),
                    elapsed: None,
                    stage: None,
                    last: None,
                    last_ok: None,
                }
            }
        },
        other => {
            *state = other;
            if matches!(state, State::Processing { .. }) {
                busy_reply(state)
            } else {
                Reply {
                    ok: false,
                    state: state.name().to_owned(),
                    message: Some("nothing to cancel".to_owned()),
                    elapsed: None,
                    stage: None,
                    last: None,
                    last_ok: None,
                }
            }
        }
    }
}

fn busy_reply(state: &State) -> Reply {
    Reply {
        ok: false,
        state: state.name().to_owned(),
        message: Some("busy: processing".to_owned()),
        elapsed: None,
        stage: None,
        last: None,
        last_ok: None,
    }
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
    status: &mut StatusUi,
    last_outcome: &mut LastOutcome,
) -> Result<()> {
    loop {
        let result = match result_rx.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return Ok(()),
            Err(TryRecvError::Disconnected) => {
                status.finish("Transcription failed: worker died");
                *last_outcome = LastOutcome::notice("Transcription failed");
                anyhow::bail!("transcription worker died");
            }
        };
        handle_worker_result(state, config, result, status, last_outcome)?;
    }
}
fn handle_worker_result(
    state: &mut State,
    config: &Config,
    result: WorkerResult,
    status: &mut StatusUi,
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
    } = result;
    let postproc_failed = matches!(&postproc, PostprocStatus::Failed);
    let postproc_ms = match postproc {
        PostprocStatus::Applied { ms } => Some(ms),
        PostprocStatus::Off | PostprocStatus::Failed => None,
    };

    match transcript {
        Ok(text) if text.trim().is_empty() => {
            tracing::info!(
                "[Daemon] state processing -> idle stt_ms={} chars=0 (no speech detected)",
                stt_elapsed.as_millis()
            );
            status.finish("Heard nothing");
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
            match inject::inject(&text, config.injection) {
                Ok(outcome) => {
                    let inject_ms = inject_started.elapsed().as_millis();
                    let total_ms = processing_started.elapsed().as_millis();
                    let message = match outcome {
                        InjectionOutcome::Typed(tool) => {
                            format!("Typed {chars} chars ({tool}){cleanup_suffix}")
                        }
                        InjectionOutcome::Clipboard => {
                            format!(
                                "Copied to clipboard — press Ctrl+V ({chars} chars){cleanup_suffix}"
                            )
                        }
                    };
                    log_processing_idle(stt_elapsed, inject_ms, chars, postproc_ms);
                    tracing::info!("[Inject] injected chars={chars} total_ms={total_ms}");
                    status.finish(&message);
                    *last_outcome = LastOutcome::success(message);
                }
                Err(error) if config.injection != InjectionMode::Clipboard => {
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
                            status.finish(&format!(
                                "Typing failed — copied to clipboard ({chars} chars){cleanup_suffix}"
                            ));
                            *last_outcome = LastOutcome::success(format!(
                                "Copied to clipboard ({chars} chars){cleanup_suffix}"
                            ));
                        }
                        Err(fallback_error) => {
                            tracing::warn!(
                                "[Inject] clipboard fallback failed chars={chars} stt_ms={} error={fallback_error:#}",
                                stt_elapsed.as_millis()
                            );
                            status.finish("Typing failed; clipboard fallback failed");
                            *last_outcome = LastOutcome::notice("Typing failed");
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        "[Inject] injection failed chars={chars} stt_ms={} error={error:#}",
                        stt_elapsed.as_millis()
                    );
                    status.finish(&format!("Injection failed: {error:#}"));
                    *last_outcome = LastOutcome::notice("Injection failed");
                }
            }
        }
        Err(error) => {
            tracing::warn!(
                "[STT] transcription failed stt_ms={} error={error}",
                stt_elapsed.as_millis()
            );
            status.finish(&format!("Transcription failed: {error}"));
            *last_outcome = LastOutcome::notice("Transcription failed");
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

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
