//! Guarded two-tier delivery (DESIGN.md › Delivery contract).
//!
//! Capture at start (250 ms deadline, else `Uncertain` forever) → gate before
//! either tier and again at final verification → tier 1 direct insertion
//! (never touches the clipboard) → tier 2 addressed paste (snapshot, write,
//! post, restore only on a confirmed edit delta *and* an unchanged clipboard)
//! → copy-only with a reason. Never a blind keystroke; a handoff that may have
//! completed is never retried through the other tier.

use crate::ports::{
    ClipToken, Clipboard, Desktop, FieldState, FieldSupport, InsertResult, PasteResult, Target,
};
use crate::surface::{CopyReason, Hud};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Poll interval while waiting for an edit delta.
const CONFIRM_POLL: Duration = Duration::from_millis(15);

pub const CAPTURE_DEADLINE: Duration = Duration::from_millis(250);
pub const TIER1_CONFIRM_WINDOW: Duration = Duration::from_millis(300);
pub const TIER2_CONFIRM_WINDOW: Duration = Duration::from_millis(2000);

/// Built-in copy-only list (not a setting): apps that forward keys elsewhere
/// (remote desktops, VMs) or are known to mis-handle synthesized input.
pub const COPY_ONLY_APPS: &[&str] = &[
    "com.microsoft.rdc.macos",
    "com.vmware.fusion",
    "com.parallels.desktop.console",
    "org.virtualbox.app.VirtualBoxVM",
    "remmina",
    "org.remmina.Remmina",
    "virt-manager",
    "vncviewer",
];

/// The take's target, fixed at start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Captured {
    Resolved(Target),
    /// Not identified within the deadline; never upgraded later.
    Uncertain,
}

/// What was captured plus the session epoch at capture time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureContext {
    pub captured: Captured,
    pub epoch: u64,
}

