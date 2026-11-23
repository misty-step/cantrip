//! One dictation from press to outcome, and the retention rules around it.
//!
//! start → capture target (≤ 250 ms, worker thread) + start recorder → HUD
//! `Listening`; stop → stage audio → transcribe (OpenRouter, or the fail-closed
//! local gate with no key / no connection) → optional rewrite with Raw
//! fallback → guarded delivery → keep text in memory → delete audio once the
//! outcome is recorded, except `Not delivered`, which keeps it staged.
//! cancel (Escape) → delete the take immediately; a late response is never
//! delivered. Retry copies a recovered dictation (never pastes).

use crate::delivery::{self, CaptureContext, Captured, ClaimWatch, Outcome, CAPTURE_DEADLINE};
use crate::offline;
use crate::ports::{
    Clipboard, Desktop, LocalRecognizer, MicError, Recorder, Recording, SecretStore,
};
use crate::speech::{Mode, OpenRouter, SpeechError, REWRITE_TIMEOUT};
use crate::staging::{DictationState, Staging};
use crate::surface::Hud;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// HUD fix when the secret store cannot be read (never treated as "no key").
pub const FIX_KEY_UNREADABLE: &str =
    "Cantrip could not read the OpenRouter key. Check the system keyring";
/// HUD fix when OpenRouter returned an error.
pub const FIX_SERVICE_FAILED: &str = "Speech service failed. Retry from the menu";
/// HUD fix when OpenRouter returned no words.
pub const FIX_NO_WORDS: &str = "No words recognized. Retry from the menu";
/// HUD after a successful Retry (copied, never pasted).
pub const RETRY_COPIED: &str = "Copied recovered dictation";

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

enum Phase {
    Idle,
    /// Recorder starting (busy, nothing to stop yet).
    Starting,
    Recording(Box<dyn Recording>),
    Processing,
}

struct Take {
    generation: u64,
    item_id: String,
    cfg: TakeConfig,
    capture: Option<mpsc::Receiver<CaptureContext>>,
    cancelled: Arc<AtomicBool>,
    started: Instant,
}

struct Inner {
    phase: Phase,
    take: Option<Take>,
    generation: u64,
    hud: Hud,
    hud_since: Instant,
    claim: ClaimWatch,
    last_text: Option<String>,
    last_report: Option<TakeReport>,
}

impl Inner {
    fn set_hud(&mut self, hud: Hud, claim: ClaimWatch) {
        self.hud = hud;
        self.hud_since = Instant::now();
        self.claim = claim;
    }
}

pub struct Dictation {
    deps: DictationDeps,
    inner: Arc<Mutex<Inner>>,
}

