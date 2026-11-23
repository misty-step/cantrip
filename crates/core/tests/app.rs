//! The running app's control surface, driven exactly as the UI and hotkey
//! backends drive it: shortcut events in, commands in, snapshots out.
mod support;

use cantrip_core::app::*;
use cantrip_core::dictation::DictationDeps;
use cantrip_core::meeting::{MeetingDeps, FOLDER_OVERLAP, NEED_KEY};
use cantrip_core::ports::*;
use cantrip_core::settings::Settings;
use cantrip_core::shortcut::KeyEvent;
use cantrip_core::speech::{Mode, OpenRouter};
use cantrip_core::staging::Staging;
use cantrip_core::surface::{Hud, RecoveryAction};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use support::*;

struct NoMeetingCapture;
impl MeetingCapture for NoMeetingCapture {
    fn start(
        &self,
        _: Option<&str>,
        _: &Path,
        _: &Path,
    ) -> Result<Box<dyn MeetingRecording>, String> {
        Err("no capture in this test".into())
    }
}

#[derive(Default)]
struct FakeLogin(AtomicBool);
impl LoginItem for FakeLogin {
    fn enabled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    fn set_enabled(&self, on: bool) -> Result<(), String> {
        self.0.store(on, Ordering::SeqCst);
        Ok(())
    }
}

struct Rig {
    app: AppCore,
    desktop: Arc<FakeDesktop>,
    transport: Arc<FakeTransport>,
    staging: Staging,
    settings_path: std::path::PathBuf,
    dir: tempfile::TempDir,
}

fn rig(transport: Arc<FakeTransport>, secrets: Arc<FakeSecrets>) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let staging = Staging::open(&dir.path().join("stage")).unwrap();
    let desktop = FakeDesktop::new(target_a());
    let clip = FakeClipboard::new();
    *desktop.clipboard.lock().unwrap() = Some(clip.clone());
    let openrouter = OpenRouter::with_base(transport.clone(), "http://stub");
    let settings_path = dir.path().join("config/settings.json");
    std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
    let app = AppCore::new(AppDeps {
        dictation: DictationDeps {
            recorder: FakeRecorder::new(),
            desktop: desktop.clone(),
            clipboard: clip,
            secrets: secrets.clone(),
            local: FakeLocal::new(LocalAvailability::NoOnDeviceEnglish),
            openrouter: openrouter.clone(),
            staging: staging.clone(),
            notify: Arc::new(|| {}),
        },
        meeting: MeetingDeps {
            capture: Arc::new(NoMeetingCapture),
            secrets: secrets.clone(),
            openrouter,
            staging: staging.clone(),
            notify: Arc::new(|| {}),
        },
        recorder: FakeRecorder::new(),
        secrets,
        login: Arc::new(FakeLogin::default()),
        settings_path: settings_path.clone(),
        home: dir.path().to_path_buf(),
    })
    .unwrap();
    Rig {
        app,
        desktop,
        transport,
        staging,
        settings_path,
        dir,
    }
}

fn clean_speech() -> Arc<FakeTransport> {
    speech("open scrum and search", || {
        ok(&chat_json("Open Scry and search.", "stop"))
    })
}

fn idle(r: &Rig) {
    assert!(r.app.dictation().wait_idle(Duration::from_secs(10)));
}

#[test]
fn hold_to_talk_through_the_shortcut_inserts_the_rewrite() {
    let r = rig(clean_speech(), FakeSecrets::with_key());
    let t = Instant::now();
    r.app.on_key(KeyEvent::Pressed(t));
    assert_eq!(r.app.snapshot().hud, Hud::Listening);
    assert!(r.app.wants_escape());
    r.app
        .on_key(KeyEvent::Released(t + Duration::from_millis(450)));
    idle(&r);
    assert_eq!(r.app.snapshot().hud, Hud::Inserted);
    assert!(r.desktop.field_text().contains("Open Scry and search."));
    assert!(r.app.snapshot().has_last_dictation);
    assert!(r.app.snapshot().recovery.is_none());
}

