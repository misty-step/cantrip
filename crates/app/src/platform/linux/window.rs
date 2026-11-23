//! PLACEHOLDER (linux-system fork): the real implementation is owned by the
//! linux-desktop fork; this no-op exists only so this branch compiles.

use crate::platform::raw_window::Raw;
use crate::platform::SurfaceKind;

pub struct WindowPatcher;

impl WindowPatcher {
    pub fn new() -> Self {
        WindowPatcher
    }

    pub fn configure_window(&self, _kind: SurfaceKind, _raw: Raw) {}

    pub fn show_hud(&self, _raw: Raw, _visible: bool) {}

    pub fn hud_anchor(&self) -> Option<(f32, f32)> {
        None
    }
}
