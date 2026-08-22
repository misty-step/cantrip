use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::{Arc, Mutex};

use cantrip::inject::{self, InjectionMode};
use cantrip::ipc::{self, Command};
use cantrip::models::{self, PARAKEET_V3_INT8};
use cantrip::telemetry;
use cantrip::{config::Config, daemon, hud, keys, paths, pipeline, settings};

/// Per-dictation post-processing request, overriding [postproc].enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum PostprocMode {
    /// Run transcript cleanup for this dictation.
    Clean,
    /// Skip transcript cleanup for this dictation.
    Raw,
}

#[derive(Debug, Parser)]
#[command(name = "cantrip", version, about = "Local-first Linux dictation")]
struct Cli {
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// Run the dictation daemon.
    Daemon {
        #[arg(long)]
        preload: bool,
    },
    /// Show the layer-shell status HUD.
    Hud {
        /// Render one frame to a PNG at PATH, then exit (visual testing).
        #[arg(long, value_name = "PATH")]
        screenshot: Option<PathBuf>,
        /// State to render with --screenshot: recording | transcribing |
        /// cleaning | sent | notice (default: recording).
        /// Requires --screenshot.
        #[arg(long, value_enum, requires = "screenshot")]
        state: Option<hud::ScreenshotState>,
    },
    /// Open the configuration window.
    Settings {
        /// Render one frame to a PNG at PATH, then exit (visual testing).
        #[arg(long, value_name = "PATH")]
        screenshot: Option<PathBuf>,
    },
    Toggle {
        /// Post-processing for this dictation: clean | raw (default: [postproc].enabled).
        #[arg(long, value_enum)]
        postproc: Option<PostprocMode>,
    },
    Start {
        /// Post-processing for this dictation: clean | raw (default: [postproc].enabled).
        #[arg(long, value_enum)]
        postproc: Option<PostprocMode>,
    },
    Stop,
    Cancel,
    Status,
    /// Re-inject the last saved transcript.
    Last,
    /// Re-run STT on the last fully-failed recording, if one was kept.
    Recover,
    Ping,
    /// Re-read the config file in the running daemon.
    Reload,
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
fn main() {
    let cli = Cli::parse();
    init_tracing(matches!(cli.command, CliCommand::Daemon { .. }));
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
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

fn open_runtime_log() -> Result<fs::File> {
    let _ = paths::ensure_dir(paths::state_dir()?).context("creating state directory")?;
    let path = paths::daemon_log_path()?;
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("opening state log {}", path.display()))?;
    // create(true) honors umask; force owner-only after open.
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
    match cli.command {
        CliCommand::Daemon { preload } => {
            let config = Config::load().context("loading configuration")?;
            daemon::run(config, preload)
        }
        CliCommand::Hud { screenshot, state } => hud::run(screenshot, state),
        CliCommand::Settings { screenshot } => settings::run(screenshot),
        CliCommand::Toggle { postproc } => send_command(Command::Toggle {
            postproc: postproc.map(|mode| mode == PostprocMode::Clean),
        }),
        CliCommand::Start { postproc } => send_command(Command::Start {
            postproc: postproc.map(|mode| mode == PostprocMode::Clean),
        }),
        CliCommand::Stop => send_command(Command::Stop),
        CliCommand::Cancel => send_command(Command::Cancel),
        CliCommand::Status => print_status(),
        CliCommand::Last => send_command(Command::Last),
        CliCommand::Recover => send_command(Command::Recover),
        CliCommand::Reload => send_command(Command::Reload),
        CliCommand::Config { command } => run_config(command),
        CliCommand::Key { command } => run_key(command),
        CliCommand::Ping => send_command(Command::Ping),
        CliCommand::Transcribe { wav } => transcribe_file(&wav),
        CliCommand::Models { command } => match command {
            ModelsCommand::Pull => pull_model(),
            ModelsCommand::Status => model_status(),
        },
        CliCommand::Doctor => doctor(),
    }
}

const CONFIG_TEMPLATE: &str = r#"injection = "auto"        # auto | paste | type | clipboard
keep_warm = true
# audio_source = "…"      # optional PipeWire target
vocabulary = []           # exact-spelling terms for postproc + cloud STT

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
    fs::create_dir_all(parent)
        .with_context(|| format!("creating configuration directory {}", parent.display()))?;
    fs::write(path, CONFIG_TEMPLATE)
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
    let reply = ipc::send_command(command)?;
    println!("state: {}", reply.state.as_str());
    if let Some(message) = &reply.message {
        println!("message: {message}");
    }
    if let Some(stage) = reply.stage {
        println!("stage: {stage}");
    }
    print_outcome(reply.outcome.as_ref());
    if !reply.ok {
        anyhow::bail!("daemon rejected the command");
    }
    Ok(())
}