#[test]
fn tap_starts_and_the_next_press_stops() {
    let r = rig(clean_speech(), FakeSecrets::with_key());
    let t = Instant::now();
    r.app.on_key(KeyEvent::Pressed(t));
    r.app
        .on_key(KeyEvent::Released(t + Duration::from_millis(120)));
    assert!(r.app.dictation().recording(), "a tap keeps recording");
    r.app.on_key(KeyEvent::Pressed(t + Duration::from_secs(3)));
    r.app
        .on_key(KeyEvent::Released(t + Duration::from_millis(3100)));
    idle(&r);
    assert_eq!(r.desktop.inserts(), 1);
}

#[test]
fn escape_while_transcribing_discards_the_take() {
    let t = FakeTransport::new(|url, _| {
        std::thread::sleep(Duration::from_millis(400));
        if url.ends_with("/audio/transcriptions") {
            ok(&transcript_json("late words"))
        } else {
            ok(&chat_json("Late words.", "stop"))
        }
    });
    let r = rig(t, FakeSecrets::with_key());
    let now = Instant::now();
    r.app.on_key(KeyEvent::Pressed(now));
    r.app
        .on_key(KeyEvent::Released(now + Duration::from_millis(500)));
    std::thread::sleep(Duration::from_millis(50));
    assert!(r.app.wants_escape(), "Escape is grabbed while transcribing");
    r.app.on_key(KeyEvent::Escape);
    idle(&r);
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(r.desktop.inserts() + r.desktop.pastes(), 0);
    assert!(r.app.snapshot().recovery.is_none());
    assert_eq!(r.app.snapshot().hud, Hud::Hidden);
    assert!(!r.app.wants_escape());
}

#[test]
fn failed_take_shows_one_recovery_row_and_confirmed_discard_clears_it() {
    let r = rig(
        FakeTransport::new(|_, _| status(500)),
        FakeSecrets::with_key(),
    );
    assert!(r.app.snapshot().recovery.is_none());
    r.app.send(Command::ToggleDictation);
    r.app.send(Command::ToggleDictation);
    idle(&r);
    let row = r
        .app
        .snapshot()
        .recovery
        .expect("staged item shows the row");
    assert!(row.text.starts_with("⚠ Dictation ") && row.text.ends_with(" failed"));
    assert_eq!(
        row.actions,
        vec![RecoveryAction::Retry, RecoveryAction::Discard]
    );
    r.app.send(Command::Recovery {
        item_id: row.item_id,
        action: RecoveryAction::Discard,
    });
    assert!(r.app.snapshot().recovery.is_none());
    assert!(r.staging.recoverable_dictations().is_empty());
}

#[test]
fn retry_from_the_row_copies_and_clears_the_item() {
    let fail = Arc::new(AtomicBool::new(true));
    let f = fail.clone();
    let t = FakeTransport::new(move |url, _| {
        if f.load(Ordering::SeqCst) {
            status(503)
        } else if url.ends_with("/audio/transcriptions") {
            ok(&transcript_json("hello"))
        } else {
            ok(&chat_json("Hello.", "stop"))
        }
    });
    let r = rig(t, FakeSecrets::with_key());
    r.app.send(Command::ToggleDictation);
    r.app.send(Command::ToggleDictation);
    idle(&r);
    let row = r.app.snapshot().recovery.unwrap();
    fail.store(false, Ordering::SeqCst);
    r.app.send(Command::Recovery {
        item_id: row.item_id,
        action: RecoveryAction::Retry,
    });
    idle(&r);
    assert!(r.app.snapshot().recovery.is_none());
    assert_eq!(
        r.desktop.inserts() + r.desktop.pastes(),
        0,
        "Retry never pastes"
    );
}

