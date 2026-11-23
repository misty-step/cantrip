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

pub mod audio;
pub mod clipboard;
pub mod dbus_util;
pub mod desktop;
pub mod hotkeys;
pub mod local;
pub mod login;
pub mod meeting_capture;
pub mod secrets;
pub mod session;
pub mod tray;
pub mod ui;
pub mod window;

use super::{Platform, Tray, UiRequest};
use std::sync::mpsc::Sender;
use std::sync::Arc;

pub fn platform(requests: Sender<UiRequest>) -> anyhow::Result<(Platform, Option<Box<dyn Tray>>)> {
    let session = session::Session::detect();
    let tray = tray::start(requests).map(|t| Box::new(t) as Box<dyn Tray>);
    let platform = Platform {
        desktop: Arc::new(desktop::LinuxDesktop::new()),
        clipboard: Arc::new(clipboard::LinuxClipboard::new()),
        recorder: Arc::new(audio::PipewireRecorder::new()),
        meeting: Arc::new(meeting_capture::PipewireMeetingCapture::new()),
        local: Arc::new(local::NoLocalRecognizer),
        secrets: Arc::new(secrets::SecretService::new()),
        login: Arc::new(login::XdgAutostart::new()),
        transport: Arc::new(crate::net::UreqTransport::new()),
        hotkeys: Box::new(hotkeys::LinuxHotkeys::new()),
        ui: Arc::new(ui::LinuxUi::new()),
        session: session.name().to_string(),
    };
    Ok((platform, tray))
}