fn print_status() -> Result<()> {
    let status = ipc::status()?;
    println!("state: {}", status.state_name());
    match &status {
        ipc::StatusSnapshot::Recording {
            elapsed, signal, ..
        } => {
            println!("elapsed: {elapsed}s");
            if let Some(signal) = signal {
                println!("audio-level: {}%", signal.level);
                println!("audio-silent: {}", signal.silent);
                print!("audio-waveform:");
                for [minimum, maximum] in signal.waveform {
                    print!(" {minimum}:{maximum}");
                }
                println!();
            }
        }
        ipc::StatusSnapshot::Processing { stage, .. } => println!("stage: {stage}"),
        ipc::StatusSnapshot::Idle { .. } | ipc::StatusSnapshot::Unknown { .. } => {}
    }
    print_outcome(status.outcome());
    Ok(())
}

fn print_outcome(outcome: Option<&ipc::TerminalOutcome>) {
    let Some(outcome) = outcome else {
        return;
    };
    println!("last: {}", outcome.message);
    if !outcome.ok {
        if paths::last_transcript_path()
            .ok()
            .is_some_and(|path| path.is_file())
        {
            println!("hint: cantrip last   # re-paste the last saved transcript");
        }
        if paths::last_failed_wav_path()
            .ok()
            .is_some_and(|path| path.is_file())
        {
            println!("hint: cantrip recover   # retry the last failed recording");
        }
    }
}

fn transcribe_file(wav: &std::path::Path) -> Result<()> {
    let config = Config::load().context("loading configuration")?;
    if config.stt.endpoint.is_none() {
        let spec = models::require(&config.stt.model)?;
        models::ensure_model(spec).context("ensuring transcription model")?;
    }
    let mut cache = None;
    let outcome = pipeline::run(
        &mut cache,
        wav,
        &config.stt,
        &config.vocabulary,
        &config.postproc,
        pipeline::Source::Transcribe,
        |_| {},
    );
    if let pipeline::ArchiveStatus::Failed(error) = &outcome.archive {
        eprintln!("warning: transcript archive failed: {error}");
    }
    emit_transcribe_telemetry(&config, &outcome);
    match &outcome.text {
        Ok(text) => {
            if matches!(outcome.postproc, pipeline::PostprocStatus::Failed { .. }) {
                eprintln!("post-processing failed; showing the raw transcript");
            }
            println!("{text}");
        }
        Err(error) => anyhow::bail!("transcribing {}: {error}", wav.display()),
    }
    Ok(())
}