#[test]
fn mode_and_post_processing_persist_and_raw_skips_the_llm() {
    let r = rig(clean_speech(), FakeSecrets::with_key());
    r.app.send(Command::SetMode(Mode::Raw));
    let saved = Settings::load(&r.settings_path).unwrap();
    assert_eq!(saved.mode, Mode::Raw);
    r.app.send(Command::ToggleDictation);
    r.app.send(Command::ToggleDictation);
    idle(&r);
    assert_eq!(r.transport.count("/chat/completions"), 0);
    r.app.send(Command::SetMode(Mode::Clean));
    r.app.send(Command::SetPostProcessing(false));
    r.app.send(Command::ToggleDictation);
    r.app.send(Command::ToggleDictation);
    idle(&r);
    assert_eq!(r.transport.count("/chat/completions"), 0);
    assert!(!Settings::load(&r.settings_path).unwrap().post_processing);
}

#[test]
fn meeting_folder_inside_staging_is_rejected_inline() {
    let r = rig(clean_speech(), FakeSecrets::with_key());
    let before = r.app.snapshot().meeting_folder;
    r.app.send(Command::ChooseMeetingFolder(
        r.staging.root().join("meetings"),
    ));
    let s = r.app.snapshot();
    assert_eq!(s.folder_error.as_deref(), Some(FOLDER_OVERLAP));
    assert_eq!(s.meeting_folder, before);
    let good = r.dir.path().join("notes");
    std::fs::create_dir(&good).unwrap();
    r.app.send(Command::ChooseMeetingFolder(good.clone()));
    let s = r.app.snapshot();
    assert!(s.folder_error.is_none());
    assert_eq!(s.meeting_folder, good);
}

#[test]
fn meeting_without_a_key_refuses_on_the_hud() {
    let r = rig(clean_speech(), FakeSecrets::none());
    r.app.send(Command::ToggleMeeting);
    assert!(!r.app.snapshot().meeting_active);
    assert_eq!(r.app.snapshot().hud, Hud::Notice(NEED_KEY.into()));
}

#[test]
fn key_state_is_saved_never_shown_and_store_errors_are_errors() {
    let r = rig(clean_speech(), FakeSecrets::none());
    r.app.refresh();
    assert_eq!(r.app.snapshot().key, KeyState::NotSet);
    r.app.send(Command::SaveKey("  sk-or-new  ".into()));
    std::thread::sleep(Duration::from_millis(100)); // saved off the UI thread
    let s = r.app.snapshot();
    assert_eq!(s.key, KeyState::Saved);
    assert!(
        !format!("{s:?}").contains("sk-or-new"),
        "the key never reaches the UI"
    );
    r.app.send(Command::RemoveKey);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(r.app.snapshot().key, KeyState::NotSet);

    let broken = rig(clean_speech(), FakeSecrets::broken());
    broken.app.refresh();
    assert!(matches!(broken.app.snapshot().key, KeyState::Error(_)));
}

#[test]
fn vocabulary_shortcut_and_login_commands_update_settings() {
    let r = rig(clean_speech(), FakeSecrets::with_key());
    r.app.send(Command::AddTerm("Hex Board".into()));
    r.app.send(Command::RemoveTerm("Scry".into()));
    let v = r.app.snapshot().vocabulary;
    assert!(v.contains(&"Hex Board".to_string()) && !v.contains(&"Scry".to_string()));
    let mut chord = r.app.snapshot().shortcut;
    chord.key = "D".into();
    r.app.send(Command::SetShortcut(chord.clone()));
    assert_eq!(r.app.take_shortcut_change(), Some(chord.clone()));
    assert_eq!(r.app.take_shortcut_change(), None);
    r.app.send(Command::SetOpenAtLogin(true));
    assert!(r.app.snapshot().open_at_login);
    let saved = Settings::load(&r.settings_path).unwrap();
    assert_eq!(saved.shortcut, chord);
    assert!(saved.open_at_login);
}

#[test]
fn quit_cancels_a_recording_take() {
    let r = rig(clean_speech(), FakeSecrets::with_key());
    r.app.send(Command::ToggleDictation);
    r.app.send(Command::Quit);
    assert!(r.app.quit_requested());
    idle(&r);
    assert_eq!(r.desktop.inserts(), 0);
}
