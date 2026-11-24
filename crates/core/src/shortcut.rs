//! The one global shortcut: chord model, per-OS display, and the press-length
//! state machine that tells hold-to-talk from tap-to-toggle without a setting.

use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

/// Holds at least this long are hold-to-talk; shorter presses toggle.
pub const HOLD_THRESHOLD: Duration = Duration::from_millis(300);

/// One user-chosen chord: at least one modifier plus one non-modifier key.
/// Modifier-only and Fn chords are not representable in v2.0.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Chord {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// ⌘ on macOS, Super on Linux.
    pub meta: bool,
    /// Key name: "Space", "A".."Z", "0".."9", "F1".."F12", "Enter", "Tab", …
    pub key: String,
}

impl Chord {
    /// `⌃⌥Space` on macOS, `Ctrl+Alt+Space` on Linux — the same chord.
    pub fn platform_default() -> Chord {
        Chord {
            ctrl: true,
            alt: true,
            shift: false,
            meta: false,
            key: "Space".into(),
        }
    }

    /// A chord is valid with at least one modifier and a non-modifier key.
    pub fn is_valid(&self) -> bool {
        (self.ctrl || self.alt || self.shift || self.meta)
            && !self.key.is_empty()
            && !matches!(
                self.key.as_str(),
                "Ctrl" | "Alt" | "Shift" | "Meta" | "Super" | "Fn" | "Escape"
            )
    }

    /// macOS rendering: `⌃⌥⇧⌘Space` (Apple's modifier order).
    pub fn display_macos(&self) -> String {
        let mut s = String::new();
        if self.ctrl {
            s.push('⌃');
        }
        if self.alt {
            s.push('⌥');
        }
        if self.shift {
            s.push('⇧');
        }
        if self.meta {
            s.push('⌘');
        }
        s.push_str(&self.key);
        s
    }

    /// Linux rendering: `Ctrl+Alt+Shift+Super+Space`.
    pub fn display_linux(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if self.ctrl {
            parts.push("Ctrl");
        }
        if self.alt {
            parts.push("Alt");
        }
        if self.shift {
            parts.push("Shift");
        }
        if self.meta {
            parts.push("Super");
        }
        parts.push(&self.key);
        parts.join("+")
    }

    /// The rendering for the OS this binary was built for.
    pub fn display(&self) -> String {
        if cfg!(target_os = "macos") {
            self.display_macos()
        } else {
            self.display_linux()
        }
    }
}

/// Raw shortcut input as a backend reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyEvent {
    /// The chord went down (macOS, X11, Wayland portals that report release).
    Pressed(Instant),
    /// The chord came up.
    Released(Instant),
    /// Activation-only session (Wayland portal without release): every
    /// activation is a tap.
    Activated(Instant),
    /// Escape while a take is active.
    Escape,
}

/// What the dictation controller should do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShortcutAction {
    Start,
    Stop,
    Cancel,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Idle,
    /// Down, take started at `since`; undecided between hold and tap.
    Down {
        since: Instant,
    },
    /// Tap-started take is running, key is up.
    Toggled,
    /// Second press of a toggled take is down; the take stops on release.
    StoppingDown,
}

/// Hold-vs-tap state machine. Press starts a take immediately (HUD within
/// 100 ms); release ≥ 300 ms later stops it (hold-to-talk); an earlier release
/// leaves it running until the next press (tap-to-toggle).
#[derive(Debug)]
pub struct PressTracker {
    state: State,
}

impl Default for PressTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl PressTracker {
    pub fn new() -> Self {
        PressTracker { state: State::Idle }
    }

    /// True while a take started by the shortcut is active.
    pub fn active(&self) -> bool {
        self.state != State::Idle
    }

    /// The take was started or stopped elsewhere (popover button, failure):
    /// keep the machine in step.
    pub fn sync_active(&mut self, active: bool) {
        self.state = match (active, self.state) {
            (false, _) => State::Idle,
            (true, State::Idle) => State::Toggled,
            (true, s) => s,
        };
    }

