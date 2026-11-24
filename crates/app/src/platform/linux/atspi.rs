//! AT-SPI2 client over the accessibility D-Bus bus (blocking `dbus` crate).
//!
//! Gives Cantrip the focused *field* on every Linux session (X11 and native
//! Wayland), tier-1 direct insertion via `EditableText`, and read-back via
//! `Text` for the edit-delta confirmation. On native Wayland it is also the
//! focus source: the app whose frame reports `STATE_ACTIVE`.
//!
//! Every call carries a short timeout and every tree walk is bounded in nodes
//! and time, so a hung application can only make the answer "unknown".

use super::x11::X11;
use cantrip_core::ports::{FieldState, InsertResult};
use dbus::blocking::stdintf::org_freedesktop_dbus::Properties;
use dbus::blocking::Connection;
use dbus::channel::Channel;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

const ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
const TEXT: &str = "org.a11y.atspi.Text";
const EDITABLE: &str = "org.a11y.atspi.EditableText";
const REGISTRY: &str = "org.a11y.atspi.Registry";
const ROOT_PATH: &str = "/org/a11y/atspi/accessible/root";

// AT-SPI StateType bit numbers.
const STATE_ACTIVE: u32 = 1;
const STATE_DEFUNCT: u32 = 6;
const STATE_EDITABLE: u32 = 7;
const STATE_FOCUSED: u32 = 12;
const STATE_SHOWING: u32 = 25;

/// Most nodes visited while looking for the focused object.
const MAX_NODES: usize = 3000;

/// One accessible object: (bus name, object path).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Obj {
    pub bus: String,
    pub path: String,
}

impl Obj {
    /// Serialized as a FieldRef / WindowRef payload.
    pub fn encode(&self) -> String {
        format!("{}|{}", self.bus, self.path)
    }

    pub fn decode(s: &str) -> Option<Obj> {
        let (bus, path) = s.split_once('|')?;
        (!bus.is_empty() && path.starts_with('/')).then(|| Obj {
            bus: bus.into(),
            path: path.into(),
        })
    }
}

/// An application's active frame (top-level window) as AT-SPI reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveApp {
    pub pid: u32,
    pub frame: Obj,
}

/// Field-level outcome of a lookup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Editable {
    Direct,
    ReadOnly,
    Gone,
}

pub struct Atspi {
    conn: Connection,
    timeout: Duration,
}

/// The accessibility bus address, in libatspi's own order so Cantrip and the
/// applications agree: `$AT_SPI_BUS_ADDRESS`, the X root `AT_SPI_BUS`
/// property, then `org.a11y.Bus.GetAddress` on the session bus.
fn bus_address(x: Option<&X11>) -> Option<String> {
    if let Ok(a) = std::env::var("AT_SPI_BUS_ADDRESS") {
        if !a.is_empty() {
            return Some(a);
        }
    }
    if let Some(x) = x {
        if !super::x11::session_is_wayland() {
            if let Some(b) = x.prop_bytes(x.root, x.atoms.AT_SPI_BUS) {
                let s = String::from_utf8_lossy(&b)
                    .trim_end_matches('\0')
                    .to_string();
                if !s.is_empty() {
                    return Some(s);
                }
            }
        }
    }
    let session = Connection::new_session().ok()?;
    let p = session.with_proxy("org.a11y.Bus", "/org/a11y/bus", Duration::from_millis(500));
    let (addr,): (String,) = p.method_call("org.a11y.Bus", "GetAddress", ()).ok()?;
    (!addr.is_empty()).then_some(addr)
}

fn has(states: &[u32], bit: u32) -> bool {
    let (word, b) = ((bit / 32) as usize, bit % 32);
    states.get(word).is_some_and(|w| w & (1 << b) != 0)
}

impl Atspi {
    /// Connect to the accessibility bus; `None` when there is none.
    pub fn connect(x: Option<&X11>, timeout: Duration) -> Option<Atspi> {
        let addr = bus_address(x)?;
        let mut ch = Channel::open_private(&addr).ok()?;
        ch.register().ok()?;
        Some(Atspi {
            conn: Connection::from(ch),
            timeout,
        })
    }

    fn states(&self, o: &Obj) -> Option<Vec<u32>> {
        let p = self
            .conn
            .with_proxy(o.bus.as_str(), o.path.as_str(), self.timeout);
        let (s,): (Vec<u32>,) = p.method_call(ACCESSIBLE, "GetState", ()).ok()?;
        Some(s)
    }

