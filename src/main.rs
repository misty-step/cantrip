use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::{Arc, Mutex};

use cantrip::ipc::{self, Command};
use cantrip::models::{self, PARAKEET_V3_INT8};
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
        CliCommand::Status => send_command(Command::Status),
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
    let reply = ipc::send(command)?;
    println!("state: {}", reply.state);
    if let Some(message) = &reply.message {
        println!("message: {message}");
    }
    if let Some(elapsed) = reply.elapsed {
        println!("elapsed: {elapsed}s");
    }
    if let Some(stage) = reply.stage {
        println!("stage: {stage}");
    }
    if let Some(last) = &reply.last {
        println!("last: {last}");
    }
    if reply.last_ok == Some(false) {
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
    if !reply.ok {
        anyhow::bail!("daemon rejected the command");
    }
    Ok(())
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

fn model_status() -> Result<()> {
    match models::installed(&PARAKEET_V3_INT8).context("checking transcription model")? {
        Some(path) => println!("installed: {}", path.display()),
        None => println!("not installed"),
    }
    Ok(())
}

fn doctor() -> Result<()> {
    println!("pw-record: {}", availability("pw-record"));
    println!("wtype: {}", availability("wtype"));
    println!("ydotool: {}", availability("ydotool"));
    println!("wl-copy: {}", availability("wl-copy"));
    match ydotool_socket() {
        Some(path) => println!("ydotool socket: {}", path.display()),
        None => println!("ydotool socket: not found"),
    }
    match models::installed(&PARAKEET_V3_INT8) {
        Ok(Some(path)) => println!("model: installed ({})", path.display()),
        Ok(None) => println!("model: not installed"),
        Err(error) => println!("model: error ({error:#})"),
    }
    match ipc::send(Command::Ping) {
        Ok(reply) if reply.ok => println!("daemon: reachable ({})", reply.state),
        Ok(reply) => println!("daemon: replied not ok ({})", reply.state),
        Err(error) => println!("daemon: unreachable ({error:#})"),
    }
    println!(
        "XDG_CURRENT_DESKTOP: {}",
        env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "unset".to_owned())
    );
    Ok(())
}

fn availability(name: &str) -> &'static str {
    if cantrip::inject::executable_in_path(name) {
        "found"
    } else {
        "not found"
    }
}

fn ydotool_socket() -> Option<PathBuf> {
    cantrip::inject::find_ydotool_socket()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn clap_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
