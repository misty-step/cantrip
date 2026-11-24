//! Every boundary between Cantrip's platform-free core and an operating system.
//!
//! The core owns *policy* (when to deliver, what to restore, what to keep); a
//! platform crate module supplies *mechanism* by implementing these traits. Each
//! trait is small, synchronous and object-safe, so a fake in a test and a real
//! macOS or Linux backend are interchangeable. Implementations must never log
//! transcript text, audio, clipboard or field contents.

use std::path::{Path, PathBuf};
use std::time::Duration;

// ───────────────────────────── focus & delivery ─────────────────────────────

/// Identity of one running application *instance*. Two processes of the same
/// program, or a recycled PID, never compare equal: `instance` must include
/// something that survives PID reuse (process start time on Linux, launch date
/// on macOS).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AppIdentity {
    /// Stable program identifier for logs and the copy-only list
    /// (bundle id on macOS; WM_CLASS / app_id / exe name on Linux).
    pub app_id: String,
    pub pid: u32,
    /// Opaque instance discriminator (e.g. "<pid>:<starttime>").
    pub instance: String,
}

/// Opaque handle to one top-level window, comparable for equality.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WindowRef(pub String);

/// Opaque handle to one editable field, interpreted only by the backend that
/// produced it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FieldRef(pub String);

/// What was focused when the take started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub app: AppIdentity,
    pub window: WindowRef,
    /// `None` when the OS does not expose the focused field.
    pub field: Option<FieldRef>,
}

/// The focused app and window right now, as reported by the OS.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FocusNow {
    pub app: AppIdentity,
    pub window: WindowRef,
}

/// Content and selection of a text field, in Unicode scalar values (chars).
/// `selection` is a half-open char range; a caret is an empty range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldState {
    pub text: String,
    pub selection: (usize, usize),
    /// True when the backend knows `text` is the complete value (not truncated).
    pub complete: bool,
}

/// How a field answers a support query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FieldSupport {
    /// The field accepts direct (tier 1) insertion.
    DirectInsert,
    /// The field exists but reports direct insertion unsupported.
    NoDirectInsert,
    /// The field no longer answers (destroyed, app hung, query failed).
    Gone,
}

/// Result of a direct (tier 1) insertion attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InsertResult {
    /// The OS accepted the write.
    Done,
    /// The OS refused *before any change* was made; tier 2 may be tried.
    Refused,
    /// An error after which a change may or may not have happened.
    Ambiguous,
}

/// Result of posting a tier-2 paste.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PasteResult {
    /// One paste was posted to the captured app.
    Posted,
    /// This session cannot address a paste to that app; nothing was sent.
    Unaddressable,
    /// Posting failed before anything was sent.
    Failed,
}

/// Focus, field and keystroke mechanism for one desktop session.
pub trait Desktop: Send + Sync {
    /// Capture the focused app, window and (where exposed) field. May block;
    /// the core enforces the 250 ms deadline on a worker thread and ignores late
    /// answers. `None` means focus could not be identified.
    fn capture_target(&self) -> Option<Target>;
    /// The focused app and window now; `None` when it cannot be checked.
    fn focus_now(&self) -> Option<FocusNow>;
    /// Whether this app instance is still running (same instance, not PID reuse).
    fn app_alive(&self, app: &AppIdentity) -> bool;
    /// Missing delivery permission, named for the HUD (e.g. "Accessibility"),
    /// or `None` when delivery is permitted.
    fn missing_delivery_permission(&self) -> Option<String>;
    /// Monotonic counter that changes whenever the machine slept, the screen
    /// locked or the user session switched.
    fn session_epoch(&self) -> u64;
    /// Ask whether the captured field takes direct insertion.
    fn field_support(&self, target: &Target) -> FieldSupport;
    /// Read the field's value and selection, `None` if unreadable.
    fn read_field(&self, target: &Target) -> Option<FieldState>;
    /// Tier 1: replace the captured field's selection with `text`. Must never
    /// touch the clipboard or send key events.
    fn insert_text(&self, target: &Target, text: &str) -> InsertResult;
    /// Whether this session can address a paste to `app` without routing it by
    /// keyboard focus.
    fn can_address_paste(&self, app: &AppIdentity) -> bool;
    /// Tier 2: post exactly one paste addressed to the captured app.
    fn post_paste(&self, target: &Target) -> PasteResult;
}

/// Opaque proof of one clipboard write by Cantrip. `unchanged_since` answers by
/// identity (ownership / change count), never by comparing text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipToken(pub u64);

/// Every item and type on the clipboard at snapshot time.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClipSnapshot {
    /// (mime-or-UTI type, bytes) per item; multiple items allowed.
    pub items: Vec<Vec<(String, Vec<u8>)>>,
}

