//! Crash-safe meeting staging. Layout under `<staging>/meetings/<id>/`:
//!
//! ```text
//! meta.json            MeetingMeta (source_id fixed before any audio)
//! you.wav others.wav   the two tracks as captured
//! parts.json           Vec<Part> once split
//! parts/<name>.wav     each part's audio
//! results/<name>.json  PartResult per part
//! cleanup.json         per-turn cleanup outcomes (keyed by turn digest)
//! summary.json         Summary keyed by transcript digest
//! note.md              the rendered complete note (kept until archived)
//! ```
//!
//! Every file is written with `staging::durable_replace`, so after a crash
//! each item is whole or absent.

use super::note::{Cleaned, Summary};
use super::parts::{Part, PartResult};
use crate::staging::{self, durable_replace, ensure_private_dir};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MeetingState {
    Recording,
    Processing,
    /// A complete note is being published (found at launch → PendingArchive).
    Publishing,
    Interrupted,
    PartsFailed,
    /// Only publication failed: publish-only Retry.
    PendingArchive,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeetingMeta {
    pub id: String,
    pub source_id: String,
    pub started_unix: u64,
    pub recorded_at: String,
    /// `YYYY-MM-DD_HHMMSSZ` file stamp.
    pub utc_stamp: String,
    pub state: MeetingState,
    pub vocabulary: Vec<String>,
    /// Folder chosen when the meeting started (Retry uses the one chosen now).
    pub folder: PathBuf,
    pub you_offset_ms: u64,
    pub others_offset_ms: u64,
    /// Recorded before writing a complete note into the Meeting folder.
    pub publish_path: Option<PathBuf>,
    /// Partial notes published so far (informational; never reopened).
    pub partial_paths: Vec<PathBuf>,
    pub failed_parts: usize,
    /// Title of the rendered complete note (for its file name).
    #[serde(default)]
    pub title: String,
}

#[derive(Clone, Debug)]
pub struct MeetingStore {
    dir: PathBuf,
}

impl MeetingStore {
    pub fn new(meetings_dir: PathBuf) -> Self {
        MeetingStore { dir: meetings_dir }
    }

    pub fn meeting_dir(&self, id: &str) -> PathBuf {
        self.dir.join(id)
    }

    pub fn you_wav(&self, id: &str) -> PathBuf {
        self.meeting_dir(id).join("you.wav")
    }

    pub fn others_wav(&self, id: &str) -> PathBuf {
        self.meeting_dir(id).join("others.wav")
    }

    pub fn part_wav(&self, id: &str, name: &str) -> PathBuf {
        self.meeting_dir(id)
            .join("parts")
            .join(format!("{name}.wav"))
    }

    pub fn note_path(&self, id: &str) -> PathBuf {
        self.meeting_dir(id).join("note.md")
    }

    /// Create the meeting directory and durably write its meta (before audio).
    pub fn create(&self, meta: &MeetingMeta) -> io::Result<()> {
        ensure_private_dir(&self.dir)?;
        let d = self.meeting_dir(&meta.id);
        ensure_private_dir(&d)?;
        ensure_private_dir(&d.join("parts"))?;
        ensure_private_dir(&d.join("results"))?;
        self.save(meta)?;
        staging::fsync_dir(&self.dir)
    }

    pub fn save(&self, meta: &MeetingMeta) -> io::Result<()> {
        write_json(&self.meeting_dir(&meta.id).join("meta.json"), meta)
    }

    pub fn load(&self, id: &str) -> io::Result<MeetingMeta> {
        read_json(&self.meeting_dir(id).join("meta.json"))
    }

    pub fn exists(&self, id: &str) -> bool {
        self.meeting_dir(id).join("meta.json").exists()
    }

    pub fn set_state(&self, id: &str, state: MeetingState) -> io::Result<MeetingMeta> {
        let mut m = self.load(id)?;
        m.state = state;
        self.save(&m)?;
        Ok(m)
    }

    /// Every readable meeting.
    pub fn list(&self) -> Vec<MeetingMeta> {
        let Ok(rd) = fs::read_dir(&self.dir) else {
            return vec![];
        };
        rd.flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter_map(|id| self.load(&id).ok())
            .collect()
    }

    pub fn parts(&self, id: &str) -> Option<Vec<Part>> {
        read_json(&self.meeting_dir(id).join("parts.json")).ok()
    }

    pub fn save_parts(&self, id: &str, parts: &[Part]) -> io::Result<()> {
        write_json(&self.meeting_dir(id).join("parts.json"), &parts)
    }

    pub fn results(&self, id: &str, parts: &[Part]) -> BTreeMap<String, PartResult> {
        parts
            .iter()
            .filter_map(|p| {
                read_json::<PartResult>(&self.result_path(id, &p.name))
                    .ok()
                    .map(|r| (p.name.clone(), r))
            })
            .collect()
    }

    fn result_path(&self, id: &str, name: &str) -> PathBuf {
        self.meeting_dir(id)
            .join("results")
            .join(format!("{name}.json"))
    }

    pub fn save_result(&self, id: &str, name: &str, r: &PartResult) -> io::Result<()> {
        write_json(&self.result_path(id, name), r)
    }

    pub fn cleanup(&self, id: &str) -> BTreeMap<String, Cleaned> {
        read_json(&self.meeting_dir(id).join("cleanup.json")).unwrap_or_default()
    }

    pub fn save_cleanup(&self, id: &str, c: &BTreeMap<String, Cleaned>) -> io::Result<()> {
        write_json(&self.meeting_dir(id).join("cleanup.json"), c)
    }

    pub fn summaries(&self, id: &str) -> BTreeMap<String, Summary> {
        read_json(&self.meeting_dir(id).join("summary.json")).unwrap_or_default()
    }

    pub fn save_summaries(&self, id: &str, s: &BTreeMap<String, Summary>) -> io::Result<()> {
        write_json(&self.meeting_dir(id).join("summary.json"), s)
    }

    pub fn save_note(&self, id: &str, note: &str) -> io::Result<()> {
        durable_replace(&self.note_path(id), note.as_bytes())
    }

    pub fn note(&self, id: &str) -> Option<String> {
        fs::read_to_string(self.note_path(id)).ok()
    }

    /// Delete the whole meeting staging (complete archived note, or confirmed
    /// Discard). Never touches anything outside staging.
    pub fn delete(&self, id: &str) -> io::Result<()> {
        match fs::remove_dir_all(self.meeting_dir(id)) {
            Ok(()) => staging::fsync_dir(&self.dir),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

fn write_json<T: Serialize + ?Sized>(path: &Path, v: &T) -> io::Result<()> {
    durable_replace(path, &serde_json::to_vec(v).expect("serialize"))
}

fn read_json<T: DeserializeOwned>(path: &Path) -> io::Result<T> {
    let b = fs::read(path)?;
    serde_json::from_slice(&b).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}
