//! Delivery choices and effect-aware outcomes shared by every platform.

use serde::{Deserialize, Serialize};

/// Whether a desktop's automatic/paste policy permits a clipboard-only handoff
/// after a proven pre-key deferral. ExplicitOnly also retains partial keyboard
/// requests rather than silently changing their destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardFallback {
    ExplicitOnly,
    OnDeferral,
}

/// Type never reads or writes the clipboard. Clipboard never sends keys.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum InjectionMode {
    #[default]
    Auto,
    Paste,
    Type,
    Clipboard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectionOutcome {
    Typed(&'static str),
    Pasted,
    Clipboard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectionFailureKind {
    Cancelled,
    Deferred,
    Failed,
    Uncertain,
}

#[derive(Debug)]
pub struct InjectionFailure {
    pub kind: InjectionFailureKind,
    /// Sanitized user-facing explanation; never includes transcript or audio.
    pub message: String,
}

impl std::fmt::Display for InjectionFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for InjectionFailure {}
