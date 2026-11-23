//! Dual-track meetings: capture on one clock, part-wise transcription with
//! timestamps, deterministic merge, faithful cleanup, summary, Markdown note,
//! and the append-only exclusive archive (DESIGN.md › Meeting pipeline).
//!
//! - [`parts`]      split tracks into upload-sized parts; per-part results
//! - [`transcript`] deterministic chronological cross-track merge
//! - [`note`]       slug, file names, Markdown rendering
//! - [`store`]      crash-safe staging layout and states
//! - [`archive`]    publication invariants 1–3 (identity, exclusive, verify)
//! - [`process`]    the work steps, including publish-only Retry (invariant 4)
//!
//! [`Meetings`] is the facade the app drives; all work runs on worker threads.

pub mod archive;
pub mod note;
pub mod parts;
pub mod process;
pub mod store;
pub mod time;
pub mod transcript;

#[cfg(test)]
mod tests;

use crate::ports::{MeetingCapture, MeetingRecording, SecretError, SecretStore};
use crate::speech::OpenRouter;
use crate::staging::{unique_suffix, Staging};
use crate::surface::RecoveryKind;
use archive::Fault;
use process::{Ctx, Finish};
pub use process::{MeetingSpeech, RetryPolicy};
use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use store::{MeetingMeta, MeetingState, MeetingStore};

pub const NEED_KEY: &str = "Meetings need an OpenRouter key. Add one in Settings";
pub const FOLDER_OVERLAP: &str = "Choose a folder outside Cantrip's private data folder";

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

struct Active {
    id: String,
    rec: Box<dyn MeetingRecording>,
}

#[derive(Default)]
struct St {
    active: Option<Active>,
    busy: BTreeSet<String>,
    last_published: Option<PathBuf>,
    last_finish: Option<Finish>,
}

type Sleep = Arc<dyn Fn(Duration) + Send + Sync>;

struct Inner {
    deps: MeetingDeps,
    speech: Arc<dyn MeetingSpeech>,
    store: MeetingStore,
    policy: RetryPolicy,
    sleep: Sleep,
    fault: Mutex<Option<Fault>>,
    st: Mutex<St>,
    idle: Condvar,
}

pub struct Meetings {
    inner: Arc<Inner>,
}

impl Meetings {
    pub fn new(deps: MeetingDeps) -> Self {
        let speech: Arc<dyn MeetingSpeech> = Arc::new(deps.openrouter.clone());
        Self::with_speech(deps, speech)
    }

    /// Construct with an explicit speech implementation (tests, fakes).
    /// Runs launch recovery: `recording`/`processing` → `interrupted`,
    /// `publishing` → `pending-archive`.
    pub fn with_speech(deps: MeetingDeps, speech: Arc<dyn MeetingSpeech>) -> Self {
        let store = MeetingStore::new(deps.staging.meetings_dir());
        for m in store.list() {
            let next = match m.state {
                MeetingState::Recording | MeetingState::Processing => MeetingState::Interrupted,
                MeetingState::Publishing => MeetingState::PendingArchive,
                _ => continue,
            };
            let _ = store.set_state(&m.id, next);
        }
        Meetings {
            inner: Arc::new(Inner {
                deps,
                speech,
                store,
                policy: RetryPolicy::default(),
                sleep: Arc::new(std::thread::sleep),
                fault: Mutex::new(None),
                st: Mutex::new(St::default()),
                idle: Condvar::new(),
            }),
        }
    }

    /// Replace the retry policy and sleeper (tests use tiny delays).
    pub fn with_retry_policy(self, policy: RetryPolicy, sleep: Sleep) -> Self {
        let inner = Arc::try_unwrap(self.inner)
            .unwrap_or_else(|_| panic!("with_retry_policy must be called before use"));
        Meetings {
            inner: Arc::new(Inner {
                policy,
                sleep,
                ..inner
            }),
        }
    }

    /// Debug/test hook: make the next verification (or flush) fail once.
    #[doc(hidden)]
    pub fn inject_fault(&self, fault: Fault) {
        *self.inner.fault.lock().unwrap() = Some(fault);
    }

