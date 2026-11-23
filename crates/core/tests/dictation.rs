//! One take end to end with fakes: pipeline, fallback, gate and retention.
mod support;

use cantrip_core::dictation::*;
use cantrip_core::ports::*;
use cantrip_core::speech::{Mode, OpenRouter};
use cantrip_core::staging::{DictationState, Staging};
use cantrip_core::surface::{CopyReason, Hud, HudNote};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use support::*;

struct Rig {
    d: Dictation,
    desktop: Arc<FakeDesktop>,
    clip: Arc<FakeClipboard>,
    transport: Arc<FakeTransport>,
    local: Arc<FakeLocal>,
    recorder: Arc<FakeRecorder>,
    staging: Staging,
    _dir: tempfile::TempDir,
}

fn rig_with(
    transport: Arc<FakeTransport>,
    secrets: Arc<FakeSecrets>,
    local: LocalAvailability,
) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let staging = Staging::open(&dir.path().join("stage")).unwrap();
    let desktop = FakeDesktop::new(target_a());
    let clip = FakeClipboard::new();
    *desktop.clipboard.lock().unwrap() = Some(clip.clone());
    let local = FakeLocal::new(local);
    let recorder = FakeRecorder::new();
    let deps = DictationDeps {
        recorder: recorder.clone(),
        desktop: desktop.clone(),
        clipboard: clip.clone(),
        secrets,
        local: local.clone(),
        openrouter: OpenRouter::with_base(transport.clone(), "http://stub"),
        staging: staging.clone(),
        notify: Arc::new(|| {}),
    };
    Rig {
        d: Dictation::new(deps),
        desktop,
        clip,
        transport,
        local,
        recorder,
        staging,
        _dir: dir,
    }
}

fn rig(transport: Arc<FakeTransport>) -> Rig {
    rig_with(
        transport,
        FakeSecrets::with_key(),
        LocalAvailability::NoOnDeviceEnglish,
    )
}

fn cfg(mode: Mode, post: bool) -> TakeConfig {
    TakeConfig {
        mode,
        post_processing: post,
        vocabulary: vec!["Scry".into()],
        microphone: None,
    }
}

fn take(r: &Rig, c: TakeConfig) {
    r.d.start(c);
    assert!(r.d.recording());
    assert_eq!(r.d.hud(), Hud::Listening);
    std::thread::sleep(Duration::from_millis(30));
    r.d.stop();
    assert!(r.d.wait_idle(Duration::from_secs(10)));
}

fn staged(r: &Rig) -> usize {
    r.staging.recoverable_dictations().len()
}

fn dictation_dirs(r: &Rig) -> usize {
    std::fs::read_dir(r.staging.root().join("dictations"))
        .unwrap()
        .count()
}

#[test]
fn clean_take_rewrites_inserts_and_deletes_audio() {
    let r = rig(speech("um open scrum", || {
        ok(&chat_json("Open Scry.", "stop"))
    }));
    take(&r, cfg(Mode::Clean, true));
    assert_eq!(r.desktop.field_text(), "Hello Open Scry.");
    assert_eq!(r.d.hud(), Hud::Inserted);
    assert_eq!(r.d.last_text().as_deref(), Some("Open Scry."));
    assert_eq!(dictation_dirs(&r), 0, "delivered audio is deleted");
    let tb = &r.transport.bodies("/audio/transcriptions")[0];
    assert_eq!(tb["keyterms"], serde_json::json!(["Scry"]));
    assert_eq!(tb["provider"]["data_collection"], "deny");
    assert!(tb.get("response_format").is_none());
    let rb = &r.transport.bodies("/chat/completions")[0];
    assert_eq!(rb["provider"]["data_collection"], "deny");
    assert!(rb["messages"][0]["content"]
        .as_str()
        .unwrap()
        .starts_with("You clean up dictated text."));
    let rep = r.d.last_report().unwrap();
    assert_eq!(rep.outcome, "Confirmed");
    assert_eq!(rep.chars, 10);
}

