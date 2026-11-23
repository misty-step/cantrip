//! PLACEHOLDER (linux-system fork): the real implementation is owned by the
//! linux-desktop fork; this one cannot write and so never claims anything.

use cantrip_core::ports::{ClipSnapshot, ClipToken, Clipboard};

pub struct LinuxClipboard;

impl LinuxClipboard {
    pub fn new() -> Self {
        LinuxClipboard
    }
}

impl Clipboard for LinuxClipboard {
    fn snapshot(&self) -> Option<ClipSnapshot> {
        None
    }
    fn write_text(&self, _text: &str, _transient: bool) -> Result<ClipToken, String> {
        Err("clipboard backend not linked in this branch".into())
    }
    fn unchanged_since(&self, _token: &ClipToken) -> bool {
        false
    }
    fn restore(&self, _snapshot: &ClipSnapshot, _token: &ClipToken) -> bool {
        false
    }
}