    /// Start recording. Refuses (Err with the user-facing message) without an
    /// OpenRouter key, without recording anything.
    pub fn start(
        &self,
        microphone: Option<&str>,
        vocabulary: Vec<String>,
        folder: PathBuf,
    ) -> Result<(), String> {
        let inner = &self.inner;
        if inner.st.lock().unwrap().active.is_some() {
            return Err("A meeting is already recording".into());
        }
        match inner.deps.secrets.get_key() {
            Ok(Some(k)) if !k.trim().is_empty() => {}
            Ok(_) => return Err(NEED_KEY.into()),
            Err(SecretError::Unavailable(_)) => {
                return Err(
                    "Cantrip could not read the OpenRouter key from the secret store".into(),
                )
            }
        }
        let started_unix = time::now_unix();
        let id = format!("m{started_unix}-{}", unique_suffix());
        let meta = MeetingMeta {
            id: id.clone(),
            source_id: random_hex(16),
            started_unix,
            recorded_at: time::iso8601(started_unix),
            utc_stamp: time::file_stamp(started_unix),
            state: MeetingState::Recording,
            vocabulary,
            folder,
            you_offset_ms: 0,
            others_offset_ms: 0,
            publish_path: None,
            partial_paths: vec![],
            failed_parts: 0,
            title: String::new(),
        };
        // source_id is durable before any audio is kept.
        inner
            .store
            .create(&meta)
            .map_err(|e| format!("Cantrip could not create meeting storage ({})", e.kind()))?;
        match inner.deps.capture.start(
            microphone,
            &inner.store.you_wav(&id),
            &inner.store.others_wav(&id),
        ) {
            Ok(rec) => {
                inner.st.lock().unwrap().active = Some(Active { id, rec });
                (inner.deps.notify)();
                Ok(())
            }
            Err(msg) => {
                let _ = inner.store.delete(&id);
                Err(msg)
            }
        }
    }

    /// Stop and process on a worker thread.
    pub fn stop(&self) {
        let inner = &self.inner;
        let Some(active) = inner.st.lock().unwrap().active.take() else {
            return;
        };
        let id = active.id.clone();
        match active.rec.stop() {
            Ok(tracks) => {
                if let Ok(mut m) = inner.store.load(&id) {
                    m.you_offset_ms = tracks.you_offset_ms;
                    m.others_offset_ms = tracks.others_offset_ms;
                    m.state = MeetingState::Processing;
                    let _ = inner.store.save(&m);
                    let folder = m.folder.clone();
                    let key_id = id.clone();
                    self.spawn(&key_id, move |i| i.process(&id, &folder));
                }
            }
            Err(_) => {
                let _ = inner.store.set_state(&id, MeetingState::Interrupted);
            }
        }
        (inner.deps.notify)();
    }

    pub fn active(&self) -> bool {
        self.inner.st.lock().unwrap().active.is_some()
    }

    /// Elapsed recording time while active.
    pub fn elapsed(&self) -> Option<Duration> {
        self.inner
            .st
            .lock()
            .unwrap()
            .active
            .as_ref()
            .map(|a| a.rec.elapsed())
    }

    /// True while a stopped meeting is processing or publishing.
    pub fn processing(&self) -> bool {
        !self.inner.st.lock().unwrap().busy.is_empty()
    }

    /// Meetings needing the recovery row, newest first.
    pub fn recoverable(&self) -> Vec<MeetingItem> {
        let st = self.inner.st.lock().unwrap();
        let active = st.active.as_ref().map(|a| a.id.clone());
        let mut items: Vec<_> = self
            .inner
            .store
            .list()
            .into_iter()
            .filter(|m| !st.busy.contains(&m.id) && Some(&m.id) != active.as_ref())
            .filter_map(|m| {
                let kind = match m.state {
                    MeetingState::Interrupted => RecoveryKind::MeetingInterrupted,
                    MeetingState::PartsFailed => RecoveryKind::MeetingPartsFailed {
                        failed_parts: m.failed_parts.max(1),
                    },
                    MeetingState::PendingArchive => RecoveryKind::MeetingPendingArchive,
                    _ => return None,
                };
                Some(MeetingItem {
                    id: m.id,
                    started_unix: m.started_unix,
                    kind,
                })
            })
            .collect();
        items.sort_by(|a, b| {
            b.started_unix
                .cmp(&a.started_unix)
                .then_with(|| b.id.cmp(&a.id))
        });
        items
    }