#[test]
fn raw_and_post_processing_off_never_call_the_llm() {
    for c in [
        cfg(Mode::Raw, true),
        cfg(Mode::Clean, false),
        cfg(Mode::Elevated, false),
    ] {
        let r = rig(speech("raw words", || panic!("LLM must not be called")));
        take(&r, c);
        assert_eq!(r.transport.count("/chat/completions"), 0);
        assert_eq!(r.desktop.field_text(), "Hello raw words");
    }
}

#[test]
fn elevated_uses_its_prompt() {
    let r = rig(speech("so basically ship it", || {
        ok(&chat_json("Ship it.", "stop"))
    }));
    take(&r, cfg(Mode::Elevated, true));
    let rb = &r.transport.bodies("/chat/completions")[0];
    assert!(rb["messages"][0]["content"]
        .as_str()
        .unwrap()
        .starts_with("You rewrite dictated text into crisp"));
    assert_eq!(r.desktop.field_text(), "Hello Ship it.");
}

#[test]
fn rewrite_failures_deliver_raw_with_cleanup_skipped() {
    let cases: Vec<Box<dyn Fn() -> Result<HttpReply, TransportError> + Send + Sync>> = vec![
        Box::new(|| status(500)),
        Box::new(|| ok(&chat_json("", "stop"))),
        Box::new(|| ok(&chat_json("Cut of", "length"))),
        Box::new(|| Err(TransportError::Failed("reset".into()))),
    ];
    for case in cases {
        let r = rig(speech("raw words", case));
        take(&r, cfg(Mode::Clean, true));
        assert_eq!(r.desktop.field_text(), "Hello raw words");
        assert_eq!(r.d.hud(), Hud::RawCleanupSkipped);
        assert_eq!(dictation_dirs(&r), 0);
    }
}

/// Raw fallback on a copy-only or unconfirmed take: the HUD shows the
/// delivery outcome *and* `Raw (cleanup skipped)`, Raw is what is delivered,
/// and the clipboard claim still drops on a foreign copy.
#[test]
fn raw_fallback_on_copy_only_takes_shows_both_outcome_and_cleanup_skipped() {
    type Rewrite = Box<dyn Fn() -> Result<HttpReply, TransportError> + Send + Sync>;
    let rewrites = || -> Vec<Rewrite> {
        vec![
            Box::new(|| status(500)),
            Box::new(|| ok(&chat_json("", "stop"))),
            Box::new(|| ok(&chat_json("Cut of", "length"))),
            Box::new(|| Err(TransportError::Failed("reset".into()))),
        ]
    };
    // Uncertain target (nothing identifiable captured at start).
    for rw in rewrites() {
        let r = rig(speech("raw words", rw));
        *r.desktop.capture.lock().unwrap() = None;
        take(&r, cfg(Mode::Clean, true));
        let hud = r.d.hud();
        assert_eq!(hud.text(), "Copied — target uncertain");
        assert_eq!(hud.note(), Some("Raw (cleanup skipped)"));
        assert_eq!(hud.claim(), Some("Text is on the clipboard"));
        assert_eq!(
            hud,
            Hud::Noted {
                outcome: Box::new(Hud::Copied {
                    reason: CopyReason::TargetUncertain,
                    on_clipboard: true
                }),
                note: HudNote::RawCleanupSkipped
            }
        );
        assert_eq!(r.clip.text().as_deref(), Some("raw words"), "Raw copied");
        assert!(r.desktop.events().is_empty(), "copy-only: zero key events");
        assert_eq!(dictation_dirs(&r), 0);
        // Sticky claim drop still works under the combined state.
        r.clip.external_copy("raw words", false);
        let hud = r.d.hud();
        assert_eq!(hud.claim(), Some("Use Copy last dictation"));
        assert_eq!(hud.note(), Some("Raw (cleanup skipped)"));
        assert_eq!(r.d.hud().claim(), Some("Use Copy last dictation"));
    }
    // Focus changed between start and delivery.
    for rw in rewrites() {
        let r = rig(speech("raw words", rw));
        *r.desktop.focus.lock().unwrap() = Some(focus_b());
        take(&r, cfg(Mode::Elevated, true));
        let hud = r.d.hud();
        assert_eq!(hud.text(), "Copied — focus changed");
        assert_eq!(hud.note(), Some("Raw (cleanup skipped)"));
        assert_eq!(r.clip.text().as_deref(), Some("raw words"));
    }
    // Even the copy failed: Not delivered, still noted.
    let r = rig(speech("raw words", || status(500)));
    *r.desktop.capture.lock().unwrap() = None;
    *r.clip.fail_writes.lock().unwrap() = true;
    take(&r, cfg(Mode::Clean, true));
    assert_eq!(
        r.d.hud(),
        HudNote::RawCleanupSkipped.with(Hud::NotDelivered)
    );
    assert_eq!(r.staging.recoverable_dictations().len(), 1);
}

