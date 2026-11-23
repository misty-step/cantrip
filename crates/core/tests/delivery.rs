//! Guarded delivery contract with fake receivers that log every write/paste.
mod support;

use cantrip_core::delivery::*;
use cantrip_core::ports::*;
use cantrip_core::surface::{CopyReason, Hud};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use support::*;

fn setup() -> (Arc<FakeDesktop>, Arc<FakeClipboard>, CaptureContext) {
    let d = FakeDesktop::new(target_a());
    let c = FakeClipboard::new();
    *d.clipboard.lock().unwrap() = Some(c.clone());
    let ctx = capture_with_deadline(d.clone(), CAPTURE_DEADLINE);
    (d, c, ctx)
}

#[test]
fn tier1_confirmed_insert_never_touches_the_clipboard() {
    let (d, c, ctx) = setup();
    let r = deliver("world", &ctx, d.as_ref(), c.as_ref());
    assert_eq!(r.outcome, Outcome::Confirmed);
    assert_eq!(r.hud, Hud::Inserted);
    assert_eq!(d.field_text(), "Hello world");
    assert_eq!(c.writes.load(Ordering::SeqCst), 0);
    assert_eq!(d.pastes(), 0);
}

#[test]
fn cross_app_focus_change_is_copy_only_with_zero_events() {
    let (d, c, ctx) = setup();
    *d.focus.lock().unwrap() = Some(focus_b());
    let r = deliver("world", &ctx, d.as_ref(), c.as_ref());
    assert_eq!(r.outcome, Outcome::Copied(CopyReason::FocusChanged));
    assert_eq!(r.hud.text(), "Copied — focus changed");
    assert_eq!(r.hud.claim(), Some("Text is on the clipboard"));
    assert!(d.events().is_empty());
    assert_eq!(c.text().as_deref(), Some("world"));
}

#[test]
fn same_app_second_window_is_focus_changed() {
    let (d, c, ctx) = setup();
    let mut f = focus_of(&target_a());
    f.window = WindowRef("A2".into());
    *d.focus.lock().unwrap() = Some(f);
    let r = deliver("x", &ctx, d.as_ref(), c.as_ref());
    assert_eq!(r.outcome, Outcome::Copied(CopyReason::FocusChanged));
    assert!(d.events().is_empty());
}

#[test]
fn same_program_other_instance_is_focus_changed() {
    let (d, c, ctx) = setup();
    let mut f = focus_of(&target_a());
    f.app.instance = "10:999".into(); // PID reused by a new process
    *d.focus.lock().unwrap() = Some(f);
    assert_eq!(
        deliver("x", &ctx, d.as_ref(), c.as_ref()).outcome,
        Outcome::Copied(CopyReason::FocusChanged)
    );
}

#[test]
fn focus_switch_at_final_verification_is_copy_only_with_zero_events() {
    let (d, c, ctx) = setup();
    // First gate sees A; the final verification sees B.
    *d.switch_after.lock().unwrap() = Some((1, Some(focus_b())));
    let r = deliver("x", &ctx, d.as_ref(), c.as_ref());
    assert_eq!(r.outcome, Outcome::Copied(CopyReason::FocusChanged));
    assert!(d.events().is_empty());
}

#[test]
fn tier2_focus_switch_at_final_verification_sends_no_paste() {
    let (d, c, ctx) = setup();
    *d.support.lock().unwrap() = FieldSupport::NoDirectInsert;
    *d.switch_after.lock().unwrap() = Some((1, Some(focus_b())));
    let r = deliver("x", &ctx, d.as_ref(), c.as_ref());
    assert_eq!(r.outcome, Outcome::Copied(CopyReason::FocusChanged));
    assert_eq!(d.pastes(), 0);
    assert_eq!(c.text().as_deref(), Some("x"));
}

