//! Typed command acknowledgements and identity-aware daemon snapshots.

pub use crate::capture::AUDIO_WAVEFORM_BINS;
use crate::config::HudConfig;
use crate::paths;
use crate::pipeline::Stage;
use crate::recovery::Take;
use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Deserializer, Serialize, Serializer};
use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

/// Chronological `[minimum, maximum]` raw signed 16-bit PCM samples.
pub type AudioWaveform = [[i16; 2]; AUDIO_WAVEFORM_BINS];
pub(crate) const REQUEST_LIMIT: usize = 4_096;
pub(crate) const REPLY_LIMIT: usize = 16 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Command {
    Toggle {
        postproc: Option<bool>,
    },
    Start {
        postproc: Option<bool>,
    },
    Stop,
    Cancel,
    /// Deliver the latest saved transcript, selected once when accepted.
    Last,
    Recover {
        id: Option<String>,
        local: bool,
        clipboard: bool,
    },
    Copy {
        id: String,
    },
    Dismiss {
        event_id: Option<u64>,
    },
    Forget {
        id: String,
    },
    Ping,
    Reload,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Request {
    Command(Command),
    Status,
    Recordings,
}

impl Request {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        if value == "status" {
            Some(Self::Status)
        } else if value == "recordings" {
            Some(Self::Recordings)
        } else {
            serde_json::from_str(value).ok().map(Self::Command)
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

impl Serialize for StateKind {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for StateKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::from)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Completeness {
    Complete,
    Partial,
    Empty,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Delivery {
    None,
    Typed,
    Pasted,
    Copied,
    Failed,
    Uncertain,
    Deferred,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Cleanup {
    Off,
    Applied,
    Skipped,
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifacts {
    pub take_id: Option<String>,
    pub audio: bool,
    pub text: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalOutcome {
    pub event_id: u64,
    pub operation_id: Option<String>,
    pub message: String,
    pub completeness: Completeness,
    pub delivery: Delivery,
    pub cleanup: Cleanup,
    pub error: Option<String>,
    pub artifacts: Artifacts,
    pub dismissed: bool,
}

impl TerminalOutcome {
    /// A complete transcript reached a delivery helper; this is not an app receipt.
    pub fn is_success(&self) -> bool {
        self.completeness == Completeness::Complete
            && matches!(
                self.delivery,
                Delivery::Typed | Delivery::Pasted | Delivery::Copied
            )
    }

    pub fn needs_attention(&self) -> bool {
        !self.dismissed
            && (matches!(
                self.completeness,
                Completeness::Partial | Completeness::Failed
            ) || matches!(
                self.delivery,
                Delivery::Failed | Delivery::Uncertain | Delivery::Deferred
            ) || self
                .error
                .as_deref()
                .is_some_and(|error| error != "cleanup-failed"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractionNotice {
    pub event_id: u64,
    pub message: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub stop: bool,
    pub cancel: bool,
    pub recover: bool,
    pub copy: bool,
    pub dismiss: bool,
    pub local_model: bool,
    pub remote_configured: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioSignal {
    pub level: u8,
    pub silent: bool,
    #[serde(
        serialize_with = "serialize_waveform",
        deserialize_with = "deserialize_waveform"
    )]
    pub waveform: AudioWaveform,
}

fn serialize_waveform<S: Serializer>(
    waveform: &AudioWaveform,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    waveform.as_slice().serialize(serializer)
}

fn deserialize_waveform<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<AudioWaveform, D::Error> {
    struct WaveformVisitor;

    impl<'de> serde::de::Visitor<'de> for WaveformVisitor {
        type Value = AudioWaveform;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                formatter,
                "exactly {AUDIO_WAVEFORM_BINS} pairs of signed 16-bit PCM samples"
            )
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut waveform = [[0, 0]; AUDIO_WAVEFORM_BINS];
            for (index, pair) in waveform.iter_mut().enumerate() {
                *pair = sequence
                    .next_element()?
                    .ok_or_else(|| serde::de::Error::invalid_length(index, &self))?;
            }
            if sequence.next_element::<[i16; 2]>()?.is_some() {
                return Err(serde::de::Error::invalid_length(
                    AUDIO_WAVEFORM_BINS + 1,
                    &self,
                ));
            }
            Ok(waveform)
        }
    }

    deserializer.deserialize_seq(WaveformVisitor)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandReply {
    pub ok: bool,
    pub state: StateKind,
    pub message: Option<String>,
    pub stage: Option<Stage>,
    pub outcome: Option<TerminalOutcome>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OperationKind {
    Dictation,
    Recovery,
    Replay,
    Forget,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub epoch: String,
    pub operation_id: Option<String>,
    pub operation_kind: Option<OperationKind>,
    pub state: StateKind,
    pub elapsed: u64,
    pub signal: Option<AudioSignal>,
    pub stage: Option<Stage>,
    pub outcome: Option<TerminalOutcome>,
    pub notice: Option<InteractionNotice>,
    pub pending_recordings: usize,
    pub capabilities: Capabilities,
    pub hud: HudConfig,
}

impl StatusSnapshot {
    pub fn state_name(&self) -> &str {
        self.state.as_str()
    }
}

/// Submit a mutation. `ok` acknowledges acceptance, not future delivery.
pub fn command(command: Command) -> Result<CommandReply> {
    let request = serde_json::to_string(&command).context("encoding daemon command")?;
    exchange(&request)
}

/// Read the complete cached status independently of mutation acknowledgements.
pub fn status() -> Result<StatusSnapshot> {
    exchange("status")
}

/// Refresh canonical per-take metadata on a dedicated reader, without transcript content.
pub fn recordings() -> Result<Vec<Take>> {
    exchange("recordings")
}

fn exchange<T: DeserializeOwned>(request: &str) -> Result<T> {
    anyhow::ensure!(
        request.len() < REQUEST_LIMIT,
        "daemon request exceeds size limit"
    );
    let socket = paths::socket_path().context("locating daemon socket")?;
    let mut stream = connect(&socket).with_context(|| {
        format!(
            "cannot connect to cantrip daemon at {}; start it with: cantrip daemon",
            socket.display()
        )
    })?;
    stream
        .set_write_timeout(Some(REQUEST_TIMEOUT))
        .context("setting daemon request timeout")?;
    writeln!(stream, "{request}").context("sending daemon request")?;
    let line = read_reply(&mut stream, Instant::now() + REQUEST_TIMEOUT)?;
    match serde_json::from_slice(&line) {
        Ok(reply) => Ok(reply),
        Err(error) => {
            if let Ok(CommandReply {
                ok: false,
                message: Some(message),
                ..
            }) = serde_json::from_slice(&line)
            {
                anyhow::bail!("{message}");
            }
            Err(error).context("parsing daemon reply")
        }
    }
}

/// A full Unix listen backlog must not hang a command before its read deadline.
fn connect(path: &Path) -> Result<UnixStream> {
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let bytes = path.as_os_str().as_bytes();
    anyhow::ensure!(
        bytes.len() < address.sun_path.len() && !bytes.contains(&0),
        "invalid daemon socket path"
    );
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (slot, byte) in address.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("creating daemon connection");
    }
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    let result = unsafe {
        libc::connect(
            fd,
            (&address as *const libc::sockaddr_un).cast(),
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("connecting to daemon socket");
    }
    stream
        .set_nonblocking(false)
        .context("configuring daemon connection")?;
    Ok(stream)
}

fn read_reply(stream: &mut UnixStream, deadline: Instant) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    let mut buffer = [0_u8; 8_192];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        anyhow::ensure!(!remaining.is_zero(), "daemon reply timed out");
        stream
            .set_read_timeout(Some(remaining))
            .context("setting daemon reply timeout")?;
        let bytes = match stream.read(&mut buffer) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result.context("reading daemon reply")?,
        };
        anyhow::ensure!(
            bytes != 0,
            "daemon closed the socket before a complete reply"
        );
        let end = buffer[..bytes].iter().position(|byte| *byte == b'\n');
        let content = &buffer[..end.unwrap_or(bytes)];
        anyhow::ensure!(
            line.len() + content.len() <= REPLY_LIMIT,
            "daemon reply exceeds size limit"
        );
        line.extend_from_slice(content);
        if end.is_some() {
            return Ok(line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(completeness: Completeness, delivery: Delivery) -> TerminalOutcome {
        TerminalOutcome {
            event_id: 7,
            operation_id: Some("epoch-2".to_owned()),
            message: "Result".to_owned(),
            completeness,
            delivery,
            cleanup: Cleanup::Off,
            error: None,
            artifacts: Artifacts::default(),
            dismissed: false,
        }
    }

    #[test]
    fn explicit_recovery_identity_cannot_be_lost_at_the_wire_boundary() {
        let request = r#"{"command":"recover","id":"chosen-take","local":true,"clipboard":true}"#;
        assert_eq!(
            Request::parse(request),
            Some(Request::Command(Command::Recover {
                id: Some("chosen-take".to_owned()),
                local: true,
                clipboard: true,
            }))
        );
        assert_eq!(Request::parse("status"), Some(Request::Status));
        assert_eq!(Request::parse("recordings"), Some(Request::Recordings));
        assert!(Request::parse(r#"{"command":"status"}"#).is_none());
        assert!(Request::parse(r#"{"command":"copy","id":"one","unexpected":"two"}"#).is_none());
    }

    #[test]
    fn partial_copy_and_uncertain_delivery_are_not_success() {
        let partial = outcome(Completeness::Partial, Delivery::Copied);
        assert!(!partial.is_success());
        assert!(partial.needs_attention());
        let uncertain = outcome(Completeness::Complete, Delivery::Uncertain);
        assert!(!uncertain.is_success());
        assert!(uncertain.needs_attention());
        let cancelled = outcome(Completeness::Cancelled, Delivery::Cancelled);
        assert!(!cancelled.is_success());
        assert!(!cancelled.needs_attention());
        assert!(!outcome(Completeness::Empty, Delivery::None).needs_attention());
    }

    #[test]
    fn dismissal_hides_attention_without_rewriting_delivery_or_artifacts() {
        let mut result = outcome(Completeness::Partial, Delivery::Copied);
        result.artifacts = Artifacts {
            take_id: Some("take".to_owned()),
            audio: true,
            text: true,
        };
        result.dismissed = true;
        assert!(!result.needs_attention());
        assert!(!result.is_success());
        assert!(result.artifacts.audio && result.artifacts.text);
        let mut cleaned = outcome(Completeness::Complete, Delivery::Pasted);
        cleaned.cleanup = Cleanup::Failed;
        cleaned.error = Some("cleanup-failed".to_owned());
        assert!(cleaned.is_success());
        assert!(!cleaned.needs_attention());
    }

    #[test]
    fn future_state_remains_unknown_not_idle() {
        let state: StateKind = serde_json::from_str(r#""calibrating""#).unwrap();
        assert_eq!(state, StateKind::Unknown("calibrating".to_owned()));
        assert_eq!(serde_json::to_value(state).unwrap(), "calibrating");
    }

    #[test]
    fn incomplete_audio_signal_is_rejected() {
        assert!(serde_json::from_str::<AudioSignal>(r#"{"level":72,"silent":false}"#).is_err());
    }

    #[test]
    fn audio_signal_wire_preserves_full_range_pcm_pairs() {
        let waveform = std::array::from_fn(|index| {
            let index = i16::try_from(index).expect("test bucket fits i16");
            [i16::MIN + index, i16::MAX - index]
        });
        let signal = AudioSignal {
            level: 100,
            silent: false,
            waveform,
        };
        let wire = serde_json::json!({
            "level": 100,
            "silent": false,
            "waveform": waveform.as_slice(),
        });
        assert_eq!(wire["waveform"].as_array().unwrap().len(), 60);
        assert_eq!(serde_json::to_value(signal).unwrap(), wire);
        assert_eq!(serde_json::from_value::<AudioSignal>(wire).unwrap(), signal);
    }

    #[test]
    fn audio_signal_rejects_wrong_waveform_lengths() {
        for length in [59, 61] {
            let wire = serde_json::json!({
                "level": 0,
                "silent": true,
                "waveform": vec![[0, 0]; length],
            });
            assert!(
                serde_json::from_value::<AudioSignal>(wire).is_err(),
                "a waveform with {length} pairs must be rejected"
            );
        }
    }

    #[test]
    fn audio_signal_rejects_malformed_or_out_of_range_pairs() {
        for invalid_pair in [
            serde_json::json!([0]),
            serde_json::json!([0, 0, 0]),
            serde_json::json!([-32_769, 0]),
            serde_json::json!([0, 32_768]),
        ] {
            let mut waveform = vec![serde_json::json!([0, 0]); 60];
            waveform[59] = invalid_pair;
            let wire = serde_json::json!({
                "level": 0,
                "silent": true,
                "waveform": waveform,
            });
            assert!(serde_json::from_value::<AudioSignal>(wire).is_err());
        }
    }

    #[test]
    fn reply_reader_rejects_unterminated_reply() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        server.write_all(b"{}").unwrap();
        drop(server);
        assert!(read_reply(&mut client, Instant::now() + Duration::from_secs(1)).is_err());
    }
}
