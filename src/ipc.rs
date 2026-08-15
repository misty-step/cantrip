//! One-line command and reply protocol over the daemon Unix socket.

pub use crate::capture::AUDIO_WAVEFORM_BINS;
use crate::paths;
use crate::pipeline::Stage;
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
    /// Re-inject the last saved transcript (paste/clipboard).
    Last,
    /// Re-run STT on the last fully-failed WAV, if kept.
    Recover,
    Ping,
    Reload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Request {
    Command(Command),
    Status,
}

impl Request {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        if value == "status" {
            Some(Self::Status)
        } else {
            Command::parse(value).map(Self::Command)
        }
    }
}

impl Command {
    fn parse(value: &str) -> Option<Self> {
        match value {
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
            Self::Last => "last",
            Self::Recover => "recover",
            Self::Ping => "ping",
            Self::Reload => "reload",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateKind {
    Idle,
    Recording,
    Processing,
    Unknown(String),
}

impl StateKind {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Idle => "idle",
            Self::Recording => "recording",
            Self::Processing => "processing",
            Self::Unknown(state) => state,
        }
    }
}

impl From<String> for StateKind {
    fn from(state: String) -> Self {
        match state.as_str() {
            "idle" => Self::Idle,
            "recording" => Self::Recording,
            "processing" => Self::Processing,
            _ => Self::Unknown(state),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalOutcome {
    pub message: String,
    pub ok: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioSignal {
    pub level: u8,
    pub silent: bool,
    pub waveform: AudioWaveform,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandReply {
    pub ok: bool,
    pub state: StateKind,
    pub message: Option<String>,
    pub stage: Option<Stage>,
    pub outcome: Option<TerminalOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusSnapshot {
    Idle {
        outcome: Option<TerminalOutcome>,
    },
    Recording {
        elapsed: u64,
        signal: Option<AudioSignal>,
        outcome: Option<TerminalOutcome>,
    },
    Processing {
        stage: Stage,
        outcome: Option<TerminalOutcome>,
    },
    Unknown {
        state: String,
        outcome: Option<TerminalOutcome>,
    },
}

impl StatusSnapshot {
    pub fn state_name(&self) -> &str {
        match self {
            Self::Idle { .. } => "idle",
            Self::Recording { .. } => "recording",
            Self::Processing { .. } => "processing",
            Self::Unknown { state, .. } => state,
        }
    }

    pub fn outcome(&self) -> Option<&TerminalOutcome> {
        match self {
            Self::Idle { outcome }
            | Self::Recording { outcome, .. }
            | Self::Processing { outcome, .. }
            | Self::Unknown { outcome, .. } => outcome.as_ref(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireReply {
    ok: bool,
    state: String,
    message: Option<String>,
    #[serde(default)]
    elapsed: Option<u64>,
    #[serde(default)]
    audio_level: Option<u8>,
    #[serde(default)]
    audio_silent: Option<bool>,
    #[serde(default)]
    audio_waveform: Option<AudioWaveform>,
    #[serde(default)]
    stage: Option<String>,
    #[serde(default)]
    last: Option<String>,
    #[serde(default)]
    last_ok: Option<bool>,
}

impl WireReply {
    pub(crate) fn command(ok: bool, state: &str, message: Option<String>) -> Self {
        Self {
            ok,
            state: state.to_owned(),
            message,
            elapsed: None,
            audio_level: None,
            audio_silent: None,
            audio_waveform: None,
            stage: None,
            last: None,
            last_ok: None,
        }
    }

    pub(crate) fn status(
        state: &str,
        elapsed: Option<u64>,
        signal: Option<AudioSignal>,
        stage: Option<&Stage>,
        outcome: Option<TerminalOutcome>,
    ) -> Self {
        let (audio_level, audio_silent, audio_waveform) = match signal {
            Some(signal) => (
                Some(signal.level),
                Some(signal.silent),
                Some(signal.waveform),
            ),
            None => (None, None, None),
        };
        let (last, last_ok) = match outcome {
            Some(outcome) => (Some(outcome.message), Some(outcome.ok)),
            None => (None, None),
        };
        Self {
            ok: true,
            state: state.to_owned(),
            message: None,
            elapsed,
            audio_level,
            audio_silent,
            audio_waveform,
            stage: stage.map(ToString::to_string),
            last,
            last_ok,
        }
    }

    pub(crate) fn with_stage(mut self, stage: &Stage) -> Self {
        self.stage = Some(stage.to_string());
        self
    }

    pub(crate) fn with_outcome(mut self, outcome: Option<TerminalOutcome>) -> Self {
        if let Some(outcome) = outcome {
            self.last = Some(outcome.message);
            self.last_ok = Some(outcome.ok);
        }
        self
    }

    fn into_command(self) -> CommandReply {
        CommandReply {
            ok: self.ok,
            state: self.state.into(),
            message: self.message,
            stage: self.stage.map(Stage::from),
            outcome: terminal_outcome(self.last, self.last_ok),
        }
    }

    fn into_status(self) -> Result<StatusSnapshot> {
        if !self.ok {
            let message = self
                .message
                .unwrap_or_else(|| "daemon rejected the status request".to_owned());
            anyhow::bail!("{message}");
        }
        let outcome = terminal_outcome(self.last, self.last_ok);
        match self.state.as_str() {
            "idle" => Ok(StatusSnapshot::Idle { outcome }),
            "recording" => {
                let signal = match (self.audio_level, self.audio_silent, self.audio_waveform) {
                    (Some(level), Some(silent), Some(waveform)) => Some(AudioSignal {
                        level,
                        silent,
                        waveform,
                    }),
                    (None, None, None) => None,
                    _ => anyhow::bail!("daemon returned incomplete recording audio status"),
                };
                Ok(StatusSnapshot::Recording {
                    elapsed: self.elapsed.unwrap_or_default(),
                    signal,
                    outcome,
                })
            }
            "processing" => Ok(StatusSnapshot::Processing {
                stage: self
                    .stage
                    .map(Stage::from)
                    .unwrap_or(Stage::Transcribing { chunk: 1, total: 1 }),
                outcome,
            }),
            _ => Ok(StatusSnapshot::Unknown {
                state: self.state,
                outcome,
            }),
        }
    }
}

fn terminal_outcome(message: Option<String>, ok: Option<bool>) -> Option<TerminalOutcome> {
    message.map(|message| TerminalOutcome {
        message,
        ok: ok.unwrap_or(false),
    })
}

/// Send a daemon command and read its acknowledgement.
pub fn send_command(command: Command) -> Result<CommandReply> {
    let timeout = match command {
        Command::Toggle { .. } | Command::Stop => Duration::from_secs(30),
        _ => Duration::from_secs(10),
    };
    Ok(exchange(command.as_str(), timeout)?.into_command())
}

/// Read a complete daemon status snapshot.
pub fn status() -> Result<StatusSnapshot> {
    exchange("status", Duration::from_secs(10))?.into_status()
}

fn exchange(request: &str, timeout: Duration) -> Result<WireReply> {
    let socket = paths::socket_path().context("locating daemon socket")?;
    let mut stream = UnixStream::connect(&socket).map_err(|error| {
        anyhow::anyhow!(
            "cannot connect to cantrip daemon at {}: {error}; start it with: cantrip daemon",
            socket.display()
        )
    })?;
    stream
        .set_read_timeout(Some(timeout))
        .context("setting daemon reply timeout")?;
    writeln!(stream, "{request}").context("sending daemon request")?;

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

    fn waveform() -> AudioWaveform {
        [
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
        ]
    }

    #[test]
    fn request_parse_round_trip_keeps_status_out_of_commands() {
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
            Command::Last,
            Command::Recover,
            Command::Ping,
            Command::Reload,
        ];
        for command in commands {
            assert_eq!(
                Request::parse(command.as_str()),
                Some(Request::Command(command))
            );
        }
        assert_eq!(Request::parse("status"), Some(Request::Status));
        assert_eq!(Request::parse("unknown"), None);
    }

    #[test]
    fn command_wire_reply_preserves_null_status_fields() {
        let reply = WireReply::command(true, "recording", Some("recording".to_owned()));
        let json = serde_json::to_value(reply).expect("reply should serialize");
        assert_eq!(json["state"], "recording");
        assert!(json["elapsed"].is_null());
        assert!(json["audio_level"].is_null());
        assert!(json["audio_silent"].is_null());
        assert!(json["audio_waveform"].is_null());
    }

    #[test]
    fn command_view_accepts_state_without_status_payload() {
        let wire: WireReply =
            serde_json::from_str(r#"{"ok":true,"state":"recording","message":"recording"}"#)
                .expect("legacy command reply should parse");
        assert_eq!(
            wire.into_command(),
            CommandReply {
                ok: true,
                state: StateKind::Recording,
                message: Some("recording".to_owned()),
                stage: None,
                outcome: None,
            }
        );
    }

    #[test]
    fn idle_status_preserves_legacy_terminal_outcome() {
        let wire: WireReply = serde_json::from_str(
            r#"{"ok":true,"state":"idle","message":null,"last":"Heard nothing"}"#,
        )
        .expect("legacy idle status should parse");
        assert_eq!(
            wire.into_status().expect("idle status should convert"),
            StatusSnapshot::Idle {
                outcome: Some(TerminalOutcome {
                    message: "Heard nothing".to_owned(),
                    ok: false,
                }),
            }
        );
    }

    #[test]
    fn processing_status_preserves_typed_stage_across_the_json_boundary() {
        let outbound = [
            (Stage::Transcribing { chunk: 1, total: 1 }, "transcribing"),
            (
                Stage::Transcribing { chunk: 2, total: 5 },
                "transcribing 2/5",
            ),
            (Stage::CleaningUp, "cleaning"),
            (Stage::Unknown("calibrating".to_owned()), "calibrating"),
        ];
        for (stage, expected) in outbound {
            let json = serde_json::to_value(WireReply::status(
                "processing",
                None,
                None,
                Some(&stage),
                None,
            ))
            .expect("status should serialize");
            assert_eq!(json["stage"], expected);
        }

        let inbound = [
            (
                Some("transcribing"),
                Stage::Transcribing { chunk: 1, total: 1 },
            ),
            (
                Some("transcribing 1/1"),
                Stage::Transcribing { chunk: 1, total: 1 },
            ),
            (
                Some("transcribing 2/5"),
                Stage::Transcribing { chunk: 2, total: 5 },
            ),
            (Some("cleaning"), Stage::CleaningUp),
            (
                Some("transcribing 0/3"),
                Stage::Unknown("transcribing 0/3".to_owned()),
            ),
            (
                Some("transcribing 4/3"),
                Stage::Unknown("transcribing 4/3".to_owned()),
            ),
            (
                Some("transcribing 1/0"),
                Stage::Unknown("transcribing 1/0".to_owned()),
            ),
            (
                Some("transcribing nope"),
                Stage::Unknown("transcribing nope".to_owned()),
            ),
            (
                Some("calibrating"),
                Stage::Unknown("calibrating".to_owned()),
            ),
            (None, Stage::Transcribing { chunk: 1, total: 1 }),
        ];

        for (wire_stage, expected) in inbound {
            let mut json = serde_json::json!({"ok": true, "state": "processing", "message": null});
            if let Some(stage) = wire_stage {
                json.as_object_mut()
                    .expect("fixture should be an object")
                    .insert(
                        "stage".to_owned(),
                        serde_json::Value::String(stage.to_owned()),
                    );
            }
            let wire: WireReply =
                serde_json::from_value(json).expect("status fixture should deserialize");
            assert_eq!(
                wire.into_status().expect("status should convert"),
                StatusSnapshot::Processing {
                    stage: expected,
                    outcome: None,
                }
            );
        }
    }

    #[test]
    fn command_stage_preserves_recover_wire_and_typed_views() {
        let stage = Stage::Transcribing { chunk: 1, total: 1 };
        let wire = WireReply::command(true, "processing", Some("recovering".to_owned()))
            .with_stage(&stage);
        let json = serde_json::to_value(&wire).expect("command reply should serialize");
        assert_eq!(json["stage"], "transcribing");
        assert_eq!(
            wire.into_command().stage,
            Some(Stage::Transcribing { chunk: 1, total: 1 })
        );
    }

    #[test]
    fn recording_status_groups_complete_audio_signal() {
        let wire = WireReply::status(
            "recording",
            Some(3),
            Some(AudioSignal {
                level: 72,
                silent: false,
                waveform: waveform(),
            }),
            None,
            None,
        );
        assert_eq!(
            wire.into_status().expect("status should convert"),
            StatusSnapshot::Recording {
                elapsed: 3,
                signal: Some(AudioSignal {
                    level: 72,
                    silent: false,
                    waveform: waveform(),
                }),
                outcome: None,
            }
        );
    }

    #[test]
    fn legacy_status_defaults_missing_recording_fields() {
        let wire: WireReply =
            serde_json::from_str(r#"{"ok":true,"state":"recording","message":null}"#)
                .expect("legacy status should parse");
        assert_eq!(
            wire.into_status().expect("legacy status should convert"),
            StatusSnapshot::Recording {
                elapsed: 0,
                signal: None,
                outcome: None,
            }
        );
    }

    #[test]
    fn future_state_remains_readable() {
        let wire: WireReply = serde_json::from_str(
            r#"{"ok":true,"state":"calibrating","message":null,"future_metric":17}"#,
        )
        .expect("future status should parse");
        assert_eq!(
            wire.into_status().expect("future status should convert"),
            StatusSnapshot::Unknown {
                state: "calibrating".to_owned(),
                outcome: None,
            }
        );
    }

    #[test]
    fn partial_audio_status_is_rejected() {
        let wire: WireReply = serde_json::from_str(
            r#"{"ok":true,"state":"recording","message":null,"audio_level":72}"#,
        )
        .expect("wire reply should parse");
        let error = wire
            .into_status()
            .expect_err("partial audio status must be rejected");
        assert!(error.to_string().contains("incomplete recording audio"));
    }
}