#[test]
fn slow_capture_is_uncertain_forever_even_when_the_answer_arrives_later() {
    let d = FakeDesktop::new(target_a());
    *d.capture_delay.lock().unwrap() = Duration::from_millis(400);
    let c = FakeClipboard::new();
    let ctx = capture_with_deadline(d.clone(), CAPTURE_DEADLINE);
    assert_eq!(ctx.captured, Captured::Uncertain);
    std::thread::sleep(Duration::from_millis(250)); // late answer lands
    let r = deliver("x", &ctx, d.as_ref(), c.as_ref());
    assert_eq!(r.outcome, Outcome::Copied(CopyReason::TargetUncertain));
    assert_eq!(r.hud.text(), "Copied — target uncertain");
    assert!(d.events().is_empty());
}

#[test]
fn unidentified_focus_is_uncertain() {
    let d = FakeDesktop::new(target_a());
    *d.capture.lock().unwrap() = None;
    let ctx = capture_with_deadline(d.clone(), CAPTURE_DEADLINE);
    assert_eq!(ctx.captured, Captured::Uncertain);
}

#[test]
fn sleep_dead_app_and_unreadable_focus_are_uncertain() {
    for mutate in [
        (|d: &FakeDesktop| {
            d.epoch.fetch_add(1, Ordering::SeqCst);
        }) as fn(&FakeDesktop),
        |d| *d.alive.lock().unwrap() = false,
        |d| *d.focus.lock().unwrap() = None,
        |d| *d.support.lock().unwrap() = FieldSupport::Gone,
    ] {
        let (d, c, ctx) = setup();
        mutate(&d);
        let r = deliver("x", &ctx, d.as_ref(), c.as_ref());
        assert_eq!(r.outcome, Outcome::Copied(CopyReason::TargetUncertain));
        assert!(d.events().is_empty());
    }
}

#[test]
fn missing_permission_names_it() {
    let (d, c, ctx) = setup();
    *d.permission.lock().unwrap() = Some("Accessibility".into());
    let r = deliver("x", &ctx, d.as_ref(), c.as_ref());
    assert_eq!(r.hud.text(), "Copied — allow Accessibility");
    assert!(d.events().is_empty());
}

#[test]
fn copy_only_list_apps_get_only_a_copy() {
    let mut t = target_a();
    t.app.app_id = "org.remmina.Remmina".into();
    let d = FakeDesktop::new(t);
    let c = FakeClipboard::new();
    let ctx = capture_with_deadline(d.clone(), CAPTURE_DEADLINE);
    assert!(matches!(
        deliver("x", &ctx, d.as_ref(), c.as_ref()).outcome,
        Outcome::Copied(_)
    ));
    assert!(d.events().is_empty());
}

#[test]
fn unsupported_session_is_paste_unsupported_with_zero_key_events() {
    let (d, c, ctx) = setup();
    *d.support.lock().unwrap() = FieldSupport::NoDirectInsert;
    *d.addressable.lock().unwrap() = false;
    let r = deliver("x", &ctx, d.as_ref(), c.as_ref());
    assert_eq!(r.hud.text(), "Copied — paste unsupported on this session");
    assert!(d.events().is_empty());
    // Refused direct insert on an unaddressable session: same outcome.
    let (d, c, ctx) = setup();
    *d.insert_mode.lock().unwrap() = InsertMode::Refused;
    *d.addressable.lock().unwrap() = false;
    let r = deliver("x", &ctx, d.as_ref(), c.as_ref());
    assert_eq!(r.outcome, Outcome::Copied(CopyReason::PasteUnsupported));
    assert_eq!(d.pastes(), 0);
}

