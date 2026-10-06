//! Native mechanisms supplied by the desktop host, never a second workflow.

use crate::audio::InputSignal;
use crate::delivery::{ClipboardFallback, InjectionFailure, InjectionMode, InjectionOutcome};
use anyhow::Result;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

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
}
