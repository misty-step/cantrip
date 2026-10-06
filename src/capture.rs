//! Native microphone capture; PCM and WAV policy live in the shared engine.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
pub use linux::Recorder;
#[cfg(target_os = "macos")]
pub use macos::{
    diagnosis, input_devices, microphone_permission, request_microphone_permission,
    CaptureDiagnosis, InputDevice, MicrophonePermission, Recorder,
};

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("Cantrip supports Linux and macOS desktop hosts");
