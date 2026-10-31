use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::{Arc, Mutex};

use cantrip::inject;
use cantrip::{actions, daemon, hud, settings};
use cantrip_engine::delivery::InjectionMode;
use cantrip_engine::ipc::{self, Command};
use cantrip_engine::models::{self, PARAKEET_V3_INT8};
use cantrip_engine::{config::Config, keys, paths, pipeline, recovery, telemetry};

/// Per-dictation post-processing request, overriding [postproc].enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum PostprocMode {
    /// Run transcript cleanup for this dictation.
    Clean,
    /// Skip transcript cleanup for this dictation.
    Raw,
}

#[derive(Debug, Parser)]
#[command(name = "cantrip", version, about = "Local-first dictation", arg_required_else_help = !cfg!(target_os = "macos"))]
struct Cli {
    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// Run the native macOS menu-bar application.
    #[cfg(target_os = "macos")]
    App,
    /// Run the dictation daemon.
    Daemon {
        #[arg(long)]
        preload: bool,
    },
    /// Show the passive status HUD.
    Hud {
        /// Render one frame to a PNG at PATH, then exit (visual testing).
        #[arg(long, value_name = "PATH")]
        screenshot: Option<PathBuf>,
        /// Scenario to render with --screenshot (default: recording).
        /// Available scenarios cover live signal, progress, and recovery outcomes.
        #[arg(long, value_enum, requires = "screenshot")]
        state: Option<hud::ScreenshotState>,
        /// Compose the --screenshot scenario for a take headed to a handoff target
        /// with this label, tinted in the live theme's first target color.
        #[arg(long, value_name = "LABEL", requires = "screenshot")]
        handoff: Option<String>,
    },
    /// Review production HUD states and transitions without running dictation.
    #[command(hide = true)]
    HudGallery {
        /// Capture the actual gallery window to PNG, then exit.
        #[arg(long, value_name = "PATH")]
        screenshot: Option<PathBuf>,
    },
    /// Open the configuration window.
    Settings {
        /// Render one frame to a PNG at PATH, then exit (visual testing).
        #[arg(long, value_name = "PATH")]
        screenshot: Option<PathBuf>,
    },
    /// Open recording recovery, explicit clipboard actions, and setup.
    Actions {
        /// Open the read-only setup diagnosis immediately.
        #[arg(long)]
        doctor: bool,
        /// Render the actual window to a PNG at PATH, then exit.
        #[arg(long, value_name = "PATH")]
        screenshot: Option<PathBuf>,
    },
    Toggle {
        /// Post-processing for this dictation: clean | raw (default: [postproc].enabled).
        #[arg(long, value_enum)]
        postproc: Option<PostprocMode>,
        /// Deliver this take to a named local command instead of the desktop.
        #[arg(long, value_name = "NAME")]
        handoff: Option<String>,
    },
    Start {
        /// Post-processing for this dictation: clean | raw (default: [postproc].enabled).
        #[arg(long, value_enum)]
        postproc: Option<PostprocMode>,
        /// Deliver this take to a named local command instead of the desktop.
        #[arg(long, value_name = "NAME")]
        handoff: Option<String>,
    },
    Stop,
    /// Stop capture or processing without deleting the recording.
    Cancel,
    Status {
        /// Emit the typed, transcript-free status snapshot.
        #[arg(long)]
        json: bool,
    },
    /// List saved recording metadata, never transcript content.
    Recordings {
        #[arg(long)]
        json: bool,
    },
    /// Copy the saved transcript for this exact recording to the clipboard.
    Copy {
        id: String,
    },
    /// Hide an outcome or interaction notice without deleting a recording.
    Dismiss {
        #[arg(long)]
        event_id: Option<u64>,
    },
    /// Forget retained audio and incomplete text; keep complete archived text.
    Forget {
        id: String,
        /// Confirm this irreversible deletion.
        #[arg(long)]
        yes: bool,
    },
    /// Re-inject the last saved transcript.
    Last,
    /// Re-run STT on a retained recording.
    Recover {
        /// Recover this exact take; omitted selects the newest unresolved audio.
        #[arg(long)]
        id: Option<String>,
        /// Use installed local Parakeet without cloud STT or cleanup.
        #[arg(long)]
        local: bool,
        /// Copy the result without sending keys to the focused application.
        #[arg(long)]
        clipboard: bool,
    },
    Ping,
    /// Re-read the config file in the running daemon.
    Reload,
    /// Microphone access for the running Cantrip, the process that records (macOS).
    Microphone {
        #[command(subcommand)]
        command: MicrophoneCommand,
    },
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    Key {
        #[command(subcommand)]
        command: KeyCommand,
    },
    /// Transcribe one WAV file and print the transcript.
    Transcribe {
        wav: PathBuf,
        /// Use installed local Parakeet without cloud STT or cleanup.
        #[arg(long)]
        local: bool,
    },
    /// Manage the local transcription model.
    Models {
        #[command(subcommand)]
        command: ModelsCommand,
    },
    /// Diagnose effective configuration and runtime prerequisites.
    Doctor,
}

#[derive(Debug, Subcommand)]
enum ModelsCommand {
    Pull,
    Status,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    Path,
    Show,
    Init,
    Edit,
}

#[derive(Debug, Subcommand)]
enum KeyCommand {
    Set { id: String },
    Rm { id: String },
    Status { id: String },
}

#[derive(Debug, Subcommand)]
enum MicrophoneCommand {
    /// Ask the running Cantrip to request macOS microphone access; you answer the prompt.
    /// For a `cantrip daemon` started from a terminal, macOS asks for and grants that
    /// terminal app, and everything it runs, instead of Cantrip.
    Allow,
}

