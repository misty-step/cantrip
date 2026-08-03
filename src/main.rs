use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::env;
use std::path::PathBuf;

use cantrip::config::Config;
use cantrip::daemon;
use cantrip::ipc::{self, Command};
use cantrip::models::{self, PARAKEET_V3_INT8};
use cantrip::stt::Transcriber;

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
    Toggle,
    Start,
    Stop,
    Cancel,
    Status,
    Ping,
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

fn main() {
    init_tracing();
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,ort=warn,transcribe_rs=warn"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        CliCommand::Daemon { preload } => {
            let config = Config::load().context("loading configuration")?;
            daemon::run(config, preload)
        }
        CliCommand::Toggle => send_command(Command::Toggle),
        CliCommand::Start => send_command(Command::Start),
        CliCommand::Stop => send_command(Command::Stop),
        CliCommand::Cancel => send_command(Command::Cancel),
        CliCommand::Status => send_command(Command::Status),
        CliCommand::Ping => send_command(Command::Ping),
        CliCommand::Transcribe { wav } => transcribe_file(&wav),
        CliCommand::Models { command } => match command {
            ModelsCommand::Pull => pull_model(),
            ModelsCommand::Status => model_status(),
        },
        CliCommand::Doctor => doctor(),
    }
}

fn send_command(command: Command) -> Result<()> {
    let reply = ipc::send(command)?;
    println!("state: {}", reply.state);
    if let Some(message) = &reply.message {
        println!("message: {message}");
    }
    if !reply.ok {
        anyhow::bail!("daemon rejected the command");
    }
    Ok(())
}

fn transcribe_file(wav: &std::path::Path) -> Result<()> {
    let model_dir =
        models::ensure_model(&PARAKEET_V3_INT8).context("ensuring transcription model")?;
    let mut transcriber = Transcriber::load(&model_dir).context("loading transcription model")?;
    let text = transcriber
        .transcribe_wav(wav)
        .with_context(|| format!("transcribing {}", wav.display()))?;
    println!("{text}");
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
    if let Some(path) = env::var_os("YDOTOOL_SOCKET") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Some(path);
        }
    }
    let mut candidates = vec![
        PathBuf::from("/tmp/.ydotool_socket"),
        PathBuf::from("/run/ydotoold.socket"),
    ];
    if let Some(runtime) = env::var_os("XDG_RUNTIME_DIR") {
        candidates.push(PathBuf::from(runtime).join(".ydotool_socket"));
    }
    candidates.into_iter().find(|path| path.exists())
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
