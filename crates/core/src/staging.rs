//! Owner-only, reboot-durable private storage (never a purgeable temp dir) and
//! durable file helpers. Holds dictation recovery items; meetings keep their
//! own subtree under [`Staging::meetings_dir`].
//!
//! OWNER: core-dictation fork (dictation items + helpers). The meeting fork
//! uses the helpers and `meetings_dir` only.

use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};

/// Write `bytes` to `path` durably, replacing it: temp file in the same dir,
/// fsync, rename, fsync dir. Only for Cantrip's private files.
pub fn durable_replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let _ = (path, bytes);
    todo!()
}

/// fsync a directory.
pub fn fsync_dir(dir: &Path) -> io::Result<()> {
    let _ = dir;
    todo!()
}

/// Create `path` (and parents) with mode 0700; tighten an existing one.
pub fn ensure_private_dir(path: &Path) -> io::Result<()> {
    let _ = path;
    todo!()
}

/// Lifecycle of one staged dictation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DictationState {
    /// Audio is being captured (found at launch → Interrupted).
    Recording,
    /// Stopped and waiting for transcription / delivery.
    Processing,
    /// Transcription failed, returned nothing, or local recognition refused.
    Failed { reason: String },
    /// Even the clipboard write failed.
    NotDelivered,
    /// The app stopped mid-take or mid-processing.
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DictationItem {
    pub id: String,
    /// Unix seconds at take start.
    pub started_unix: u64,
    pub state: DictationState,
}

/// The private data root: `<root>/dictations/<id>/{audio.wav,item.json}`,
/// `<root>/meetings/…`.
#[derive(Clone, Debug)]
pub struct Staging {
    root: PathBuf,
}

impl Staging {
    /// Open (creating owner-only) the staging root. Items found in
    /// `Recording`/`Processing` become `Interrupted`.
    pub fn open(root: &Path) -> io::Result<Staging> {
        let _ = root;
        todo!()
    }

    /// Default per-user root: `$XDG_DATA_HOME/cantrip/v2` (Linux),
    /// `~/Library/Application Support/Cantrip/v2` (macOS).
    pub fn default_root() -> PathBuf {
        todo!()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn meetings_dir(&self) -> PathBuf {
        self.root.join("meetings")
    }

    /// Create a new dictation item in `Recording`; returns (item, wav path).
    pub fn new_dictation(&self, started_unix: u64) -> io::Result<(DictationItem, PathBuf)> {
        let _ = started_unix;
        todo!()
    }

    pub fn dictation_audio(&self, id: &str) -> PathBuf {
        self.root.join("dictations").join(id).join("audio.wav")
    }

    /// Durably record a new state.
    pub fn set_dictation_state(&self, id: &str, state: DictationState) -> io::Result<()> {
        let _ = (id, state);
        todo!()
    }

    /// Delete the item and its audio.
    pub fn delete_dictation(&self, id: &str) -> io::Result<()> {
        let _ = id;
        todo!()
    }

    /// Items needing recovery (Failed, NotDelivered, Interrupted), newest first.
    pub fn recoverable_dictations(&self) -> Vec<DictationItem> {
        todo!()
    }
}
