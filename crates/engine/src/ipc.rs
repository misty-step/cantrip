//! Typed command acknowledgements and identity-aware daemon snapshots.

pub use crate::audio::AUDIO_WAVEFORM_BINS;
use crate::config::HudConfig;
use crate::paths;
use crate::pipeline::Stage;
use crate::recovery::Take;
use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Deserializer, Serialize, Serializer};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
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
        #[serde(default)]
        handoff: Option<String>,
    },
    Start {
        postproc: Option<bool>,
        #[serde(default)]
        handoff: Option<String>,
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

impl Command {
    /// Log class: the command and, for recording starts, its handoff target name.
    /// Target names are operator configuration, never transcript text; a malformed
    /// client-supplied name is logged as `invalid`, not echoed.
    pub(crate) fn class(&self) -> String {
        let with_handoff = |name: &str, handoff: &Option<String>| match handoff.as_deref() {
            None => format!("{name} handoff=none"),
            Some(target) if crate::config::valid_handoff_name(target) => {
                format!("{name} handoff={target}")
            }
            Some(_) => format!("{name} handoff=invalid"),
        };
        match self {
            Self::Toggle { handoff, .. } => with_handoff("toggle", handoff),
            Self::Start { handoff, .. } => with_handoff("start", handoff),
            Self::Stop => "stop".to_owned(),
            Self::Cancel => "cancel".to_owned(),
            Self::Last => "last".to_owned(),
            Self::Recover { .. } => "recover".to_owned(),
            Self::Copy { .. } => "copy".to_owned(),
            Self::Dismiss { .. } => "dismiss".to_owned(),
            Self::Forget { .. } => "forget".to_owned(),
            Self::Ping => "ping".to_owned(),
            Self::Reload => "reload".to_owned(),
        }
    }
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
    HandedOff,
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
    /// The non-default target this outcome's take was sent toward; None for the default.
    #[serde(default)]
    pub handoff: Option<Handoff>,
}

/// A take headed to a named handoff target instead of the default desktop delivery.
/// Fixed when the take starts; clients show `label` in `color` and never the default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handoff {
    pub name: String,
    pub label: String,
    /// `#rrggbb` from the active theme when the take started.
    #[serde(serialize_with = "serialize_rgb", deserialize_with = "deserialize_rgb")]
    pub color: [u8; 3],
}

fn serialize_rgb<S: Serializer>(
    [r, g, b]: &[u8; 3],
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.collect_str(&format_args!("#{r:02x}{g:02x}{b:02x}"))
}

fn deserialize_rgb<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<[u8; 3], D::Error> {
    struct RgbVisitor;

    impl<'de> serde::de::Visitor<'de> for RgbVisitor {
        type Value = [u8; 3];

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("#rrggbb")
        }

        fn visit_str<E: serde::de::Error>(self, text: &str) -> std::result::Result<Self::Value, E> {
            parse_rgb(text).ok_or_else(|| E::custom("expected #rrggbb"))
        }
    }

    deserializer.deserialize_str(RgbVisitor)
}

fn parse_rgb(text: &str) -> Option<[u8; 3]> {
    let digits = text.strip_prefix('#')?;
    if digits.len() != 6 || !digits.is_ascii() {
        return None;
    }
    Some([
        u8::from_str_radix(&digits[0..2], 16).ok()?,
        u8::from_str_radix(&digits[2..4], 16).ok()?,
        u8::from_str_radix(&digits[4..6], 16).ok()?,
    ])
}

