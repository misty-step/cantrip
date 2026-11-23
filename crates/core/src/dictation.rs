//! One dictation from press to outcome, and the retention rules around it.
//!
//! start → capture target (≤ 250 ms, worker thread) + start recorder → HUD
//! `Listening`; stop → stage audio → transcribe (OpenRouter, or the fail-closed
//! local gate with no key / no connection) → optional rewrite with Raw
//! fallback → guarded delivery → keep text in memory → delete audio once the
//! outcome is recorded, except `Not delivered`, which keeps it staged.
//! cancel (Escape) → delete the take immediately; a late response is never
//! delivered. Retry copies a recovered dictation (never pastes).
//!
//! OWNER: core-dictation fork.

use crate::delivery::ClaimWatch;
use crate::ports::{Clipboard, Desktop, LocalRecognizer, Recorder, SecretStore};
use crate::speech::{Mode, OpenRouter};
use crate::staging::Staging;
use crate::surface::Hud;
use std::sync::{Arc, Mutex};

/// Mechanisms one dictation controller needs.
#[derive(Clone)]
pub struct DictationDeps {
    pub recorder: Arc<dyn Recorder>,
    pub desktop: Arc<dyn Desktop>,
    pub clipboard: Arc<dyn Clipboard>,
    pub secrets: Arc<dyn SecretStore>,
    pub local: Arc<dyn LocalRecognizer>,
    pub openrouter: OpenRouter,
    pub staging: Staging,
    /// Called (from any thread) whenever the observable state changes, so the
    /// UI can repaint.
    pub notify: Arc<dyn Fn() + Send + Sync>,
}

/// Per-take choices, read from Settings when the take starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TakeConfig {
    pub mode: Mode,
    pub post_processing: bool,
    pub vocabulary: Vec<String>,
    pub microphone: Option<String>,
}

/// A finished take's record for evidence and logs (no text).
#[derive(Clone, Debug, PartialEq)]
pub struct TakeReport {
    pub outcome: String,
    pub transcribe_ms: Option<u64>,
    pub rewrite_ms: Option<u64>,
    pub total_ms: u64,
    pub chars: usize,
}

pub struct Dictation {
    deps: DictationDeps,
    shared: Arc<Mutex<()>>,
}

impl Dictation {
    pub fn new(deps: DictationDeps) -> Self {
        Dictation {
            deps,
            shared: Arc::new(Mutex::new(())),
        }
    }

    /// True from start until the take's processing finishes or it is cancelled.
    pub fn busy(&self) -> bool {
        let _ = (&self.deps, &self.shared);
        todo!()
    }

    /// True while audio is being recorded.
    pub fn recording(&self) -> bool {
        todo!()
    }

    /// Start a take. Shows `Listening` immediately; mic failure shows
    /// `Microphone blocked` (or the device error) and leaves nothing staged.
    pub fn start(&self, cfg: TakeConfig) {
        let _ = cfg;
        todo!()
    }

    /// Stop recording and process on a worker thread.
    pub fn stop(&self) {
        todo!()
    }

    /// Escape: discard the active take at once; hide the HUD; delete audio.
    pub fn cancel(&self) {
        todo!()
    }

    /// Current HUD state, with the clipboard claim re-checked (sticky drop).
    pub fn hud(&self) -> Hud {
        todo!()
    }

    /// Live input level while recording.
    pub fn level(&self) -> f32 {
        todo!()
    }

    /// The last dictation's text (memory only, until quit).
    pub fn last_text(&self) -> Option<String> {
        todo!()
    }

    /// Copy last dictation; returns false when there is none or copy failed.
    pub fn copy_last(&self) -> bool {
        todo!()
    }

    /// Retry a staged dictation: transcribe (+ rewrite per `cfg`), then copy
    /// only; on success delete its audio.
    pub fn retry(&self, id: &str, cfg: TakeConfig) {
        let _ = (id, cfg);
        todo!()
    }

    /// Confirmed Discard of a staged dictation.
    pub fn discard(&self, id: &str) {
        let _ = id;
        todo!()
    }

    /// Report of the most recent finished take (evidence / CLI).
    pub fn last_report(&self) -> Option<TakeReport> {
        let _ = ClaimWatch::new(None);
        todo!()
    }

    /// Block until the current take finished processing (CLI and tests).
    pub fn wait_idle(&self, timeout: std::time::Duration) -> bool {
        let _ = timeout;
        todo!()
    }
}
