//! Owner-only, reboot-durable private storage (never a purgeable temp dir) and
//! durable file helpers. Dictation items live here; meetings keep their own
//! subtree and state under [`Staging::meetings_dir`].

use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Write `bytes` to `path` durably, replacing it: temp file in the same dir,
/// fsync, rename, fsync dir. Only for Cantrip's private files.
pub fn durable_replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(".{name}.{}.tmp", unique_suffix()));
    let result = (|| {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)?;
        fsync_dir(dir)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// fsync a directory.
pub fn fsync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Create `path` (and parents) with mode 0700; tighten an existing one.
pub fn ensure_private_dir(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

/// Process-unique suffix for private file names.
pub fn unique_suffix() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!(
        "{}-{nanos:x}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
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
        ensure_private_dir(root)?;
        let staging = Staging {
            root: root.to_path_buf(),
        };
        ensure_private_dir(&staging.root.join("dictations"))?;
        ensure_private_dir(&staging.meetings_dir())?;
        for item in staging.all_dictations() {
            if matches!(
                item.state,
                DictationState::Recording | DictationState::Processing
            ) {
                staging.set_dictation_state(&item.id, DictationState::Interrupted)?;
            }
        }
        Ok(staging)
    }

    /// Default per-user root: `$XDG_DATA_HOME/cantrip/v2` (Linux),
    /// `~/Library/Application Support/Cantrip/v2` (macOS).
    pub fn default_root() -> PathBuf {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| "/".into());
        if cfg!(target_os = "macos") {
            home.join("Library/Application Support/Cantrip/v2")
        } else {
            std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .unwrap_or_else(|| home.join(".local/share"))
                .join("cantrip/v2")
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn meetings_dir(&self) -> PathBuf {
        self.root.join("meetings")
    }

    /// Create a new dictation item in `Recording`; returns (item, wav path).
    pub fn new_dictation(&self, started_unix: u64) -> io::Result<(DictationItem, PathBuf)> {
        let id = format!("d{started_unix}-{}", unique_suffix());
        let dir = self.root.join("dictations").join(&id);
        ensure_private_dir(&dir)?;
        let item = DictationItem {
            id: id.clone(),
            started_unix,
            state: DictationState::Recording,
        };
        self.write_item(&item)?;
        fsync_dir(&self.root.join("dictations"))?;
        Ok((item, dir.join("audio.wav")))
    }

    pub fn dictation_audio(&self, id: &str) -> PathBuf {
        self.root.join("dictations").join(id).join("audio.wav")
    }

    /// Durably record a new state.
    pub fn set_dictation_state(&self, id: &str, state: DictationState) -> io::Result<()> {
        let mut item = self.read_item(id)?;
        item.state = state;
        self.write_item(&item)
    }

    /// The item, if it exists and is readable.
    pub fn dictation(&self, id: &str) -> Option<DictationItem> {
        self.read_item(id).ok()
    }

    /// Delete the item and its audio.
    pub fn delete_dictation(&self, id: &str) -> io::Result<()> {
        let dir = self.root.join("dictations").join(id);
        match fs::remove_dir_all(&dir) {
            Ok(()) => fsync_dir(&self.root.join("dictations")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Items needing recovery (Failed, NotDelivered, Interrupted), newest first.
    pub fn recoverable_dictations(&self) -> Vec<DictationItem> {
        let mut items: Vec<_> = self
            .all_dictations()
            .into_iter()
            .filter(|i| {
                matches!(
                    i.state,
                    DictationState::Failed { .. }
                        | DictationState::NotDelivered
                        | DictationState::Interrupted
                )
            })
            .collect();
        items.sort_by(|a, b| b.started_unix.cmp(&a.started_unix).then(b.id.cmp(&a.id)));
        items
    }

    fn item_path(&self, id: &str) -> PathBuf {
        self.root.join("dictations").join(id).join("item.json")
    }

    fn read_item(&self, id: &str) -> io::Result<DictationItem> {
        let bytes = fs::read(self.item_path(id))?;
        serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    fn write_item(&self, item: &DictationItem) -> io::Result<()> {
        let bytes = serde_json::to_vec(item).expect("item serialize");
        durable_replace(&self.item_path(&item.id), &bytes)
    }

    /// Every readable dictation item (unreadable ones are skipped, never deleted).
    fn all_dictations(&self) -> Vec<DictationItem> {
        let Ok(rd) = fs::read_dir(self.root.join("dictations")) else {
            return Vec::new();
        };
        rd.flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter_map(|id| self.read_item(&id).ok())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_is_owner_only_and_items_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("stage");
        let st = Staging::open(&root).unwrap();
        let mode = fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let (item, wav) = st.new_dictation(100).unwrap();
        assert_eq!(wav, st.dictation_audio(&item.id));
        assert!(st.recoverable_dictations().is_empty());
        st.set_dictation_state(&item.id, DictationState::NotDelivered)
            .unwrap();
        assert_eq!(
            st.recoverable_dictations()[0].state,
            DictationState::NotDelivered
        );
        st.delete_dictation(&item.id).unwrap();
        assert!(st.recoverable_dictations().is_empty());
    }

    #[test]
    fn takes_found_recording_at_launch_become_interrupted_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let st = Staging::open(dir.path()).unwrap();
        let (a, _) = st.new_dictation(10).unwrap();
        let (b, _) = st.new_dictation(20).unwrap();
        st.set_dictation_state(&b.id, DictationState::Processing)
            .unwrap();
        let st = Staging::open(dir.path()).unwrap();
        let items = st.recoverable_dictations();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, b.id);
        assert_eq!(items[1].id, a.id);
        assert!(items.iter().all(|i| i.state == DictationState::Interrupted));
    }

    #[test]
    fn durable_replace_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.json");
        durable_replace(&p, b"one").unwrap();
        durable_replace(&p, b"two").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