fn main() {
    let cli = Cli::parse();
    let dual_sink = matches!(cli.command, Some(CliCommand::Daemon { .. }));
    #[cfg(target_os = "macos")]
    let dual_sink = dual_sink || matches!(cli.command, None | Some(CliCommand::App));
    init_tracing(dual_sink);
    if let Err(error) = run(cli) {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn init_tracing(dual_sink: bool) {
    // Always pin ort/transcribe_rs to warn so dependency info logs cannot
    // carry transcript text even when RUST_LOG is broad.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
        .add_directive("ort=warn".parse().expect("static directive"))
        .add_directive("transcribe_rs=warn".parse().expect("static directive"));
    // Daemon sessions tee into the state log so a long dictation failure
    // is still visible after a detached hub session or a reboot.
    if dual_sink {
        if let Ok(file) = open_runtime_log() {
            let tee = TeeWriter::new(file);
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(tee)
                .try_init();
            return;
        }
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(io::stderr)
        .try_init();
}

fn open_runtime_log() -> Result<fs::File> {
    let _ = paths::ensure_dir(paths::state_dir()?).context("creating state directory")?;
    let path = paths::daemon_log_path()?;
    let mut options = fs::OpenOptions::new();
    options.create(true).append(true).mode(0o600);
    #[cfg(target_os = "macos")]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    let file = options
        .open(&path)
        .with_context(|| format!("opening state log {}", path.display()))?;
    #[cfg(target_os = "macos")]
    paths::privatize_file(&file).context("privatizing state log")?;
    // Preserve the Linux log-opening contract; native ACL normalization is
    // descriptor-based and rejects links/non-regular files before mutation.
    #[cfg(target_os = "linux")]
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("setting permissions on {}", path.display()))?;
    Ok(file)
}

/// Write each tracing line to stderr and the runtime log file.
#[derive(Clone)]
struct TeeWriter {
    file: Arc<Mutex<fs::File>>,
}

impl TeeWriter {
    fn new(file: fs::File) -> Self {
        Self {
            file: Arc::new(Mutex::new(file)),
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for TeeWriter {
    type Writer = TeeHandle;

    fn make_writer(&'a self) -> Self::Writer {
        TeeHandle {
            file: Arc::clone(&self.file),
        }
    }
}

struct TeeHandle {
    file: Arc<Mutex<fs::File>>,
}

impl Write for TeeHandle {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Best-effort file write: a full disk must not silence stderr.
        if let Ok(mut file) = self.file.lock() {
            let _ = file.write_all(buf);
            let _ = file.flush();
        }
        io::stderr().write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Ok(mut file) = self.file.lock() {
            let _ = file.flush();
        }
        io::stderr().flush()
    }
}

fn run(cli: Cli) -> Result<()> {
    let command = match cli.command {
        Some(command) => command,
        None => {
            #[cfg(target_os = "macos")]
            return cantrip::macos::run();
            #[cfg(not(target_os = "macos"))]
            anyhow::bail!("a Cantrip command is required");
        }
    };
    match command {
        #[cfg(target_os = "macos")]
        CliCommand::App => cantrip::macos::run(),
        CliCommand::Daemon { preload } => {
            let config = Config::load().context("loading configuration")?;
            daemon::run(config, preload)
        }
        CliCommand::Hud {
            screenshot,
            state,
            handoff,
        } => hud::run(screenshot, state, handoff),
        CliCommand::HudGallery { screenshot } => hud::gallery::run(screenshot),
        CliCommand::Settings { screenshot } => settings::run(screenshot),
        CliCommand::Actions { screenshot, doctor } => actions::run(screenshot, doctor),
        CliCommand::Toggle { postproc, handoff } => send_command(Command::Toggle {
            postproc: postproc.map(|mode| mode == PostprocMode::Clean),
            handoff,
        }),
        CliCommand::Start { postproc, handoff } => send_command(Command::Start {
            postproc: postproc.map(|mode| mode == PostprocMode::Clean),
            handoff,
        }),
        CliCommand::Stop => send_command(Command::Stop),
        CliCommand::Cancel => send_command(Command::Cancel),
        CliCommand::Status { json } => print_status(json),
        CliCommand::Recordings { json } => print_recordings(json),
        CliCommand::Copy { id } => send_command(Command::Copy { id }),
        CliCommand::Dismiss { event_id } => send_command(Command::Dismiss { event_id }),
        CliCommand::Forget { id, yes } => {
            anyhow::ensure!(
                yes,
                "forget deletes retained audio; confirm with --yes, or use cantrip actions"
            );
            send_command(Command::Forget { id })
        }
        CliCommand::Last => send_command(Command::Last),
        CliCommand::Recover {
            id,
            local,
            clipboard,
        } => send_command(Command::Recover {
            id,
            local,
            clipboard,
        }),
        CliCommand::Reload => send_command(Command::Reload),
        CliCommand::Microphone { command } => match command {
            MicrophoneCommand::Allow => send_command(Command::AllowMicrophone),
        },
        CliCommand::Config { command } => run_config(command),
        CliCommand::Key { command } => run_key(command),
        CliCommand::Ping => send_command(Command::Ping),
        CliCommand::Transcribe { wav, local } => transcribe_file(&wav, local),
        CliCommand::Models { command } => match command {
            ModelsCommand::Pull => pull_model(),
            ModelsCommand::Status => model_status(),
        },
        CliCommand::Doctor => doctor(),
    }
}

#[cfg(target_os = "linux")]
const CONFIG_HEADER: &str = "injection = \"auto\"        # auto | paste | type | clipboard\n";
#[cfg(target_os = "macos")]
const CONFIG_HEADER: &str = "injection = \"clipboard\"   # copy locally, then paste manually\n# hotkey = \"Control+Alt+Space\"\n";

const CONFIG_TEMPLATE: &str = r#"keep_warm = true
# audio_source = "…"      # optional native device ID (PipeWire target on Linux)
vocabulary = []           # exact-spelling terms for postproc + cloud STT

[hud]
labels = false            # always show state captions
# reduced_motion = true  # omit to follow the desktop preference

[stt]
model = "parakeet-tdt-0.6b-v3-int8"   # local registry name
# Cloud STT: set endpoint to any OpenAI-compatible base URL and the
# engine switches to POST {endpoint}/audio/transcriptions (multipart).
# endpoint = "https://api.groq.com/openai/v1"
# model = "whisper-large-v3-turbo"
# api_key_id = "groq"

[postproc]
enabled = false
endpoint = "http://localhost:11434/v1"
model = ""                # required when enabled
# api_key_id = "openai"   # keyring entry; omit for local endpoints
timeout_ms = 30000
passes = 1                # cleanup rounds; 2 adds a proofread pass
min_chars = 40            # skip cleanup under this length; 0 = never skip
instructions = ""         # optional extra style guidance
# Per-dictation override (two hotkeys): `cantrip toggle --postproc clean|raw`
# forces cleanup on/off for that capture, regardless of `enabled` above.
"#;

fn run_config(command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Path => {
            println!("{}", config_path()?.display());
            Ok(())
        }
        ConfigCommand::Show => {
            let config = Config::load().context("loading configuration")?;
            let rendered = toml::to_string_pretty(&config).context("serializing configuration")?;
            println!("{rendered}");
            Ok(())
        }
        ConfigCommand::Init => {
            let path = config_path()?;
            write_config_template(&path, true)?;
            println!("{}", path.display());
            Ok(())
        }
        ConfigCommand::Edit => edit_config(),
    }
}

fn config_path() -> Result<PathBuf> {
    paths::config_file().context("locating config file")
}

fn write_config_template(path: &Path, refuse_existing: bool) -> Result<()> {
    if refuse_existing && path.exists() {
        anyhow::bail!("configuration file already exists: {}", path.display());
    }
    let parent = path
        .parent()
        .context("configuration path has no parent directory")?;
    paths::ensure_dir(parent.to_path_buf())
        .with_context(|| format!("creating configuration directory {}", parent.display()))?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .create_new(refuse_existing)
        .truncate(!refuse_existing)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("opening configuration template {}", path.display()))?;
    file.write_all(CONFIG_HEADER.as_bytes())
        .and_then(|()| file.write_all(CONFIG_TEMPLATE.as_bytes()))
        .with_context(|| format!("writing configuration template {}", path.display()))?;
    Ok(())
}

fn edit_config() -> Result<()> {
    let path = config_path()?;
    if !path.exists() {
        write_config_template(&path, false)?;
    }

    let editor = env::var_os("EDITOR")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "vi".into());
    let status = ProcessCommand::new(editor)
        .arg(&path)
        .status()
        .with_context(|| format!("starting editor for {}", path.display()))?;
    if !status.success() {
        anyhow::bail!("editor exited unsuccessfully; configuration was not validated");
    }

    Config::load().map_err(|error| {
        anyhow!(
            "configuration is invalid after editing: {error:#}; please re-edit {}",
            path.display()
        )
    })?;
    println!("validated {}", path.display());
    Ok(())
}

fn run_key(command: KeyCommand) -> Result<()> {
    match command {
        KeyCommand::Set { id } => {
            let secret = read_secret(&id)?;
            if secret.is_empty() {
                anyhow::bail!("secret cannot be empty");
            }
            keys::set(&id, &secret)?;
            println!("stored key '{id}'");
            Ok(())
        }
        KeyCommand::Rm { id } => {
            keys::delete(&id)?;
            println!("removed key '{id}'");
            Ok(())
        }
        KeyCommand::Status { id } => {
            if keys::exists(&id)? {
                println!("present");
            } else {
                println!("absent");
            }
            Ok(())
        }
    }
}

/// Read a secret without leaving it in terminal scrollback: piped stdin is
/// read as-is; a TTY gets a prompt with echo disabled.
fn read_secret(id: &str) -> Result<String> {
    if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
        let mut input = String::new();
        std::io::stdin()
            .read_to_string(&mut input)
            .context("reading secret from stdin")?;
        return Ok(input.trim_end_matches(['\r', '\n']).to_owned());
    }

    use std::io::{BufRead, Write};
    eprint!("enter key '{id}' (input hidden): ");
    std::io::stderr().flush().context("flushing prompt")?;

    let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, termios.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("reading terminal attributes");
    }
    let mut no_echo = unsafe { termios.assume_init() };
    let original = no_echo;
    no_echo.c_lflag &= !libc::ECHO;
    if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &no_echo) } != 0 {
        return Err(std::io::Error::last_os_error()).context("disabling terminal echo");
    }
    let mut line = String::new();
    let read_result = std::io::stdin().lock().read_line(&mut line);
    let restored = unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &original) };
    eprintln!();
    read_result.context("reading secret from terminal")?;
    if restored != 0 {
        return Err(std::io::Error::last_os_error()).context("restoring terminal echo");
    }
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

