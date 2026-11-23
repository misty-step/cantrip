//! The global shortcut via Carbon `RegisterEventHotKey` (the `global-hotkey`
//! crate), which reports press *and* release on macOS — hold-to-talk and
//! tap-to-toggle both work — and needs no Accessibility or Input Monitoring
//! grant. Escape is registered as a second hot key only while a take is
//! active, so it is never captured otherwise.
//!
//! The manager must be created on the main thread (Carbon events arrive on
//! the main run loop, which winit drives). Not run on a Mac yet; type-checked
//! only (see `platform::macos` docs).

use crate::platform::macos_logic as logic;
use crate::platform::{HotkeyMode, Hotkeys};
use cantrip_core::shortcut::{Chord, KeyEvent};
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use objc2::MainThreadMarker;
use std::str::FromStr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Hot key ids the event handler routes on.
#[derive(Default)]
struct Ids {
    chord: AtomicU32,
    escape: AtomicU32,
}

pub struct MacHotkeys {
    manager: GlobalHotKeyManager,
    chord: Option<HotKey>,
    escape: HotKey,
    escape_on: bool,
    ids: Arc<Ids>,
    sink: Arc<Mutex<Option<Sender<KeyEvent>>>>,
}

// The manager is only touched from the thread that created it (main); the
// trait requires Send so the Platform bundle can be assembled and moved into
// the UI owner on that same thread.
unsafe impl Send for MacHotkeys {}

impl MacHotkeys {
    pub fn new(_mtm: MainThreadMarker) -> anyhow::Result<Self> {
        let manager = GlobalHotKeyManager::new()
            .map_err(|e| anyhow::anyhow!("global shortcut unavailable: {e}"))?;
        let ids = Arc::new(Ids::default());
        let sink: Arc<Mutex<Option<Sender<KeyEvent>>>> = Arc::new(Mutex::new(None));
        let escape = HotKey::new(None, Code::Escape);
        ids.escape.store(escape.id(), Ordering::SeqCst);
        let (h_ids, h_sink) = (ids.clone(), sink.clone());
        GlobalHotKeyEvent::set_event_handler(Some(move |ev: GlobalHotKeyEvent| {
            let now = Instant::now();
            let event = if ev.id() == h_ids.escape.load(Ordering::SeqCst) {
                (ev.state() == HotKeyState::Pressed).then_some(KeyEvent::Escape)
            } else if ev.id() == h_ids.chord.load(Ordering::SeqCst) {
                Some(match ev.state() {
                    HotKeyState::Pressed => KeyEvent::Pressed(now),
                    HotKeyState::Released => KeyEvent::Released(now),
                })
            } else {
                None
            };
            if let (Some(event), Ok(sink)) = (event, h_sink.lock()) {
                if let Some(tx) = sink.as_ref() {
                    let _ = tx.send(event);
                }
            }
        }));
        Ok(MacHotkeys {
            manager,
            chord: None,
            escape,
            escape_on: false,
            ids,
            sink,
        })
    }
}

fn to_hotkey(chord: &Chord) -> Result<HotKey, String> {
    if !chord.is_valid() {
        return Err("Choose a shortcut with a modifier and a key".into());
    }
    let name = logic::chord_key_code_name(&chord.key)
        .ok_or_else(|| format!("{} cannot be used as a shortcut key", chord.key))?;
    let code = Code::from_str(&name).map_err(|_| format!("unknown key {}", chord.key))?;
    let (ctrl, alt, shift, meta) = logic::chord_modifiers(chord);
    let mut mods = Modifiers::empty();
    if ctrl {
        mods |= Modifiers::CONTROL;
    }
    if alt {
        mods |= Modifiers::ALT;
    }
    if shift {
        mods |= Modifiers::SHIFT;
    }
    if meta {
        mods |= Modifiers::META;
    }
    Ok(HotKey::new(Some(mods), code))
}

impl Hotkeys for MacHotkeys {
    fn register(&mut self, chord: &Chord, sink: Sender<KeyEvent>) -> Result<HotkeyMode, String> {
        let hotkey = to_hotkey(chord)?;
        if let Some(old) = self.chord.take() {
            let _ = self.manager.unregister(old);
        }
        self.manager.register(hotkey).map_err(|e| {
            format!(
                "{} is already used by another app ({e})",
                chord.display_macos()
            )
        })?;
        self.ids.chord.store(hotkey.id(), Ordering::SeqCst);
        self.chord = Some(hotkey);
        if let Ok(mut s) = self.sink.lock() {
            *s = Some(sink);
        }
        Ok(HotkeyMode::PressRelease)
    }

    fn set_escape_grab(&mut self, active: bool) {
        if active == self.escape_on {
            return;
        }
        let ok = if active {
            self.manager.register(self.escape).is_ok()
        } else {
            self.manager.unregister(self.escape).is_ok()
        };
        if ok {
            self.escape_on = active;
        }
    }
}