impl TerminalOutcome {
    /// A complete transcript reached a delivery helper; this is not an app receipt.
    pub fn is_success(&self) -> bool {
        self.completeness == Completeness::Complete
            && matches!(
                self.delivery,
                Delivery::Typed | Delivery::Pasted | Delivery::Copied | Delivery::HandedOff
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
    /// The latest outcome needs the operator (`TerminalOutcome::needs_attention`);
    /// clears when the next take replaces it or it is dismissed.
    #[serde(default)]
    pub attention: bool,
    pub capabilities: Capabilities,
    pub hud: HudConfig,
    /// The active take's non-default target; None for the default flow and when idle.
    #[serde(default)]
    pub handoff: Option<Handoff>,
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
pub(crate) fn connect(path: &Path) -> Result<UnixStream> {
    #[cfg(target_os = "macos")]
    let deadline = Instant::now() + REQUEST_TIMEOUT;
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
    #[cfg(target_os = "linux")]
    let address_length = std::mem::size_of::<libc::sockaddr_un>();
    #[cfg(target_os = "macos")]
    let address_length = {
        let length = std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
        address.sun_len = length as u8;
        length
    };
    #[cfg(target_os = "linux")]
    let socket_type = libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK;
    #[cfg(target_os = "macos")]
    let socket_type = libc::SOCK_STREAM;
    let fd = unsafe { libc::socket(libc::AF_UNIX, socket_type, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("creating daemon connection");
    }
    // SAFETY: socket returned a new descriptor, now owned by the stream even
    // when subsequent Darwin descriptor/socket configuration fails.
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    #[cfg(target_os = "macos")]
    {
        // Darwin does not accept Linux's socket creation flags. Set both
        // before connecting, and suppress SIGPIPE just as native std sockets do.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(std::io::Error::last_os_error()).context("privatizing daemon connection");
        }
        stream
            .set_nonblocking(true)
            .context("configuring non-blocking daemon connection")?;
        let enabled: libc::c_int = 1;
        if unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                (&enabled as *const libc::c_int).cast(),
                std::mem::size_of_val(&enabled) as libc::socklen_t,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error())
                .context("configuring daemon socket writes");
        }
    }
    let result = unsafe {
        libc::connect(
            stream.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            address_length as libc::socklen_t,
        )
    };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        #[cfg(target_os = "linux")]
        return Err(error).context("connecting to daemon socket");
        #[cfg(target_os = "macos")]
        {
            if matches!(
                error.raw_os_error(),
                Some(libc::EINPROGRESS | libc::EALREADY | libc::EINTR)
            ) {
                complete_connect(stream.as_raw_fd(), deadline)?;
            } else {
                return Err(error).context("connecting to daemon socket");
            }
        }
    }
    stream
        .set_nonblocking(false)
        .context("configuring daemon connection")?;
    Ok(stream)
}

