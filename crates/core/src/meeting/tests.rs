//! Behavioral tests for the meeting pipeline and archive invariants, using a
//! fake two-track capture (synthetic tones) and a fake, call-counting speech.

use super::archive::{self, Fault};
use super::process::{Finish, MeetingSpeech, RetryPolicy};
use super::store::MeetingState;
use super::*;
use crate::ports::{HttpReply, MeetingTracks, Transport, TransportError};
use crate::speech::{Segment, SpeechError, Transcript};
use crate::surface::RecoveryKind;
use crate::wav;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{symlink, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

// ───────────────────────────── fakes ─────────────────────────────

struct NoTransport;
impl Transport for NoTransport {
    fn post_json(
        &self,
        _: &str,
        _: &str,
        _: &serde_json::Value,
        _: Option<Duration>,
    ) -> Result<HttpReply, TransportError> {
        panic!("tests never reach the network")
    }
}

struct Secrets(Mutex<Result<Option<String>, SecretError>>);
impl SecretStore for Secrets {
    fn get_key(&self) -> Result<Option<String>, SecretError> {
        self.0.lock().unwrap().clone()
    }
    fn set_key(&self, k: &str) -> Result<(), SecretError> {
        *self.0.lock().unwrap() = Ok(Some(k.into()));
        Ok(())
    }
    fn delete_key(&self) -> Result<(), SecretError> {
        *self.0.lock().unwrap() = Ok(None);
        Ok(())
    }
}

/// Writes its configured tracks when stopped (or at start when `eager`).
struct Capture {
    you: Vec<i16>,
    others: Vec<i16>,
    you_offset_ms: u64,
    others_offset_ms: u64,
    eager: bool,
    starts: AtomicUsize,
}

struct Rec {
    you: (PathBuf, Vec<i16>),
    others: (PathBuf, Vec<i16>),
    offsets: (u64, u64),
    started: Instant,
}

impl MeetingRecording for Rec {
    fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
    fn stop(self: Box<Self>) -> Result<MeetingTracks, String> {
        fs::write(&self.you.0, wav::encode_pcm16(&self.you.1, 16_000)).unwrap();
        fs::write(&self.others.0, wav::encode_pcm16(&self.others.1, 16_000)).unwrap();
        Ok(MeetingTracks {
            you: self.you.0.clone(),
            others: self.others.0.clone(),
            you_offset_ms: self.offsets.0,
            others_offset_ms: self.offsets.1,
        })
    }
}

impl MeetingCapture for Capture {
    fn start(
        &self,
        _device: Option<&str>,
        you: &Path,
        others: &Path,
    ) -> Result<Box<dyn MeetingRecording>, String> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        if self.eager {
            fs::write(you, wav::encode_pcm16(&self.you, 16_000)).unwrap();
            fs::write(others, wav::encode_pcm16(&self.others, 16_000)).unwrap();
        }
        Ok(Box::new(Rec {
            you: (you.to_path_buf(), self.you.clone()),
            others: (others.to_path_buf(), self.others.clone()),
            offsets: (self.you_offset_ms, self.others_offset_ms),
            started: Instant::now(),
        }))
    }
}

#[derive(Clone)]
enum Reply {
    Ok(Transcript),
    Err(SpeechError),
}

/// Answers per part name (file stem); counts every call.
#[derive(Default)]
struct Speech {
    replies: Mutex<BTreeMap<String, Vec<Reply>>>,
    transcribe_calls: Mutex<Vec<String>>,
    chat_calls: AtomicUsize,
    fail_chat: Mutex<bool>,
}

impl Speech {
    fn set(&self, part: &str, replies: Vec<Reply>) {
        self.replies.lock().unwrap().insert(part.into(), replies);
    }
    fn calls(&self) -> usize {
        self.transcribe_calls.lock().unwrap().len() + self.chat_calls.load(Ordering::SeqCst)
    }
    fn transcribed(&self, part: &str) -> usize {
        self.transcribe_calls
            .lock()
            .unwrap()
            .iter()
            .filter(|p| *p == part)
            .count()
    }
}

fn said(text: &str, segs: &[(u64, u64, &str)]) -> Reply {
    Reply::Ok(Transcript {
        text: text.into(),
        segments: segs
            .iter()
            .map(|(a, b, t)| Segment {
                start_ms: *a,
                end_ms: *b,
                text: (*t).into(),
            })
            .collect(),
        cost_usd: None,
    })
}

