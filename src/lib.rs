//! Native Linux and macOS hosts for the shared Cantrip dictation engine.
//!
//! Workflow and private history live in `cantrip-engine`. This crate owns
//! capture/delivery mechanisms, desktop surfaces and application lifecycle.
//!
//! Privacy rule (inherited from Vox): never log transcript content, only
//! character counts. Log tags use brackets: `[Daemon]`, `[Capture]`, `[STT]`,
//! `[Postproc]`, `[Inject]`, `[Models]`, `[HUD]`.

pub mod actions;
pub mod capture;
pub mod daemon;
pub mod desktop;
pub mod hud;
pub mod inject;
#[cfg(target_os = "macos")]
pub mod macos;
mod platform;
pub mod settings;
pub mod theme;