fn send_command(command: Command) -> Result<()> {
    if matches!(
        &command,
        Command::Last | Command::Recover { .. } | Command::Copy { .. } | Command::Forget { .. }
    ) {
        ipc::recordings().context("refreshing saved recording metadata")?;
    }
    let reply = ipc::command(command)?;
    println!("state: {}", reply.state.as_str());
    if let Some(message) = &reply.message {
        println!("message: {message}");
    }
    if let Some(stage) = reply.stage {
        println!("stage: {stage}");
    }
    print_outcome(reply.outcome.as_ref());
    if !reply.ok {
        let error = reply
            .error
            .as_deref()
            .unwrap_or("daemon rejected the command");
        anyhow::bail!("{error}");
    }
    Ok(())
}

fn print_status(json: bool) -> Result<()> {
    let status = ipc::status()?;
    if json {
        println!(
            "{}",
            serde_json::to_string(&status).context("serializing status")?
        );
        return Ok(());
    }
    println!("state: {}", status.state_name());
    if status.state != ipc::StateKind::Idle {
        println!("elapsed: {}s", status.elapsed);
    }
    if let Some(handoff) = &status.handoff {
        println!("handoff: {} ({})", handoff.label, handoff.name);
    }
    if let Some(stage) = &status.stage {
        println!("stage: {stage}");
    }
    if let Some(signal) = &status.signal {
        println!("audio-level: {}%", signal.level);
        println!("audio-silent: {}", signal.silent);
    }
    if let Some(notice) = &status.notice {
        println!("notice: {} (event {})", notice.message, notice.event_id);
    }
    print_outcome(status.outcome.as_ref());
    if let Some(access) = &status.microphone {
        println!("microphone: {}", enum_label(&access.authorization));
        if let Some(disclosure) = access.grantee.disclosure() {
            println!("microphone-grantee: {disclosure}");
        }
    }
    let pending = status.pending_recordings;
    println!("pending-recordings: {pending}");
    if pending > 0 {
        println!("hint: cantrip actions   # choose the matching saved recording");
    }
    Ok(())
}

