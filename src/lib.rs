//! Cantrip: local-first dictation for Linux.
//!
//! Pipeline: trigger -> capture (pw-record) -> STT (Parakeet via transcribe-rs)
//! -> optional cleanup -> guarded native Wayland delivery (paste | type | clipboard).
//!
//! Privacy rule (inherited from Vox): never log transcript content, only
//! character counts. Log tags use brackets: `[Daemon]`, `[Capture]`, `[STT]`,
//! `[Postproc]`, `[Inject]`, `[Models]`, `[HUD]`.

mod archive;

pub mod actions;
pub mod capture;
pub mod config;
pub mod daemon;
pub mod desktop;
pub mod hud;
pub mod inject;
pub mod ipc;
pub mod keys;
pub mod models;
pub mod paths;
pub mod pipeline;
pub mod postproc;
pub mod recovery;
pub mod settings;
pub mod stt;
pub mod telemetry;
pub mod theme;
