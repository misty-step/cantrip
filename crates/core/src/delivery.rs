//! Guarded two-tier delivery (DESIGN.md › Delivery contract).
//!
//! Capture at start (250 ms deadline, else `Uncertain` forever) → gate before
//! either tier and again at final verification → tier 1 direct insertion
//! (never touches the clipboard) → tier 2 addressed paste (snapshot, write,
//! post, restore only on a confirmed edit delta *and* an unchanged clipboard)
//! → copy-only with a reason. Never a blind keystroke; a handoff that may have
//! completed is never retried through the other tier.
//!
//! OWNER: core-dictation fork.

use crate::ports::{ClipToken, Clipboard, Desktop, FieldState, Target};
use crate::surface::{CopyReason, Hud};
use std::sync::Arc;
use std::time::Duration;

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
/// the take is `Uncertain`.
pub fn capture_with_deadline(desktop: Arc<dyn Desktop>, deadline: Duration) -> CaptureContext {
    let _ = (desktop, deadline);
    todo!()
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
    let _ = (text, ctx, desktop, clipboard);
    todo!()
}

/// Copy-only: put the text on the clipboard, send nothing, say why.
pub fn copy_only(text: &str, reason: CopyReason, clipboard: &dyn Clipboard) -> Delivered {
    let _ = (text, reason, clipboard);
    todo!()
}

/// `confirmed` evidence: `after` equals `before` with exactly its selection
/// replaced by `inserted`, the caret right after the insertion, both values
/// complete, and the content actually changed.
pub fn edit_delta_confirms(before: &FieldState, after: &FieldState, inserted: &str) -> bool {
    let _ = (before, after, inserted);
    todo!()
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
