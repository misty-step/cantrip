//! Linux platform: X11 and Wayland (KDE Plasma, GNOME, wlroots).
//!
//! Mechanism map:
//! - audio: PipeWire `pw-record` ([`audio`], [`meeting_capture`])
//! - secrets: freedesktop Secret Service over D-Bus ([`secrets`])
//! - login: XDG autostart entry ([`login`])
//! - tray: StatusNotifierItem when a watcher exists ([`tray`]); trayless
//!   sessions open the popover from the launcher entry (`cantrip popover`)
//! - local recognition: none guaranteed on-device → fails closed ([`local`])
//! - focus, delivery, clipboard, shortcut, window behavior: [`desktop`],
//!   [`clipboard`], [`hotkeys`], [`window`]
//! - Sway: compositor IPC for the shortcut and Escape ([`sway`])
//! - HUD on native Wayland: a `wlr-layer-shell` overlay ([`layer_hud`])
//! - sleep: systemd-logind inhibitor locks + `PrepareForSleep` ([`sleep`])

pub mod atspi;
pub mod audio;
pub mod clipboard;
pub mod dbus_util;
pub mod desktop;
pub mod hotkeys;
pub mod layer_hud;
pub mod local;
pub mod login;
pub mod meeting_capture;
pub mod secrets;
pub mod session;
pub mod sleep;
pub mod sway;
pub mod tray;
pub mod ui;
pub mod window;
pub mod x11;

use super::{Platform, Tray, UiRequest};
use std::sync::mpsc::Sender;
use std::sync::Arc;

pub fn platform(requests: Sender<UiRequest>) -> anyhow::Result<(Platform, Option<Box<dyn Tray>>)> {
    let session = session::Session::detect();
    let tray = tray::start(requests).map(|t| Box::new(t) as Box<dyn Tray>);
    let clipboard = clipboard::LinuxClipboard::new();
    let name = format!(
        "{} (clipboard: {})",
        session.name(),
        clipboard.backend_name()
    );
    let platform = Platform {
        desktop: Arc::new(desktop::LinuxDesktop::new()),
        clipboard: Arc::new(clipboard),
        recorder: Arc::new(audio::PipewireRecorder::new()),
        meeting: Arc::new(meeting_capture::PipewireMeetingCapture::new()),
        local: Arc::new(local::NoLocalRecognizer),
        secrets: Arc::new(secrets::SecretService::new()),
        login: Arc::new(login::XdgAutostart::new()),
        transport: Arc::new(crate::net::UreqTransport::new()),
        hotkeys: Box::new(hotkeys::LinuxHotkeys::new()),
        ui: Arc::new(ui::LinuxUi::new()),
        session: name,
    };
    Ok((platform, tray))
}

/// Native Wayland: never let an EGL swap wait on frame callbacks. The egui
/// root window is a small transparent window kept out of the way there (the HUD is a layer
/// surface); if the compositor stops sending it frame callbacks (occluded),
/// a vsynced swap would block the UI thread that also runs the HUD and the
/// shortcut/Escape bookkeeping.
pub fn event_loop_tweaks(options: &mut eframe::NativeOptions) {
    if x11::session_is_wayland() {
        options.glow_options.vsync = false;
    }
}