#[test]
fn rewrite_timeout_on_an_uncertain_target_shows_both_after_four_seconds() {
    let r = rig(speech("raw words", || {
        std::thread::sleep(Duration::from_secs(6));
        ok(&chat_json("late", "stop"))
    }));
    *r.desktop.capture.lock().unwrap() = None;
    take(&r, cfg(Mode::Clean, true));
    let hud = r.d.hud();
    assert_eq!(hud.text(), "Copied — target uncertain");
    assert_eq!(hud.note(), Some("Raw (cleanup skipped)"));
    assert_eq!(r.clip.text().as_deref(), Some("raw words"));
}

#[test]
fn successful_rewrite_on_a_copy_only_take_has_no_note() {
    let r = rig(speech("raw words", || ok(&chat_json("Raw words.", "stop"))));
    *r.desktop.capture.lock().unwrap() = None;
    take(&r, cfg(Mode::Clean, true));
    let hud = r.d.hud();
    assert_eq!(hud.text(), "Copied — target uncertain");
    assert_eq!(hud.note(), None);
    assert_eq!(r.clip.text().as_deref(), Some("Raw words."));
}

#[test]
fn retry_with_raw_fallback_says_so_beside_the_recovered_copy() {
    let flaky = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let f2 = flaky.clone();
    let t = FakeTransport::new(move |url, _| {
        if url.ends_with("/audio/transcriptions") {
            if f2.swap(false, Ordering::SeqCst) {
                return status(503);
            }
            return ok(&transcript_json("recovered words"));
        }
        status(500)
    });
    let r = rig(t);
    take(&r, cfg(Mode::Clean, true));
    let id = r.staging.recoverable_dictations()[0].id.clone();
    r.d.retry(&id, cfg(Mode::Clean, true));
    assert!(r.d.wait_idle(Duration::from_secs(10)));
    assert_eq!(r.clip.text().as_deref(), Some("recovered words"));
    let hud = r.d.hud();
    assert_eq!(hud.text(), RETRY_COPIED);
    assert_eq!(hud.note(), Some("Raw (cleanup skipped)"));
}

#[test]
fn rewrite_timeout_delivers_raw_after_four_seconds() {
    let r = rig(speech("raw words", || {
        std::thread::sleep(Duration::from_secs(6));
        ok(&chat_json("late", "stop"))
    }));
    r.d.start(cfg(Mode::Clean, true));
    std::thread::sleep(Duration::from_millis(20));
    let t = Instant::now();
    r.d.stop();
    assert!(r.d.wait_idle(Duration::from_secs(10)));
    assert!(
        t.elapsed() < Duration::from_millis(4700),
        "{:?}",
        t.elapsed()
    );
    assert_eq!(r.desktop.field_text(), "Hello raw words");
    assert_eq!(r.d.hud(), Hud::RawCleanupSkipped);
}

