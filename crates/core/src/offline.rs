//! The fail-closed no-key / no-network gate (DESIGN.md › Local recognition
//! fails closed). Entered only for a finished dictation with no key or no
//! connection, never for meetings. Re-checks availability every time; on any
//! refusal no audio leaves the machine and the recording stays staged.
//!
//! OWNER: core-dictation fork.

use crate::ports::{LocalAvailability, LocalRecognizer};
use std::path::Path;

pub const FIX_ALLOW: &str = "Allow speech recognition for Cantrip, or add an OpenRouter key";
pub const FIX_NO_ON_DEVICE: &str = "No on-device English speech here. Add an OpenRouter key";

/// Run on-device recognition or refuse with the HUD fix text
/// (shown as `Not transcribed. <fix>`).
pub fn recognize_locally(local: &dyn LocalRecognizer, wav: &Path) -> Result<String, String> {
    let _ = (local, wav, LocalAvailability::Ready);
    todo!()
}
