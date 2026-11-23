//! The OS boundary of the app crate. Each OS module returns one [`Platform`]
//! bundle of core port implementations plus the few UI/system hooks the core
//! does not need (hotkeys, tray, window behavior, fonts, theme, folder picker).
//! Nothing outside `platform/` contains `cfg(target_os)` except `main.rs`.

use cantrip_core::ports::{
    Clipboard, Desktop, LocalRecognizer, LoginItem, MeetingCapture, Recorder, SecretStore,
    Transport,
};
use cantrip_core::shortcut::{Chord, KeyEvent};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::Arc;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;

/// How the registered shortcut reports presses on this session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeyMode {
    /// Press and release reported: hold-to-talk and tap-to-toggle.
    PressRelease,
    /// Activation only (some Wayland portals): tap-to-toggle.
    ActivationOnly,
}

/// The one global shortcut plus Escape-during-a-take.
pub trait Hotkeys: Send {
    /// (Re)register `chord`; events go to `sink`. Replaces any earlier chord.
    fn register(&mut self, chord: &Chord, sink: Sender<KeyEvent>) -> Result<HotkeyMode, String>;
    /// Grab Escape only while a take is active (`KeyEvent::Escape` to the sink).
    fn set_escape_grab(&mut self, active: bool);
}

/// Requests from the tray / launcher / IPC to the UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiRequest {
    ShowPopover,
    ShowSettings,
    ToggleDictation,
}

/// Status-area icon. Activation (click) sends `UiRequest::ShowPopover`.
pub trait Tray: Send {
    /// Reflect recording state in the icon (red when recording).
    fn set_recording(&self, recording: bool);
}

/// Which native window a UI surface is, for platform window patching.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceKind {
    Hud,
    Popover,
    Settings,
}

/// Window behavior and look hooks for the egui UI.
pub trait UiPlatform: Send + Sync {
    /// Patch a freshly created native window: the HUD becomes non-activating,
    /// click-through, on every workspace, above others; the popover floats.
    /// `raw` is the platform window handle (X11 window id, NSView pointer…).
    fn configure_window(&self, kind: SurfaceKind, raw: raw_window::Raw);
    /// Show / hide the HUD window without activating Cantrip.
    fn show_hud(&self, raw: raw_window::Raw, visible: bool);
    /// Bottom-centre point (logical px) of the display under the pointer, for
    /// the HUD, and the work-area top-right for the popover, if known.
    fn hud_anchor(&self) -> Option<(f32, f32)>;
    /// The system appearance right now.
    fn prefers_dark(&self) -> bool;
    /// (sans-serif, monospace) system font bytes.
    fn system_fonts(&self) -> (Option<Vec<u8>>, Option<Vec<u8>>);
    /// Native folder picker. `Err` when no picker exists on this session (the
    /// UI then offers an inline path field).
    fn choose_folder(&self, start: &std::path::Path) -> Result<Option<PathBuf>, String>;
}

pub mod raw_window {
    /// A native window handle as an integer (X11 window id, Wayland surface
    /// pointer, NSView pointer).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Raw {
        X11(u64),
        Wayland(usize),
        AppKit(usize),
        Unknown,
    }
}

/// Everything the app needs from one OS session.
pub struct Platform {
    pub desktop: Arc<dyn Desktop>,
    pub clipboard: Arc<dyn Clipboard>,
    pub recorder: Arc<dyn Recorder>,
    pub meeting: Arc<dyn MeetingCapture>,
    pub local: Arc<dyn LocalRecognizer>,
    pub secrets: Arc<dyn SecretStore>,
    pub login: Arc<dyn LoginItem>,
    pub transport: Arc<dyn Transport>,
    pub hotkeys: Box<dyn Hotkeys>,
    pub ui: Arc<dyn UiPlatform>,
    /// Session description for logs and the release matrix
    /// (e.g. "linux-x11", "linux-wayland-kde", "macos").
    pub session: String,
}

/// Build the platform for this session. `requests` receives tray/launcher
/// activations. Returns the tray when this session has one (trayless sessions
/// use the launcher entry → `cantrip popover`).
pub fn detect(requests: Sender<UiRequest>) -> anyhow::Result<(Platform, Option<Box<dyn Tray>>)> {
    #[cfg(target_os = "linux")]
    {
        linux::platform(requests)
    }
    #[cfg(target_os = "macos")]
    {
        macos::platform(requests)
    }
}