#[test]
fn tier1_ambiguous_or_unconfirmed_never_tries_tier2() {
    for mode in [InsertMode::Ambiguous, InsertMode::Opaque] {
        let (d, c, ctx) = setup();
        *d.insert_mode.lock().unwrap() = mode;
        let r = deliver("x", &ctx, d.as_ref(), c.as_ref());
        assert_eq!(r.outcome, Outcome::Unconfirmed { tier: 1 });
        assert_eq!(r.hud.text(), "Sent — unconfirmed");
        assert_eq!(d.pastes(), 0);
        assert_eq!(c.writes.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn tier1_refusal_falls_through_to_tier2() {
    let (d, c, ctx) = setup();
    *d.insert_mode.lock().unwrap() = InsertMode::Refused;
    let r = deliver("there", &ctx, d.as_ref(), c.as_ref());
    assert_eq!(r.outcome, Outcome::Confirmed);
    assert_eq!(d.pastes(), 1);
    assert!(r.restored);
}

#[test]
fn tier2_confirmed_with_unchanged_clipboard_restores_every_item() {
    let (d, c, ctx) = setup();
    *d.support.lock().unwrap() = FieldSupport::NoDirectInsert;
    let before = c.snapshot_now();
    let r = deliver("world", &ctx, d.as_ref(), c.as_ref());
    assert_eq!(r.outcome, Outcome::Confirmed);
    assert_eq!(r.hud, Hud::Inserted);
    assert!(r.restored);
    assert_eq!(d.field_text(), "Hello world");
    assert_eq!(c.snapshot_now(), before, "old text and image restored");
}

#[test]
fn concurrent_same_text_rich_copy_refuses_restore() {
    let (d, c, ctx) = setup();
    *d.support.lock().unwrap() = FieldSupport::NoDirectInsert;
    // Right after Cantrip writes, another app copies the very same text (rich).
    *c.external_after_checks.lock().unwrap() = Some((0, "world".into()));
    let r = deliver("world", &ctx, d.as_ref(), c.as_ref());
    assert_eq!(r.outcome, Outcome::Confirmed);
    assert!(!r.restored);
    assert_eq!(c.restores.load(Ordering::SeqCst), 0);
    assert_eq!(
        c.snapshot_now().items[0].len(),
        2,
        "the other app's rich copy survives"
    );
}

#[test]
fn swallowed_paste_is_unconfirmed_and_restores_nothing() {
    let (d, c, ctx) = setup();
    *d.support.lock().unwrap() = FieldSupport::NoDirectInsert;
    *d.paste_mode.lock().unwrap() = PasteMode::Swallow;
    let r = deliver("world", &ctx, d.as_ref(), c.as_ref());
    assert_eq!(r.outcome, Outcome::Unconfirmed { tier: 2 });
    assert_eq!(r.hud.text(), "Posted — unconfirmed");
    assert_eq!(r.hud.claim(), Some("Text is on the clipboard"));
    assert!(!r.restored);
    assert_eq!(c.restores.load(Ordering::SeqCst), 0);
    assert_eq!(c.text().as_deref(), Some("world"));
}

#[test]
fn no_identified_field_is_copy_only_never_a_paste() {
    // Tier 2 is only for an identified field that cannot take direct insertion.
    let (d, c, ctx) = setup();
    let mut t = target_a();
    t.field = None;
    *d.capture.lock().unwrap() = Some(t);
    let ctx2 = capture_with_deadline(d.clone(), CAPTURE_DEADLINE);
    let _ = ctx;
    let r = deliver("world", &ctx2, d.as_ref(), c.as_ref());
    assert_eq!(r.outcome, Outcome::Copied(CopyReason::TargetUncertain));
    assert_eq!(d.pastes() + d.inserts(), 0);
    assert_eq!(c.text().as_deref(), Some("world"));
}

#[test]
fn claim_drops_when_clipboard_changes_before_the_hud() {
    let (d, c, ctx) = setup();
    *d.focus.lock().unwrap() = Some(focus_b());
    *c.external_after_checks.lock().unwrap() = Some((0, "other".into()));
    let r = deliver("x", &ctx, d.as_ref(), c.as_ref());
    assert_eq!(r.hud.claim(), Some("Use Copy last dictation"));
}

#[test]
fn claim_drops_while_the_hud_is_visible_and_never_returns() {
    let (d, c, ctx) = setup();
    *d.focus.lock().unwrap() = Some(focus_b());
    let r = deliver("x", &ctx, d.as_ref(), c.as_ref());
    let mut watch = ClaimWatch::new(r.clip.clone());
    assert!(watch.check(c.as_ref()));
    c.external_copy("other", false);
    assert!(!watch.check(c.as_ref()));
    // Even if the other app's copy is replaced by identical text later.
    assert!(!watch.check(c.as_ref()));
}

#[test]
fn clipboard_write_failure_is_not_delivered() {
    let (d, c, ctx) = setup();
    *d.focus.lock().unwrap() = Some(focus_b());
    *c.fail_writes.lock().unwrap() = true;
    let r = deliver("x", &ctx, d.as_ref(), c.as_ref());
    assert_eq!(r.outcome, Outcome::Failed);
    assert_eq!(r.hud.text(), "Not delivered. Use Copy last dictation");
    let (d, c, ctx) = setup();
    *d.support.lock().unwrap() = FieldSupport::NoDirectInsert;
    *c.fail_writes.lock().unwrap() = true;
    assert_eq!(
        deliver("x", &ctx, d.as_ref(), c.as_ref()).outcome,
        Outcome::Failed
    );
    assert_eq!(d.pastes(), 0);
}

fn fs(text: &str, sel: (usize, usize)) -> FieldState {
    FieldState {
        text: text.into(),
        selection: sel,
        complete: true,
    }
}

#[test]
fn edit_delta_counts_emoji_and_combining_marks_right() {
    let before = fs("Café 👋|", (6, 6));
    let ins = "e\u{301}🎉";
    let after = fs("Café 👋e\u{301}🎉|", (9, 9));
    assert!(edit_delta_confirms(&before, &after, ins));
    // Off-by-one caret (UTF-16 or byte offsets) fails.
    assert!(!edit_delta_confirms(
        &before,
        &fs("Café 👋e\u{301}🎉|", (10, 10)),
        ins
    ));
}

#[test]
fn edit_delta_replaces_selection_and_rejects_everything_else() {
    let before = fs("Send to Bob now", (8, 11));
    assert!(edit_delta_confirms(
        &before,
        &fs("Send to Ana now", (11, 11)),
        "Ana"
    ));
    // Identical replacement is never Inserted.
    assert!(!edit_delta_confirms(
        &before,
        &fs("Send to Bob now", (11, 11)),
        "Bob"
    ));
    // Autocorrect / concurrent edit.
    assert!(!edit_delta_confirms(
        &before,
        &fs("Send to Anna now", (12, 12)),
        "Ana"
    ));
    // Truncated readback.
    let mut trunc = fs("Send to Ana now", (11, 11));
    trunc.complete = false;
    assert!(!edit_delta_confirms(&before, &trunc, "Ana"));
    // No change.
    assert!(!edit_delta_confirms(&before, &before.clone(), "Ana"));
}

#[test]
fn copy_only_reports_claim_and_token() {
    let c = FakeClipboard::new();
    let r = copy_only("x", CopyReason::TargetUncertain, c.as_ref());
    assert!(r.clip.is_some());
    assert_eq!(r.hud.claim(), Some("Text is on the clipboard"));
}

#[test]
fn randomized_switch_timings_never_deliver_after_a_switch_before_final_verification() {
    // focus_now is consulted at the first gate (call 1) and at the final
    // verification (call 2). A switch visible at either is copy-only with zero
    // events; a switch after the final verification may still deliver to A.
    let mut seed: u64 = 0x9E3779B97F4A7C15;
    for i in 0..200 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let switch_after = (seed % 4) as usize;
        let tier2 = i % 2 == 1;
        let (d, c, ctx) = setup();
        if tier2 {
            *d.support.lock().unwrap() = FieldSupport::NoDirectInsert;
        }
        *d.switch_after.lock().unwrap() = Some((switch_after, Some(focus_b())));
        let r = deliver("x", &ctx, d.as_ref(), c.as_ref());
        if switch_after < 2 {
            assert_eq!(
                r.outcome,
                Outcome::Copied(CopyReason::FocusChanged),
                "i={i}"
            );
            assert!(d.events().is_empty(), "i={i}");
        } else {
            assert_eq!(r.outcome, Outcome::Confirmed, "i={i}");
            assert_eq!(d.events().len(), 1);
        }
    }
}
