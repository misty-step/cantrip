//! The egui UI: popover, single-pane Settings and the HUD pill, one codebase
//! for macOS and Linux. A pure view over `cantrip_core::app::Control`.
//! OWNER: ui fork.

use crate::platform::{UiPlatform, UiRequest};
use cantrip_core::app::Control;
use std::sync::mpsc::Receiver;
use std::sync::Arc;

/// Run the UI event loop on the main thread until Quit.
/// `requests` carries tray / launcher / IPC activations.
pub fn run(
    control: Arc<dyn Control>,
    platform: Arc<dyn UiPlatform>,
    requests: Receiver<UiRequest>,
) -> anyhow::Result<()> {
    let _ = (control, platform, requests);
    todo!()
}
