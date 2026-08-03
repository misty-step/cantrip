//! Cantrip: local-first dictation for Linux.
//!
//! Pipeline: trigger -> capture (pw-record) -> STT (Parakeet via transcribe-rs)
//! -> inject (wtype | ydotool | clipboard).
//!
//! Privacy rule (inherited from Vox): never log transcript content, only
//! character counts. Log tags use brackets: `[Daemon]`, `[Capture]`, `[STT]`,
//! `[Postproc]`, `[Inject]`, `[Models]`.

pub mod capture;
pub mod config;
pub mod daemon;
pub mod inject;
pub mod ipc;
pub mod keys;
pub mod models;
pub mod paths;
pub mod postproc;
pub mod stt;
