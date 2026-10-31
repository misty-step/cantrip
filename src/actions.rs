//! `cantrip actions` — Recordings and Recovery: deliberately opened,
//! metadata-only recovery and setup.
//!
//! It lists saved takes by their stable IDs, copies a saved transcript or
//! transcribes a take's retained audio again to the clipboard, forgets a take's
//! retained audio only after explicit confirmation, dismisses outcomes, and
//! runs setup checks. Transcript text never reaches this window: the engine
//! rereads it and delivers it.
//!
//! [`model`] holds that state for both platforms and [`effects`] runs its
//! observations and actions on worker threads. Linux renders it in an egui
//! window (`linux`). macOS renders it in an AppKit window inside the menu-bar
//! app (`crate::macos::recordings`); there `cantrip actions` asks the running
//! app to show its window.
//!
//! `cantrip actions --screenshot <path>` renders the window to a PNG and exits,
//! for visual testing without a screenshot utility.

pub(crate) mod effects;
#[cfg(not(target_os = "macos"))]
mod linux;
pub(crate) mod model;

use anyhow::Result;
use std::path::PathBuf;

/// Entry point for the `cantrip actions` subcommand; `doctor` runs the setup
/// check as the window opens.
#[cfg(not(target_os = "macos"))]
pub fn run(screenshot: Option<PathBuf>, doctor: bool) -> Result<()> {
    linux::run(screenshot, doctor)
}

/// Entry point for the `cantrip actions` subcommand: show the menu-bar app's
/// Recordings and Recovery window, on its setup check with `doctor`, or render
/// it offscreen to the `screenshot` PNG.
#[cfg(target_os = "macos")]
pub fn run(screenshot: Option<PathBuf>, doctor: bool) -> Result<()> {
    use crate::macos::requests::{self, Show};
    match screenshot {
        Some(path) => crate::macos::recordings::screenshot(&path, doctor),
        None if doctor => requests::show(Show::CheckSetup),
        None => requests::show(Show::Recordings),
    }
}

/// Human-readable local capture time, shared by the explicit metadata CLI.
pub fn take_time(unix_ms: u64) -> String {
    let Ok(seconds) = libc::time_t::try_from(unix_ms / 1000) else {
        return "Capture time unavailable".to_owned();
    };
    let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
    // localtime_r writes only our stack allocation and uses the system timezone.
    let result = unsafe { libc::localtime_r(&seconds, local.as_mut_ptr()) };
    if result.is_null() {
        return "Capture time unavailable".to_owned();
    }
    let local = unsafe { local.assume_init() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} local",
        local.tm_year + 1900,
        local.tm_mon + 1,
        local.tm_mday,
        local.tm_hour,
        local.tm_min,
        local.tm_sec
    )
}

pub fn take_duration(duration_ms: Option<u64>) -> String {
    match duration_ms {
        Some(ms) if ms < 1000 => format!("{ms} ms"),
        Some(ms) => format!("{}m {:02}s", ms / 60_000, (ms / 1000) % 60),
        None => "duration unknown".to_owned(),
    }
}
