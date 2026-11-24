//! Menu-bar status item. A click opens Cantrip's popover (the egui control
//! surface with every control named — never an icon-only menu). While
//! recording the glyph turns into a red record dot.
//!
//! Created on the main thread; `set_recording` hops to the main queue when
//! called elsewhere. Not run on a Mac yet; type-checked only.

use super::ns;
use crate::platform::{Tray, UiRequest};
use dispatch2::{DispatchQueue, MainThreadBound};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSColor, NSImage, NSStatusBar, NSStatusItem, NSVariableStatusItemLength};
use objc2_foundation::{NSObject, NSObjectProtocol};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

struct TargetIvars {
    tx: Mutex<Sender<UiRequest>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "CantripStatusItemTarget"]
    #[ivars = TargetIvars]
    struct StatusTarget;

    unsafe impl NSObjectProtocol for StatusTarget {}

    impl StatusTarget {
        #[unsafe(method(statusItemClicked:))]
        fn clicked(&self, _sender: Option<&AnyObject>) {
            if let Ok(tx) = self.ivars().tx.lock() {
                let _ = tx.send(UiRequest::ShowPopover);
            }
        }
    }
);

impl StatusTarget {
    fn new(mtm: MainThreadMarker, tx: Sender<UiRequest>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TargetIvars { tx: Mutex::new(tx) });
        unsafe { msg_send![super(this), init] }
    }
}

struct Parts {
    item: Retained<NSStatusItem>,
    _target: Retained<StatusTarget>,
}

pub struct MacTray {
    parts: Arc<MainThreadBound<Parts>>,
}

fn apply_glyph(mtm: MainThreadMarker, item: &NSStatusItem, recording: bool) {
    let Some(button) = item.button(mtm) else {
        return;
    };
    let (symbol, label) = if recording {
        ("record.circle.fill", "Cantrip — recording")
    } else {
        ("waveform", "Cantrip")
    };
    if let Some(image) =
        NSImage::imageWithSystemSymbolName_accessibilityDescription(&ns(symbol), Some(&ns(label)))
    {
        // Colour means state only: template (follows the menu bar) when idle,
        // system red while recording.
        image.setTemplate(!recording);
        button.setImage(Some(&image));
    }
    let tint = recording.then(NSColor::systemRedColor);
    button.setContentTintColor(tint.as_deref());
    button.setToolTip(Some(&ns(label)));
}

impl MacTray {
    pub fn new(mtm: MainThreadMarker, requests: Sender<UiRequest>) -> anyhow::Result<Self> {
        let bar = NSStatusBar::systemStatusBar();
        let item = bar.statusItemWithLength(NSVariableStatusItemLength);
        let target = StatusTarget::new(mtm, requests);
        if let Some(button) = item.button(mtm) {
            unsafe {
                button.setTarget(Some(&target));
                button.setAction(Some(sel!(statusItemClicked:)));
            }
        }
        apply_glyph(mtm, &item, false);
        Ok(MacTray {
            parts: Arc::new(MainThreadBound::new(
                Parts {
                    item,
                    _target: target,
                },
                mtm,
            )),
        })
    }
}

impl Tray for MacTray {
    fn set_recording(&self, recording: bool) {
        if let Some(mtm) = MainThreadMarker::new() {
            apply_glyph(mtm, &self.parts.get(mtm).item, recording);
            return;
        }
        let parts = self.parts.clone();
        DispatchQueue::main().exec_async(move || {
            if let Some(mtm) = MainThreadMarker::new() {
                apply_glyph(mtm, &parts.get(mtm).item, recording);
            }
        });
    }
}
