//! Focus capture and delivery on macOS: NSWorkspace for the frontmost app,
//! Accessibility (AX) for its focused window and field, tier 1 as a write of
//! `kAXSelectedText` (replaces the selection, never touches the clipboard or
//! keyboard), tier 2 as one ⌘V posted with `CGEventPostToPid` — addressed to
//! the captured process, never to the HID event tap (which would route by
//! keyboard focus).
//!
//! Not run on a Mac yet; type-checked only (see `platform::macos` docs).

use crate::platform::macos_logic as logic;
use cantrip_core::ports::{
    AppIdentity, Desktop, FieldRef, FieldState, FieldSupport, FocusNow, InsertResult, PasteResult,
    Target, WindowRef,
};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{
    NSRunningApplication, NSWorkspace, NSWorkspaceScreensDidSleepNotification,
    NSWorkspaceSessionDidBecomeActiveNotification, NSWorkspaceSessionDidResignActiveNotification,
    NSWorkspaceWillSleepNotification,
};
use objc2_application_services::{
    kAXTrustedCheckOptionPrompt, AXError, AXIsProcessTrusted, AXIsProcessTrustedWithOptions,
    AXUIElement, AXValue, AXValueType,
};
use objc2_core_foundation::{
    CFBoolean, CFDictionary, CFNumber, CFRange, CFRetained, CFString, CFType,
};
use objc2_core_graphics::{CGEvent, CGEventFlags, CGEventSource, CGEventSourceStateID};
use objc2_foundation::{
    NSDistributedNotificationCenter, NSNotification, NSNotificationName, NSObjectProtocol, NSString,
};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Per-call AX messaging timeout: a hung app answers "cannot complete" fast
/// instead of stalling capture or the final verification.
const AX_TIMEOUT_SECS: f32 = 0.2;
/// How many window/field handles the registry keeps alive.
const REGISTRY_CAP: usize = 64;

/// A retained AX element. AXUIElementRef is a CFType documented as usable
/// from any thread; Cantrip never mutates it, only messages through it.
#[derive(Clone)]
struct Ax(CFRetained<AXUIElement>);
unsafe impl Send for Ax {}
unsafe impl Sync for Ax {}

/// Opaque id → AX element map backing `WindowRef` / `FieldRef`.
#[derive(Default)]
struct Registry {
    next: u64,
    entries: VecDeque<(String, Ax)>,
}

impl Registry {
    /// Id for `el`, reusing an existing id when CFEqual (same AX object).
    fn intern(&mut self, prefix: &str, el: CFRetained<AXUIElement>) -> String {
        if let Some((id, _)) = self
            .entries
            .iter()
            .find(|(id, e)| id.starts_with(prefix) && *e.0 == *el)
        {
            return id.clone();
        }
        self.next += 1;
        let id = format!("{prefix}{}", self.next);
        self.entries.push_back((id.clone(), Ax(el)));
        while self.entries.len() > REGISTRY_CAP {
            self.entries.pop_front();
        }
        id
    }

    fn get(&self, id: &str) -> Option<Ax> {
        self.entries
            .iter()
            .find(|(k, _)| k == id)
            .map(|(_, e)| e.clone())
    }
}

