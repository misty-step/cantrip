//! The general pasteboard. Identity is `changeCount`: every write by any app
//! increments it, so "unchanged since Cantrip wrote" is an exact identity
//! check, never a text comparison. Cantrip's text carries the nspasteboard.org
//! transient + concealed markers so clipboard managers skip it.
//!
//! Not run on a Mac yet; type-checked only (see `platform::macos` docs).

use crate::platform::macos_logic as logic;
use cantrip_core::ports::{ClipSnapshot, ClipToken, Clipboard};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{NSPasteboard, NSPasteboardItem, NSPasteboardWriting};
use objc2_foundation::{NSArray, NSData, NSString};
use std::sync::Mutex;

pub struct MacClipboard {
    /// Serializes Cantrip's own check-then-write sequences.
    lock: Mutex<()>,
}

impl MacClipboard {
    pub fn new() -> Self {
        MacClipboard {
            lock: Mutex::new(()),
        }
    }
}

impl Default for MacClipboard {
    fn default() -> Self {
        Self::new()
    }
}

fn general() -> Retained<NSPasteboard> {
    NSPasteboard::generalPasteboard()
}

impl Clipboard for MacClipboard {
    fn snapshot(&self) -> Option<ClipSnapshot> {
        let _g = self.lock.lock().ok()?;
        let pb = general();
        let mut snap = ClipSnapshot::default();
        let Some(items) = pb.pasteboardItems() else {
            // Unreadable (macOS 15+ may deny programmatic reads): no snapshot.
            return None;
        };
        for item in items.iter() {
            let mut entry = Vec::new();
            for ty in item.types().iter() {
                if let Some(data) = item.dataForType(&ty) {
                    entry.push((ty.to_string(), data.to_vec()));
                }
            }
            snap.items.push(entry);
        }
        Some(snap)
    }

    fn write_text(&self, text: &str, transient: bool) -> Result<ClipToken, String> {
        let _g = self
            .lock
            .lock()
            .map_err(|_| "clipboard lock poisoned".to_string())?;
        let pb = general();
        pb.clearContents();
        let item = NSPasteboardItem::new();
        for ty in logic::pasteboard_types(transient) {
            let ok = if ty == logic::PB_PLAIN_TEXT {
                item.setString_forType(&NSString::from_str(text), &NSString::from_str(ty))
            } else {
                // Marker types carry no data; their presence is the signal.
                item.setData_forType(&NSData::new(), &NSString::from_str(ty))
            };
            if !ok && ty == logic::PB_PLAIN_TEXT {
                return Err("pasteboard refused text".into());
            }
        }
        let writer: Retained<ProtocolObject<dyn NSPasteboardWriting>> =
            ProtocolObject::from_retained(item);
        let array = NSArray::from_retained_slice(&[writer]);
        if !pb.writeObjects(&array) {
            return Err("pasteboard write failed".into());
        }
        Ok(ClipToken(pb.changeCount() as u64))
    }

    fn unchanged_since(&self, token: &ClipToken) -> bool {
        general().changeCount() as u64 == token.0
    }

    fn restore(&self, snapshot: &ClipSnapshot, token: &ClipToken) -> bool {
        let Ok(_g) = self.lock.lock() else {
            return false;
        };
        let pb = general();
        if pb.changeCount() as u64 != token.0 {
            return false;
        }
        let mut writers: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = Vec::new();
        for entry in &snapshot.items {
            let item = NSPasteboardItem::new();
            for (ty, bytes) in entry {
                item.setData_forType(&NSData::with_bytes(bytes), &NSString::from_str(ty));
            }
            writers.push(ProtocolObject::from_retained(item));
        }
        // The narrowest window macOS offers: re-check immediately before clearing.
        if pb.changeCount() as u64 != token.0 {
            return false;
        }
        pb.clearContents();
        if writers.is_empty() {
            return true;
        }
        pb.writeObjects(&NSArray::from_retained_slice(&writers))
    }
}
