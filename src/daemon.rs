//! The cantrip daemon and its socket-driven state machine.

use crate::capture::{self, Recorder};
use crate::config::Config;
use crate::inject::{self, InjectionMode, InjectionOutcome};
use crate::ipc::{Command, Reply};
use crate::models::{self, PARAKEET_V3_INT8};
use crate::paths;
use crate::stt::Transcriber;
use anyhow::{Context, Result};
use notify_rust::Notification;
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

struct WorkerResult {
    result: std::result::Result<String, String>,
    stt_elapsed: Duration,
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

    let model_dir = models::installed(&PARAKEET_V3_INT8)?
        .context("model not installed — run: cantrip models pull")?;
    tracing::info!("[Models] model is installed");

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
        handle: worker,
    } = spawn_worker(model_dir, warm);
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
    let loop_result = serve(&listener, &config, &runtime_dir, &job_tx, &result_rx);
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
    jobs: Sender<PathBuf>,
    results: Receiver<WorkerResult>,
    ready: Receiver<std::result::Result<(), String>>,
    handle: JoinHandle<()>,
}

fn spawn_worker(model_dir: PathBuf, warm: bool) -> WorkerChannels {
    let (job_tx, job_rx) = mpsc::channel::<PathBuf>();
    let (result_tx, result_rx) = mpsc::channel::<WorkerResult>();
    let (ready_tx, ready_rx) = mpsc::channel::<std::result::Result<(), String>>();
    let worker = thread::spawn(move || {
        let mut transcriber = if warm {
            match Transcriber::load(&model_dir) {
                Ok(transcriber) => Some(transcriber),
                Err(error) => {
                    let _ = ready_tx.send(Err(format!("loading transcription model: {error:#}")));
                    return;
                }
            }
        } else {
            None
        };
        let _ = ready_tx.send(Ok(()));

        while let Ok(wav) = job_rx.recv() {
            let wav = RecordingCleanup(wav);
            let started = Instant::now();
            let transcription = transcribe_one(&mut transcriber, &model_dir, &wav.0)
                .map_err(|error| format!("{error:#}"));
            let chars = transcription
                .as_ref()
                .map_or(0, |text| text.chars().count());
            if result_tx
                .send(WorkerResult {
                    result: transcription,
                    stt_elapsed: started.elapsed(),
                })
                .is_err()
            {
                tracing::warn!("[Daemon] transcription result dropped chars={chars}");
            }
        }
    });
    WorkerChannels {
        jobs: job_tx,
        results: result_rx,
        ready: ready_rx,
        handle: worker,
    }
}

fn transcribe_one(
    transcriber: &mut Option<Transcriber>,
    model_dir: &Path,
    wav: &Path,
) -> Result<String> {
    if transcriber.is_none() {
        *transcriber = Some(Transcriber::load(model_dir)?);
    }
    transcriber
        .as_mut()
        .context("transcription worker has no model")?
        .transcribe_wav(wav)
}

fn serve(
    listener: &UnixListener,
    config: &Config,
    runtime_dir: &Path,
    job_tx: &Sender<PathBuf>,
    result_rx: &Receiver<WorkerResult>,
) -> Result<()> {
    let mut state = State::Idle;
    loop {
        drain_worker_results(&mut state, config, result_rx)?;
        if SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }

        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    if let Err(error) =
                        handle_connection(stream, &mut state, config, runtime_dir, job_tx)
                    {
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
            Ok(result) => handle_worker_result(&mut state, config, result)?,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                tracing::warn!("[STT] transcription result timed out during shutdown");
                notify("Transcription failed: worker timed out");
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
    config: &Config,
    runtime_dir: &Path,
    job_tx: &Sender<PathBuf>,
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
        Some(command) => execute(command, state, config, runtime_dir, job_tx),
        None => Reply {
            ok: false,
            state: state.name().to_owned(),
            message: Some("unknown command".to_owned()),
        },
    };
    let json = serde_json::to_string(&reply).context("serializing daemon reply")?;
    writeln!(stream, "{json}").context("writing daemon reply")?;
    Ok(())
}

