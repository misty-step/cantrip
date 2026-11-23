//! Dual-track meetings: capture on one clock, part-wise transcription with
//! timestamps, deterministic merge, faithful cleanup, summary, Markdown note,
//! and the append-only exclusive archive (DESIGN.md › Meeting pipeline).
//!
//! OWNER: core-meeting fork. Submodules and their APIs are the fork's to
//! design; [`Meetings`] is the fixed facade the app drives.

use crate::ports::{MeetingCapture, SecretStore};
use crate::speech::OpenRouter;
use crate::staging::Staging;
use crate::surface::RecoveryKind;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct MeetingDeps {
    pub capture: Arc<dyn MeetingCapture>,
    pub secrets: Arc<dyn SecretStore>,
    pub openrouter: OpenRouter,
    pub staging: Staging,
    pub notify: Arc<dyn Fn() + Send + Sync>,
}

/// A staged meeting that needs the recovery row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeetingItem {
    pub id: String,
    pub started_unix: u64,
    pub kind: RecoveryKind,
}

pub struct Meetings {
    deps: MeetingDeps,
}

impl Meetings {
    pub fn new(deps: MeetingDeps) -> Self {
        Meetings { deps }
    }

    /// Start recording. Refuses (Err with the user-facing message) without an
    /// OpenRouter key, without recording anything.
    pub fn start(
        &self,
        microphone: Option<&str>,
        vocabulary: Vec<String>,
        folder: PathBuf,
    ) -> Result<(), String> {
        let _ = (microphone, vocabulary, folder, &self.deps);
        todo!()
    }

    /// Stop and process on a worker thread.
    pub fn stop(&self) {
        todo!()
    }

    pub fn active(&self) -> bool {
        todo!()
    }

    /// Elapsed recording time while active.
    pub fn elapsed(&self) -> Option<Duration> {
        todo!()
    }

    /// True while a stopped meeting is processing or publishing.
    pub fn processing(&self) -> bool {
        todo!()
    }

    /// Meetings needing the recovery row, newest first.
    pub fn recoverable(&self) -> Vec<MeetingItem> {
        todo!()
    }

    /// Retry: transcribe unresolved parts and finish (parts-failed /
    /// interrupted) or publish only (pending-archive, zero API calls), into
    /// the folder chosen now.
    pub fn retry(&self, id: &str, folder: PathBuf) {
        let _ = (id, folder);
        todo!()
    }

    /// Save partial notes (parts-failed only); staging is never deleted.
    pub fn save_partial(&self, id: &str, folder: PathBuf) {
        let _ = (id, folder);
        todo!()
    }

    /// Confirmed Discard; never touches the Meeting folder.
    pub fn discard(&self, id: &str) {
        let _ = id;
        todo!()
    }

    /// Path of the most recently published note (evidence / CLI).
    pub fn last_published(&self) -> Option<PathBuf> {
        todo!()
    }

    /// Block until processing finished (CLI and tests).
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let _ = timeout;
        todo!()
    }
}

/// Settings-time folder check (invariant 1a): `Err` with "Choose a folder
/// outside Cantrip's private data folder" when `folder` overlaps staging by
/// filesystem identity after symlink resolution. Never creates `folder`.
pub fn check_folder_choice(folder: &std::path::Path, staging: &Staging) -> Result<(), String> {
    let _ = (folder, staging);
    todo!()
}