    fn children(&self, o: &Obj) -> Vec<Obj> {
        let p = self
            .conn
            .with_proxy(o.bus.as_str(), o.path.as_str(), self.timeout);
        let r: Result<(Vec<(String, dbus::Path<'static>)>,), _> =
            p.method_call(ACCESSIBLE, "GetChildren", ());
        match r {
            Ok((v,)) => v
                .into_iter()
                .filter(|(b, p)| !b.is_empty() && &**p != "/org/a11y/atspi/null")
                .map(|(bus, path)| Obj {
                    bus,
                    path: path.to_string(),
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    fn interfaces(&self, o: &Obj) -> Vec<String> {
        let p = self
            .conn
            .with_proxy(o.bus.as_str(), o.path.as_str(), self.timeout);
        let r: Result<(Vec<String>,), _> = p.method_call(ACCESSIBLE, "GetInterfaces", ());
        r.map(|(v,)| v).unwrap_or_default()
    }

    fn pid_of(&self, bus: &str) -> Option<u32> {
        let p = self.conn.with_proxy(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            self.timeout,
        );
        let (pid,): (u32,) = p
            .method_call("org.freedesktop.DBus", "GetConnectionUnixProcessID", (bus,))
            .ok()?;
        Some(pid)
    }

    /// Application roots registered on the bus.
    fn apps(&self) -> Vec<Obj> {
        self.children(&Obj {
            bus: REGISTRY.into(),
            path: ROOT_PATH.into(),
        })
    }

    /// The application root whose connection belongs to `pid`.
    pub fn app_for_pid(&self, pid: u32) -> Option<Obj> {
        self.apps()
            .into_iter()
            .find(|a| self.pid_of(&a.bus) == Some(pid))
    }

    fn name(&self, o: &Obj) -> Option<String> {
        use dbus::blocking::stdintf::org_freedesktop_dbus::Properties;
        let p = self
            .conn
            .with_proxy(o.bus.as_str(), o.path.as_str(), self.timeout);
        p.get::<String>(ACCESSIBLE, "Name").ok()
    }

    /// The frame of `app` for the X11 window titled `title`: the active one
    /// when the toolkit reports it, else the *only* frame with that title.
    /// While Cantrip's shortcut grab is held the toolkit clears STATE_ACTIVE
    /// (keyboard focus moved to the grab), but keeps its focused widget, so the
    /// title is what still ties the X11 window to its accessible frame.
    /// Several frames with that title → `None` (never guess a field).
    pub fn frame_for_window(&self, app: &Obj, title: Option<&str>) -> Option<Obj> {
        if let Some(f) = self.active_frame(app) {
            return Some(f);
        }
        let title = title?;
        let mut matches = self
            .children(app)
            .into_iter()
            .filter(|f| self.name(f).as_deref() == Some(title));
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
    }

    /// The frame of `app` reporting STATE_ACTIVE.
    pub fn active_frame(&self, app: &Obj) -> Option<Obj> {
        self.children(app)
            .into_iter()
            .find(|f| self.states(f).is_some_and(|s| has(&s, STATE_ACTIVE)))
    }

    /// Native-Wayland focus source: the one app with an active frame.
    pub fn active_app(&self, deadline: Instant) -> Option<ActiveApp> {
        let mut found = None;
        for app in self.apps() {
            if Instant::now() > deadline {
                return None;
            }
            if let Some(frame) = self.active_frame(&app) {
                if found.is_some() {
                    // Two apps claim to be active: not identifiable.
                    return None;
                }
                found = Some(ActiveApp {
                    pid: self.pid_of(&app.bus)?,
                    frame,
                });
            }
        }
        found
    }

    /// Breadth-first search under `frame` for the STATE_FOCUSED object that
    /// implements `Text`, pruning subtrees that are not showing.
    pub fn focused_text(&self, frame: &Obj, deadline: Instant) -> Option<Obj> {
        let mut queue = VecDeque::from([frame.clone()]);
        let mut seen = 0usize;
        while let Some(o) = queue.pop_front() {
            seen += 1;
            if seen > MAX_NODES || Instant::now() > deadline {
                return None;
            }
            let Some(st) = self.states(&o) else { continue };
            if o != *frame && !has(&st, STATE_SHOWING) {
                continue;
            }
            if has(&st, STATE_FOCUSED) {
                let ifs = self.interfaces(&o);
                return ifs.iter().any(|i| i == TEXT).then_some(o);
            }
            queue.extend(self.children(&o));
        }
        None
    }

    /// Whether the field still answers and takes direct insertion.
    pub fn editable(&self, o: &Obj) -> Editable {
        let Some(st) = self.states(o) else {
            return Editable::Gone;
        };
        if has(&st, STATE_DEFUNCT) {
            return Editable::Gone;
        }
        let ifs = self.interfaces(o);
        if ifs.is_empty() {
            return Editable::Gone;
        }
        if has(&st, STATE_EDITABLE) && ifs.iter().any(|i| i == EDITABLE) {
            Editable::Direct
        } else {
            Editable::ReadOnly
        }
    }

    fn selection(&self, o: &Obj) -> Option<(usize, usize)> {
        let p = self
            .conn
            .with_proxy(o.bus.as_str(), o.path.as_str(), self.timeout);
        let (n,): (i32,) = p.method_call(TEXT, "GetNSelections", ()).ok()?;
        if n > 0 {
            let (a, b): (i32, i32) = p.method_call(TEXT, "GetSelection", (0i32,)).ok()?;
            if a >= 0 && b >= 0 && a != b {
                return Some((a.min(b) as usize, a.max(b) as usize));
            }
        }
        let caret: i32 = p.get(TEXT, "CaretOffset").ok()?;
        (caret >= 0).then_some((caret as usize, caret as usize))
    }

    /// Value + selection in chars; `complete` only when the returned text has
    /// exactly `CharacterCount` chars.
    pub fn read(&self, o: &Obj) -> Option<FieldState> {
        let p = self
            .conn
            .with_proxy(o.bus.as_str(), o.path.as_str(), self.timeout);
        let count: i32 = p.get(TEXT, "CharacterCount").ok()?;
        let (text,): (String,) = p.method_call(TEXT, "GetText", (0i32, -1i32)).ok()?;
        let selection = self.selection(o)?;
        let complete = count >= 0 && text.chars().count() == count as usize;
        Some(FieldState {
            text,
            selection,
            complete,
        })
    }

    /// Replace the selection with `text` (DeleteText, then InsertText at the
    /// selection start, then move the caret after it).
    pub fn insert(&self, o: &Obj, text: &str) -> InsertResult {
        let Some((start, end)) = self.selection(o) else {
            return InsertResult::Refused;
        };
        let p = self
            .conn
            .with_proxy(o.bus.as_str(), o.path.as_str(), self.timeout);
        let mut changed = false;
        if end > start {
            let r: Result<(bool,), _> =
                p.method_call(EDITABLE, "DeleteText", (start as i32, end as i32));
            match r {
                Ok((true,)) => changed = true,
                Ok((false,)) => return InsertResult::Refused,
                Err(e) if is_no_reply(&e) => return InsertResult::Ambiguous,
                Err(_) => return InsertResult::Refused,
            }
        }
        // ATK's `length` is in bytes.
        let r: Result<(bool,), _> = p.method_call(
            EDITABLE,
            "InsertText",
            (start as i32, text, text.len() as i32),
        );
        let result = match r {
            Ok((true,)) => InsertResult::Done,
            Ok((false,)) if !changed => InsertResult::Refused,
            Err(e) if !changed && !is_no_reply(&e) => InsertResult::Refused,
            _ => InsertResult::Ambiguous,
        };
        if result == InsertResult::Done {
            let caret = (start + text.chars().count()) as i32;
            let _: Result<(bool,), _> = p.method_call(TEXT, "SetCaretOffset", (caret,));
        }
        result
    }
}

/// A timeout / no-reply error may hide a completed call.
fn is_no_reply(e: &dbus::Error) -> bool {
    matches!(
        e.name(),
        Some("org.freedesktop.DBus.Error.NoReply")
            | Some("org.freedesktop.DBus.Error.Timeout")
            | None
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn obj_round_trips_and_rejects_garbage() {
        let o = Obj {
            bus: ":1.42".into(),
            path: "/org/a11y/atspi/accessible/7".into(),
        };
        assert_eq!(Obj::decode(&o.encode()), Some(o));
        assert_eq!(Obj::decode("nope"), None);
        assert_eq!(Obj::decode("|/x"), None);
        assert_eq!(Obj::decode(":1.2|relative"), None);
    }

    #[test]
    fn state_bits_span_two_words() {
        let s = [1 << STATE_FOCUSED | 1 << STATE_EDITABLE, 1 << (33 - 32)];
        assert!(has(&s, STATE_FOCUSED));
        assert!(has(&s, STATE_EDITABLE));
        assert!(!has(&s, STATE_ACTIVE));
        assert!(has(&s, 33));
        assert!(!has(&[], 1));
    }
}
