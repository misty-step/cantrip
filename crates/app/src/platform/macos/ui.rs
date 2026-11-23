//! Window behavior, fonts, appearance and the folder picker for the egui UI.
//!
//! The HUD window is raised to the status-window level, ignores the mouse
//! (click pass-through), joins every Space (including full-screen ones) and is
//! ordered front with `orderFrontRegardless` — Cantrip is an accessory app and
//! never activates itself, so the HUD never takes focus from the app being
//! dictated into. Not run on a Mac yet; type-checked only.

use super::ns;
use crate::platform::macos_logic as logic;
use crate::platform::raw_window::Raw;
use crate::platform::UiPlatform;
use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication, NSColor, NSEvent,
    NSModalResponseOK, NSOpenPanel, NSScreen, NSStatusWindowLevel, NSView, NSWindow,
    NSWindowCollectionBehavior,
};
use objc2_foundation::{NSArray, NSURL};
use std::path::{Path, PathBuf};

/// Distance of the HUD's bottom edge above the Dock / screen bottom, points.
const HUD_MARGIN: f64 = 72.0;

pub struct MacUi;

impl MacUi {
    pub fn new() -> Self {
        MacUi
    }
}

impl Default for MacUi {
    fn default() -> Self {
        Self::new()
    }
}

/// The NSWindow hosting an egui viewport, from its NSView pointer.
fn window_of(raw: Raw, _mtm: MainThreadMarker) -> Option<Retained<NSWindow>> {
    let Raw::AppKit(ptr) = raw else {
        return None;
    };
    if ptr == 0 {
        return None;
    }
    // SAFETY: winit hands out a live NSView for the viewport's lifetime; we
    // only borrow it on the main thread during this call.
    let view: &NSView = unsafe { &*(ptr as *const NSView) };
    view.window()
}

/// Apply the eframe/winit event-loop settings Cantrip needs on macOS: an
/// accessory (menu-bar) app that never steals activation at launch and adds
/// no default menu. Pass to `NativeOptions::event_loop_builder`.
pub fn event_loop_tweaks(builder: &mut eframe::EventLoopBuilder<eframe::UserEvent>) {
    use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
    builder
        .with_activation_policy(ActivationPolicy::Accessory)
        .with_activate_ignoring_other_apps(false)
        .with_default_menu(false);
}

impl UiPlatform for MacUi {
    fn configure_hud(&self, raw: Raw) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let Some(window) = window_of(raw, mtm) else {
            return;
        };
        {
            {
                window.setLevel(NSStatusWindowLevel);
                window.setIgnoresMouseEvents(true);
                window.setCollectionBehavior(
                    NSWindowCollectionBehavior::CanJoinAllSpaces
                        | NSWindowCollectionBehavior::FullScreenAuxiliary
                        | NSWindowCollectionBehavior::Stationary
                        | NSWindowCollectionBehavior::IgnoresCycle,
                );
                window.setHidesOnDeactivate(false);
                window.setOpaque(false);
                window.setBackgroundColor(Some(&NSColor::clearColor()));
                window.setHasShadow(false);
            }
        }
    }

    fn show_hud(&self, raw: Raw, visible: bool) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let Some(window) = window_of(raw, mtm) else {
            return;
        };
        if visible {
            // Never makeKey / activate: the dictated-into app keeps focus.
            window.orderFrontRegardless();
        }
        // Hidden = parked off-screen by the UI; never ordered out (see trait).
    }

    fn hud_anchor(&self) -> Option<(f32, f32)> {
        let mtm = MainThreadMarker::new()?;
        let screens = NSScreen::screens(mtm);
        let primary = screens.iter().next()?;
        let primary_h = primary.frame().size.height;
        let mouse = NSEvent::mouseLocation();
        let screen = screens
            .iter()
            .find(|s| {
                let f = s.frame();
                logic::frame_contains(
                    (f.origin.x, f.origin.y, f.size.width, f.size.height),
                    mouse.x,
                    mouse.y,
                )
            })
            .unwrap_or(primary);
        let v = screen.visibleFrame();
        let (x, y) = logic::hud_anchor_from_visible_frame(
            primary_h,
            (v.origin.x, v.origin.y, v.size.width, v.size.height),
            HUD_MARGIN,
        );
        // Points → physical pixels (the trait's unit).
        let scale = screen.backingScaleFactor() as f32;
        Some((x * scale, y * scale))
    }

    fn prefers_dark(&self) -> bool {
        let Some(mtm) = MainThreadMarker::new() else {
            return false;
        };
        let app = NSApplication::sharedApplication(mtm);
        let appearance = app.effectiveAppearance();
        let names =
            unsafe { NSArray::from_slice(&[NSAppearanceNameAqua, NSAppearanceNameDarkAqua]) };
        appearance
            .bestMatchFromAppearancesWithNames(&names)
            .is_some_and(|n| &*n == unsafe { NSAppearanceNameDarkAqua })
    }

    fn system_fonts(&self) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
        let read_first = |paths: &[&str]| paths.iter().find_map(|p| std::fs::read(p).ok());
        (
            read_first(&[
                "/System/Library/Fonts/SFNS.ttf",
                "/System/Library/Fonts/SFNSText.ttf",
                "/System/Library/Fonts/Supplemental/Arial.ttf",
            ]),
            read_first(&[
                "/System/Library/Fonts/SFNSMono.ttf",
                "/System/Library/Fonts/Supplemental/Courier New.ttf",
            ]),
        )
    }

    fn choose_folder(&self, start: &Path) -> Result<Option<PathBuf>, String> {
        let mtm = MainThreadMarker::new().ok_or("folder picker needs the main thread")?;
        let panel = NSOpenPanel::openPanel(mtm);
        panel.setCanChooseDirectories(true);
        panel.setCanChooseFiles(false);
        panel.setAllowsMultipleSelection(false);
        panel.setCanCreateDirectories(true);
        if start.is_dir() {
            let url = NSURL::fileURLWithPath(&ns(&start.to_string_lossy()));
            panel.setDirectoryURL(Some(&url));
        }
        if panel.runModal() != NSModalResponseOK {
            return Ok(None);
        }
        Ok(panel
            .URL()
            .and_then(|u| u.path())
            .map(|p| PathBuf::from(p.to_string())))
    }
}