fn print_outcome(outcome: Option<&ipc::TerminalOutcome>) {
    let Some(outcome) = outcome else {
        return;
    };
    println!("last: {}", outcome.message);
    if let Some(handoff) = &outcome.handoff {
        println!("last-handoff: {} ({})", handoff.label, handoff.name);
    }
    println!("event: {}", outcome.event_id);
    println!("completeness: {}", enum_label(&outcome.completeness));
    println!("delivery: {}", enum_label(&outcome.delivery));
    println!("cleanup: {}", enum_label(&outcome.cleanup));
    println!("dismissed: {}", outcome.dismissed);
    if let Some(error) = &outcome.error {
        println!("error: {error}");
    }
    if let Some(id) = &outcome.artifacts.take_id {
        println!("recording: {id}");
        println!("hint: cantrip recordings   # inspect saved recording metadata");
        if outcome.delivery == ipc::Delivery::Uncertain {
            println!("hint: check the destination or clipboard before trying again");
        } else if matches!(
            outcome.delivery,
            ipc::Delivery::None
                | ipc::Delivery::Deferred
                | ipc::Delivery::Failed
                | ipc::Delivery::Cancelled
        ) || (outcome.completeness == ipc::Completeness::Partial
            && outcome.delivery == ipc::Delivery::Copied)
        {
            if outcome.artifacts.text {
                println!("hint: cantrip copy {id}   # copy this recording's saved text");
            }
            if outcome.artifacts.audio {
                println!("hint: cantrip recover --id {id} --local --clipboard");
            }
        }
    }
}

fn print_recordings(json: bool) -> Result<()> {
    // The daemon owns mutations and its cached list. Read-only history remains
    // discoverable while it is offline; never infer an offline daemon state.
    let takes = match ipc::recordings() {
        Ok(takes) => takes,
        Err(_) => recovery::list().context("reading saved recording metadata")?,
    };
    if json {
        println!(
            "{}",
            serde_json::to_string(&takes).context("serializing recordings")?
        );
    } else if takes.is_empty() {
        println!("No saved recordings.");
    } else {
        for take in takes {
            println!(
                "{}  {}  {}  {}  text={} audio={} pending={}",
                take.id,
                actions::take_time(take.created_at_unix_ms),
                actions::take_duration(take.duration_ms),
                if take.partial {
                    "partial"
                } else if take.text_available {
                    "complete text"
                } else {
                    "no text"
                },
                take.text_available,
                take.audio_available,
                take.unresolved,
            );
        }
        println!("Open cantrip actions to recover, copy, or forget a specific recording.");
    }
    Ok(())
}

