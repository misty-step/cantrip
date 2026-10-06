//! Native macOS Copy; guarded keyboard delivery is an explicit unsupported
//! capability, not an Accessibility/frontmost-app approximation.
//!
//! Native pasteboard acknowledgement is not application receipt. No previous
//! clipboard is read or restored, and no payload is ever passed to a subprocess.

use super::{
    cancelled, check_deadline, failed, failure, DeliveryAvailability, DeliveryGuard, Result,
};
use cantrip_engine::delivery::{
    InjectionFailure, InjectionFailureKind, InjectionMode, InjectionOutcome,
};
use objc2::{
    exception,
    rc::{autoreleasepool, Retained},
};
use objc2_app_kit::{NSPasteboard, NSPasteboardContentsOptions, NSPasteboardTypeString};
use objc2_foundation::NSString;
use std::{
    panic::AssertUnwindSafe,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

const INJECT_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) fn delivery_availability() -> DeliveryAvailability {
    DeliveryAvailability {
        keyboard: false,
        clipboard: true,
        automatic: false,
        reason: Some(crate::desktop::UNSUPPORTED_DELIVERY),
    }
}

/// Mechanisms that are actually available under the current safety contract.
/// Accessibility permission alone cannot add an authorized keyboard backend.
pub fn planned_backend_names(
    mode: InjectionMode,
    _keyboard: bool,
    clipboard: bool,
) -> Vec<&'static str> {
    if mode == InjectionMode::Clipboard && clipboard {
        vec!["clipboard"]
    } else {
        Vec::new()
    }
}

pub fn inject(
    text: &str,
    mode: InjectionMode,
    guard: &DeliveryGuard,
    cancel: &AtomicBool,
) -> Result<InjectionOutcome> {
    let deadline = Instant::now() + INJECT_TIMEOUT;
    select_delivery(text, mode, guard, cancel, || {
        copy_text(text, cancel, deadline, NSPasteboard::generalPasteboard)
    })
}

// Keep the clipboard factory lazy: strict Type, unverified automatic delivery,
// empty text and cancellation must not even open/claim the general pasteboard.
fn select_delivery(
    text: &str,
    mode: InjectionMode,
    guard: &DeliveryGuard,
    cancel: &AtomicBool,
    copy: impl FnOnce() -> Result<()>,
) -> Result<InjectionOutcome> {
    cancelled(cancel)?;
    if text.is_empty() {
        return Err(failed("There is no text to deliver."));
    }
    if mode != InjectionMode::Clipboard {
        return Err(failure(
            InjectionFailureKind::Deferred,
            guard.desktop.denial(),
        ));
    }
    copy()?;
    Ok(InjectionOutcome::Clipboard)
}

/// Isolate Objective-C communication exceptions without formatting them: an
/// exception's reason could contain user data. Track the first mutation before
/// calling the server, because an exception does not prove it had no effect.
fn copy_text(
    text: &str,
    cancel: &AtomicBool,
    deadline: Instant,
    pasteboard: impl FnOnce() -> Retained<NSPasteboard>,
) -> Result<()> {
    cancelled(cancel)?;
    check_deadline(deadline)?;
    let mut mutation_possible = false;
    let result = exception::catch(AssertUnwindSafe(|| {
        autoreleasepool(|_| {
            // Prepare lossless Unicode text before any externally visible
            // mutation. Clipboard delivery preserves paragraphs and controls.
            let string = NSString::from_str(text);
            let board = pasteboard();
            cancelled(cancel)?;
            check_deadline(deadline)?;
            mutation_possible = true;
            // CurrentHostOnly prevents dictation from being automatically
            // published through Universal Clipboard. This clears/claims once;
            // no clipboard read or restoration occurs.
            board.prepareForNewContentsWithOptions(NSPasteboardContentsOptions::CurrentHostOnly);
            // Complete this single commit without an intervening cancellation
            // check; once ownership changes, stopping here leaves it empty.
            // SAFETY: NSPasteboardTypeString is the framework's immutable text
            // type constant, available on every supported macOS version.
            if !board.setString_forType(&string, unsafe { NSPasteboardTypeString }) {
                return Err(copy_uncertain());
            }
            // Native methods can block in pasteboard-server communication.
            // These checks classify an overdue/interrupted acknowledged write;
            // they do not pretend to preempt an in-flight native IPC call.
            cancelled(cancel).map_err(|_| copy_uncertain())?;
            check_deadline(deadline).map_err(|_| copy_uncertain())?;
            Ok(())
        })
    }));
    match result {
        Ok(result) => result,
        Err(_) if mutation_possible => Err(copy_uncertain()),
        Err(_) => Err(failed(
            "Cannot communicate with the macOS pasteboard; no copy was attempted.",
        )),
    }
}