pub trait Clipboard: Send + Sync {
    /// Snapshot every item type; `None` if access is denied or unreadable.
    fn snapshot(&self) -> Option<ClipSnapshot>;
    /// Place `text` (marked transient/concealed where the OS supports it).
    fn write_text(&self, text: &str, transient: bool) -> Result<ClipToken, String>;
    /// True only while nothing replaced Cantrip's write identified by `token`.
    fn unchanged_since(&self, token: &ClipToken) -> bool;
    /// Put `snapshot` back. Called by the core only after it has verified
    /// `unchanged_since`; implementations re-check atomically where possible
    /// and return false if the clipboard changed meanwhile.
    fn restore(&self, snapshot: &ClipSnapshot, token: &ClipToken) -> bool;
}

// ───────────────────────────── audio ─────────────────────────────

/// Why a microphone could not start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MicError {
    /// Permission denied by the OS or the user.
    Blocked,
    /// The chosen device is missing or failed.
    Unavailable(String),
}

/// A running single-track recording writing a 16 kHz mono PCM16 WAV file.
pub trait Recording: Send {
    /// Recent input level 0.0..=1.0 for the HUD meter.
    fn level(&self) -> f32;
    /// Stop and finalize the WAV file; returns its path.
    fn stop(self: Box<Self>) -> Result<PathBuf, String>;
    /// Stop without keeping anything (Escape). The file may be deleted.
    fn cancel(self: Box<Self>);
}

pub trait Recorder: Send + Sync {
    /// Input devices as (stable id, human name).
    fn devices(&self) -> Vec<(String, String)>;
    /// Start recording `device` (`None` = system default) into `wav_path`.
    fn start(&self, device: Option<&str>, wav_path: &Path) -> Result<Box<dyn Recording>, MicError>;
}

/// A running two-track meeting capture. Both tracks are written as 16 kHz mono
/// PCM16 WAV files whose first sample is timestamped on one shared monotonic
/// clock.
pub trait MeetingRecording: Send {
    /// Elapsed time since capture start on the shared clock.
    fn elapsed(&self) -> Duration;
    /// Stop both tracks and finalize their files.
    fn stop(self: Box<Self>) -> Result<MeetingTracks, String>;
}

/// Finalized meeting audio.
#[derive(Clone, Debug, PartialEq)]
pub struct MeetingTracks {
    pub you: PathBuf,
    pub others: PathBuf,
    /// Offset of each track's first sample from the shared clock origin.
    pub you_offset_ms: u64,
    pub others_offset_ms: u64,
}

pub trait MeetingCapture: Send + Sync {
    /// Start capturing microphone (`You`) and system output (`Others`) into the
    /// given paths. Errors are user-facing messages.
    fn start(
        &self,
        device: Option<&str>,
        you_path: &Path,
        others_path: &Path,
    ) -> Result<Box<dyn MeetingRecording>, String>;
}

// ───────────────────────────── local recognition ─────────────────────────────

/// Whether guaranteed on-device English recognition can run here, now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalAvailability {
    /// Authorized, installed, and guaranteed to stay on the device.
    Ready,
    /// The OS permission was denied or not granted.
    NotAuthorized,
    /// No on-device English recognizer exists or is installed here.
    NoOnDeviceEnglish,
}

pub trait LocalRecognizer: Send + Sync {
    /// Re-checked at the start of every attempt, never cached or relaxed.
    /// May request the OS permission (only ever called for a finished no-key /
    /// no-network dictation).
    fn availability(&self) -> LocalAvailability;
    /// Recognize with the strict on-device-only flag set. Must fail rather than
    /// send audio anywhere.
    fn recognize_on_device(&self, wav: &Path) -> Result<String, String>;
}

// ───────────────────────────── secrets, login, network ─────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SecretError {
    /// The secret store could not be read; this is an error, never "no key".
    Unavailable(String),
}

pub trait SecretStore: Send + Sync {
    /// `Ok(None)` only when the store answered that no key exists.
    fn get_key(&self) -> Result<Option<String>, SecretError>;
    fn set_key(&self, key: &str) -> Result<(), SecretError>;
    fn delete_key(&self) -> Result<(), SecretError>;
}

pub trait LoginItem: Send + Sync {
    fn enabled(&self) -> bool;
    fn set_enabled(&self, on: bool) -> Result<(), String>;
}

/// HTTPS transport to OpenRouter, implemented by the app crate (ureq).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpReply {
    pub status: u16,
    pub body: String,
    pub retry_after: Option<Duration>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransportError {
    /// No connection could be made at all (DNS failure, unreachable, refused):
    /// nothing was sent.
    Offline(String),
    /// The request may have been sent; it failed or timed out afterwards.
    Failed(String),
    /// The per-request timeout elapsed.
    Timeout,
}

pub trait Transport: Send + Sync {
    /// POST `body` as JSON to `url` with a bearer key. `timeout` bounds the
    /// whole exchange when given; `None` waits for a response or real failure.
    fn post_json(
        &self,
        url: &str,
        key: &str,
        body: &serde_json::Value,
        timeout: Option<Duration>,
    ) -> Result<HttpReply, TransportError>;
}
