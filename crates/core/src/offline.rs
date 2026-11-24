//! The fail-closed no-key / no-network gate (DESIGN.md › Local recognition
//! fails closed). Entered only for a finished dictation with no key or no
//! connection, never for meetings. Re-checks availability every time; on any
//! refusal no audio leaves the machine and the recording stays staged.

use crate::ports::{LocalAvailability, LocalRecognizer};
use std::path::Path;

pub const FIX_ALLOW: &str = "Allow speech recognition for Cantrip, or add an OpenRouter key";
pub const FIX_NO_ON_DEVICE: &str = "No on-device English speech here. Add an OpenRouter key";

/// Run on-device recognition or refuse with the HUD fix text
/// (shown as `Not transcribed. <fix>`).
pub fn recognize_locally(local: &dyn LocalRecognizer, wav: &Path) -> Result<String, String> {
    match local.availability() {
        LocalAvailability::NotAuthorized => Err(FIX_ALLOW.to_string()),
        LocalAvailability::NoOnDeviceEnglish => Err(FIX_NO_ON_DEVICE.to_string()),
        LocalAvailability::Ready => match local.recognize_on_device(wav) {
            Ok(text) if !text.trim().is_empty() => Ok(text.trim().to_string()),
            _ => Err(FIX_NO_ON_DEVICE.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Local {
        avail: LocalAvailability,
        result: Result<String, String>,
        checks: AtomicUsize,
        runs: AtomicUsize,
    }

    impl LocalRecognizer for Local {
        fn availability(&self) -> LocalAvailability {
            self.checks.fetch_add(1, Ordering::SeqCst);
            self.avail.clone()
        }
        fn recognize_on_device(&self, _wav: &Path) -> Result<String, String> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            self.result.clone()
        }
    }

    fn local(avail: LocalAvailability, result: Result<String, String>) -> Local {
        Local {
            avail,
            result,
            checks: AtomicUsize::new(0),
            runs: AtomicUsize::new(0),
        }
    }

    #[test]
    fn refusals_never_run_recognition_and_name_the_fix() {
        let l = local(LocalAvailability::NotAuthorized, Ok("x".into()));
        assert_eq!(
            recognize_locally(&l, Path::new("a.wav")),
            Err(FIX_ALLOW.into())
        );
        let l2 = local(LocalAvailability::NoOnDeviceEnglish, Ok("x".into()));
        assert_eq!(
            recognize_locally(&l2, Path::new("a.wav")),
            Err(FIX_NO_ON_DEVICE.into())
        );
        assert_eq!(
            l.runs.load(Ordering::SeqCst) + l2.runs.load(Ordering::SeqCst),
            0
        );
    }

    #[test]
    fn availability_is_rechecked_every_attempt() {
        let l = local(LocalAvailability::Ready, Ok(" hello ".into()));
        assert_eq!(
            recognize_locally(&l, Path::new("a.wav")),
            Ok("hello".into())
        );
        assert_eq!(
            recognize_locally(&l, Path::new("a.wav")),
            Ok("hello".into())
        );
        assert_eq!(l.checks.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn empty_or_failed_on_device_result_refuses() {
        let l = local(LocalAvailability::Ready, Ok("  ".into()));
        assert!(recognize_locally(&l, Path::new("a.wav")).is_err());
        let l = local(LocalAvailability::Ready, Err("boom".into()));
        assert_eq!(
            recognize_locally(&l, Path::new("a.wav")),
            Err(FIX_NO_ON_DEVICE.into())
        );
    }
}
