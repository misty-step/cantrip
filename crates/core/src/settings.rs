//! The persisted user choices: the six Settings-pane values (the OpenRouter key
//! lives only in the OS secret store, never here) plus the popover's Mode and
//! Post-Processing switch. Nothing else is configurable.

use crate::shortcut::Chord;
use crate::speech::Mode;
use crate::vocabulary;
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Settings (1): the one global shortcut.
    pub shortcut: Chord,
    /// Settings (2): `None` = system default input.
    pub microphone: Option<String>,
    /// Settings (4): one flat list of canonical spellings.
    pub vocabulary: Vec<String>,
    /// Settings (5): `None` = default (a Supercortex `captures/voice`).
    pub meeting_folder: Option<PathBuf>,
    /// Settings (6). Mirrors the OS login item; the OS is the authority.
    pub open_at_login: bool,
    /// Popover (3).
    pub mode: Mode,
    /// Popover (4).
    pub post_processing: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            shortcut: Chord::platform_default(),
            microphone: None,
            vocabulary: vocabulary::seed(),
            meeting_folder: None,
            open_at_login: false,
            mode: Mode::Clean,
            post_processing: true,
        }
    }
}

impl Settings {
    /// Load from `path`; a missing file gives defaults. A corrupt file is an
    /// error (never silently reset the user's choices).
    pub fn load(path: &Path) -> io::Result<Settings> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(e) => Err(e),
        }
    }

    /// Durably replace the settings file (write, fsync, rename, fsync dir).
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(self).expect("settings serialize");
        crate::staging::durable_replace(path, &bytes)
    }

    /// The Meeting folder in effect: the chosen one, else the default
    /// Supercortex `captures/voice` under the home directory.
    pub fn effective_meeting_folder(&self, home: &Path) -> PathBuf {
        self.meeting_folder
            .clone()
            .unwrap_or_else(|| default_meeting_folder(home))
    }
}

/// Default Meeting folder: `~/supercortex/captures/voice` (a Supercortex
/// checkout's `captures/voice/`). Never created by Cantrip.
pub fn default_meeting_folder(home: &Path) -> PathBuf {
    home.join("supercortex").join("captures").join("voice")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_seed_vocabulary_and_clean_with_post_processing() {
        let s = Settings::default();
        assert_eq!(s.vocabulary.len(), 14);
        assert_eq!(s.mode, Mode::Clean);
        assert!(s.post_processing);
        assert!(s.microphone.is_none());
    }

    #[test]
    fn round_trip_and_missing_file_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        assert_eq!(Settings::load(&path).unwrap(), Settings::default());
        let s = Settings {
            mode: Mode::Elevated,
            vocabulary: vec!["Scry".into()],
            ..Default::default()
        };
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path).unwrap(), s);
    }

    #[test]
    fn corrupt_file_is_an_error_not_a_reset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, b"{not json").unwrap();
        assert!(Settings::load(&path).is_err());
    }
}