/// Notification observer tokens, kept only to stay registered.
struct Observers(#[allow(dead_code)] Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>);
unsafe impl Send for Observers {}
unsafe impl Sync for Observers {}

pub struct MacDesktop {
    registry: Mutex<Registry>,
    epoch: Arc<AtomicU64>,
    prompted: AtomicBool,
    _observers: Observers,
}

impl MacDesktop {
    pub fn new() -> Self {
        let epoch = Arc::new(AtomicU64::new(0));
        let observers = Observers(register_epoch_observers(&epoch));
        MacDesktop {
            registry: Mutex::new(Registry::default()),
            epoch,
            prompted: AtomicBool::new(false),
            _observers: observers,
        }
    }

    fn element(&self, id: &str) -> Option<Ax> {
        self.registry.lock().ok()?.get(id)
    }

    fn field(&self, target: &Target) -> Option<Ax> {
        target.field.as_ref().and_then(|f| self.element(&f.0))
    }

    /// The frontmost app's identity and AX application element.
    fn frontmost(&self) -> Option<(AppIdentity, CFRetained<AXUIElement>)> {
        let app = NSWorkspace::sharedWorkspace().frontmostApplication()?;
        let identity = identity_of(&app);
        let ax = unsafe { AXUIElement::new_application(identity.pid as libc::pid_t) };
        unsafe {
            let _ = ax.set_messaging_timeout(AX_TIMEOUT_SECS);
        }
        Some((identity, ax))
    }

    fn focused_window_id(&self, app: &AXUIElement) -> Option<String> {
        let win = copy_element(app, "AXFocusedWindow").ok()?;
        Some(self.registry.lock().ok()?.intern("w", win))
    }
}

impl Default for MacDesktop {
    fn default() -> Self {
        Self::new()
    }
}

fn identity_of(app: &NSRunningApplication) -> AppIdentity {
    let pid = app.processIdentifier().max(0) as u32;
    let launch = app.launchDate().map(|d| d.timeIntervalSince1970());
    let app_id = app
        .bundleIdentifier()
        .map(|b| b.to_string())
        .unwrap_or_else(|| format!("pid:{pid}"));
    AppIdentity {
        app_id,
        pid,
        instance: logic::app_instance(pid, launch),
    }
}

fn cfstr(s: &str) -> CFRetained<CFString> {
    CFString::from_str(s)
}

/// `AXUIElementCopyAttributeValue`, returning the AX error code on failure.
fn copy_attr(el: &AXUIElement, name: &str) -> Result<CFRetained<CFType>, i32> {
    let mut value: *const CFType = std::ptr::null();
    let err = unsafe { el.copy_attribute_value(&cfstr(name), NonNull::from(&mut value)) };
    if err != AXError::Success {
        return Err(err.0);
    }
    NonNull::new(value as *mut CFType)
        .map(|p| unsafe { CFRetained::from_raw(p) })
        .ok_or(logic::AX_NO_VALUE)
}

fn copy_element(el: &AXUIElement, name: &str) -> Result<CFRetained<AXUIElement>, i32> {
    copy_attr(el, name)?
        .downcast::<AXUIElement>()
        .map_err(|_| logic::AX_FAILURE)
}

fn copy_string(el: &AXUIElement, name: &str) -> Option<String> {
    let v = copy_attr(el, name).ok()?;
    v.downcast_ref::<CFString>().map(|s| s.to_string())
}

fn copy_range(el: &AXUIElement, name: &str) -> Option<(usize, usize)> {
    let v = copy_attr(el, name).ok()?;
    let ax = v.downcast_ref::<AXValue>()?;
    let mut range = CFRange::new(0, 0);
    let ok = unsafe {
        ax.value(
            AXValueType::CFRange,
            NonNull::from(&mut range).cast::<c_void>(),
        )
    };
    if !ok || range.location < 0 || range.length < 0 {
        return None;
    }
    Some((range.location as usize, range.length as usize))
}

fn copy_i64(el: &AXUIElement, name: &str) -> Option<i64> {
    let v = copy_attr(el, name).ok()?;
    v.downcast_ref::<CFNumber>()?.as_i64()
}

/// Bump `epoch` on sleep, screen sleep, screen lock and fast user switching.
fn register_epoch_observers(
    epoch: &Arc<AtomicU64>,
) -> Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>> {
    let mut tokens = Vec::new();
    let names: [&NSNotificationName; 4] = unsafe {
        [
            NSWorkspaceWillSleepNotification,
            NSWorkspaceScreensDidSleepNotification,
            NSWorkspaceSessionDidResignActiveNotification,
            NSWorkspaceSessionDidBecomeActiveNotification,
        ]
    };
    let center = NSWorkspace::sharedWorkspace().notificationCenter();
    for name in names {
        let e = epoch.clone();
        let block = block2::RcBlock::new(move |_n: NonNull<NSNotification>| {
            e.fetch_add(1, Ordering::SeqCst);
        });
        let token = unsafe {
            center.addObserverForName_object_queue_usingBlock(Some(name), None, None, &block)
        };
        tokens.push(token);
    }
    // Screen lock is only published on the distributed center.
    let distributed = NSDistributedNotificationCenter::defaultCenter();
    let locked = NSString::from_str("com.apple.screenIsLocked");
    let e = epoch.clone();
    let block = block2::RcBlock::new(move |_n: NonNull<NSNotification>| {
        e.fetch_add(1, Ordering::SeqCst);
    });
    let token = unsafe {
        distributed.addObserverForName_object_queue_usingBlock(Some(&locked), None, None, &block)
    };
    tokens.push(token);
    tokens
}

impl Desktop for MacDesktop {
    fn capture_target(&self) -> Option<Target> {
        let (app, ax_app) = self.frontmost()?;
        let window = self.focused_window_id(&ax_app)?;
        let field = copy_element(&ax_app, "AXFocusedUIElement")
            .ok()
            .and_then(|f| Some(self.registry.lock().ok()?.intern("f", f)))
            .map(FieldRef);
        Some(Target {
            app,
            window: WindowRef(window),
            field,
        })
    }

    fn focus_now(&self) -> Option<FocusNow> {
        let (app, ax_app) = self.frontmost()?;
        let window = self.focused_window_id(&ax_app)?;
        Some(FocusNow {
            app,
            window: WindowRef(window),
        })
    }

    fn app_alive(&self, app: &AppIdentity) -> bool {
        match NSRunningApplication::runningApplicationWithProcessIdentifier(app.pid as libc::pid_t)
        {
            Some(running) => {
                !running.isTerminated() && identity_of(&running).instance == app.instance
            }
            None => false,
        }
    }

    fn missing_delivery_permission(&self) -> Option<String> {
        if unsafe { AXIsProcessTrusted() } {
            return None;
        }
        // Just in time: the first delivery that needs Accessibility asks once.
        if !self.prompted.swap(true, Ordering::SeqCst) {
            let key: &CFString = unsafe { kAXTrustedCheckOptionPrompt };
            let value: &CFBoolean = CFBoolean::new(true);
            let options = CFDictionary::<CFString, CFBoolean>::from_slices(&[key], &[value]);
            unsafe {
                AXIsProcessTrustedWithOptions(Some(options.as_opaque()));
            }
        }
        Some("Accessibility".to_string())
    }

    fn session_epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    fn field_support(&self, target: &Target) -> FieldSupport {
        let Some(field) = self.field(target) else {
            return FieldSupport::Gone;
        };
        let mut settable: u8 = 0;
        let err = unsafe {
            field
                .0
                .is_attribute_settable(&cfstr("AXSelectedText"), NonNull::from(&mut settable))
        };
        logic::classify_support(err.0, settable != 0)
    }

    fn read_field(&self, target: &Target) -> Option<FieldState> {
        let field = self.field(target)?;
        let text = copy_string(&field.0, "AXValue")?;
        let (loc, len) = copy_range(&field.0, "AXSelectedTextRange")?;
        let selection = logic::utf16_range_to_chars(&text, loc, len)?;
        let complete = logic::value_is_complete(&text, copy_i64(&field.0, "AXNumberOfCharacters"));
        Some(FieldState {
            text,
            selection,
            complete,
        })
    }

    fn insert_text(&self, target: &Target, text: &str) -> InsertResult {
        let Some(field) = self.field(target) else {
            return InsertResult::Refused;
        };
        let value = cfstr(text);
        let err = unsafe {
            field
                .0
                .set_attribute_value(&cfstr("AXSelectedText"), &value)
        };
        logic::classify_insert(err.0)
    }

    fn can_address_paste(&self, _app: &AppIdentity) -> bool {
        true
    }

    fn post_paste(&self, target: &Target) -> PasteResult {
        if !self.app_alive(&target.app) {
            return PasteResult::Failed;
        }
        let source = CGEventSource::new(CGEventSourceStateID::Private);
        let down = CGEvent::new_keyboard_event(source.as_deref(), logic::KVK_ANSI_V, true);
        let up = CGEvent::new_keyboard_event(source.as_deref(), logic::KVK_ANSI_V, false);
        let (Some(down), Some(up)) = (down, up) else {
            return PasteResult::Failed;
        };
        CGEvent::set_flags(Some(&down), CGEventFlags::MaskCommand);
        CGEvent::set_flags(Some(&up), CGEventFlags::MaskCommand);
        let pid = target.app.pid as libc::pid_t;
        CGEvent::post_to_pid(pid, Some(&down));
        CGEvent::post_to_pid(pid, Some(&up));
        PasteResult::Posted
    }
}