impl MeetingSpeech for Speech {
    fn transcribe_part(
        &self,
        _key: &str,
        wav_path: &Path,
        terms: &[String],
    ) -> Result<Transcript, SpeechError> {
        assert!(terms.iter().any(|t| t == "Scry"), "keyterms passed");
        let name = wav_path.file_stem().unwrap().to_str().unwrap().to_string();
        self.transcribe_calls.lock().unwrap().push(name.clone());
        let mut map = self.replies.lock().unwrap();
        let r = match map.get_mut(&name) {
            Some(v) if v.len() > 1 => v.remove(0),
            Some(v) => v[0].clone(),
            None => said(
                &format!("words of {name}"),
                &[(0, 900, &format!("words of {name}"))],
            ),
        };
        match r {
            Reply::Ok(t) => Ok(t),
            Reply::Err(e) => Err(e),
        }
    }

    fn chat(&self, _key: &str, system: &str, user: &str, _max: u32) -> Result<String, SpeechError> {
        self.chat_calls.fetch_add(1, Ordering::SeqCst);
        if *self.fail_chat.lock().unwrap() {
            return Err(SpeechError::Http {
                status: 400,
                retryable: false,
                retry_after: None,
            });
        }
        if system.starts_with("You clean up") {
            let src = user.split("Source:\n").nth(1).unwrap_or("");
            Ok(format!("{src}."))
        } else {
            Ok(r#"{"title":"Planning Sync","summary":"We planned the week.","decisions":["Ship Friday"],"actions":["Write the docs"]}"#.into())
        }
    }
}

// ───────────────────────────── harness ─────────────────────────────

struct H {
    _t: tempfile::TempDir,
    root: PathBuf,
    staging: Staging,
    folder: PathBuf,
    speech: Arc<Speech>,
    secrets: Arc<Secrets>,
    capture: Arc<Capture>,
}

fn tone(ms: u32) -> Vec<i16> {
    (0..16 * ms)
        .map(|i| ((i as f32 * 0.07).sin() * 0.3 * 32767.0) as i16)
        .collect()
}

fn silence(ms: u32) -> Vec<i16> {
    vec![0; (16 * ms) as usize]
}

impl H {
    fn new(you: Vec<i16>, others: Vec<i16>) -> H {
        Self::with_capture(you, others, false)
    }

    fn with_capture(you: Vec<i16>, others: Vec<i16>, eager: bool) -> H {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().to_path_buf();
        let staging = Staging::open(&root.join("staging")).unwrap();
        let folder = root.join("notes");
        fs::create_dir(&folder).unwrap();
        H {
            _t: t,
            root,
            staging,
            folder,
            speech: Arc::new(Speech::default()),
            secrets: Arc::new(Secrets(Mutex::new(Ok(Some("sk-test".into()))))),
            capture: Arc::new(Capture {
                you,
                others,
                you_offset_ms: 0,
                others_offset_ms: 250,
                eager,
                starts: AtomicUsize::new(0),
            }),
        }
    }

    fn meetings(&self) -> Meetings {
        let deps = MeetingDeps {
            capture: self.capture.clone(),
            secrets: self.secrets.clone(),
            openrouter: OpenRouter::with_base(Arc::new(NoTransport), "http://127.0.0.1:9"),
            staging: self.staging.clone(),
            notify: Arc::new(|| {}),
        };
        Meetings::with_speech(deps, self.speech.clone()).with_retry_policy(
            RetryPolicy {
                attempts: 3,
                base_delay: Duration::from_millis(1),
                max_delay: Duration::from_millis(5),
            },
            Arc::new(|_| {}),
        )
    }

    /// Record and stop one meeting into `folder`; wait for processing.
    fn run(&self, m: &Meetings, folder: &Path) -> String {
        m.start(None, crate::vocabulary::seed(), folder.to_path_buf())
            .unwrap();
        let id = m
            .inner
            .st
            .lock()
            .unwrap()
            .active
            .as_ref()
            .unwrap()
            .id
            .clone();
        m.stop();
        assert!(m.wait_idle(Duration::from_secs(20)));
        id
    }

    fn store(&self) -> MeetingStore {
        MeetingStore::new(self.staging.meetings_dir())
    }

    fn state(&self, id: &str) -> MeetingState {
        self.store().load(id).unwrap().state
    }

