//! Native mechanisms supplied by the desktop host, never a second workflow.

use crate::audio::InputSignal;
use crate::delivery::{ClipboardFallback, InjectionFailure, InjectionMode, InjectionOutcome};
use crate::ipc::{MicrophoneAuthorization, MicrophoneGrantee};
use anyhow::Result;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// A selected microphone is absent. Safe to show in command/status feedback;
/// other native errors remain private and use the generic capture failure.
#[derive(Debug)]
pub struct MissingMicrophone(pub String);

impl std::fmt::Display for MissingMicrophone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "configured microphone `{}` not found — reconnect it or select an input in Settings",
            self.0
        )
    }
}

impl std::error::Error for MissingMicrophone {}

/// This capture owner has no microphone authorization to request: Linux
/// records without one, and macOS asks only for an app bundle that declares
/// its microphone purpose. Safe to show in command feedback.
#[derive(Debug)]
pub struct MicrophoneAccessUnsupported(pub &'static str);

impl std::fmt::Display for MicrophoneAccessUnsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for MicrophoneAccessUnsupported {}

/// An owned live capture. Stop transfers its finalized WAV; failures retain the
/// original path for the engine's durability/recovery path.
pub trait Recorder: Send {
    fn input_signal(&mut self) -> Option<InputSignal>;
    fn request_stop(&mut self) -> Result<()>;
    fn stop(self: Box<Self>) -> Result<PathBuf>;
}

/// Stop-time destination/session history and its native delivery mechanism.
/// Implementations must defer unknown or interrupted history and never retry
/// after an external handoff may have occurred. Clipboard mode needs no focus
/// permit; strict Type must never read or write the clipboard.
pub trait DeliveryPermit: Send + Sync {
    fn clipboard_fallback(&self) -> ClipboardFallback;
    fn deliver(
        &self,
        text: &str,
        mode: InjectionMode,
        cancel: &AtomicBool,
    ) -> Result<InjectionOutcome, InjectionFailure>;
}

/// The selected desktop integration. The engine owns policy, identities,
/// cancellation and durable state; a platform supplies only native mechanisms.
pub trait Platform: Send + Sync {
    /// Failure must quiesce native producers and finalize any WAV prefix before
    /// returning; the engine durably retains remaining audio. Remove only
    /// conclusively empty, unaccepted startup artifacts.
    fn start_recording(&self, wav: &Path, source: Option<&str>) -> Result<Box<dyn Recorder>>;
    fn prepare_delivery(&self);
    fn delivery_permit(&self) -> Arc<dyn DeliveryPermit>;
    fn handoff_color(&self, slot: usize) -> [u8; 3];
    /// Operational process identity only: no arguments, titles or transcript.
    fn sender_identity(&self, stream: &UnixStream) -> String;
    /// The microphone authorization the OS reports to this process, which is
    /// the capture owner; `Requesting` while its explicit request awaits the
    /// operator. The default, None, means capture needs no per-app authorization.
    fn microphone_authorization(&self) -> Option<MicrophoneAuthorization> {
        None
    }
    /// Who macOS asks and grants for this process: Cantrip itself, or the app
    /// that started it. Fixed for the process lifetime, so the engine reads it
    /// once; None wherever `microphone_authorization` is None.
    fn microphone_grantee(&self) -> Option<MicrophoneGrantee> {
        None
    }
    /// Start the operator's explicit microphone request in this process and
    /// return at once: the host awaits the answer off the engine thread and
    /// reports `Requesting` meanwhile. Only the explicit command calls this,
    /// never start, status or polling. The default has nothing to request.
    fn request_microphone_access(&self) -> Result<MicrophoneAuthorization> {
        Err(
            MicrophoneAccessUnsupported("This desktop has no microphone permission to request.")
                .into(),
        )
    }
}