fn pull_model() -> Result<()> {
    println!("pulling model...");
    let model_dir =
        models::ensure_model(&PARAKEET_V3_INT8).context("pulling transcription model")?;
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
    let error_class = outcome.text.as_ref().err().map(|_| "stt-failed".to_owned());
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
        stt_model: config.stt.model.clone(),
        stt_remote: config.stt.endpoint.is_some(),
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

#[derive(Debug, Clone, Copy)]
struct DoctorTools {
    pw_record: bool,
    wtype: bool,
    ydotool: bool,
    ydotool_socket: bool,
    wl_copy: bool,
}

fn doctor() -> Result<()> {
    let tools = DoctorTools {
        pw_record: inject::executable_in_path("pw-record"),
        wtype: inject::executable_in_path("wtype"),
        ydotool: inject::executable_in_path("ydotool"),
        ydotool_socket: inject::find_ydotool_socket().is_some(),
        wl_copy: inject::executable_in_path("wl-copy"),
    };
    let config_path = paths::config_file().context("locating config file")?;
    let config_exists = config_path.exists();
    let config = Config::load();

    match &config {
        Ok(_) if config_exists => println!("config: ready"),
        Ok(_) => println!("config: defaults in use — run: cantrip config init"),
        Err(_) => {
            println!("config: blocked — invalid or unreadable; run: cantrip config edit")
        }
    }
    println!("{}", capture_diagnosis(config.as_ref().ok(), tools));

    if let Ok(config) = &config {
        println!("{}", stt_diagnosis(config));
        println!("{}", cleanup_diagnosis(config));
        println!("{}", telemetry_diagnosis(config));
        println!("{}", injection_diagnosis(config.injection, tools));
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

    let daemon = match ipc::send_command(Command::Ping) {
        Ok(reply) if reply.ok => daemon_diagnosis(Some(reply.state.as_str())),
        _ => daemon_diagnosis(None),
    };
    println!("{daemon}");
    Ok(())
}

fn daemon_diagnosis(state: Option<&str>) -> String {
    match state {
        Some(state) => format!("daemon: reachable ({state})"),
        None => {
            "daemon: not running — run: cantrip daemon (first session) or systemctl --user enable --now cantrip (daily driver)"
                .to_owned()
        }
    }
}

fn capture_diagnosis(config: Option<&Config>, tools: DoctorTools) -> String {
    let source = match config.and_then(|config| config.audio_source.as_ref()) {
        Some(_) => "configured source",
        None if config.is_some() => "default input",
        None => "source unknown until config is valid",
    };
    if tools.pw_record {
        format!("capture: ready (pw-record; {source})")
    } else {
        format!("capture: blocked (pw-record missing; {source}) — install PipeWire")
    }
}

fn stt_diagnosis(config: &Config) -> String {
    if let Some(endpoint) = &config.stt.endpoint {
        return format!(
            "stt: remote configured (model={}; endpoint={}; credential={})",
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

fn injection_diagnosis(mode: InjectionMode, tools: DoctorTools) -> String {
    let ydotool_ready = tools.ydotool && tools.ydotool_socket;
    let order = inject::planned_backend_names(mode, tools.wtype, ydotool_ready, tools.wl_copy);
    let ready = match mode {
        InjectionMode::Auto => tools.wtype || ydotool_ready || tools.wl_copy,
        InjectionMode::Paste => tools.wl_copy && (tools.wtype || ydotool_ready),
        InjectionMode::Type => tools.wtype || ydotool_ready,
        InjectionMode::Clipboard => tools.wl_copy,
    };
    let mode = injection_mode_name(mode);
    let order = if order.is_empty() {
        "none".to_owned()
    } else {
        order.join(" -> ")
    };
    if ready {
        format!("injection: ready (mode={mode}; order={order})")
    } else {
        let action = match mode {
            "paste" => "install wl-clipboard and wtype, or change injection mode",
            "type" => "install wtype or ydotool with its daemon",
            "clipboard" => "install wl-clipboard",
            _ => "install wl-clipboard or wtype",
        };
        format!("injection: blocked (mode={mode}; order={order}) — {action}")
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
    use clap::CommandFactory;

    #[test]
    fn clap_definition_is_valid() {
        Cli::command().debug_assert();
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
            stt: cantrip::config::SttConfig {
                model: "speech-model".to_owned(),
                endpoint: Some(endpoint),
                api_key_id: Some("private-key-name".to_owned()),
            },
            ..Config::default()
        };
        let line = stt_diagnosis(&config);
        assert_eq!(
            line,
            "stt: remote configured (model=speech-model; endpoint=https://api.example.test; credential=keyring id configured)"
        );
        assert!(!line.contains("secret"));
        assert!(!line.contains("hidden"));
        assert!(!line.contains("private-key-name"));
    }

    #[test]
    fn cleanup_diagnosis_reports_lane_and_rejects_empty_endpoint() {
        let mut config = Config {
            postproc: cantrip::config::PostprocConfig {
                enabled: true,
                endpoint: "http://localhost:11434/v1".to_owned(),
                model: "qwen3:8b".to_owned(),
                ..Default::default()
            },
            ..Config::default()
        };
        assert_eq!(
            cleanup_diagnosis(&config),
            "cleanup: local configured (model=qwen3:8b; endpoint=http://localhost:11434; credential=none; min_chars=40)"
        );

        config.postproc.endpoint.clear();
        assert_eq!(
            cleanup_diagnosis(&config),
            "cleanup: blocked (endpoint is empty) — run: cantrip settings"
        );
    }
    #[test]
    fn daemon_diagnosis_names_terminal_and_systemd_paths() {
        assert_eq!(daemon_diagnosis(Some("idle")), "daemon: reachable (idle)");
        assert_eq!(
            daemon_diagnosis(None),
            "daemon: not running — run: cantrip daemon (first session) or systemctl --user enable --now cantrip (daily driver)"
        );
    }

    #[test]
    fn injection_diagnosis_uses_execution_backend_order() {
        let line = injection_diagnosis(
            InjectionMode::Auto,
            DoctorTools {
                pw_record: true,
                wtype: true,
                ydotool: true,
                ydotool_socket: false,
                wl_copy: true,
            },
        );
        assert_eq!(
            line,
            "injection: ready (mode=auto; order=paste -> wtype -> clipboard)"
        );
    }

    #[test]
    fn injection_diagnosis_explains_strict_mode_blocker() {
        let line = injection_diagnosis(
            InjectionMode::Paste,
            DoctorTools {
                pw_record: true,
                wtype: false,
                ydotool: false,
                ydotool_socket: false,
                wl_copy: true,
            },
        );
        assert_eq!(
            line,
            "injection: blocked (mode=paste; order=none) — install wl-clipboard and wtype, or change injection mode"
        );
    }
}