    fn trap(&self) -> PathBuf {
        let p = self.staging.root().join("trap");
        fs::create_dir_all(&p).unwrap();
        p
    }
}

/// Every entry beneath `root` without following symlinks:
/// (relative path, kind, inode, mtime ns, size, content digest).
fn snapshot(root: &Path) -> BTreeMap<PathBuf, (String, u64, i64, u64, String)> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, (String, u64, i64, u64, String)>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let m = fs::symlink_metadata(&p).unwrap();
            let ft = m.file_type();
            let (kind, digest) = if ft.is_symlink() {
                ("link".into(), format!("{:?}", fs::read_link(&p).unwrap()))
            } else if ft.is_dir() {
                ("dir".into(), String::new())
            } else if ft.is_file() {
                ("file".into(), archive::digest(&fs::read(&p).unwrap()))
            } else {
                ("special".into(), String::new())
            };
            let mtime = m.mtime() * 1_000_000_000 + m.mtime_nsec();
            out.insert(
                p.strip_prefix(root).unwrap().to_path_buf(),
                (kind, m.ino(), mtime, m.size(), digest),
            );
            if ft.is_dir() {
                walk(root, &p, out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn notes_under(dir: &Path) -> Vec<PathBuf> {
    snapshot(dir)
        .into_iter()
        .filter(|(p, v)| v.0 == "file" && p.extension().is_some_and(|e| e == "md"))
        .map(|(p, _)| dir.join(p))
        .collect()
}

fn mm_dir(folder: &Path, unix: u64) -> PathBuf {
    let (y, m) = time::year_month(unix);
    folder.join("cantrip-meeting").join(y).join(m)
}

// ───────────────────────────── pipeline ─────────────────────────────

#[test]
fn complete_meeting_merges_tracks_publishes_and_archives() {
    let h = H::new(tone(7000), tone(7000));
    h.speech.set(
        "you-000",
        vec![said(
            "",
            &[(1000, 2000, "um so Scry is ready"), (5000, 6000, "ship it")],
        )],
    );
    // Others offset 250 ms: 2750 → 3000, 4750 → 5000 (tie with You's 5000).
    h.speech.set(
        "others-000",
        vec![said("", &[(2750, 3500, "great"), (4750, 5500, "agreed")])],
    );
    let m = h.meetings();
    let id = h.run(&m, &h.folder.clone());
    let Some(Finish::Archived(path)) = m.last_finish() else {
        panic!("{:?}", m.last_finish())
    };
    assert_eq!(m.last_published(), Some(path.clone()));
    let meta_stamp = path.file_name().unwrap().to_str().unwrap().to_string();
    assert!(meta_stamp.ends_with("Z-planning-sync.md"), "{meta_stamp}");
    assert!(path.starts_with(h.folder.join("cantrip-meeting")));
    let note = fs::read_to_string(&path).unwrap();
    assert!(note.starts_with("---\nsource: \"cantrip-meeting\"\nsource_id: \""));
    assert!(note.contains("\nstatus: complete\n"));
    assert!(note.contains("# Planning Sync\n\n## Summary\n\nWe planned the week.\n\n## Decisions\n\n- Ship Friday\n\n## Actions\n\n- [ ] Write the docs\n\n## Transcript\n\n"));
    let transcript = note.split("## Transcript\n\n").nth(1).unwrap();
    assert_eq!(
        transcript,
        "[00:01] You: um so Scry is ready.\n\n[00:03] Others: great.\n\n[00:05] You: ship it.\n\n[00:05] Others: agreed.\n"
    );
    // Staging deleted only after verification; one file in the folder.
    assert!(!h.store().exists(&id));
    assert_eq!(notes_under(&h.folder), vec![path]);
    assert!(m.recoverable().is_empty());
}

#[test]
fn rebuilding_from_the_same_staging_is_byte_identical() {
    let h = H::new(tone(2000), tone(2000));
    // Keep staging by publishing into a folder that does not exist.
    let m = h.meetings();
    let id = h.run(&m, &h.root.join("missing"));
    assert_eq!(h.state(&id), MeetingState::PendingArchive);
    let staged = h.store().note(&id).unwrap();
    let calls = h.speech.calls();
    let meta = h.store().load(&id).unwrap();
    let parts = h.store().parts(&id).unwrap();
    let results = h.store().results(&id, &parts);
    let (again, _) = m
        .inner
        .ctx_run(|c| c.build_note(&meta, &parts, &results, Some("sk-test"), false))
        .unwrap();
    assert_eq!(again, staged);
    assert_eq!(h.speech.calls(), calls, "rebuild uses only staged results");
}

#[test]
fn unresolved_voiced_part_blocks_completion_and_keeps_its_audio() {
    let h = H::new(tone(2000), tone(2000));
    h.speech.set(
        "others-000",
        vec![Reply::Err(SpeechError::Http {
            status: 500,
            retryable: true,
            retry_after: None,
        })],
    );
    let m = h.meetings();
    let id = h.run(&m, &h.folder.clone());
    assert_eq!(h.state(&id), MeetingState::PartsFailed);
    assert_eq!(h.speech.transcribed("others-000"), 3, "bounded retries");
    assert_eq!(h.speech.transcribed("you-000"), 1);
    assert!(notes_under(&h.folder).is_empty(), "no complete note");
    assert!(h.store().part_wav(&id, "others-000").exists());
    assert!(h.store().you_wav(&id).exists() && h.store().others_wav(&id).exists());
    assert_eq!(
        m.recoverable()[0].kind,
        RecoveryKind::MeetingPartsFailed { failed_parts: 1 }
    );
}

#[test]
fn empty_transcript_despite_speech_is_unresolved_and_4xx_is_not_retried() {
    let h = H::new(tone(2000), tone(2000));
    h.speech.set("you-000", vec![said("  ", &[])]);
    h.speech.set(
        "others-000",
        vec![Reply::Err(SpeechError::Http {
            status: 400,
            retryable: false,
            retry_after: None,
        })],
    );
    let m = h.meetings();
    let id = h.run(&m, &h.folder.clone());
    assert_eq!(h.state(&id), MeetingState::PartsFailed);
    assert_eq!(h.store().load(&id).unwrap().failed_parts, 2);
    assert_eq!(h.speech.transcribed("others-000"), 1);
}

#[test]
fn transient_errors_retry_then_resolve() {
    let h = H::new(tone(2000), silence(2000));
    h.speech.set(
        "you-000",
        vec![
            Reply::Err(SpeechError::Http {
                status: 429,
                retryable: true,
                retry_after: Some(Duration::from_millis(2)),
            }),
            Reply::Err(SpeechError::Offline("dns".into())),
            said("hello", &[(0, 500, "hello")]),
        ],
    );
    let m = h.meetings();
    h.run(&m, &h.folder.clone());
    assert!(matches!(m.last_finish(), Some(Finish::Archived(_))));
    assert_eq!(h.speech.transcribed("you-000"), 3);
}

#[test]
fn silent_part_completes_empty_without_a_call() {
    let h = H::new(tone(2000), silence(2000));
    let m = h.meetings();
    h.run(&m, &h.folder.clone());
    assert!(matches!(m.last_finish(), Some(Finish::Archived(_))));
    assert_eq!(h.speech.transcribed("others-000"), 0);
    assert_eq!(h.speech.transcribed("you-000"), 1);
}

#[test]
fn no_timestamps_still_place_a_180ms_no_at_its_own_start() {
    let mut you = silence(1000);
    you.extend(tone(180));
    you.extend(silence(1000));
    let h = H::new(you, silence(2000));
    // The leading second of silence is its own (silent, never sent) part.
    h.speech.set("you-001", vec![said("No.", &[])]);
    let m = h.meetings();
    h.run(&m, &h.folder.clone());
    let Some(Finish::Archived(p)) = m.last_finish() else {
        panic!()
    };
    let note = fs::read_to_string(p).unwrap();
    assert!(note.contains("[00:00] You: No.."), "{note}");
}

#[test]
fn failed_cleanup_marks_the_turn_and_failed_summary_says_so() {
    let h = H::new(tone(2000), silence(2000));
    *h.speech.fail_chat.lock().unwrap() = true;
    let m = h.meetings();
    h.run(&m, &h.folder.clone());
    let Some(Finish::Archived(p)) = m.last_finish() else {
        panic!("{:?}", m.last_finish())
    };
    let note = fs::read_to_string(&p).unwrap();
    assert!(note.contains("[00:00] You: [cleanup failed: 00:00–00:01] words of you-000"));
    assert!(note.contains("# Meeting\n\n## Summary\n\nSummary failed\n"));
    assert!(p.to_str().unwrap().ends_with("Z-meeting.md"));
}

#[test]
fn save_partial_preserves_staging_and_retry_completes_without_redoing_parts() {
    let h = H::new(tone(2000), tone(2000));
    h.speech.set(
        "others-000",
        vec![Reply::Err(SpeechError::Transport("reset".into()))],
    );
    let m = h.meetings();
    let id = h.run(&m, &h.folder.clone());
    assert_eq!(h.state(&id), MeetingState::PartsFailed);
    let staged_before = snapshot(&h.store().meeting_dir(&id).join("parts"));

    m.save_partial(&id, h.folder.clone());
    assert!(m.wait_idle(Duration::from_secs(10)));
    let Some(Finish::PartialSaved(partial)) = m.last_finish() else {
        panic!("{:?}", m.last_finish())
    };
    assert!(partial
        .to_str()
        .unwrap()
        .ends_with("-planning-sync-partial.md"));
    let pnote = fs::read_to_string(&partial).unwrap();
    assert!(pnote.contains("\nstatus: partial\n"));
    assert!(
        pnote.contains("[00:00] Others: [transcription failed: 00:00–00:03]"),
        "{pnote}"
    );
    assert_eq!(h.state(&id), MeetingState::PartsFailed);
    assert_eq!(
        snapshot(&h.store().meeting_dir(&id).join("parts")),
        staged_before
    );
    assert!(m.recoverable().iter().any(|i| i.id == id));

    // A second save takes the next partial name.
    m.save_partial(&id, h.folder.clone());
    assert!(m.wait_idle(Duration::from_secs(10)));
    let Some(Finish::PartialSaved(p2)) = m.last_finish() else {
        panic!()
    };
    assert!(p2.to_str().unwrap().ends_with("-partial-2.md"));

    // Retry: only the failed part is transcribed again; partials untouched.
    let snap_partial = snapshot(partial.parent().unwrap());
    h.speech
        .set("others-000", vec![said("fixed", &[(0, 500, "fixed")])]);
    m.retry(&id, h.folder.clone());
    assert!(m.wait_idle(Duration::from_secs(10)));
    let Some(Finish::Archived(complete)) = m.last_finish() else {
        panic!("{:?}", m.last_finish())
    };
    assert!(complete.to_str().unwrap().ends_with("-planning-sync.md"));
    assert_eq!(h.speech.transcribed("you-000"), 1);
    let after = snapshot(partial.parent().unwrap());
    for (k, v) in &snap_partial {
        assert_eq!(after.get(k), Some(v), "pre-existing {k:?} unchanged");
    }
    assert!(!h.store().exists(&id));
}

// ───────────────────────────── archive invariants ─────────────────────────────

/// Get a meeting to `parts-failed` (others fails) in `folder`.
fn parts_failed(h: &H, m: &Meetings) -> String {
    h.speech.set(
        "others-000",
        vec![Reply::Err(SpeechError::Http {
            status: 400,
            retryable: false,
            retry_after: None,
        })],
    );
    h.run(m, &h.folder.clone())
}

#[test]
fn symlinked_publication_dirs_into_staging_get_nothing_created_or_written() {
    for level in 0..3 {
        let h = H::new(tone(1500), silence(1500));
        let trap = h.trap();
        let now = time::now_unix();
        let (y, mo) = time::year_month(now);
        let link_at = match level {
            0 => h.folder.join("cantrip-meeting"),
            1 => {
                fs::create_dir(h.folder.join("cantrip-meeting")).unwrap();
                h.folder.join("cantrip-meeting").join(&y)
            }
            _ => {
                fs::create_dir_all(h.folder.join("cantrip-meeting").join(&y)).unwrap();
                h.folder.join("cantrip-meeting").join(&y).join(&mo)
            }
        };
        symlink(&trap, &link_at).unwrap();
        let folder_before = snapshot(&h.folder);
        let m = h.meetings();
        let id = h.run(&m, &h.folder.clone());
        assert_eq!(h.state(&id), MeetingState::PendingArchive, "level {level}");
        assert_eq!(fs::read_dir(&trap).unwrap().count(), 0, "level {level}");
        assert_eq!(snapshot(&h.folder), folder_before, "level {level}");
        assert!(h.store().note(&id).is_some(), "staged note intact");

        // A partial note under the same overlap stays parts-failed, writes nothing.
        h.speech.set(
            "you-000",
            vec![Reply::Err(SpeechError::Http {
                status: 400,
                retryable: false,
                retry_after: None,
            })],
        );
        let id2 = h.run(&m, &h.folder.clone());
        assert_eq!(h.state(&id2), MeetingState::PartsFailed);
        m.save_partial(&id2, h.folder.clone());
        assert!(m.wait_idle(Duration::from_secs(10)));
        assert!(matches!(
            m.last_finish(),
            Some(Finish::PartialBlocked(archive::Blocked::Overlap))
        ));
        assert_eq!(h.state(&id2), MeetingState::PartsFailed);
        assert_eq!(fs::read_dir(&trap).unwrap().count(), 0);
        assert_eq!(snapshot(&h.folder), folder_before);
    }
}

#[test]
fn chosen_folder_overlapping_staging_gets_zero_writes() {
    let h = H::new(tone(1500), silence(1500));
    let inside = h.staging.root().join("notes-inside");
    fs::create_dir(&inside).unwrap();
    assert_eq!(
        check_folder_choice(&inside, &h.staging),
        Err(FOLDER_OVERLAP.to_string())
    );
    assert_eq!(
        check_folder_choice(&h.staging.root().join("not/yet"), &h.staging),
        Err(FOLDER_OVERLAP.to_string())
    );
    assert_eq!(
        check_folder_choice(&h.root, &h.staging),
        Err(FOLDER_OVERLAP.to_string()),
        "a folder containing staging overlaps"
    );
    assert_eq!(check_folder_choice(&h.folder, &h.staging), Ok(()));
    let link = h.root.join("sneaky");
    symlink(&inside, &link).unwrap();
    assert!(check_folder_choice(&link, &h.staging).is_err());

    let m = h.meetings();
    let id = h.run(&m, &link);
    assert_eq!(h.state(&id), MeetingState::PendingArchive);
    assert_eq!(fs::read_dir(&inside).unwrap().count(), 0);
    assert!(m
        .recoverable()
        .iter()
        .any(|i| i.kind == RecoveryKind::MeetingPendingArchive));
}

#[test]
fn chosen_folder_is_never_created() {
    let h = H::new(tone(1500), silence(1500));
    let missing = h.root.join("not-there");
    let m = h.meetings();
    let id = h.run(&m, &missing);
    assert_eq!(h.state(&id), MeetingState::PendingArchive);
    assert!(!missing.exists());
}

/// Get a complete note to pending-archive with a recorded publish path, by
/// injecting a verification failure on the first publication.
fn pending_with_recorded(h: &H, m: &Meetings, fault: Fault) -> (String, PathBuf) {
    m.inject_fault(fault);
    let id = h.run(m, &h.folder.clone());
    assert_eq!(h.state(&id), MeetingState::PendingArchive);
    let rec = h.store().load(&id).unwrap().publish_path.unwrap();
    assert!(rec.exists());
    (id, rec)
}

#[test]
fn injected_verify_or_flush_failure_keeps_staging_and_retry_publishes_with_zero_calls() {
    for fault in [Fault::Verify, Fault::Flush] {
        let h = H::new(tone(1500), silence(1500));
        let m = h.meetings();
        let (id, rec) = pending_with_recorded(&h, &m, fault);
        assert!(h.store().note(&id).is_some());
        assert!(h.store().you_wav(&id).exists());
        let calls = h.speech.calls();
        m.retry(&id, h.folder.clone());
        assert!(m.wait_idle(Duration::from_secs(10)));
        assert_eq!(m.last_finish(), Some(Finish::Archived(rec.clone())));
        assert_eq!(
            h.speech.calls(),
            calls,
            "publish-only Retry makes zero API calls"
        );
        assert!(!h.store().exists(&id));
        assert_eq!(notes_under(&h.folder), vec![rec]);
    }
}

#[test]
fn publish_only_retry_to_unwritable_folder_makes_zero_calls_then_publishes() {
    let h = H::new(tone(1500), silence(1500));
    let m = h.meetings();
    let missing = h.root.join("offline-share");
    let id = h.run(&m, &missing);
    assert_eq!(h.state(&id), MeetingState::PendingArchive);
    let calls = h.speech.calls();
    m.retry(&id, missing.clone());
    assert!(m.wait_idle(Duration::from_secs(10)));
    assert_eq!(h.state(&id), MeetingState::PendingArchive);
    fs::create_dir(&missing).unwrap();
    m.retry(&id, missing.clone());
    assert!(m.wait_idle(Duration::from_secs(10)));
    assert!(matches!(m.last_finish(), Some(Finish::Archived(_))));
    assert_eq!(h.speech.calls(), calls);
}

#[test]
fn recorded_path_resolving_into_staging_is_refused_and_disjoint_folder_gets_fresh_note() {
    let h = H::new(tone(1500), silence(1500));
    let m = h.meetings();
    let (id, rec) = pending_with_recorded(&h, &m, Fault::Verify);
    // The user's MM directory is now a symlink into staging holding a
    // byte-identical copy under the recorded name.
    let mm = rec.parent().unwrap().to_path_buf();
    let trap = h.trap();
    let copy = trap.join(rec.file_name().unwrap());
    fs::copy(&rec, &copy).unwrap();
    fs::rename(&mm, h.root.join("moved-away")).unwrap();
    symlink(&trap, &mm).unwrap();
    let trap_before = snapshot(&trap);

    // Current folder also resolves through the same link: overlap → nothing.
    let folder_before = snapshot(&h.folder);
    let stage_before = snapshot(&h.store().meeting_dir(&id));
    m.retry(&id, h.folder.clone());
    assert!(m.wait_idle(Duration::from_secs(10)));
    assert!(matches!(
        m.last_finish(),
        Some(Finish::PendingArchive(archive::Blocked::Overlap))
    ));
    assert_eq!(h.state(&id), MeetingState::PendingArchive);
    assert_eq!(
        snapshot(&h.folder),
        folder_before,
        "zero create/write/delete"
    );
    assert_eq!(snapshot(&trap), trap_before);
    let stage_after = snapshot(&h.store().meeting_dir(&id));
    for k in ["note.md", "you.wav", "others.wav", "parts.json"] {
        assert_eq!(
            stage_after.get(Path::new(k)),
            stage_before.get(Path::new(k)),
            "{k}"
        );
    }

    // A disjoint folder chosen now gets a fresh, verified publication.
    let folder2 = h.root.join("notes2");
    fs::create_dir(&folder2).unwrap();
    m.retry(&id, folder2.clone());
    assert!(m.wait_idle(Duration::from_secs(10)));
    let Some(Finish::Archived(fresh)) = m.last_finish() else {
        panic!("{:?}", m.last_finish())
    };
    assert!(fresh.starts_with(&folder2));
    assert_eq!(snapshot(&trap), trap_before, "refused path left untouched");
    assert!(!h.store().exists(&id));
}

#[test]
fn fifo_at_recorded_path_neither_hangs_nor_is_replaced() {
    let h = H::new(tone(1500), silence(1500));
    let m = h.meetings();
    let (id, rec) = pending_with_recorded(&h, &m, Fault::Verify);
    fs::remove_file(&rec).unwrap();
    let c = std::ffi::CString::new(rec.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);

    let t = Instant::now();
    let r = archive::verify_and_flush(&rec, "x", &[], None);
    assert!(r.is_err());
    assert!(
        t.elapsed() < Duration::from_millis(100),
        "{:?}",
        t.elapsed()
    );

    let t = Instant::now();
    m.retry(&id, h.folder.clone());
    assert!(m.wait_idle(Duration::from_secs(10)));
    assert!(t.elapsed() < Duration::from_secs(2));
    let Some(Finish::Archived(fresh)) = m.last_finish() else {
        panic!("{:?}", m.last_finish())
    };
    assert_ne!(fresh, rec);
    assert!(fresh.to_str().unwrap().ends_with("-2.md"));
    assert!(fs::symlink_metadata(&rec)
        .unwrap()
        .file_type()
        .is_fifo_like());
}

trait FifoLike {
    fn is_fifo_like(&self) -> bool;
}
impl FifoLike for fs::FileType {
    fn is_fifo_like(&self) -> bool {
        use std::os::unix::fs::FileTypeExt;
        self.is_fifo()
    }
}

#[test]
fn symlink_at_recorded_path_is_not_followed() {
    let h = H::new(tone(1500), silence(1500));
    let m = h.meetings();
    let (id, rec) = pending_with_recorded(&h, &m, Fault::Verify);
    let outside = h.root.join("outside.md");
    fs::copy(&rec, &outside).unwrap();
    fs::remove_file(&rec).unwrap();
    symlink(&outside, &rec).unwrap();
    let outside_before = snapshot(&h.root.join("notes"));
    m.retry(&id, h.folder.clone());
    assert!(m.wait_idle(Duration::from_secs(10)));
    let Some(Finish::Archived(fresh)) = m.last_finish() else {
        panic!()
    };
    assert_ne!(fresh, rec);
    assert!(fs::symlink_metadata(&rec).unwrap().file_type().is_symlink());
    let after = snapshot(&h.root.join("notes"));
    for (k, v) in outside_before.iter().filter(|(_, v)| v.0 != "dir") {
        assert_eq!(after.get(k), Some(v));
    }
}

#[test]
fn taken_name_gets_suffix_and_preexisting_files_are_untouched_with_no_temp_files() {
    let h = H::new(tone(1500), silence(1500));
    let now = time::now_unix();
    let dir = mm_dir(&h.folder, now);
    fs::create_dir_all(&dir).unwrap();
    // Pre-create the first candidate name for every second around now.
    let mut pre = Vec::new();
    for dt in 0..5 {
        let p = dir.join(archive::candidate_name(
            &time::file_stamp(now + dt),
            "planning-sync",
            false,
            1,
        ));
        fs::write(&p, format!("user's own file {dt}")).unwrap();
        pre.push(p);
    }
    fs::write(h.folder.join("unrelated.txt"), "keep").unwrap();
    let before = snapshot(&h.folder);
    let m = h.meetings();
    h.run(&m, &h.folder.clone());
    let Some(Finish::Archived(p)) = m.last_finish() else {
        panic!("{:?}", m.last_finish())
    };
    assert!(
        p.to_str().unwrap().ends_with("-planning-sync-2.md"),
        "{p:?}"
    );
    let after = snapshot(&h.folder);
    for (k, v) in &before {
        if v.0 == "file" {
            assert_eq!(after.get(k), Some(v), "pre-existing {k:?} byte-identical");
        }
    }
    let new: Vec<_> = after.keys().filter(|k| !before.contains_key(*k)).collect();
    assert_eq!(new.len(), 1, "only the note appeared: {new:?}");
    assert!(after.keys().all(|k| !k.to_str().unwrap().contains(".tmp")
        && !k.file_name().unwrap().to_str().unwrap().starts_with('.')));
}

// ───────────────────────────── facade ─────────────────────────────

#[test]
fn start_without_key_refuses_and_records_nothing() {
    let h = H::new(tone(500), silence(500));
    *h.secrets.0.lock().unwrap() = Ok(None);
    let m = h.meetings();
    assert_eq!(
        m.start(None, vec![], h.folder.clone()),
        Err(NEED_KEY.to_string())
    );
    *h.secrets.0.lock().unwrap() = Err(SecretError::Unavailable("locked".into()));
    assert!(m.start(None, vec![], h.folder.clone()).is_err());
    assert_eq!(h.capture.starts.load(Ordering::SeqCst), 0);
    assert_eq!(fs::read_dir(h.staging.meetings_dir()).unwrap().count(), 0);
    assert!(!m.active());
}

#[test]
fn crash_mid_meeting_is_interrupted_at_relaunch_and_retry_finishes_it() {
    let h = H::with_capture(tone(1500), silence(1500), true);
    {
        let m = h.meetings();
        m.start(None, crate::vocabulary::seed(), h.folder.clone())
            .unwrap();
        assert!(m.active());
        assert!(m.elapsed().is_some());
        // Simulated crash: the process goes away without stop().
        std::mem::forget(m);
    }
    let m = h.meetings();
    let items = m.recoverable();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].kind, RecoveryKind::MeetingInterrupted);
    m.retry(&items[0].id, h.folder.clone());
    assert!(m.wait_idle(Duration::from_secs(10)));
    assert!(matches!(m.last_finish(), Some(Finish::Archived(_))));
    assert!(m.recoverable().is_empty());
}

#[test]
fn publishing_found_at_launch_becomes_pending_archive() {
    let h = H::new(tone(1500), silence(1500));
    let m = h.meetings();
    let id = h.run(&m, &h.root.join("missing"));
    h.store().set_state(&id, MeetingState::Publishing).unwrap();
    let m = h.meetings();
    assert_eq!(h.state(&id), MeetingState::PendingArchive);
    assert_eq!(m.recoverable()[0].kind, RecoveryKind::MeetingPendingArchive);
}

#[test]
fn discard_removes_staging_and_never_touches_the_folder() {
    let h = H::new(tone(1500), tone(1500));
    let m = h.meetings();
    let id = parts_failed(&h, &m);
    m.save_partial(&id, h.folder.clone());
    assert!(m.wait_idle(Duration::from_secs(10)));
    let before = snapshot(&h.folder);
    m.discard(&id);
    assert!(!h.store().exists(&id));
    assert_eq!(snapshot(&h.folder), before);
    assert!(m.recoverable().is_empty());
}
