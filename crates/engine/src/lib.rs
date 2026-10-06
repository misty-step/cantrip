//! One authoritative dictation workflow, shared by desktop hosts and file transcription.
//!
//! Native capture and desktop delivery enter through [`ports`]. UI, hotkeys,
//! window systems and application startup remain outside this crate.

mod archive;

pub mod audio;
pub mod config;
pub mod delivery;
pub mod engine;
pub mod ipc;
pub mod keys;
pub mod models;
pub mod paths;
pub mod pipeline;
pub mod ports;
pub mod postproc;
pub mod recovery;
pub mod stt;
pub mod telemetry;
pub mod typesafe;
