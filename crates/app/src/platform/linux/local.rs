//! Local (no-key / no-network) speech recognition on Linux.
//!
//! The contract requires recognition that is authorized, installed and
//! *guaranteed* on-device, with no implicit model download. No Linux desktop
//! ships such a recognizer as part of the OS, and Cantrip v2 bundles none, so
//! this backend always answers `NoOnDeviceEnglish`: the core then refuses,
//! keeps the recording staged and shows `Not transcribed. No on-device English
//! speech here. Add an OpenRouter key`. Nothing is ever sent anywhere.

use cantrip_core::ports::{LocalAvailability, LocalRecognizer};
use std::path::Path;

#[derive(Default)]
pub struct NoLocalRecognizer;

impl LocalRecognizer for NoLocalRecognizer {
    fn availability(&self) -> LocalAvailability {
        LocalAvailability::NoOnDeviceEnglish
    }

    fn recognize_on_device(&self, _wav: &Path) -> Result<String, String> {
        Err("no on-device English recognizer on this Linux session".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_local_recognition_fails_closed() {
        let r = NoLocalRecognizer;
        assert_eq!(r.availability(), LocalAvailability::NoOnDeviceEnglish);
        assert!(r
            .recognize_on_device(Path::new("/nonexistent.wav"))
            .is_err());
    }
}