fn write_client_error(stream: &mut UnixStream, state: &State, message: &str) -> Result<()> {
    let reply = Reply {
        ok: false,
        state: state.name().to_owned(),
        message: Some(message.to_owned()),
    };
    let json = serde_json::to_string(&reply).context("serializing client error reply")?;
    writeln!(stream, "{json}").context("writing client error reply")?;
    Ok(())
}

fn execute(
    command: Command,
    state: &mut State,
    config: &Config,
    runtime_dir: &Path,
    job_tx: &Sender<PathBuf>,
) -> Reply {
    match command {
        Command::Toggle => match state {
            State::Idle => start_recording(state, config, runtime_dir),
            State::Recording { .. } => stop_recording(state, job_tx),
            State::Processing { .. } => busy_reply(state),
        },
        Command::Start => match state {
            State::Idle => start_recording(state, config, runtime_dir),
            _ => Reply {
                ok: false,
                state: state.name().to_owned(),
                message: Some("busy".to_owned()),
            },
        },
        Command::Stop => match state {
            State::Recording { .. } => stop_recording(state, job_tx),
            State::Idle => Reply {
                ok: false,
                state: state.name().to_owned(),
                message: Some("not recording".to_owned()),
            },
            State::Processing { .. } => busy_reply(state),
        },
        Command::Cancel => cancel_recording(state),
        Command::Status => Reply {
            ok: true,
            state: state.name().to_owned(),
            message: None,
        },
        Command::Ping => Reply {
            ok: true,
            state: state.name().to_owned(),
            message: Some("pong".to_owned()),
        },
    }
}

fn start_recording(state: &mut State, config: &Config, runtime_dir: &Path) -> Reply {
    let wav = runtime_dir.join(format!("rec-{}.wav", unix_millis()));
    match Recorder::start(&wav, config.audio_source.as_deref()) {
        Ok(recorder) => {
            *state = State::Recording {
                recorder,
                started: Instant::now(),
            };
            tracing::info!("[Daemon] state idle -> recording");
            notify("Listening…");
            Reply {
                ok: true,
                state: state.name().to_owned(),
                message: Some("recording".to_owned()),
            }
        }
        Err(error) => Reply {
            ok: false,
            state: state.name().to_owned(),
            message: Some(format!("starting recording failed: {error:#}")),
        },
    }
}

fn stop_recording(state: &mut State, job_tx: &Sender<PathBuf>) -> Reply {
    let State::Recording { recorder, started } = std::mem::replace(state, State::Idle) else {
        unreachable!("stop_recording called outside recording state");
    };

    let record_secs = started.elapsed().as_secs_f64();
    let wav = match recorder.stop() {
        Ok(wav) => wav,
        Err(error) => {
            notify(&format!("Recording failed: {error:#}"));
            return Reply {
                ok: false,
                state: state.name().to_owned(),
                message: Some(format!("stopping recording failed: {error:#}")),
            };
        }
    };
    if job_tx.send(wav.clone()).is_err() {
        if let Err(error) = capture::remove_recording(&wav) {
            tracing::warn!("[Capture] recording cleanup failed: {error:#}");
        }
        notify("Transcription failed: worker unavailable");
        return Reply {
            ok: false,
            state: state.name().to_owned(),
            message: Some("transcription worker unavailable".to_owned()),
        };
    }
    *state = State::Processing {
        started: Instant::now(),
    };
    tracing::info!("[Daemon] state recording -> processing record_secs={record_secs:.3}");
    notify("Transcribing…");
    Reply {
        ok: true,
        state: state.name().to_owned(),
        message: Some("processing".to_owned()),
    }
}

fn cancel_recording(state: &mut State) -> Reply {
    let previous = std::mem::replace(state, State::Idle);
    match previous {
        State::Recording { recorder, .. } => match recorder.cancel() {
            Ok(()) => {
                tracing::info!("[Daemon] state recording -> idle (cancelled)");
                notify("Cancelled");
                Reply {
                    ok: true,
                    state: state.name().to_owned(),
                    message: Some("cancelled".to_owned()),
                }
            }
            Err(error) => Reply {
                ok: false,
                state: state.name().to_owned(),
                message: Some(format!("cancelling recording failed: {error:#}")),
            },
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
    }
}

fn drain_worker_results(
    state: &mut State,
    config: &Config,
    result_rx: &Receiver<WorkerResult>,
) -> Result<()> {
    loop {
        let result = match result_rx.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return Ok(()),
            Err(TryRecvError::Disconnected) => {
                anyhow::bail!("transcription worker died");
            }
        };
        handle_worker_result(state, config, result)?;
    }
}

