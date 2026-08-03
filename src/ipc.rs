//! One-line command and reply protocol over the daemon Unix socket.

use crate::paths;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Toggle,
    Start,
    Stop,
    Cancel,
    Status,
    Ping,
    Reload,
}

impl Command {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "toggle" => Some(Self::Toggle),
            "start" => Some(Self::Start),
            "stop" => Some(Self::Stop),
            "cancel" => Some(Self::Cancel),
            "status" => Some(Self::Status),
            "ping" => Some(Self::Ping),
            "reload" => Some(Self::Reload),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Toggle => "toggle",
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Cancel => "cancel",
            Self::Status => "status",
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
    /// Processing sub-stage, present while state is "processing":
    /// "transcribing" | "cleaning".
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
        Command::Toggle | Command::Stop => Duration::from_secs(30),
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
            Command::Toggle,
            Command::Start,
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
            state: "processing".to_owned(),
            message: Some("queued".to_owned()),
            elapsed: Some(3),
            stage: Some("cleaning".to_owned()),
            last: Some("Typed 42 chars".to_owned()),
            last_ok: Some(true),
        };
        let json = serde_json::to_string(&reply).expect("reply should serialize");
        let decoded: Reply = serde_json::from_str(&json).expect("reply should deserialize");
        assert_eq!(decoded, reply);
    }
}
