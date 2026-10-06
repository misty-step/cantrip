//! Native delivery mechanisms selected by the desktop host.
//!
//! Policy, configuration and effect-aware result types live in
//! `cantrip_engine::delivery`; this module owns only native destination permits
//! and input/clipboard handoffs. A native acknowledgement is not app receipt.

use cantrip_engine::delivery::{
    InjectionFailure, InjectionFailureKind, InjectionMode, InjectionOutcome,
};
#[cfg(target_os = "linux")]
use std::borrow::Cow;
use std::{
    env,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
pub use linux::{inject, planned_backend_names, virtual_keyboard_available};
#[cfg(target_os = "macos")]
pub use macos::{inject, planned_backend_names};

type Result<T, E = InjectionFailure> = std::result::Result<T, E>;

/// Intended destination and uninterrupted session history, captured at stop.
#[derive(Clone)]
pub struct DeliveryGuard {
    desktop: crate::desktop::Guard,
}

impl DeliveryGuard {
    /// Linux prepares native observation. Unsupported hosts have no observer
    /// capable of authorizing input; explicit clipboard delivery needs none.
    pub fn prepare() {
        #[cfg(target_os = "linux")]
        crate::desktop::Guard::prepare();
    }

    /// Capture cached history immediately. An unready/unsupported permit defers.
    pub fn capture() -> Self {
        Self {
            desktop: crate::desktop::Guard::capture(),
        }
    }

    #[cfg(target_os = "linux")]
    fn check(&self, cancel: &AtomicBool) -> Result<()> {
        cancelled(cancel)?;
        self.desktop
            .check()
            .map_err(|message| failure(InjectionFailureKind::Deferred, message))?;
        cancelled(cancel)
    }

    #[cfg(target_os = "linux")]
    fn check_history(&self, cancel: &AtomicBool) -> Result<()> {
        cancelled(cancel)?;
        self.desktop
            .check_history()
            .map_err(|message| failure(InjectionFailureKind::Deferred, message))
    }
}

impl cantrip_engine::ports::DeliveryPermit for DeliveryGuard {
    fn clipboard_fallback(&self) -> cantrip_engine::delivery::ClipboardFallback {
        if cfg!(target_os = "linux") {
            cantrip_engine::delivery::ClipboardFallback::OnDeferral
        } else {
            cantrip_engine::delivery::ClipboardFallback::ExplicitOnly
        }
    }
    fn deliver(
        &self,
        text: &str,
        mode: InjectionMode,
        cancel: &AtomicBool,
    ) -> Result<InjectionOutcome> {
        inject(text, mode, self, cancel)
    }
}

/// Native mechanisms and separately the evidence needed to authorize input.
/// `keyboard` does not imply that a stop-time destination permit is verified.
#[derive(Clone, Copy, Debug)]
pub struct DeliveryAvailability {
    pub keyboard: bool,
    pub clipboard: bool,
    pub automatic: bool,
    pub reason: Option<&'static str>,
}

pub fn delivery_availability() -> DeliveryAvailability {
    #[cfg(target_os = "linux")]
    {
        DeliveryAvailability {
            keyboard: virtual_keyboard_available(),
            clipboard: executable_in_path("wl-copy"),
            // This is platform support, not authorization of the current focus.
            // The Linux guard independently verifies every stop-time permit.
            automatic: true,
            reason: None,
        }
    }
    #[cfg(target_os = "macos")]
    {
        macos::delivery_availability()
    }
}

/// True when `name` resolves to an executable regular file on PATH.
pub fn executable_in_path(name: &str) -> bool {
    let Some(path_var) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&path_var).any(|directory| {
        let directory = if directory.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            directory
        };
        directory
            .join(name)
            .metadata()
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    })
}

fn failure(kind: InjectionFailureKind, message: impl Into<String>) -> InjectionFailure {
    InjectionFailure {
        kind,
        message: message.into(),
    }
}

fn failed(message: &'static str) -> InjectionFailure {
    failure(InjectionFailureKind::Failed, message)
}

fn cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        Err(failure(
            InjectionFailureKind::Cancelled,
            "Delivery was cancelled before any further input.",
        ))
    } else {
        Ok(())
    }
}

fn check_deadline(deadline: Instant) -> Result<()> {
    if Instant::now() >= deadline {
        Err(failed(
            "Delivery exceeded its time limit; no further input was sent.",
        ))
    } else {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn after_keys(error: InjectionFailure) -> InjectionFailure {
    failure(
        InjectionFailureKind::Uncertain,
        format!(
            "Keyboard delivery was interrupted and may be partial. No fallback was attempted. {}",
            error.message
        ),
    )
}

/// Controls must not become Return/Tab/Escape in a live application. Unicode
/// prose is preserved; consecutive controls collapse into one literal space.
/// Paragraph-preserving delivery uses Paste or explicit Clipboard instead.
#[cfg(target_os = "linux")]
fn normalize_for_typing(text: &str) -> Cow<'_, str> {
    if !text.chars().any(char::is_control) {
        return Cow::Borrowed(text);
    }
    let mut result = String::with_capacity(text.len());
    let mut pending_space = false;
    for character in text.chars() {
        if character.is_control() {
            pending_space = true;
        } else {
            if pending_space {
                result.push(' ');
                pending_space = false;
            }
            result.push(character);
        }
    }
    if pending_space {
        result.push(' ');
    }
    Cow::Owned(result)
}
