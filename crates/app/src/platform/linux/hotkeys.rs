//! PLACEHOLDER (linux-system fork): the real implementation is owned by the
//! linux-desktop fork.

use crate::platform::{HotkeyMode, Hotkeys};
use cantrip_core::shortcut::{Chord, KeyEvent};
use std::sync::mpsc::Sender;

pub struct LinuxHotkeys;

impl LinuxHotkeys {
    pub fn new() -> Self {
        LinuxHotkeys
    }
}

impl Hotkeys for LinuxHotkeys {
    fn register(&mut self, _chord: &Chord, _sink: Sender<KeyEvent>) -> Result<HotkeyMode, String> {
        Err("global shortcut backend not linked in this branch".into())
    }
    fn set_escape_grab(&mut self, _active: bool) {}
}
