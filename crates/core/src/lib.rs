//! Cantrip v2 core: every product rule that does not depend on an operating
//! system. Platform mechanisms enter only through [`ports`].
//!
//! Module map (deep modules with small interfaces):
//! - [`shortcut`]  hold-to-talk vs tap-to-toggle vs Escape, from raw key events
//! - [`speech`]    OpenRouter transcription + rewrite contracts and fallbacks
//! - [`offline`]   the fail-closed no-key / no-network local recognition gate
//! - [`delivery`]  guarded two-tier delivery and clipboard safety
//! - [`dictation`] one take from press to outcome, with retention rules
//! - [`meeting`]   dual-track meeting processing, notes and the archive
//! - [`staging`]   owner-only durable private storage and recovery items
//! - [`surface`]   the closed control surface and HUD vocabulary
//! - [`settings`]  the six Settings values plus mode / post-processing
//! - [`app`]       the facade the UI drives: snapshot in, commands out

pub mod app;
pub mod delivery;
pub mod dictation;
pub mod meeting;
pub mod offline;
pub mod ports;
pub mod settings;
pub mod shortcut;
pub mod speech;
pub mod staging;
pub mod surface;
pub mod vocabulary;
pub mod wav;