fn copy_uncertain() -> InjectionFailure {
    failure(
        InjectionFailureKind::Uncertain,
        "The macOS clipboard handoff was interrupted and may have completed. No retry or clipboard restoration was attempted.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn private_board(test: &str) -> Retained<NSPasteboard> {
        // Process and case namespaces isolate parallel tests from both the
        // general board and each other's server-backed pasteboard lifetime.
        let name = NSString::from_str(&format!(
            "com.misty-step.cantrip.test.{}.{}",
            std::process::id(),
            test
        ));
        NSPasteboard::pasteboardWithName(&name)
    }

    #[test]
    fn unsupported_destination_never_opens_a_clipboard_for_keyboard_modes() {
        let cancel = AtomicBool::new(false);
        let guard = DeliveryGuard::capture();
        for mode in [
            InjectionMode::Auto,
            InjectionMode::Paste,
            InjectionMode::Type,
        ] {
            let error = select_delivery("private transcript", mode, &guard, &cancel, || {
                panic!("unverified keyboard delivery touched a clipboard")
            })
            .unwrap_err();
            assert_eq!(error.kind, InjectionFailureKind::Deferred);
            assert!(!error.to_string().contains("private transcript"));
        }
    }

    #[test]
    fn cancelled_copy_does_not_open_a_native_pasteboard() {
        let error = copy_text(
            "private transcript",
            &AtomicBool::new(true),
            Instant::now() + INJECT_TIMEOUT,
            || panic!("cancelled copy opened the pasteboard"),
        )
        .unwrap_err();
        assert_eq!(error.kind, InjectionFailureKind::Cancelled);
    }

    #[test]
    fn slow_native_preflight_cannot_claim_a_pasteboard_after_its_deadline() {
        autoreleasepool(|_| {
            let board = private_board("deadline");
            let cancel = AtomicBool::new(false);
            let seed = "previous private clipboard";
            let seeded = copy_text(seed, &cancel, Instant::now() + INJECT_TIMEOUT, || {
                board.clone()
            });
            let result = copy_text(
                "synthetic-private-dictation",
                &cancel,
                Instant::now() + Duration::from_millis(10),
                || {
                    std::thread::sleep(Duration::from_millis(30));
                    board.clone()
                },
            );
            let value = board.stringForType(unsafe { NSPasteboardTypeString });
            // SAFETY: this is a private unique board, never a standard board.
            let _: () = unsafe { objc2::msg_send![&board, releaseGlobally] };
            seeded.unwrap();
            let error = result.unwrap_err();
            assert_eq!(error.kind, InjectionFailureKind::Failed);
            assert!(!error.to_string().contains("synthetic-private-dictation"));
            assert_eq!(value.unwrap().to_string(), seed);
        });
    }

    // A named pasteboard is private test state, not the person's general
    // clipboard. No live input, screen/microphone capture or AX grant is used.
    #[test]
    fn native_named_pasteboard_preserves_unicode_and_paragraphs_without_ax() {
        autoreleasepool(|_| {
            let board = private_board("unicode");
            let text = "Καλημέρα\n世界\r\n🙂 café\tfin";
            let copied = copy_text(
                text,
                &AtomicBool::new(false),
                Instant::now() + INJECT_TIMEOUT,
                || board.clone(),
            );
            let value = board.stringForType(unsafe { NSPasteboardTypeString });
            // SAFETY: releaseGlobally is documented for private named boards
            // only. objc2 omits this oneway-void selector from typed bindings.
            let _: () = unsafe { objc2::msg_send![&board, releaseGlobally] };
            copied.unwrap();
            assert_eq!(value.unwrap().to_string(), text);
        });
    }
}