#[test]
fn no_key_refuses_without_any_request_and_keeps_audio() {
    for (avail, fix) in [
        (
            LocalAvailability::NotAuthorized,
            cantrip_core::offline::FIX_ALLOW,
        ),
        (
            LocalAvailability::NoOnDeviceEnglish,
            cantrip_core::offline::FIX_NO_ON_DEVICE,
        ),
    ] {
        let t = speech("x", || panic!("no request without a key"));
        let r = rig_with(t, FakeSecrets::none(), avail);
        take(&r, cfg(Mode::Clean, true));
        assert_eq!(r.transport.total(), 0);
        assert_eq!(r.d.hud(), Hud::NotTranscribed(fix.into()));
        assert_eq!(staged(&r), 1);
        assert!(r
            .staging
            .dictation_audio(&r.staging.recoverable_dictations()[0].id)
            .exists());
        assert!(r.desktop.events().is_empty());
        assert_eq!(r.local.runs.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn no_key_with_ready_on_device_recognition_delivers_raw_on_device() {
    let t = speech("x", || panic!("no request without a key"));
    let r = rig_with(t, FakeSecrets::none(), LocalAvailability::Ready);
    take(&r, cfg(Mode::Clean, true));
    assert_eq!(r.transport.total(), 0);
    assert_eq!(r.desktop.field_text(), "Hello local words");
    assert_eq!(r.d.hud(), Hud::OnDevice);
    assert_eq!(dictation_dirs(&r), 0);
}

#[test]
fn offline_network_enters_the_local_gate() {
    let t = FakeTransport::new(|_, _| Err(TransportError::Offline("dns".into())));
    let r = rig_with(t, FakeSecrets::with_key(), LocalAvailability::Ready);
    take(&r, cfg(Mode::Clean, true));
    assert_eq!(r.local.runs.load(Ordering::SeqCst), 1);
    assert_eq!(r.desktop.field_text(), "Hello local words");
}

#[test]
fn cloud_error_or_empty_keeps_audio_and_never_runs_local() {
    for t in [
        FakeTransport::new(|_, _| status(500)),
        FakeTransport::new(|_, _| ok(&transcript_json(""))),
        FakeTransport::new(|_, _| Err(TransportError::Failed("reset".into()))),
    ] {
        let r = rig_with(t, FakeSecrets::with_key(), LocalAvailability::Ready);
        take(&r, cfg(Mode::Clean, true));
        assert_eq!(r.local.checks.load(Ordering::SeqCst), 0);
        assert!(matches!(r.d.hud(), Hud::NotTranscribed(_)));
        let items = r.staging.recoverable_dictations();
        assert_eq!(items.len(), 1);
        assert!(matches!(items[0].state, DictationState::Failed { .. }));
        assert!(r.desktop.events().is_empty());
    }
}

#[test]
fn secret_store_error_is_not_no_key() {
    let t = speech("x", || panic!("no rewrite"));
    let r = rig_with(t, FakeSecrets::broken(), LocalAvailability::Ready);
    take(&r, cfg(Mode::Clean, true));
    assert_eq!(r.transport.total(), 0);
    assert_eq!(
        r.local.checks.load(Ordering::SeqCst),
        0,
        "never local on a keyring error"
    );
    assert_eq!(r.d.hud(), Hud::NotTranscribed(FIX_KEY_UNREADABLE.into()));
    assert_eq!(staged(&r), 1);
}

#[test]
fn escape_deletes_at_once_and_a_late_response_is_never_delivered() {
    let r = rig(FakeTransport::new(|url, _| {
        if url.ends_with("/audio/transcriptions") {
            std::thread::sleep(Duration::from_millis(300));
        }
        ok(&transcript_json("late words"))
    }));
    // Escape while recording.
    r.d.start(cfg(Mode::Raw, true));
    r.d.cancel();
    assert_eq!(dictation_dirs(&r), 0);
    assert_eq!(r.d.hud(), Hud::Hidden);
    assert!(!r.d.busy());
    // Escape while transcribing.
    r.d.start(cfg(Mode::Raw, true));
    std::thread::sleep(Duration::from_millis(20));
    r.d.stop();
    std::thread::sleep(Duration::from_millis(100));
    r.d.cancel();
    assert_eq!(dictation_dirs(&r), 0);
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        r.desktop.events().is_empty(),
        "cancelled take never delivered"
    );
    assert_eq!(r.clip.writes.load(Ordering::SeqCst), 0);
    assert_eq!(r.d.last_text(), None);
    assert_eq!(r.d.hud(), Hud::Hidden);
}

#[test]
fn copied_and_unconfirmed_takes_delete_audio_but_not_delivered_keeps_it() {
    let r = rig(speech("words", || panic!()));
    *r.desktop.focus.lock().unwrap() = Some(focus_b());
    take(&r, cfg(Mode::Raw, true));
    assert!(matches!(
        r.d.hud(),
        Hud::Copied {
            on_clipboard: true,
            ..
        }
    ));
    assert_eq!(dictation_dirs(&r), 0);

    let r = rig(speech("words", || panic!()));
    *r.desktop.focus.lock().unwrap() = Some(focus_b());
    *r.clip.fail_writes.lock().unwrap() = true;
    take(&r, cfg(Mode::Raw, true));
    assert_eq!(r.d.hud(), Hud::NotDelivered);
    let items = r.staging.recoverable_dictations();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].state, DictationState::NotDelivered);
    assert!(r.staging.dictation_audio(&items[0].id).exists());
    assert_eq!(
        r.d.last_text().as_deref(),
        Some("words"),
        "Copy last dictation still holds it"
    );
}