    /// Retry: transcribe unresolved parts and finish (parts-failed /
    /// interrupted) or publish only (pending-archive, zero API calls), into
    /// the folder chosen now.
    pub fn retry(&self, id: &str, folder: PathBuf) {
        let Ok(meta) = self.inner.store.load(id) else {
            return;
        };
        let id = id.to_string();
        match meta.state {
            MeetingState::PendingArchive => self.spawn(&id.clone(), move |i| {
                i.ctx_run(|c| c.publish_only(&id, &folder))
            }),
            MeetingState::Interrupted | MeetingState::PartsFailed => {
                self.spawn(&id.clone(), move |i| i.process(&id, &folder))
            }
            _ => {}
        }
    }

    /// Save partial notes (parts-failed only); staging is never deleted.
    pub fn save_partial(&self, id: &str, folder: PathBuf) {
        let id = id.to_string();
        self.spawn(&id.clone(), move |i| {
            let key = i.key();
            i.ctx_run(|c| c.save_partial(&id, key.as_deref(), &folder))
        });
    }

    /// Confirmed Discard; never touches the Meeting folder.
    pub fn discard(&self, id: &str) {
        {
            let st = self.inner.st.lock().unwrap();
            if st.busy.contains(id) || st.active.as_ref().is_some_and(|a| a.id == id) {
                return;
            }
        }
        let _ = self.inner.store.delete(id);
        (self.inner.deps.notify)();
    }

    /// Path of the most recently published note (evidence / CLI).
    pub fn last_published(&self) -> Option<PathBuf> {
        self.inner.st.lock().unwrap().last_published.clone()
    }

    /// How the most recent work step ended (evidence / CLI / tests).
    pub fn last_finish(&self) -> Option<Finish> {
        self.inner.st.lock().unwrap().last_finish.clone()
    }

    /// Block until processing finished (CLI and tests).
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut st = self.inner.st.lock().unwrap();
        while !st.busy.is_empty() {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            st = self.inner.idle.wait_timeout(st, deadline - now).unwrap().0;
        }
        true
    }

    /// Run `f` for meeting `id` on a worker thread unless it is already busy.
    fn spawn(&self, id: &str, f: impl FnOnce(&Inner) -> Finish + Send + 'static) {
        {
            let mut st = self.inner.st.lock().unwrap();
            if !st.busy.insert(id.to_string()) {
                return;
            }
        }
        (self.inner.deps.notify)();
        let inner = self.inner.clone();
        let id = id.to_string();
        std::thread::spawn(move || {
            let finish = f(&inner);
            {
                let mut st = inner.st.lock().unwrap();
                if let Finish::Archived(p) | Finish::PartialSaved(p) = &finish {
                    st.last_published = Some(p.clone());
                }
                st.last_finish = Some(finish);
                st.busy.remove(&id);
            }
            inner.idle.notify_all();
            (inner.deps.notify)();
        });
    }
}

impl Inner {
    fn key(&self) -> Option<String> {
        self.deps
            .secrets
            .get_key()
            .ok()
            .flatten()
            .filter(|k| !k.trim().is_empty())
    }

    fn ctx_run<T>(&self, f: impl FnOnce(&Ctx) -> T) -> T {
        let staging_root = self.deps.staging.root().to_path_buf();
        let fault = || self.fault.lock().unwrap().take();
        let ctx = Ctx {
            store: &self.store,
            staging_root: &staging_root,
            speech: self.speech.as_ref(),
            policy: &self.policy,
            sleep: self.sleep.as_ref(),
            fault: &fault,
        };
        f(&ctx)
    }

    fn process(&self, id: &str, folder: &Path) -> Finish {
        let key = self.key();
        self.ctx_run(|c| c.process(id, key.as_deref(), folder))
    }
}

/// Settings-time folder check (invariant 1a): `Err` with "Choose a folder
/// outside Cantrip's private data folder" when `folder` overlaps staging by
/// filesystem identity after symlink resolution. Never creates `folder`.
pub fn check_folder_choice(folder: &Path, staging: &Staging) -> Result<(), String> {
    match archive::folder_choice_overlaps(folder, staging.root()) {
        Ok(false) => Ok(()),
        Ok(true) => Err(FOLDER_OVERLAP.into()),
        Err(_) => Err("Cantrip could not check this folder. Choose another folder".into()),
    }
}

fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if !ok {
        // Fallback: time and process entropy; uniqueness, not secrecy.
        let seed = format!("{:?}{}", Instant::now(), unique_suffix());
        buf = archive::digest(seed.as_bytes()).as_bytes()[..n].to_vec();
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}