fn enum_label(value: &impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

fn transcribe_file(wav: &Path, local: bool) -> Result<()> {
    let mut config = Config::load().context("loading configuration")?;
    if local {
        config.stt = Default::default();
        config.postproc.enabled = false;
    }
    if config.stt.endpoint.is_none() {
        let spec = models::require(&config.stt.model)?;
        if local {
            let expected = paths::models_dir()?.join(spec.dir_name);
            anyhow::ensure!(
                models::installed(spec)?.is_some(),
                "local model not installed at {} — run: cantrip models pull",
                expected.display()
            );
        } else {
            models::ensure_model(spec, None).context("ensuring transcription model")?;
        }
    }
    let mut cache = None;
    let outcome = pipeline::run(
        &mut cache,
        wav,
        &config.stt,
        &config.vocabulary,
        &config.postproc,
        pipeline::RunContext {
            source: pipeline::Source::Transcribe,
            take_id: None,
            cancel: None,
        },
        |_| {},
    );
    if let pipeline::ArchiveStatus::Failed(error) = &outcome.archive {
        eprintln!("warning: transcript archive failed: {error}");
    }
    emit_transcribe_telemetry(&config, &outcome);
    if let Some(notice) = outcome.cloud_notice() {
        eprintln!("{notice}");
        if outcome.partial || outcome.text.is_err() {
            eprintln!("Original WAV is unchanged. Use cantrip transcribe --local <WAV> with an installed local model, or check cloud setup.");
        } else {
            eprintln!("Original WAV is unchanged. No retry needed.");
        }
        if let Some(cause) = outcome.cloud_failure {
            if outcome.partial || outcome.text.is_err() || cause.is_payment_or_budget() {
                eprintln!("{}", cause.advice());
            }
        }
    }
    match &outcome.text {
        Ok(text) => {
            if matches!(outcome.postproc, pipeline::PostprocStatus::Failed { .. }) {
                eprintln!("post-processing failed; showing the raw transcript");
            }
            println!("{text}");
            if outcome.partial {
                anyhow::bail!(
                    "partial transcript only; later audio failed, original WAV is unchanged"
                );
            }
        }
        Err(error) => {
            if outcome.cloud_failure.is_some() {
                anyhow::bail!("no complete transcript produced; original WAV is unchanged");
            }
            anyhow::bail!("transcribing {}: {error}", wav.display());
        }
    }
    Ok(())
}

fn pull_model() -> Result<()> {
    println!("pulling model...");
    let model_dir =
        models::ensure_model(&PARAKEET_V3_INT8, None).context("pulling transcription model")?;
    println!("installed: {}", model_dir.display());
    Ok(())
}

/// Export the one-shot CLI job. Blocking here is fine: `transcribe` is a
/// foreground command and the export is bounded and content-free.
fn emit_transcribe_telemetry(config: &Config, outcome: &pipeline::Outcome) {
    if !config.telemetry.enabled {
        return;
    }
    let chars = outcome.text.as_ref().map_or(0, |text| text.chars().count());
    let error_class = if outcome.text.is_err() {
        Some("stt-failed".to_owned())
    } else if outcome.partial {
        Some("stt-partial".to_owned())
    } else {
        None
    };
    let (cleanup_state, cleanup_ms) = match &outcome.postproc {
        pipeline::PostprocStatus::Applied { ms } => ("applied", Some(*ms as u64)),
        pipeline::PostprocStatus::Failed { ms } => ("failed", Some(*ms as u64)),
        pipeline::PostprocStatus::SkippedShort { .. } => ("skipped_short", None),
        pipeline::PostprocStatus::Off => ("off", None),
    };
    let attempted = matches!(
        outcome.postproc,
        pipeline::PostprocStatus::Applied { .. } | pipeline::PostprocStatus::Failed { .. }
    );
    let stt_ms = outcome.stt_elapsed.as_millis() as u64;
    let job = telemetry::JobTelemetry {
        source: "transcribe",
        capture_ms: 0,
        stt_ms,
        stt_model: if outcome.local_fallback && outcome.stt_backend == pipeline::SttBackend::Local {
            PARAKEET_V3_INT8.dir_name.to_owned()
        } else {
            config.stt.model.clone()
        },
        stt_remote: outcome.stt_backend != pipeline::SttBackend::Local,
        chars,
        partial: outcome.partial,
        cleanup_state,
        cleanup_ms,
        cleanup_model: attempted.then(|| config.postproc.model.clone()),
        tokens_in: outcome
            .postproc_usage
            .as_ref()
            .map(|usage| usage.prompt_tokens),
        tokens_out: outcome
            .postproc_usage
            .as_ref()
            .map(|usage| usage.completion_tokens),
        tokens_total: outcome
            .postproc_usage
            .as_ref()
            .map(|usage| usage.total_tokens),
        inject_ms: None,
        delivered: None,
        error_class,
        total_ms: stt_ms + cleanup_ms.unwrap_or(0),
    };
    let reporter = telemetry::TelemetryReporter::spawn();
    reporter.report(&config.telemetry, job);
    // Dropping via shutdown drains the queue before the process exits:
    // the CLI equivalent of a flush.
    reporter.shutdown();
}

fn telemetry_diagnosis(config: &Config) -> String {
    if !config.telemetry.enabled {
        return "telemetry: disabled (set [telemetry] enabled = true to opt in)".to_owned();
    }
    if config.telemetry.public_key.trim().is_empty() {
        return "telemetry: blocked (public_key is empty)".to_owned();
    }
    let key_id = config.telemetry.api_key_id.as_deref().unwrap_or("langfuse");
    let credential = if keys::exists(key_id).unwrap_or(false) {
        "present"
    } else {
        "absent — run: cantrip key set langfuse"
    };
    format!(
        "telemetry: enabled (endpoint={}; credential={}; content=counts-only)",
        endpoint_origin(&config.telemetry.endpoint),
        credential
    )
}

fn model_status() -> Result<()> {
    match models::installed(&PARAKEET_V3_INT8).context("checking transcription model")? {
        Some(path) => println!("installed: {}", path.display()),
        None => println!("not installed"),
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy)]
struct DoctorTools {
    pw_record: bool,
    virtual_keyboard: bool,
    wl_copy: bool,
}

#[cfg(target_os = "linux")]
fn doctor() -> Result<()> {
    let tools = DoctorTools {
        pw_record: inject::executable_in_path("pw-record"),
        virtual_keyboard: inject::virtual_keyboard_available(),
        wl_copy: inject::executable_in_path("wl-copy"),
    };
    let config_path = paths::config_file().context("locating config file")?;
    let config_exists = config_path.exists();
    let config = Config::load();

    match &config {
        Ok(_) if config_exists => println!("config: ready"),
        Ok(_) => println!("config: defaults in use — open Settings to customize"),
        Err(_) => {
            println!("config: blocked — invalid or unreadable; open Settings to repair")
        }
    }
    println!("{}", capture_diagnosis(config.as_ref().ok(), tools));

    if let Ok(config) = &config {
        println!("{}", stt_diagnosis(config));
        println!("{}", cleanup_diagnosis(config));
        println!("{}", telemetry_diagnosis(config));
        println!("{}", injection_diagnosis(config.injection, tools));
        for (name, target) in &config.handoff {
            println!("{}", handoff_diagnosis(name, target));
        }
    } else {
        println!("stt: blocked — fix config first");
        println!("cleanup: blocked — fix config first");
        println!("telemetry: blocked — fix config first");
        println!("injection: blocked — fix config first");
    }
    let wayland = env::var_os("WAYLAND_DISPLAY").is_some()
        || env::var("XDG_SESSION_TYPE").is_ok_and(|value| value == "wayland");
    if wayland {
        println!(
            "hud: Wayland session detected; layer-shell support is checked when the HUD starts"
        );
    } else {
        println!("hud: blocked — run Cantrip from a Wayland session");
    }

    match ipc::status() {
        Ok(status) => println!("daemon: reachable ({})", status.state_name()),
        Err(_) => println!("daemon: not running or unreachable — open cantrip actions for setup"),
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn doctor() -> Result<()> {
    let config = Config::load();
    match &config {
        Ok(_) if paths::config_file()?.exists() => println!("config: ready"),
        Ok(_) => println!("config: defaults in use — open Settings to customize"),
        Err(_) => println!("config: blocked — invalid or unreadable; open Settings to repair"),
    }
    let status = ipc::status().ok();
    println!(
        "{}",
        microphone_diagnosis(
            status.as_ref().map(|status| status.microphone.clone()),
            cantrip::capture::microphone_permission,
        )
    );
    let capture = cantrip::capture::diagnosis(
        config
            .as_ref()
            .ok()
            .and_then(|config| config.audio_source.as_deref()),
    );
    println!("capture: {capture}");
    if let Ok(config) = &config {
        println!("{}", stt_diagnosis(config));
        println!("{}", cleanup_diagnosis(config));
        println!("{}", telemetry_diagnosis(config));
        let available = inject::delivery_availability();
        println!(
            "injection: mode={}; clipboard={}; automatic={}",
            injection_mode_name(config.injection),
            available.clipboard,
            available.automatic
        );
        if let Some(reason) = available.reason {
            println!("injection: {reason}");
        }
        for (name, target) in &config.handoff {
            println!("{}", handoff_diagnosis(name, target));
        }
    } else {
        println!("stt: blocked — fix config first");
        println!("cleanup: blocked — fix config first");
        println!("telemetry: blocked — fix config first");
    }
    println!("hud: native passive panel; launch Cantrip.app or cantrip hud");
    match status {
        Some(status) => println!("daemon: reachable ({})", status.state_name()),
        None => println!("daemon: not running or unreachable — open Cantrip.app"),
    }
    Ok(())
}

/// Microphone access as the running Cantrip, which records, reports it. Only
/// without one does doctor ask its own process, and labels that answer: macOS
/// answers each process separately.
#[cfg(target_os = "macos")]
fn microphone_diagnosis(
    owner: Option<Option<ipc::MicrophoneAccess>>,
    this_process: impl FnOnce() -> ipc::MicrophoneAuthorization,
) -> String {
    match owner {
        Some(Some(access)) => format!(
            "microphone: {} — reported by the running Cantrip",
            access.guidance()
        ),
        Some(None) => {
            "microphone: not reported by the running Cantrip; restart Cantrip from this version"
                .to_owned()
        }
        // With no capture owner there is no grantee to name; this advice is
        // what Cantrip.app's own process would need.
        None => format!(
            "microphone: {} — this process only; Cantrip is not running and may differ",
            ipc::MicrophoneAccess {
                authorization: this_process(),
                grantee: ipc::MicrophoneGrantee::Cantrip,
            }
            .guidance()
        ),
    }
}

fn executable_ready(command: &str) -> bool {
    let is_executable = |path: &Path| {
        fs::metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    };
    if command.contains('/') {
        return is_executable(Path::new(command));
    }
    env::var_os("PATH")
        .is_some_and(|paths| env::split_paths(&paths).any(|dir| is_executable(&dir.join(command))))
}

/// Only `pass-env` uses `--` to separate its options from the command it runs.
/// A directly spawned executable keeps its own `--` arguments untouched.
const HANDOFF_LAUNCHER: &str = "pass-env";

fn handoff_target_index(command: &[String]) -> Option<usize> {
    let launcher = command.first()?;
    let name = Path::new(launcher).file_name()?.to_str()?;
    if name == HANDOFF_LAUNCHER {
        let separator = command.iter().position(|arg| arg == "--")?;
        (separator + 1 < command.len()).then_some(separator + 1)
    } else {
        Some(0)
    }
}

/// A handoff may run `python3 /path/script.py`; the interpreter is not the
/// whole story. Return the first absolute script operand, ignoring modes that
/// consume code or a module instead of a file.
fn interpreter_script(command: &[String], executable_index: usize) -> Option<&str> {
    let executable = command.get(executable_index)?;
    let name = Path::new(executable).file_name()?.to_str()?;
    if !matches!(
        name,
        "python" | "python3" | "sh" | "bash" | "node" | "perl" | "ruby" | "php"
    ) {
        return None;
    }
    for arg in command.iter().skip(executable_index + 1) {
        if matches!(
            arg.as_str(),
            "-c" | "-m" | "-e" | "-s" | "--eval" | "--execute"
        ) {
            return None;
        }
        if arg.starts_with('-') {
            continue;
        }
        return arg.starts_with('/').then_some(arg.as_str());
    }
    None
}

fn handoff_diagnosis(name: &str, target: &cantrip_engine::config::HandoffTarget) -> String {
    let launcher = target
        .command
        .first()
        .map(String::as_str)
        .unwrap_or_default();
    let target_index = handoff_target_index(&target.command);
    let executable = target_index
        .and_then(|index| target.command.get(index))
        .map(String::as_str);
    let missing_script = target_index
        .and_then(|index| interpreter_script(&target.command, index))
        .filter(|path| !Path::new(path).is_file());
    let missing = match target_index {
        None => Some(""),
        Some(_) if !executable_ready(launcher) => Some(launcher),
        Some(_) if !executable_ready(executable.unwrap_or_default()) => {
            Some(executable.unwrap_or_default())
        }
        Some(_) => missing_script,
    };
    let status = match missing {
        Some("") => "blocked — required command or script missing".to_owned(),
        Some(path) => format!("blocked — required command or script missing: {path}"),
        None => "ready".to_owned(),
    };
    let command = match executable {
        Some(executable) if executable != launcher => format!("{launcher} -> {executable}"),
        _ => launcher.to_owned(),
    };
    format!("handoff.{name}: {status} (command={command})")
}

#[cfg(target_os = "linux")]
fn capture_diagnosis(config: Option<&Config>, tools: DoctorTools) -> String {
    let source = match config.and_then(|config| config.audio_source.as_ref()) {
        Some(_) => "configured source",
        None if config.is_some() => "default input",
        None => "source unknown until config is valid",
    };
    if tools.pw_record {
        if let Some(source) = config.and_then(|config| config.audio_source.as_deref()) {
            if let Err(error) = cantrip::capture::check_source(source) {
                return format!("capture: blocked — {error}");
            }
        }
        format!("capture: ready (pw-record; {source})")
    } else {
        format!("capture: blocked (pw-record missing; {source}) — install PipeWire")
    }
}

fn stt_diagnosis(config: &Config) -> String {
    if let Some(endpoint) = &config.stt.endpoint {
        let fallback = match models::installed(&PARAKEET_V3_INT8) {
            Ok(Some(_)) => "automatic local fallback ready",
            Ok(None) => "local fallback unavailable — run: cantrip models pull",
            Err(_) => "local fallback could not be inspected — run: cantrip models status",
        };
        return format!(
            "stt: remote configured (model={}; endpoint={}; credential={}); {fallback}",
            config.stt.model,
            endpoint_origin(endpoint),
            credential_status(config.stt.api_key_id.as_deref())
        );
    }

    let spec = match models::require(&config.stt.model) {
        Ok(spec) => spec,
        Err(_) => {
            return format!(
                "stt: blocked (unknown local model={}) — run: cantrip config edit",
                config.stt.model
            );
        }
    };
    match models::installed(spec) {
        Ok(Some(_)) => format!("stt: local ready (model={})", config.stt.model),
        Ok(None) => format!(
            "stt: blocked (local model={} not installed) — run: cantrip models pull",
            config.stt.model
        ),
        Err(_) => format!(
            "stt: blocked (could not inspect local model={}) — run: cantrip models status",
            config.stt.model
        ),
    }
}

fn cleanup_diagnosis(config: &Config) -> String {
    if !config.postproc.enabled {
        return "cleanup: disabled (raw transcript is delivered)".to_owned();
    }
    if config.postproc.endpoint.trim().is_empty() {
        return "cleanup: blocked (endpoint is empty) — run: cantrip settings".to_owned();
    }
    if config.postproc.timeout_ms == 0 {
        return "cleanup: blocked (timeout_ms is zero) — run: cantrip settings".to_owned();
    }
    let endpoint = endpoint_origin(&config.postproc.endpoint);
    let lane = endpoint_lane(&endpoint);
    format!(
        "cleanup: {lane} configured (model={}; endpoint={}; credential={}; min_chars={})",
        config.postproc.model,
        endpoint,
        credential_status(config.postproc.api_key_id.as_deref()),
        config.postproc.min_chars
    )
}

#[cfg(target_os = "linux")]
fn injection_diagnosis(mode: InjectionMode, tools: DoctorTools) -> String {
    let order = inject::planned_backend_names(mode, tools.virtual_keyboard, tools.wl_copy);
    let available = !order.is_empty();
    let mode_name = injection_mode_name(mode);
    let order = if available {
        order.join(" -> ")
    } else {
        "none".to_owned()
    };
    if available {
        format!("injection: backends available (mode={mode_name}; order={order}); automatic keys still require verified Hyprland focus and logind lock/suspend history at delivery")
    } else {
        let action = match mode {
            InjectionMode::Paste => "requires wl-clipboard and Wayland virtual-keyboard support; use Settings to choose clipboard-only delivery",
            InjectionMode::Type => "requires Wayland virtual-keyboard support; strict type mode does not use the clipboard",
            InjectionMode::Clipboard => "install wl-clipboard",
            InjectionMode::Auto => "requires Wayland virtual-keyboard support or wl-clipboard",
        };
        format!("injection: unavailable (mode={mode_name}; order={order}) — {action}")
    }
}

fn injection_mode_name(mode: InjectionMode) -> &'static str {
    match mode {
        InjectionMode::Auto => "auto",
        InjectionMode::Paste => "paste",
        InjectionMode::Type => "type",
        InjectionMode::Clipboard => "clipboard",
    }
}

fn credential_status(id: Option<&str>) -> &'static str {
    if id.is_some_and(|id| !id.trim().is_empty()) {
        "keyring id configured"
    } else {
        "none"
    }
}

/// Show only a URL origin. Userinfo, path, query, and fragment can contain
/// credentials or tenant identifiers and never belong in diagnostic output.
fn endpoint_origin(endpoint: &str) -> String {
    let Some((scheme, remainder)) = endpoint.trim().split_once("://") else {
        return "configured endpoint (address hidden)".to_owned();
    };
    let authority = remainder
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .rsplit('@')
        .next()
        .unwrap_or_default();
    if scheme.is_empty() || authority.is_empty() {
        "configured endpoint (address hidden)".to_owned()
    } else {
        format!("{scheme}://{authority}")
    }
}

fn endpoint_lane(origin: &str) -> &'static str {
    let authority = origin
        .split_once("://")
        .map(|(_, authority)| authority)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if authority == "localhost"
        || authority.starts_with("localhost:")
        || authority == "127.0.0.1"
        || authority.starts_with("127.0.0.1:")
        || authority == "[::1]"
        || authority.starts_with("[::1]:")
    {
        "local"
    } else {
        "remote"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn doctor_reports_the_capture_owners_microphone_access_and_labels_its_own() {
        use ipc::{MicrophoneAccess, MicrophoneAuthorization as Access, MicrophoneGrantee};
        let running = |reported| {
            microphone_diagnosis(Some(reported), || {
                panic!("a running Cantrip answers for itself")
            })
        };
        let own = microphone_diagnosis(None, || Access::Authorized);
        let owner = |authorization, grantee| {
            running(Some(MicrophoneAccess {
                authorization,
                grantee,
            }))
        };
        assert_ne!(owner(Access::Authorized, MicrophoneGrantee::Cantrip), own);
        assert_ne!(running(None), own);
        // An outside daemon's answer names the app macOS grants, never Cantrip's row.
        for authorization in [Access::NotDetermined, Access::Denied, Access::Restricted] {
            let terminal = MicrophoneGrantee::Launcher(Some("Terminal".to_owned()));
            let launched = owner(authorization, terminal);
            assert!(launched.contains("Terminal"), "{launched}");
            assert!(!owner(authorization, MicrophoneGrantee::Cantrip).contains("Terminal"));
        }
    }

    #[test]
    fn endpoint_diagnostics_drop_sensitive_url_components() {
        // Built at runtime so the working-tree secret scan does not treat a
        // userinfo URL literal as a credential.
        let endpoint = format!(
            "https://{user}:{pass}@api.example.test/v1/private?api_key={key}#tenant",
            user = "user",
            pass = "password",
            key = "secret",
        );
        let origin = endpoint_origin(&endpoint);
        assert_eq!(origin, "https://api.example.test");
        assert!(!origin.contains("user"));
        assert!(!origin.contains("password"));
        assert!(!origin.contains("secret"));
        assert!(!origin.contains("tenant"));
    }

    #[test]
    fn remote_stt_diagnosis_reports_lane_without_credential_details() {
        let endpoint = format!(
            "https://{user}:{pass}@api.example.test/v1?key={key}",
            user = "user",
            pass = "secret",
            key = "hidden",
        );
        let config = Config {
            stt: cantrip_engine::config::SttConfig {
                model: "speech-model".to_owned(),
                endpoint: Some(endpoint),
                api_key_id: Some("private-key-name".to_owned()),
            },
            ..Config::default()
        };
        let line = stt_diagnosis(&config);
        assert!(!line.contains("secret"));
        assert!(!line.contains("hidden"));
        assert!(!line.contains("private-key-name"));
    }
    #[test]
    fn doctor_checks_handoff_executable_permissions() {
        let root = PathBuf::from(env::var_os("HOME").unwrap())
            .join(".cache/tmp")
            .join(format!("cantrip-doctor-{}", recovery::new_id()));
        fs::create_dir_all(&root).unwrap();
        let executable = root.join("receiver");
        fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        let target = cantrip_engine::config::HandoffTarget {
            command: vec![executable.display().to_string()],
            timeout_seconds: 15,
            label: None,
            local_only: false,
        };
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(handoff_diagnosis("pepper", &target).contains("handoff.pepper: ready"));
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(handoff_diagnosis("pepper", &target).contains("blocked"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn doctor_checks_an_interpreters_script_after_a_launcher() {
        let root = PathBuf::from(env::var_os("HOME").unwrap())
            .join(".cache/tmp")
            .join(format!("cantrip-doctor-script-{}", recovery::new_id()));
        fs::create_dir_all(&root).unwrap();
        let launcher = root.join("pass-env");
        let python = root.join("python3");
        for executable in [&launcher, &python] {
            fs::write(executable, "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let missing = root.join("missing-script.py");
        let target = cantrip_engine::config::HandoffTarget {
            command: vec![
                launcher.display().to_string(),
                "--".to_owned(),
                python.display().to_string(),
                missing.display().to_string(),
            ],
            timeout_seconds: 15,
            label: None,
            local_only: false,
        };
        let line = handoff_diagnosis("maren", &target);
        assert!(line.contains("blocked"), "{line}");
        assert!(line.contains("missing-script.py"), "{line}");
        fs::write(&missing, "# script\n").unwrap();
        assert!(handoff_diagnosis("maren", &target).contains("ready"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn doctor_checks_the_handoff_target_after_a_launcher() {
        let root = PathBuf::from(env::var_os("HOME").unwrap())
            .join(".cache/tmp")
            .join(format!("cantrip-doctor-target-{}", recovery::new_id()));
        fs::create_dir_all(&root).unwrap();
        let launcher = root.join("pass-env");
        fs::write(&launcher, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700)).unwrap();
        let missing = root.join("missing-target");
        let target = cantrip_engine::config::HandoffTarget {
            command: vec![
                launcher.display().to_string(),
                "--".to_owned(),
                missing.display().to_string(),
            ],
            timeout_seconds: 15,
            label: None,
            local_only: false,
        };
        let line = handoff_diagnosis("home", &target);
        assert!(line.contains("blocked"), "{line}");
        assert!(line.contains("missing-target"), "{line}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn doctor_does_not_reinterpret_a_direct_commands_double_dash() {
        let root = PathBuf::from(env::var_os("HOME").unwrap())
            .join(".cache/tmp")
            .join(format!("cantrip-doctor-direct-{}", recovery::new_id()));
        fs::create_dir_all(&root).unwrap();
        let executable = root.join("receiver");
        fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let target = cantrip_engine::config::HandoffTarget {
            command: vec![
                executable.display().to_string(),
                "--".to_owned(),
                "argument".to_owned(),
            ],
            timeout_seconds: 15,
            label: None,
            local_only: false,
        };
        assert!(handoff_diagnosis("direct", &target).contains("ready"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn doctor_skips_interpreter_code_and_module_modes() {
        let root = PathBuf::from(env::var_os("HOME").unwrap())
            .join(".cache/tmp")
            .join(format!("cantrip-doctor-inline-{}", recovery::new_id()));
        fs::create_dir_all(&root).unwrap();
        let launcher = root.join("pass-env");
        let python = root.join("python3");
        for executable in [&launcher, &python] {
            fs::write(executable, "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let target = cantrip_engine::config::HandoffTarget {
            command: vec![
                launcher.display().to_string(),
                "--".to_owned(),
                python.display().to_string(),
                "-c".to_owned(),
                "print(1)".to_owned(),
                "/not/a/script".to_owned(),
            ],
            timeout_seconds: 15,
            label: None,
            local_only: false,
        };
        assert!(handoff_diagnosis("inline", &target).contains("ready"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn doctor_blocks_a_launcher_without_a_target() {
        let root = PathBuf::from(env::var_os("HOME").unwrap())
            .join(".cache/tmp")
            .join(format!("cantrip-doctor-empty-{}", recovery::new_id()));
        fs::create_dir_all(&root).unwrap();
        let launcher = root.join("pass-env");
        fs::write(&launcher, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700)).unwrap();
        let target = cantrip_engine::config::HandoffTarget {
            command: vec![launcher.display().to_string(), "--".to_owned()],
            timeout_seconds: 15,
            label: None,
            local_only: false,
        };
        let line = handoff_diagnosis("empty", &target);
        assert!(line.contains("blocked"), "{line}");
        fs::remove_dir_all(root).unwrap();
    }
}