/// Text produced for a take, before delivery.
struct Produced {
    text: String,
    /// `Raw (cleanup skipped)` or `On-device`.
    note: Option<Hud>,
    transcribe_ms: Option<u64>,
    rewrite_ms: Option<u64>,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn ms(d: Duration) -> u64 {
    d.as_millis() as u64
}

/// Transcribe (cloud, or the fail-closed local gate) and optionally rewrite.
/// `Err` carries the HUD fix; the caller keeps the audio staged.
fn produce(deps: &DictationDeps, wav: &Path, cfg: &TakeConfig) -> Result<Produced, String> {
    let local = |deps: &DictationDeps| {
        offline::recognize_locally(deps.local.as_ref(), wav).map(|text| Produced {
            text,
            note: Some(Hud::OnDevice),
            transcribe_ms: None,
            rewrite_ms: None,
        })
    };
    let key = match deps.secrets.get_key() {
        Err(_) => return Err(FIX_KEY_UNREADABLE.to_string()),
        Ok(None) => return local(deps),
        Ok(Some(k)) => k,
    };
    let t0 = Instant::now();
    let transcript = match deps
        .openrouter
        .transcribe(&key, wav, &cfg.vocabulary, false)
    {
        Ok(t) => t,
        // No connection could be made: nothing was sent; the local gate decides.
        Err(SpeechError::Offline(_)) => return local(deps),
        Err(SpeechError::Empty) => return Err(FIX_NO_WORDS.to_string()),
        Err(_) => return Err(FIX_SERVICE_FAILED.to_string()),
    };
    let transcribe_ms = Some(ms(t0.elapsed()));
    if transcript.text.trim().is_empty() {
        return Err(FIX_NO_WORDS.to_string());
    }
    if !cfg.post_processing || cfg.mode == Mode::Raw {
        return Ok(Produced {
            text: transcript.text,
            note: None,
            transcribe_ms,
            rewrite_ms: None,
        });
    }
    let t1 = Instant::now();
    let rewritten = deps.openrouter.rewrite(
        &key,
        cfg.mode,
        &transcript.text,
        &cfg.vocabulary,
        REWRITE_TIMEOUT,
    );
    let rewrite_ms = Some(ms(t1.elapsed()));
    Ok(match rewritten {
        Ok(text) => Produced {
            text,
            note: None,
            transcribe_ms,
            rewrite_ms,
        },
        Err(_) => Produced {
            text: transcript.text,
            note: Some(Hud::RawCleanupSkipped),
            transcribe_ms,
            rewrite_ms,
        },
    })
}

impl Dictation {
    pub fn new(deps: DictationDeps) -> Self {
        Dictation {
            deps,
            inner: Arc::new(Mutex::new(Inner {
                phase: Phase::Idle,
                take: None,
                generation: 0,
                hud: Hud::Hidden,
                hud_since: Instant::now(),
                claim: ClaimWatch::new(None),
                last_text: None,
                last_report: None,
            })),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn notify(&self) {
        (self.deps.notify)();
    }

    /// True from start until the take's processing finishes or it is cancelled.
    pub fn busy(&self) -> bool {
        !matches!(self.lock().phase, Phase::Idle)
    }

    /// True while audio is being recorded (or the recorder is starting).
    pub fn recording(&self) -> bool {
        matches!(self.lock().phase, Phase::Recording(_) | Phase::Starting)
    }

    /// Start a take. Shows `Listening` immediately; mic failure shows
    /// `Microphone blocked` (or the device error) and leaves nothing staged.
    pub fn start(&self, cfg: TakeConfig) {
        let (generation, item_id, wav) = {
            let mut g = self.lock();
            if !matches!(g.phase, Phase::Idle) {
                return;
            }
            let (item, wav) = match self.deps.staging.new_dictation(unix_now()) {
                Ok(x) => x,
                Err(_) => {
                    g.set_hud(
                        Hud::Notice("Cannot record: private storage unavailable".into()),
                        ClaimWatch::new(None),
                    );
                    drop(g);
                    self.notify();
                    return;
                }
            };
            g.generation += 1;
            let generation = g.generation;
            g.phase = Phase::Starting;
            g.set_hud(Hud::Listening, ClaimWatch::new(None));
            // Capture the target concurrently with starting the microphone.
            let (tx, rx) = mpsc::channel();
            let desktop = self.deps.desktop.clone();
            std::thread::spawn(move || {
                let _ = tx.send(delivery::capture_with_deadline(desktop, CAPTURE_DEADLINE));
            });
            g.take = Some(Take {
                generation,
                item_id: item.id.clone(),
                cfg: cfg.clone(),
                capture: Some(rx),
                cancelled: Arc::new(AtomicBool::new(false)),
                started: Instant::now(),
            });
            (generation, item.id, wav)
        };
        self.notify();
        let started = self.deps.recorder.start(cfg.microphone.as_deref(), &wav);
        let mut g = self.lock();
        let still_ours = g.generation == generation
            && g.take
                .as_ref()
                .is_some_and(|t| !t.cancelled.load(Ordering::SeqCst));
        match started {
            Ok(rec) if still_ours => g.phase = Phase::Recording(rec),
            Ok(rec) => {
                // Cancelled while the recorder was starting.
                drop(g);
                rec.cancel();
                let _ = self.deps.staging.delete_dictation(&item_id);
                return;
            }
            Err(e) => {
                let _ = self.deps.staging.delete_dictation(&item_id);
                if still_ours {
                    g.phase = Phase::Idle;
                    g.take = None;
                    let hud = match e {
                        MicError::Blocked => Hud::MicrophoneBlocked,
                        MicError::Unavailable(_) => Hud::Notice("Microphone unavailable".into()),
                    };
                    g.set_hud(hud, ClaimWatch::new(None));
                }
            }
        }
        drop(g);
        self.notify();
    }

    /// Stop recording and process on a worker thread.
    pub fn stop(&self) {
        let (rec, take) = {
            let mut g = self.lock();
            if !matches!(g.phase, Phase::Recording(_)) {
                return;
            }
            let Phase::Recording(rec) = std::mem::replace(&mut g.phase, Phase::Processing) else {
                unreachable!()
            };
            let Some(mut take) = g.take.take() else {
                g.phase = Phase::Idle;
                return;
            };
            let capture = take.capture.take();
            g.take = Some(Take {
                generation: take.generation,
                item_id: take.item_id.clone(),
                cfg: take.cfg.clone(),
                capture: None,
                cancelled: take.cancelled.clone(),
                started: take.started,
            });
            take.capture = capture;
            g.set_hud(Hud::Transcribing, ClaimWatch::new(None));
            (rec, take)
        };
        self.notify();
        let deps = self.deps.clone();
        let inner = self.inner.clone();
        std::thread::spawn(move || process(deps, inner, rec, take));
    }

    /// Escape: discard the active take at once; hide the HUD; delete audio.
    pub fn cancel(&self) {
        let (rec, item_id) = {
            let mut g = self.lock();
            let Some(take) = g.take.take() else {
                return;
            };
            if matches!(g.phase, Phase::Idle) {
                return;
            }
            take.cancelled.store(true, Ordering::SeqCst);
            let rec = match std::mem::replace(&mut g.phase, Phase::Idle) {
                Phase::Recording(r) => Some(r),
                _ => None,
            };
            g.set_hud(Hud::Hidden, ClaimWatch::new(None));
            (rec, take.item_id)
        };
        if let Some(r) = rec {
            r.cancel();
        }
        let _ = self.deps.staging.delete_dictation(&item_id);
        self.notify();
    }

    /// Current HUD state, with the clipboard claim re-checked (sticky drop)
    /// and terminal states faded after their dwell.
    pub fn hud(&self) -> Hud {
        let mut g = self.lock();
        if g.hud.claim().is_some() {
            let clipboard = self.deps.clipboard.clone();
            if !g.claim.check(clipboard.as_ref()) {
                g.hud.drop_claim();
            }
        }
        if let Some(d) = g.hud.dwell_ms() {
            if g.hud_since.elapsed() >= Duration::from_millis(d) {
                g.hud = Hud::Hidden;
            }
        }
        g.hud.clone()
    }

    /// Live input level while recording.
    pub fn level(&self) -> f32 {
        match &self.lock().phase {
            Phase::Recording(r) => r.level(),
            _ => 0.0,
        }
    }

    /// The last dictation's text (memory only, until quit).
    pub fn last_text(&self) -> Option<String> {
        self.lock().last_text.clone()
    }

    /// Copy last dictation; returns false when there is none or copy failed.
    pub fn copy_last(&self) -> bool {
        let Some(text) = self.last_text() else {
            return false;
        };
        self.deps.clipboard.write_text(&text, true).is_ok()
    }

    /// Retry a staged dictation: transcribe (+ rewrite per `cfg`), then copy
    /// only; on success delete its audio.
    pub fn retry(&self, id: &str, cfg: TakeConfig) {
        {
            let mut g = self.lock();
            if !matches!(g.phase, Phase::Idle) || self.deps.staging.dictation(id).is_none() {
                return;
            }
            g.phase = Phase::Processing;
            g.set_hud(Hud::Transcribing, ClaimWatch::new(None));
        }
        self.notify();
        let deps = self.deps.clone();
        let inner = self.inner.clone();
        let id = id.to_string();
        std::thread::spawn(move || {
            let wav = deps.staging.dictation_audio(&id);
            let hud = match produce(&deps, &wav, &cfg) {
                Ok(p) => match deps.clipboard.write_text(&p.text, true) {
                    Ok(_) => {
                        let _ = deps.staging.delete_dictation(&id);
                        lock(&inner).last_text = Some(p.text);
                        Hud::Notice(RETRY_COPIED.into())
                    }
                    Err(_) => {
                        let _ = deps
                            .staging
                            .set_dictation_state(&id, DictationState::NotDelivered);
                        lock(&inner).last_text = Some(p.text);
                        Hud::NotDelivered
                    }
                },
                Err(fix) => {
                    let _ = deps.staging.set_dictation_state(
                        &id,
                        DictationState::Failed {
                            reason: fix.clone(),
                        },
                    );
                    Hud::NotTranscribed(fix)
                }
            };
            {
                let mut g = lock(&inner);
                g.phase = Phase::Idle;
                g.set_hud(hud, ClaimWatch::new(None));
            }
            (deps.notify)();
        });
    }

    /// Confirmed Discard of a staged dictation.
    pub fn discard(&self, id: &str) {
        let _ = self.deps.staging.delete_dictation(id);
        self.notify();
    }

    /// Report of the most recent finished take (evidence / CLI).
    pub fn last_report(&self) -> Option<TakeReport> {
        self.lock().last_report.clone()
    }

    /// Block until the current take finished processing (CLI and tests).
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let until = Instant::now() + timeout;
        while self.busy() {
            if Instant::now() >= until {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    }
}

fn lock(inner: &Mutex<Inner>) -> std::sync::MutexGuard<'_, Inner> {
    inner.lock().unwrap_or_else(|e| e.into_inner())
}

/// The worker: finalize audio, produce text, deliver, apply retention.
fn process(deps: DictationDeps, inner: Arc<Mutex<Inner>>, rec: Box<dyn Recording>, take: Take) {
    let cancelled = || take.cancelled.load(Ordering::SeqCst);
    let ours = |g: &Inner| g.generation == take.generation && !cancelled();
    let finish = |hud: Hud, claim: ClaimWatch, report: Option<TakeReport>, text: Option<String>| {
        let mut g = lock(&inner);
        if ours(&g) {
            g.phase = Phase::Idle;
            g.take = None;
            g.set_hud(hud, claim);
            if let Some(r) = report {
                g.last_report = Some(r);
            }
            if let Some(t) = text {
                g.last_text = Some(t);
            }
        }
        drop(g);
        (deps.notify)();
    };
    let staging = &deps.staging;
    let wav = match rec.stop() {
        Ok(p) => p,
        Err(_) => {
            if !cancelled() {
                let _ = staging.set_dictation_state(
                    &take.item_id,
                    DictationState::Failed {
                        reason: "recording failed".into(),
                    },
                );
            }
            return finish(
                Hud::NotTranscribed("Recording failed. Retry from the menu".into()),
                ClaimWatch::new(None),
                None,
                None,
            );
        }
    };
    if cancelled() {
        let _ = staging.delete_dictation(&take.item_id);
        return;
    }
    let _ = staging.set_dictation_state(&take.item_id, DictationState::Processing);
    let ctx = take
        .capture
        .as_ref()
        .and_then(|rx| rx.recv_timeout(Duration::from_secs(1)).ok())
        .unwrap_or(CaptureContext {
            captured: Captured::Uncertain,
            epoch: deps.desktop.session_epoch(),
        });
    let produced = produce(&deps, &wav, &take.cfg);
    // A cancelled take is never delivered, even if a response arrived.
    if cancelled() {
        let _ = staging.delete_dictation(&take.item_id);
        return;
    }
    let p = match produced {
        Ok(p) => p,
        Err(fix) => {
            let _ = staging.set_dictation_state(
                &take.item_id,
                DictationState::Failed {
                    reason: fix.clone(),
                },
            );
            return finish(Hud::NotTranscribed(fix), ClaimWatch::new(None), None, None);
        }
    };
    let delivered = delivery::deliver(
        &p.text,
        &ctx,
        deps.desktop.as_ref(),
        deps.clipboard.as_ref(),
    );
    match delivered.outcome {
        // Even the copy failed: keep the audio staged for Retry / Discard.
        Outcome::Failed => {
            let _ = staging.set_dictation_state(&take.item_id, DictationState::NotDelivered);
        }
        _ => {
            let _ = staging.delete_dictation(&take.item_id);
        }
    }
    let hud = match (&delivered.hud, &p.note) {
        (Hud::Copied { .. } | Hud::NotDelivered, _) | (_, None) => delivered.hud.clone(),
        (_, Some(note)) => note.clone(),
    };
    let claim = if hud.claim().is_some() {
        ClaimWatch::new(delivered.clip.clone())
    } else {
        ClaimWatch::new(None)
    };
    let report = TakeReport {
        outcome: format!("{:?}", delivered.outcome),
        transcribe_ms: p.transcribe_ms,
        rewrite_ms: p.rewrite_ms,
        total_ms: ms(take.started.elapsed()),
        chars: p.text.chars().count(),
    };
    finish(hud, claim, Some(report), Some(p.text));
}
