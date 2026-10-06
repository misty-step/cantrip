//! The global dictation shortcut.
//!
//! One Carbon hot key, registered through `global-hotkey`, toggles dictation from
//! any app and follows `hotkey` in the configuration file. Carbon hot keys need
//! neither Accessibility nor Input Monitoring access; media keys would need an
//! event tap, so they are refused instead.

use cantrip_engine::{config::Config, paths};
use global_hotkey::{
    hotkey::{Code, HotKey, Modifiers},
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
};
use std::{fs, time::SystemTime};

/// The shortcut used while `config.hotkey` is absent.
pub(crate) const DEFAULT: &str = "Control+Option+Space";

/// A shortcut that is safe to register system-wide, with its macOS symbol label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Shortcut {
    hotkey: HotKey,
    label: String,
}

impl Shortcut {
    /// The configured shortcut, or [`DEFAULT`] when none is configured.
    pub(crate) fn configured(text: Option<&str>) -> Result<Self, String> {
        Self::parse(text.unwrap_or(DEFAULT))
    }

    /// Modifiers and one key joined by `+`, e.g. `Control+Option+Space` or
    /// `Command+Shift+D`. A key without Control, Option or Command would capture
    /// ordinary typing everywhere, so only function keys may stand alone.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let hotkey: HotKey = text.parse().map_err(|_| {
            format!("“{text}” is not a shortcut. Use modifiers and one key, like {DEFAULT}.")
        })?;
        if matches!(
            hotkey.key,
            Code::MediaPlay
                | Code::MediaPause
                | Code::MediaPlayPause
                | Code::MediaStop
                | Code::MediaTrackNext
                | Code::MediaTrackPrevious
                | Code::MediaFastForward
                | Code::MediaRewind
        ) {
            return Err(
                "Media keys need Input Monitoring access. Choose a key combination instead."
                    .to_owned(),
            );
        }
        let label = label(&hotkey);
        if !hotkey
            .mods
            .intersects(Modifiers::CONTROL | Modifiers::ALT | Modifiers::SUPER)
            && !function_key(hotkey.key)
        {
            return Err(format!(
                "{label} would capture ordinary typing in every app. Add Control, Option or Command."
            ));
        }
        Ok(Self { hotkey, label })
    }

    /// The macOS symbol form, e.g. `⌃⌥Space`.
    pub(crate) fn label(&self) -> &str {
        &self.label
    }
}

fn function_key(code: Code) -> bool {
    matches!(
        code,
        Code::F1
            | Code::F2
            | Code::F3
            | Code::F4
            | Code::F5
            | Code::F6
            | Code::F7
            | Code::F8
            | Code::F9
            | Code::F10
            | Code::F11
            | Code::F12
            | Code::F13
            | Code::F14
            | Code::F15
            | Code::F16
            | Code::F17
            | Code::F18
            | Code::F19
            | Code::F20
    )
}

/// Modifiers in the system's order (⌃⌥⇧⌘), then the key's printed name.
fn label(hotkey: &HotKey) -> String {
    let mut label = String::new();
    for (modifier, symbol) in [
        (Modifiers::CONTROL, '⌃'),
        (Modifiers::ALT, '⌥'),
        (Modifiers::SHIFT, '⇧'),
        (Modifiers::SUPER, '⌘'),
    ] {
        if hotkey.mods.contains(modifier) {
            label.push(symbol);
        }
    }
    let name = hotkey.key.to_string();
    label.push_str(match hotkey.key {
        Code::Enter => "↩",
        Code::Tab => "⇥",
        Code::Escape => "⎋",
        Code::Backspace => "⌫",
        Code::Delete => "⌦",
        Code::ArrowUp => "↑",
        Code::ArrowDown => "↓",
        Code::ArrowLeft => "←",
        Code::ArrowRight => "→",
        _ => name
            .strip_prefix("Key")
            .or_else(|| name.strip_prefix("Digit"))
            .unwrap_or(&name),
    });
    label
}

/// The registered global shortcut, following the configured one. Dropping it
/// unregisters the hot key.
pub(crate) struct Registration {
    manager: Result<GlobalHotKeyManager, String>,
    active: Option<Shortcut>,
    /// Why the configured shortcut is not the registered one, if it is not.
    problem: Option<String>,
    /// The configuration file's modification time when last applied.
    seen: Option<SystemTime>,
}

impl Registration {
    /// Install the process's one hot-key handler; `pressed` runs on the main
    /// thread for each press of the registered shortcut.
    pub(crate) fn new(pressed: impl Fn() + Send + Sync + 'static) -> Self {
        let manager = GlobalHotKeyManager::new()
            .map_err(|error| format!("Global shortcuts are unavailable: {error}"));
        GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
            if event.state == HotKeyState::Pressed {
                pressed();
            }
        }));
        Self {
            problem: manager.as_ref().err().cloned(),
            manager,
            active: None,
            seen: None,
        }
    }

    /// Register the configured shortcut when the configuration file changes,
    /// and retry while none is registered: an app holding the combination may
    /// release it. A refused replacement keeps the previous shortcut.
    pub(crate) fn refresh(&mut self) {
        let Ok(manager) = &self.manager else {
            return;
        };
        let stamp = paths::config_file()
            .ok()
            .and_then(|path| fs::metadata(path).ok())
            .and_then(|metadata| metadata.modified().ok());
        if self.active.is_some() && self.seen == stamp {
            return;
        }
        self.seen = stamp;
        let configured = match Config::load() {
            Ok(config) => Shortcut::configured(config.hotkey.as_deref()),
            Err(_) => {
                Err("The configuration needs repair before the shortcut can change.".to_owned())
            }
        };
        self.problem = configured
            .and_then(|shortcut| register(manager, &mut self.active, shortcut))
            .err();
    }

    /// The registered shortcut, if any.
    pub(crate) fn active(&self) -> Option<&Shortcut> {
        self.active.as_ref()
    }

    /// Why the configured shortcut is not the registered one, if it is not.
    pub(crate) fn problem(&self) -> Option<&str> {
        self.problem.as_deref()
    }
}

/// Make `shortcut` the registered one. Its predecessor is released only after
/// macOS accepts it, so a refused replacement leaves the previous one working.
fn register(
    manager: &GlobalHotKeyManager,
    active: &mut Option<Shortcut>,
    shortcut: Shortcut,
) -> Result<(), String> {
    if active.as_ref() == Some(&shortcut) {
        return Ok(());
    }
    manager.register(shortcut.hotkey).map_err(|_| {
        format!(
            "macOS did not accept {}; another app may already use it.",
            shortcut.label
        )
    })?;
    if let Some(previous) = active.replace(shortcut) {
        // Carbon refuses to unregister only a reference it never issued.
        let _ = manager.unregister(previous.hotkey);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_safe_system_wide_shortcuts_register() {
        assert!(
            Shortcut::configured(None).is_ok(),
            "the default must register"
        );
        assert!(
            Shortcut::parse("F5").is_ok(),
            "a function key may stand alone"
        );
        for text in ["Space", "Shift+A", "Enter", "MediaPlayPause", "Control+"] {
            assert!(Shortcut::parse(text).is_err(), "{text} must be refused");
        }
    }
}
