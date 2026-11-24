//! macOS 15+ platform backend, built on objc2 bindings to AppKit,
//! ApplicationServices (Accessibility), CoreGraphics, AVFoundation, Core Audio,
//! Speech, Security and ServiceManagement.
//!
//! **Verification boundary:** every item here is type-checked and linted for
//! `aarch64-apple-darwin` and `x86_64-apple-darwin` from Linux, and its pure
//! logic (`platform::macos_logic`) is unit-tested on Linux. None of it has run
//! on a Mac yet; runtime behavior (AX answers per app, ⌘V via
//! `CGEventPostToPid`, the Core Audio process tap, TCC prompts, window
//! layering) awaits a physical macOS 15+ host.
//!
//! Threading: [`platform`] must be called on the main thread before the eframe
//! event loop starts (status item, hotkey manager and appearance queries are
//! main-thread AppKit objects). Desktop, clipboard, audio, speech and secret
//! calls are safe from worker threads.

mod audio;
mod clipboard;
mod desktop;
mod hotkeys;
pub mod sleep;
mod speech;
mod system;
mod tray;
mod ui;

use super::{Platform, Tray, UiRequest};
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use std::sync::mpsc::Sender;
use std::sync::Arc;

pub use ui::event_loop_tweaks;

/// Build the macOS platform. Call on the main thread.
pub fn platform(requests: Sender<UiRequest>) -> anyhow::Result<(Platform, Option<Box<dyn Tray>>)> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| anyhow::anyhow!("macOS platform must be created on the main thread"))?;
    // Menu-bar utility: no Dock icon, never activates itself at launch.
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let desktop = Arc::new(desktop::MacDesktop::new());
    let recorder = Arc::new(audio::MacRecorder::new());
    let meeting = Arc::new(audio::MacMeetingCapture::new(recorder.clone()));
    let tray: Box<dyn Tray> = Box::new(tray::MacTray::new(mtm, requests)?);
    let platform = Platform {
        desktop,
        clipboard: Arc::new(clipboard::MacClipboard::new()),
        recorder,
        meeting,
        local: Arc::new(speech::MacLocalRecognizer::new()),
        secrets: Arc::new(system::KeychainSecrets),
        login: Arc::new(system::MacLoginItem),
        transport: Arc::new(crate::net::UreqTransport::new()),
        hotkeys: Box::new(hotkeys::MacHotkeys::new(mtm)?),
        ui: Arc::new(ui::MacUi::new()),
        session: "macos".to_string(),
    };
    Ok((platform, Some(tray)))
}

/// `NSString` from `&str`.
pub(crate) fn ns(s: &str) -> objc2::rc::Retained<objc2_foundation::NSString> {
    objc2_foundation::NSString::from_str(s)
}