#[test]
fn hud_claim_drops_while_visible() {
    let r = rig(speech("words", || panic!()));
    *r.desktop.focus.lock().unwrap() = Some(focus_b());
    take(&r, cfg(Mode::Raw, true));
    assert_eq!(r.d.hud().claim(), Some("Text is on the clipboard"));
    r.clip.external_copy("words", false); // same text, different owner
    assert_eq!(r.d.hud().claim(), Some("Use Copy last dictation"));
    assert_eq!(r.d.hud().claim(), Some("Use Copy last dictation"));
}

#[test]
fn retry_copies_only_and_deletes_audio_on_success() {
    let flaky = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let f2 = flaky.clone();
    let t = FakeTransport::new(move |url, _| {
        if url.ends_with("/audio/transcriptions") {
            if f2.swap(false, Ordering::SeqCst) {
                return status(503);
            }
            return ok(&transcript_json("recovered words"));
        }
        ok(&chat_json("Recovered words.", "stop"))
    });
    let r = rig(t);
    take(&r, cfg(Mode::Clean, true));
    let items = r.staging.recoverable_dictations();
    assert_eq!(items.len(), 1);
    r.d.retry(&items[0].id, cfg(Mode::Clean, true));
    assert!(r.d.wait_idle(Duration::from_secs(10)));
    assert!(
        r.desktop.events().is_empty(),
        "Retry never inserts or pastes"
    );
    assert_eq!(r.clip.text().as_deref(), Some("Recovered words."));
    assert_eq!(r.d.hud(), Hud::Notice(RETRY_COPIED.into()));
    assert_eq!(dictation_dirs(&r), 0);
    assert!(r.d.copy_last());
}

#[test]
fn discard_deletes_a_staged_take() {
    let r = rig(FakeTransport::new(|_, _| status(500)));
    take(&r, cfg(Mode::Raw, true));
    let id = r.staging.recoverable_dictations()[0].id.clone();
    r.d.discard(&id);
    assert_eq!(dictation_dirs(&r), 0);
}

#[test]
fn blocked_microphone_stages_nothing() {
    let r = rig(speech("x", || panic!()));
    *r.recorder.blocked.lock().unwrap() = Some(MicError::Blocked);
    r.d.start(cfg(Mode::Raw, true));
    assert_eq!(r.d.hud(), Hud::MicrophoneBlocked);
    assert!(!r.d.busy());
    assert_eq!(dictation_dirs(&r), 0);
}