/// Capture on a worker thread; a result later than `deadline` is ignored and
/// the take is `Uncertain`. The session epoch is read *inside* the worker,
/// before the target, so a slow epoch query (logind over D-Bus, a resume
/// clock) counts against the same budget and can never push capture past it.
pub fn capture_with_deadline(desktop: Arc<dyn Desktop>, deadline: Duration) -> CaptureContext {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let epoch = desktop.session_epoch();
        let target = desktop.capture_target();
        let _ = tx.send((epoch, target)); // late answers are dropped
    });
    match rx.recv_timeout(deadline) {
        Ok((epoch, Some(t))) => CaptureContext {
            captured: Captured::Resolved(t),
            epoch,
        },
        Ok((epoch, None)) => CaptureContext {
            captured: Captured::Uncertain,
            epoch,
        },
        // An uncertain take never reaches the gate, so its epoch is unused.
        Err(_) => CaptureContext {
            captured: Captured::Uncertain,
            epoch: 0,
        },
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Confirmed,
    /// tier 1 or 2.
    Unconfirmed {
        tier: u8,
    },
    Copied(CopyReason),
    /// Even the clipboard write failed.
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivered {
    pub outcome: Outcome,
    /// Initial HUD (claim computed at the time of reporting).
    pub hud: Hud,
    /// Present when Cantrip's text is (or was) on the clipboard; the UI keeps
    /// checking it while the HUD shows.
    pub clip: Option<ClipToken>,
    /// Whether the pre-dispatch clipboard snapshot was restored.
    pub restored: bool,
}

/// Deliver `text` to the captured target per the contract.
pub fn deliver(
    text: &str,
    ctx: &CaptureContext,
    desktop: &dyn Desktop,
    clipboard: &dyn Clipboard,
) -> Delivered {
    let target = match &ctx.captured {
        Captured::Resolved(t) => t,
        Captured::Uncertain => return copy_only(text, CopyReason::TargetUncertain, clipboard),
    };
    if let Err(reason) = gate(target, ctx.epoch, desktop) {
        return copy_only(text, reason, clipboard);
    }
    if on_copy_only_list(&target.app.app_id) {
        return copy_only(text, CopyReason::TargetUncertain, clipboard);
    }
    // Tier 2 is only for an *identified* field that cannot take direct
    // insertion (PRODUCT.md journey 1.3); no identified field → copy-only.
    let support = match &target.field {
        None => FieldSupport::Gone,
        Some(_) => desktop.field_support(target),
    };
    match support {
        FieldSupport::Gone => copy_only(text, CopyReason::TargetUncertain, clipboard),
        FieldSupport::NoDirectInsert => tier2(text, target, ctx.epoch, desktop, clipboard),
        FieldSupport::DirectInsert => {
            let before = desktop.read_field(target);
            // Final verification, immediately before dispatch.
            if let Err(reason) = gate(target, ctx.epoch, desktop) {
                return copy_only(text, reason, clipboard);
            }
            match desktop.insert_text(target, text) {
                InsertResult::Done => {
                    let until = Instant::now() + TIER1_CONFIRM_WINDOW;
                    let confirmed = before
                        .as_ref()
                        .is_some_and(|b| await_delta(desktop, target, b, text, until));
                    if confirmed {
                        settled(Outcome::Confirmed, Hud::Inserted)
                    } else {
                        settled(Outcome::Unconfirmed { tier: 1 }, Hud::SentUnconfirmed)
                    }
                }
                // May have landed: never retried through the other tier.
                InsertResult::Ambiguous => {
                    settled(Outcome::Unconfirmed { tier: 1 }, Hud::SentUnconfirmed)
                }
                InsertResult::Refused => tier2(text, target, ctx.epoch, desktop, clipboard),
            }
        }
    }
}

/// Copy-only: put the text on the clipboard, send nothing, say why.
pub fn copy_only(text: &str, reason: CopyReason, clipboard: &dyn Clipboard) -> Delivered {
    match clipboard.write_text(text, true) {
        Err(_) => Delivered {
            outcome: Outcome::Failed,
            hud: Hud::NotDelivered,
            clip: None,
            restored: false,
        },
        Ok(token) => {
            let on_clipboard = clipboard.unchanged_since(&token);
            Delivered {
                outcome: Outcome::Copied(reason.clone()),
                hud: Hud::Copied {
                    reason,
                    on_clipboard,
                },
                clip: Some(token),
                restored: false,
            }
        }
    }
}

/// `confirmed` evidence: `after` equals `before` with exactly its selection
/// replaced by `inserted`, the caret right after the insertion, both values
/// complete, and the content actually changed.
pub fn edit_delta_confirms(before: &FieldState, after: &FieldState, inserted: &str) -> bool {
    if !before.complete || !after.complete || inserted.is_empty() {
        return false;
    }
    let chars: Vec<char> = before.text.chars().collect();
    let (start, end) = before.selection;
    if start > end || end > chars.len() {
        return false;
    }
    let prefix: String = chars[..start].iter().collect();
    let selected: String = chars[start..end].iter().collect();
    let suffix: String = chars[end..].iter().collect();
    if selected == inserted {
        // Replacing text with identical text is not an observable edit.
        return false;
    }
    let expected = format!("{prefix}{inserted}{suffix}");
    let caret = start + inserted.chars().count();
    after.text == expected && after.selection == (caret, caret) && after.text != before.text
}

/// The gate, run before either tier and again at final verification.
fn gate(target: &Target, epoch: u64, desktop: &dyn Desktop) -> Result<(), CopyReason> {
    if let Some(p) = desktop.missing_delivery_permission() {
        return Err(CopyReason::Allow(p));
    }
    if desktop.session_epoch() != epoch || !desktop.app_alive(&target.app) {
        return Err(CopyReason::TargetUncertain);
    }
    let now = desktop.focus_now().ok_or(CopyReason::TargetUncertain)?;
    if now.app != target.app || now.window != target.window {
        return Err(CopyReason::FocusChanged);
    }
    Ok(())
}

fn on_copy_only_list(app_id: &str) -> bool {
    COPY_ONLY_APPS
        .iter()
        .any(|a| a.eq_ignore_ascii_case(app_id))
}

fn settled(outcome: Outcome, hud: Hud) -> Delivered {
    Delivered {
        outcome,
        hud,
        clip: None,
        restored: false,
    }
}

/// Poll the field until it shows exactly Cantrip's edit or `until` passes.
/// The deadline is checked *after* each read-back completes: a read that
/// blocks past the deadline never confirms, however good its answer.
fn await_delta(
    desktop: &dyn Desktop,
    target: &Target,
    before: &FieldState,
    text: &str,
    until: Instant,
) -> bool {
    loop {
        let after = desktop.read_field(target);
        let now = Instant::now();
        if now > until {
            return false;
        }
        if after.is_some_and(|a| edit_delta_confirms(before, &a, text)) {
            return true;
        }
        std::thread::sleep(CONFIRM_POLL.min(until - now));
    }
}

/// Tier 2: addressed paste with snapshot / guarded restore.
fn tier2(
    text: &str,
    target: &Target,
    epoch: u64,
    desktop: &dyn Desktop,
    clipboard: &dyn Clipboard,
) -> Delivered {
    if !desktop.can_address_paste(&target.app) {
        return copy_only(text, CopyReason::PasteUnsupported, clipboard);
    }
    let before = target
        .field
        .as_ref()
        .and_then(|_| desktop.read_field(target));
    let snapshot = clipboard.snapshot();
    let token = match clipboard.write_text(text, true) {
        Ok(t) => t,
        Err(_) => {
            return Delivered {
                outcome: Outcome::Failed,
                hud: Hud::NotDelivered,
                clip: None,
                restored: false,
            }
        }
    };
    let copied = |reason: CopyReason| Delivered {
        outcome: Outcome::Copied(reason.clone()),
        hud: Hud::Copied {
            reason,
            on_clipboard: clipboard.unchanged_since(&token),
        },
        clip: Some(token.clone()),
        restored: false,
    };
    // Final verification, immediately before dispatch.
    if let Err(reason) = gate(target, epoch, desktop) {
        return copied(reason);
    }
    match desktop.post_paste(target) {
        PasteResult::Unaddressable => copied(CopyReason::PasteUnsupported),
        PasteResult::Failed => copied(CopyReason::TargetUncertain),
        PasteResult::Posted => {
            let until = Instant::now() + TIER2_CONFIRM_WINDOW;
            let confirmed = before
                .as_ref()
                .is_some_and(|b| await_delta(desktop, target, b, text, until));
            if confirmed {
                // Restore only inside the window: a clipboard check that
                // completes past the deadline restores nothing.
                let restored = match &snapshot {
                    Some(snap) if clipboard.unchanged_since(&token) && Instant::now() <= until => {
                        clipboard.restore(snap, &token)
                    }
                    _ => false,
                };
                Delivered {
                    outcome: Outcome::Confirmed,
                    hud: Hud::Inserted,
                    clip: if restored { None } else { Some(token) },
                    restored,
                }
            } else {
                Delivered {
                    outcome: Outcome::Unconfirmed { tier: 2 },
                    hud: Hud::PostedUnconfirmed {
                        on_clipboard: clipboard.unchanged_since(&token),
                    },
                    clip: Some(token),
                    restored: false,
                }
            }
        }
    }
}

/// Sticky clipboard claim: true while Cantrip's write is unchanged; once false
/// it never becomes true again.
#[derive(Debug)]
pub struct ClaimWatch {
    token: Option<ClipToken>,
    held: bool,
}

impl ClaimWatch {
    pub fn new(token: Option<ClipToken>) -> Self {
        let held = token.is_some();
        ClaimWatch { token, held }
    }

    /// Re-check now; returns whether "Text is on the clipboard" may show.
    pub fn check(&mut self, clipboard: &dyn Clipboard) -> bool {
        if self.held {
            if let Some(t) = &self.token {
                self.held = clipboard.unchanged_since(t);
            }
        }
        self.held
    }
}
