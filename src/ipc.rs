//! One-line command and reply protocol over the daemon Unix socket.

pub use crate::capture::AUDIO_WAVEFORM_BINS;
use crate::paths;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;
pub type AudioWaveform = [[i8; 2]; AUDIO_WAVEFORM_BINS];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Toggle {
        /// Post-processing override for this capture: Some(true) = clean,
        /// Some(false) = raw, None = follow [postproc].enabled.
        postproc: Option<bool>,
    },
    Start {
        postproc: Option<bool>,
    },
    Stop,
    Cancel,
    Status,
    /// Re-inject the last saved transcript (paste/clipboard).
    Last,
    /// Re-run STT on the last fully-failed WAV, if kept.
    Recover,
    Ping,
    Reload,
}

impl Command {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "toggle" => Some(Self::Toggle { postproc: None }),
            "toggle-clean" => Some(Self::Toggle {
                postproc: Some(true),
            }),
            "toggle-raw" => Some(Self::Toggle {
                postproc: Some(false),
            }),
            "start" => Some(Self::Start { postproc: None }),
            "start-clean" => Some(Self::Start {
                postproc: Some(true),
            }),
            "start-raw" => Some(Self::Start {
                postproc: Some(false),
            }),
            "stop" => Some(Self::Stop),
            "cancel" => Some(Self::Cancel),
            "status" => Some(Self::Status),
            "last" => Some(Self::Last),
            "recover" => Some(Self::Recover),
            "ping" => Some(Self::Ping),
            "reload" => Some(Self::Reload),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Toggle { postproc: None } => "toggle",
            Self::Toggle {
                postproc: Some(true),
            } => "toggle-clean",
            Self::Toggle {
                postproc: Some(false),
            } => "toggle-raw",
            Self::Start { postproc: None } => "start",
            Self::Start {
                postproc: Some(true),
            } => "start-clean",
            Self::Start {
                postproc: Some(false),
            } => "start-raw",
            Self::Stop => "stop",
            Self::Cancel => "cancel",
            Self::Status => "status",
            Self::Last => "last",
            Self::Recover => "recover",
            Self::Ping => "ping",
            Self::Reload => "reload",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    pub state: String,
    pub message: Option<String>,
    /// Recording elapsed seconds, present while state is "recording".
    #[serde(default)]
    pub elapsed: Option<u64>,
    /// Peak level for the newest recording samples, logarithmically mapped to
    /// 0..=100. Absent outside recording or when live WAV monitoring is not
    /// available.
    #[serde(default)]
    pub audio_level: Option<u8>,
    /// Whether the recording has remained near digital silence for at least
    /// three seconds. Absent outside recording or when monitoring is unknown.
    #[serde(default)]
    pub audio_silent: Option<bool>,
    /// Chronological min/max PCM envelope bins for the latest daemon-owned
    /// sample window. Each `[minimum, maximum]` pair uses a logarithmic
    /// -100..=100 scale.
    #[serde(default)]
    pub audio_waveform: Option<AudioWaveform>,
    /// Processing sub-stage, present while state is "processing":
    /// "transcribing", "transcribing N/M", or "cleaning".
    #[serde(default)]
    pub stage: Option<String>,
    /// Most recent terminal message (Typed N chars, Heard nothing,
    /// Cancelled, ...), cleared when a new recording starts.
    #[serde(default)]
    pub last: Option<String>,
    /// Whether the last terminal message was a delivered dictation.
    #[serde(default)]
    pub last_ok: Option<bool>,
}

/// Send one command and read one JSON reply from the daemon.
pub fn send(cmd: Command) -> Result<Reply> {
    let socket = paths::socket_path().context("locating daemon socket")?;
    let mut stream = UnixStream::connect(&socket).map_err(|error| {
        anyhow::anyhow!(
            "cannot connect to cantrip daemon at {}: {error}; start it with: cantrip daemon",
            socket.display()
        )
    })?;

    let timeout = match cmd {
        Command::Toggle { .. } | Command::Stop => Duration::from_secs(30),
        _ => Duration::from_secs(10),
    };
    stream
        .set_read_timeout(Some(timeout))
        .context("setting daemon reply timeout")?;
    writeln!(stream, "{}", cmd.as_str()).context("sending daemon command")?;

    let mut line = String::new();
    let mut reader = BufReader::new(stream);
    let bytes = reader
        .read_line(&mut line)
        .context("reading daemon reply")?;
    if bytes == 0 {
        anyhow::bail!("daemon closed the socket without a reply");
    }
    serde_json::from_str(line.trim()).context("parsing daemon reply")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_parse_round_trip() {
        let commands = [
            Command::Toggle { postproc: None },
            Command::Toggle {
                postproc: Some(true),
            },
            Command::Toggle {
                postproc: Some(false),
            },
            Command::Start { postproc: None },
            Command::Start {
                postproc: Some(true),
            },
            Command::Start {
                postproc: Some(false),
            },
            Command::Stop,
            Command::Cancel,
            Command::Status,
            Command::Ping,
            Command::Reload,
        ];
        for command in commands {
            assert_eq!(Command::parse(command.as_str()), Some(command));
        }
        assert_eq!(Command::parse("unknown"), None);
    }

    #[test]
    fn reply_json_round_trip() {
        let reply = Reply {
            ok: true,
            state: "recording".to_owned(),
            message: Some("recording".to_owned()),
            elapsed: Some(3),
            audio_level: Some(72),
            audio_silent: Some(false),
            audio_waveform: Some([
                [-18, 22],
                [-35, 41],
                [-64, 72],
                [-48, 55],
                [-86, 94],
                [-61, 68],
                [-29, 36],
                [-70, 78],
                [-45, 51],
                [-25, 33],
                [-12, 18],
            ]),
            stage: None,
            last: None,
            last_ok: None,
        };
        let json = serde_json::to_string(&reply).expect("reply should serialize");
        let decoded: Reply = serde_json::from_str(&json).expect("reply should deserialize");
        assert_eq!(decoded, reply);
    }
}