    pub fn on_event(&mut self, ev: KeyEvent) -> ShortcutAction {
        use ShortcutAction as A;
        match (self.state, ev) {
            (_, KeyEvent::Escape) => {
                if self.state == State::Idle {
                    A::None
                } else {
                    self.state = State::Idle;
                    A::Cancel
                }
            }
            (State::Idle, KeyEvent::Pressed(t)) => {
                self.state = State::Down { since: t };
                A::Start
            }
            (State::Down { since }, KeyEvent::Released(t)) => {
                if t.saturating_duration_since(since) >= HOLD_THRESHOLD {
                    self.state = State::Idle;
                    A::Stop
                } else {
                    self.state = State::Toggled;
                    A::None
                }
            }
            (State::Toggled, KeyEvent::Pressed(_)) => {
                self.state = State::StoppingDown;
                A::Stop
            }
            (State::StoppingDown, KeyEvent::Released(_)) => {
                self.state = State::Idle;
                A::None
            }
            (State::Idle, KeyEvent::Activated(_)) => {
                self.state = State::Toggled;
                A::Start
            }
            (State::Toggled | State::Down { .. }, KeyEvent::Activated(_)) => {
                self.state = State::Idle;
                A::Stop
            }
            // Auto-repeat presses while down, stray releases: ignore.
            _ => A::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn default_chord_reads_per_os() {
        let c = Chord::platform_default();
        assert_eq!(c.display_macos(), "⌃⌥Space");
        assert_eq!(c.display_linux(), "Ctrl+Alt+Space");
        assert!(c.is_valid());
    }

    #[test]
    fn modifier_only_and_bare_keys_are_invalid() {
        let mut c = Chord::platform_default();
        c.key = "Alt".into();
        assert!(!c.is_valid());
        let bare = Chord {
            ctrl: false,
            alt: false,
            shift: false,
            meta: false,
            key: "A".into(),
        };
        assert!(!bare.is_valid());
    }

    #[test]
    fn hold_of_300ms_or_more_is_hold_to_talk() {
        let t = t0();
        let mut m = PressTracker::new();
        assert_eq!(m.on_event(KeyEvent::Pressed(t)), ShortcutAction::Start);
        assert_eq!(
            m.on_event(KeyEvent::Released(t + Duration::from_millis(300))),
            ShortcutAction::Stop
        );
        assert!(!m.active());
    }

    #[test]
    fn short_press_toggles_and_next_press_stops() {
        let t = t0();
        let mut m = PressTracker::new();
        assert_eq!(m.on_event(KeyEvent::Pressed(t)), ShortcutAction::Start);
        assert_eq!(
            m.on_event(KeyEvent::Released(t + Duration::from_millis(299))),
            ShortcutAction::None
        );
        assert!(m.active());
        let t2 = t + Duration::from_secs(5);
        assert_eq!(m.on_event(KeyEvent::Pressed(t2)), ShortcutAction::Stop);
        // Releasing the stopping press does not start a new take.
        assert_eq!(
            m.on_event(KeyEvent::Released(t2 + Duration::from_millis(900))),
            ShortcutAction::None
        );
        assert!(!m.active());
    }

    #[test]
    fn autorepeat_presses_while_held_are_ignored() {
        let t = t0();
        let mut m = PressTracker::new();
        m.on_event(KeyEvent::Pressed(t));
        assert_eq!(
            m.on_event(KeyEvent::Pressed(t + Duration::from_millis(50))),
            ShortcutAction::None
        );
        assert_eq!(
            m.on_event(KeyEvent::Released(t + Duration::from_millis(800))),
            ShortcutAction::Stop
        );
    }

    #[test]
    fn escape_cancels_in_every_active_state_and_is_inert_when_idle() {
        let t = t0();
        let mut m = PressTracker::new();
        assert_eq!(m.on_event(KeyEvent::Escape), ShortcutAction::None);
        m.on_event(KeyEvent::Pressed(t));
        assert_eq!(m.on_event(KeyEvent::Escape), ShortcutAction::Cancel);
        assert!(!m.active());
        // A release after Escape must not stop or start anything.
        assert_eq!(
            m.on_event(KeyEvent::Released(t + Duration::from_secs(1))),
            ShortcutAction::None
        );
        m.on_event(KeyEvent::Pressed(t));
        m.on_event(KeyEvent::Released(t + Duration::from_millis(10)));
        assert_eq!(m.on_event(KeyEvent::Escape), ShortcutAction::Cancel);
    }

    #[test]
    fn activation_only_sessions_treat_every_press_as_a_tap() {
        let t = t0();
        let mut m = PressTracker::new();
        assert_eq!(m.on_event(KeyEvent::Activated(t)), ShortcutAction::Start);
        assert_eq!(
            m.on_event(KeyEvent::Activated(t + Duration::from_secs(3))),
            ShortcutAction::Stop
        );
        assert!(!m.active());
    }

    #[test]
    fn popover_started_take_is_stopped_by_the_next_press() {
        let t = t0();
        let mut m = PressTracker::new();
        m.sync_active(true);
        assert_eq!(m.on_event(KeyEvent::Pressed(t)), ShortcutAction::Stop);
    }
}
