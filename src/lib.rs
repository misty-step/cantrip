//! Cantrip: local-first dictation for Linux.
//!
//! Pipeline: trigger -> capture (pw-record) -> STT (Parakeet via transcribe-rs)
//! -> paste-first inject (clipboard + `Ctrl+Shift+V`; `type`/`clipboard` modes).
//!
//! Privacy rule (inherited from Vox): never log transcript content, only
//! character counts. Log tags use brackets: `[Daemon]`, `[Capture]`, `[STT]`,
//! `[Postproc]`, `[Inject]`, `[Models]`, `[HUD]`.

mod archive;

pub mod capture;
pub mod config;
pub mod daemon;
pub mod hud;
pub mod inject;
pub mod ipc;
pub mod keys;
pub mod models;
pub mod paths;
pub mod pipeline;
pub mod postproc;
pub mod settings;
pub mod stt;