fn handle_worker_result(state: &mut State, config: &Config, result: WorkerResult) -> Result<()> {
    let processing_started = match state {
        State::Processing { started } => *started,
        _ => {
            tracing::warn!("[Daemon] ignored transcription result outside processing state");
            return Ok(());
        }
    };

    match result.result {
        Ok(text) if text.trim().is_empty() => {
            tracing::info!(
                "[Daemon] state processing -> idle stt_ms={} chars=0 (no speech detected)",
                result.stt_elapsed.as_millis()
            );
            notify("Heard nothing");
        }
        Ok(text) => {
            let chars = text.chars().count();
            let inject_started = Instant::now();
            match inject::inject(&text, config.injection) {
                Ok(outcome) => {
                    let inject_ms = inject_started.elapsed().as_millis();
                    let total_ms = processing_started.elapsed().as_millis();
                    let message = match outcome {
                        InjectionOutcome::Typed(tool) => format!("Typed {chars} chars ({tool})"),
                        InjectionOutcome::Clipboard => {
                            format!("Copied to clipboard — press Ctrl+V ({chars} chars)")
                        }
                    };
                    tracing::info!(
                        "[Daemon] state processing -> idle stt_ms={} inject_ms={} chars={chars}",
                        result.stt_elapsed.as_millis(),
                        inject_ms
                    );
                    tracing::info!("[Inject] injected chars={chars} total_ms={total_ms}");
                    notify(&message);
                }
                Err(error) if config.injection != InjectionMode::Clipboard => {
                    tracing::warn!(
                        "[Inject] injection failed chars={chars} stt_ms={} error={error:#}",
                        result.stt_elapsed.as_millis()
                    );
                    match inject::inject(&text, InjectionMode::Clipboard) {
                        Ok(_) => {
                            let inject_ms = inject_started.elapsed().as_millis();
                            let total_ms = processing_started.elapsed().as_millis();
                            tracing::info!(
                                "[Daemon] state processing -> idle stt_ms={} inject_ms={} chars={chars}",
                                result.stt_elapsed.as_millis(),
                                inject_ms
                            );
                            tracing::info!(
                                "[Inject] clipboard fallback chars={chars} total_ms={total_ms}"
                            );
                            notify(&format!(
                                "Typing failed — copied to clipboard ({chars} chars)"
                            ));
                        }
                        Err(fallback_error) => {
                            tracing::warn!(
                                "[Inject] clipboard fallback failed chars={chars} stt_ms={} error={fallback_error:#}",
                                result.stt_elapsed.as_millis()
                            );
                            notify("Typing failed; clipboard fallback failed");
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        "[Inject] injection failed chars={chars} stt_ms={} error={error:#}",
                        result.stt_elapsed.as_millis()
                    );
                    notify(&format!("Injection failed: {error:#}"));
                }
            }
        }
        Err(error) => {
            tracing::warn!(
                "[STT] transcription failed stt_ms={} error={error}",
                result.stt_elapsed.as_millis()
            );
            notify(&format!("Transcription failed: {error}"));
        }
    }
    *state = State::Idle;
    Ok(())
}

fn shutdown_state(state: &mut State) {
    let previous = std::mem::replace(state, State::Idle);
    if let State::Recording { recorder, .. } = previous {
        if let Err(error) = recorder.cancel() {
            tracing::warn!("[Capture] shutdown cancellation failed: {error:#}");
        }
    }
}

fn notify(body: &str) {
    if let Err(error) = Notification::new().summary("Cantrip").body(body).show() {
        tracing::debug!("[Daemon] notification unavailable: {}", error);
    }
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