#[cfg(target_os = "macos")]
fn complete_connect(fd: libc::c_int, deadline: Instant) -> Result<()> {
    let mut descriptor = libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    };
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        anyhow::ensure!(!remaining.is_zero(), "daemon connection timed out");
        let milliseconds =
            remaining.as_millis().max(1).min(libc::c_int::MAX as u128) as libc::c_int;
        let result = unsafe { libc::poll(&mut descriptor, 1, milliseconds) };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("waiting for daemon connection");
        }
        if result == 0 {
            continue;
        }
        let mut error: libc::c_int = 0;
        let mut length = std::mem::size_of_val(&error) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&mut error as *mut libc::c_int).cast(),
                &mut length,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error()).context("checking daemon connection");
        }
        if error != 0 {
            return Err(std::io::Error::from_raw_os_error(error))
                .context("connecting to daemon socket");
        }
        return Ok(());
    }
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
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SOCKET_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct SocketDirectory(PathBuf);

    impl SocketDirectory {
        fn new() -> Self {
            #[cfg(target_os = "macos")]
            let base = PathBuf::from("/private/tmp");
            #[cfg(target_os = "linux")]
            let base = std::env::temp_dir().canonicalize().unwrap();
            let path = base.join(format!(
                "ct-ipc-{}-{}",
                std::process::id(),
                SOCKET_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .unwrap();
            Self(path)
        }

        fn socket(&self) -> PathBuf {
            self.0.join("socket")
        }
    }

    impl Drop for SocketDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

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
            handoff: None,
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

    #[test]
    fn native_connection_is_close_on_exec_and_supports_bounded_request_io() {
        let directory = SocketDirectory::new();
        let socket = directory.socket();
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut client = connect(&socket).unwrap();
        let descriptor_flags = unsafe { libc::fcntl(client.as_raw_fd(), libc::F_GETFD) };
        assert!(descriptor_flags >= 0);
        assert_ne!(descriptor_flags & libc::FD_CLOEXEC, 0);
        let status_flags = unsafe { libc::fcntl(client.as_raw_fd(), libc::F_GETFL) };
        assert!(status_flags >= 0);
        assert_eq!(status_flags & libc::O_NONBLOCK, 0);
        #[cfg(target_os = "macos")]
        {
            let mut enabled: libc::c_int = 0;
            let mut length = std::mem::size_of_val(&enabled) as libc::socklen_t;
            assert_eq!(
                unsafe {
                    libc::getsockopt(
                        client.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_NOSIGPIPE,
                        (&mut enabled as *mut libc::c_int).cast(),
                        &mut length,
                    )
                },
                0
            );
            assert_eq!(enabled, 1);
        }
        client
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        client.write_all(b"status\n").unwrap();
        let (mut server, _) = listener.accept().unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut request = [0; 7];
        server.read_exact(&mut request).unwrap();
        assert_eq!(&request, b"status\n");
        server.write_all(b"{\"state\":\"idle\"}\n").unwrap();
        assert_eq!(
            read_reply(&mut client, Instant::now() + Duration::from_secs(1)).unwrap(),
            b"{\"state\":\"idle\"}"
        );
    }

    #[test]
    fn a_full_listen_backlog_cannot_block_connection_without_a_deadline() {
        let directory = SocketDirectory::new();
        let socket = directory.socket();
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 1) }, 0);
        let started = Instant::now();
        let mut queued = Vec::new();
        let mut full = false;
        for _ in 0..64 {
            match connect(&socket) {
                Ok(stream) => queued.push(stream),
                Err(_) => {
                    full = true;
                    break;
                }
            }
        }
        assert!(
            !queued.is_empty(),
            "the real socket must first accept a connection"
        );
        assert!(full, "the reduced backlog must reach capacity");
        assert!(started.elapsed() < REQUEST_TIMEOUT + Duration::from_secs(2));
        let (_accepted, _) = listener.accept().unwrap();
        let _next = connect(&socket).unwrap();
    }

    #[test]
    fn socket_address_capacity_and_nul_are_rejected_before_connect() {
        let address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        let too_long = PathBuf::from("a".repeat(address.sun_path.len()));
        assert!(connect(&too_long)
            .unwrap_err()
            .to_string()
            .contains("invalid daemon socket path"));
        assert!(connect(Path::new("cantrip\0socket"))
            .unwrap_err()
            .to_string()
            .contains("invalid daemon socket path"));
    }

    #[test]
    fn handoff_travels_as_a_hex_color_and_its_absence_is_the_default_flow() {
        let mut value = outcome(Completeness::Complete, Delivery::HandedOff);
        value.handoff = Some(Handoff {
            name: "kaylee".to_owned(),
            label: "Kaylee".to_owned(),
            color: [0xe6, 0x8b, 0x05],
        });
        let json = serde_json::to_value(&value).unwrap();
        // The Omarchy bar widget uses this string directly as a QML color.
        assert_eq!(json["handoff"]["color"], "#e68b05");
        let back: TerminalOutcome = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(back, value);
        let mut old = json.clone();
        old.as_object_mut().unwrap().remove("handoff");
        let old: TerminalOutcome = serde_json::from_value(old).unwrap();
        assert_eq!(old.handoff, None);
        let mut bad = json;
        bad["handoff"]["color"] = "magenta".into();
        assert!(serde_json::from_value::<TerminalOutcome>(bad).is_err());
    }

    #[test]
    fn rgb_wire_parser_accepts_mixed_case_hex_and_rejects_non_ascii_without_panicking() {
        let mut wire = serde_json::json!({
            "name": "handoff",
            "label": "Handoff",
            "color": "#AbCdEf",
        });
        let handoff: Handoff = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(handoff.color, [0xab, 0xcd, 0xef]);
        assert_eq!(serde_json::to_value(handoff).unwrap()["color"], "#abcdef");
        for invalid in [
            "#éffff", "#f🦀f", "#12345", "#1234567", "#12gg00", "123456", " #123456",
        ] {
            wire["color"] = invalid.into();
            assert!(serde_json::from_value::<Handoff>(wire.clone()).is_err());
        }
    }

    #[test]
    fn handoff_commands_roundtrip_and_old_clients_default_to_none() {
        for command in [
            Command::Toggle {
                postproc: Some(true),
                handoff: Some("pepper".into()),
            },
            Command::Start {
                postproc: None,
                handoff: Some("pepper".into()),
            },
        ] {
            let json = serde_json::to_string(&command).unwrap();
            assert_eq!(serde_json::from_str::<Command>(&json).unwrap(), command);
        }
        assert_eq!(
            serde_json::from_str::<Command>(r#"{"command":"toggle","postproc":null}"#).unwrap(),
            Command::Toggle {
                postproc: None,
                handoff: None
            }
        );
        assert_eq!(
            serde_json::from_str::<Command>(r#"{"command":"start","postproc":false}"#).unwrap(),
            Command::Start {
                postproc: Some(false),
                handoff: None
            }
        );
        assert_eq!(
            serde_json::to_string(&Delivery::HandedOff).unwrap(),
            "\"handed-off\""
        );
        assert!(outcome(Completeness::Complete, Delivery::HandedOff).is_success());
    }
}
