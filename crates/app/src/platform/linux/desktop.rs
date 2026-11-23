//! Linux focus, field and delivery mechanism (`cantrip_core::ports::Desktop`).
//!
//! - **Focus.** X11 sessions: EWMH `_NET_ACTIVE_WINDOW` + `_NET_WM_PID` +
//!   `WM_CLASS`. Native Wayland sessions: AT-SPI (the one application whose
//!   frame reports `STATE_ACTIVE`), because X11 cannot see native Wayland
//!   windows. Neither available → `None` (the core makes the take uncertain).
//! - **Field / tier 1.** AT-SPI `Text` + `EditableText` on every session.
//! - **Tier 2.** X11 only: one Ctrl+V `XSendEvent` addressed to the captured
//!   window id — never XTEST, never routed by keyboard focus. Native Wayland
//!   has no addressed paste, so `can_address_paste` is false there.
//! - **Session epoch.** Suspend is detected exactly as growth of
//!   `CLOCK_BOOTTIME − CLOCK_MONOTONIC` (monotonic time stops while
//!   suspended); a locked or inactive logind session bumps the epoch on every
//!   query, so delivery during or across a lock observed at a check is
//!   copy-only. A lock that begins and ends entirely between two checks
//!   without a suspend is not detected (documented limitation).

use super::atspi::{Atspi, Editable, Obj};
use super::x11::{self, X11};
use cantrip_core::ports::{
    AppIdentity, Desktop, FieldState, FieldSupport, FocusNow, InsertResult, PasteResult, Target,
    WindowRef,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{
    ConnectionExt as _, EventMask, KeyButMask, KeyPressEvent, KEY_PRESS_EVENT, KEY_RELEASE_EVENT,
};

/// Per-call AT-SPI timeout during capture (the core's deadline is 250 ms).
const CAPTURE_CALL_TIMEOUT: Duration = Duration::from_millis(120);
/// Per-call AT-SPI timeout for checks, reads and writes.
const CALL_TIMEOUT: Duration = Duration::from_millis(400);
/// Internal bound for one capture; late answers are discarded by the core.
const CAPTURE_BUDGET: Duration = Duration::from_millis(240);

const KEYSYM_V: u32 = 0x0076;
const KEYSYM_CONTROL_L: u32 = 0xffe3;

#[derive(Default)]
struct Epoch {
    base_sleep_ms: Option<i64>,
    counter: u64,
}

pub struct LinuxDesktop {
    x: Option<Arc<X11>>,
    wayland: bool,
    epoch: Mutex<Epoch>,
}

impl Default for LinuxDesktop {
    fn default() -> Self {
        Self::new()
    }
}

impl LinuxDesktop {
    pub fn new() -> LinuxDesktop {
        LinuxDesktop {
            x: X11::connect(),
            wayland: x11::session_is_wayland(),
            epoch: Mutex::new(Epoch::default()),
        }
    }

    fn atspi(&self, timeout: Duration) -> Option<Atspi> {
        Atspi::connect(self.x.as_deref(), timeout)
    }

    fn identity(pid: u32, class: Option<String>) -> Option<AppIdentity> {
        Some(AppIdentity {
            app_id: class.or_else(|| x11::proc_comm(pid))?,
            pid,
            instance: x11::instance_of(pid)?,
        })
    }

    /// X11 focus: (app, window id).
    fn x11_focus(&self) -> Option<(AppIdentity, u32)> {
        let x = self.x.as_ref()?;
        let win = x.active_window()?;
        let pid = x.window_pid(win)?;
        Some((Self::identity(pid, x.window_class(win))?, win))
    }

    fn x11_window(target: &Target) -> Option<u32> {
        let hex = target.window.0.strip_prefix("x11:")?;
        u32::from_str_radix(hex.trim_start_matches("0x"), 16).ok()
    }

    /// The X input-focus window if it is `top` or one of its descendants.
    fn focus_child_within(x: &X11, top: u32) -> Option<u32> {
        let focus = x.conn.get_input_focus().ok()?.reply().ok()?.focus;
        let mut w = focus;
        for _ in 0..16 {
            if w == top {
                return Some(focus);
            }
            if w == x.root || w == x11rb::NONE || w <= 1 {
                return None;
            }
            w = x.conn.query_tree(w).ok()?.reply().ok()?.parent;
        }
        None
    }

    fn field_obj(target: &Target) -> Option<Obj> {
        Obj::decode(&target.field.as_ref()?.0)
    }

    fn sleep_ms() -> Option<i64> {
        fn ms(clock: libc::clockid_t) -> Option<i64> {
            let mut ts = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: clock_gettime writes into a valid, owned timespec.
            (unsafe { libc::clock_gettime(clock, &mut ts) } == 0)
                .then(|| ts.tv_sec * 1000 + ts.tv_nsec / 1_000_000)
        }
        Some(ms(libc::CLOCK_BOOTTIME)? - ms(libc::CLOCK_MONOTONIC)?)
    }

    /// (locked, active) of this logind session, if logind answers.
    fn logind_state() -> Option<(bool, bool)> {
        let bus = dbus::blocking::Connection::new_system().ok()?;
        let p = bus.with_proxy(
            "org.freedesktop.login1",
            "/org/freedesktop/login1/session/auto",
            Duration::from_millis(200),
        );
        use dbus::blocking::stdintf::org_freedesktop_dbus::Properties;
        let locked: bool = p.get("org.freedesktop.login1.Session", "LockedHint").ok()?;
        let active: bool = p.get("org.freedesktop.login1.Session", "Active").ok()?;
        Some((locked, active))
    }
}

impl Desktop for LinuxDesktop {
    fn capture_target(&self) -> Option<Target> {
        let deadline = Instant::now() + CAPTURE_BUDGET;
        if !self.wayland {
            let (app, win) = self.x11_focus()?;
            let title = self.x.as_ref().and_then(|x| x.window_title(win));
            let field = self.atspi(CAPTURE_CALL_TIMEOUT).and_then(|a| {
                let root = a.app_for_pid(app.pid)?;
                let frame = a.frame_for_window(&root, title.as_deref())?;
                a.focused_text(&frame, deadline)
            });
            return Some(Target {
                app,
                window: WindowRef(format!("x11:{win:x}")),
                field: field.map(|o| cantrip_core::ports::FieldRef(o.encode())),
            });
        }
        let a = self.atspi(CAPTURE_CALL_TIMEOUT)?;
        let active = a.active_app(deadline)?;
        let app = Self::identity(active.pid, None)?;
        let field = a.focused_text(&active.frame, deadline);
        Some(Target {
            app,
            window: WindowRef(format!("atspi:{}", active.frame.encode())),
            field: field.map(|o| cantrip_core::ports::FieldRef(o.encode())),
        })
    }

    fn focus_now(&self) -> Option<FocusNow> {
        if !self.wayland {
            let (app, win) = self.x11_focus()?;
            return Some(FocusNow {
                app,
                window: WindowRef(format!("x11:{win:x}")),
            });
        }
        let a = self.atspi(CALL_TIMEOUT)?;
        let active = a.active_app(Instant::now() + Duration::from_millis(500))?;
        Some(FocusNow {
            app: Self::identity(active.pid, None)?,
            window: WindowRef(format!("atspi:{}", active.frame.encode())),
        })
    }

    fn app_alive(&self, app: &AppIdentity) -> bool {
        x11::instance_of(app.pid).as_deref() == Some(app.instance.as_str())
    }

    fn missing_delivery_permission(&self) -> Option<String> {
        // X11 and AT-SPI need no grant; Wayland addressed paste does not exist.
        None
    }

    fn session_epoch(&self) -> u64 {
        let mut e = self.epoch.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(now) = Self::sleep_ms() {
            match e.base_sleep_ms {
                Some(base) if now > base + 500 => {
                    e.counter += 1;
                    e.base_sleep_ms = Some(now);
                }
                None => e.base_sleep_ms = Some(now),
                _ => {}
            }
        }
        if let Some((locked, active)) = Self::logind_state() {
            if locked || !active {
                e.counter += 1;
            }
        }
        e.counter
    }

    fn field_support(&self, target: &Target) -> FieldSupport {
        let Some(obj) = Self::field_obj(target) else {
            return FieldSupport::NoDirectInsert;
        };
        let Some(a) = self.atspi(CALL_TIMEOUT) else {
            return FieldSupport::Gone;
        };
        match a.editable(&obj) {
            Editable::Direct => FieldSupport::DirectInsert,
            Editable::ReadOnly => FieldSupport::NoDirectInsert,
            Editable::Gone => FieldSupport::Gone,
        }
    }

    fn read_field(&self, target: &Target) -> Option<FieldState> {
        let obj = Self::field_obj(target)?;
        self.atspi(CALL_TIMEOUT)?.read(&obj)
    }

    fn insert_text(&self, target: &Target, text: &str) -> InsertResult {
        let Some(obj) = Self::field_obj(target) else {
            return InsertResult::Refused;
        };
        match self.atspi(CALL_TIMEOUT) {
            Some(a) => a.insert(&obj, text),
            None => InsertResult::Refused,
        }
    }

    fn can_address_paste(&self, _app: &AppIdentity) -> bool {
        !self.wayland && self.x.is_some()
    }

    fn post_paste(&self, target: &Target) -> PasteResult {
        if self.wayland {
            return PasteResult::Unaddressable;
        }
        let (Some(x), Some(win)) = (self.x.as_ref(), Self::x11_window(target)) else {
            return PasteResult::Unaddressable;
        };
        let Some(keycode) = x.keycode_for(KEYSYM_V) else {
            return PasteResult::Failed;
        };
        // Toolkits read keys on their focus sub-window (GTK3's InputOnly
        // child); use it only when it lies inside the captured window, so the
        // event stays addressed to the captured target.
        let win = Self::focus_child_within(x, win).unwrap_or(win);
        let ctrl = x.keycode_for(KEYSYM_CONTROL_L);
        let ev = |kind: u8, detail: u8, state: KeyButMask| KeyPressEvent {
            response_type: kind,
            detail,
            sequence: 0,
            time: x11rb::CURRENT_TIME,
            root: x.root,
            event: win,
            child: x11rb::NONE,
            root_x: 1,
            root_y: 1,
            event_x: 1,
            event_y: 1,
            state,
            same_screen: true,
        };
        // Exactly what a keyboard would produce: Control down, V down/up,
        // Control up — every event addressed to the captured window.
        let none = KeyButMask::from(0u16);
        let mut seq = Vec::new();
        if let Some(c) = ctrl {
            seq.push((KEY_PRESS_EVENT, c, none, EventMask::KEY_PRESS));
        }
        seq.push((
            KEY_PRESS_EVENT,
            keycode,
            KeyButMask::CONTROL,
            EventMask::KEY_PRESS,
        ));
        seq.push((
            KEY_RELEASE_EVENT,
            keycode,
            KeyButMask::CONTROL,
            EventMask::KEY_RELEASE,
        ));
        if let Some(c) = ctrl {
            seq.push((
                KEY_RELEASE_EVENT,
                c,
                KeyButMask::CONTROL,
                EventMask::KEY_RELEASE,
            ));
        }
        for (i, (kind, code, state, mask)) in seq.into_iter().enumerate() {
            let sent = x
                .conn
                // propagate=false: only clients listening on exactly this
                // window may receive it — never an ancestor or the root.
                .send_event(false, win, mask, ev(kind, code, state))
                .ok()
                .and_then(|c| c.check().ok());
            if sent.is_none() {
                // Nothing reached the app before the first failure.
                return if i == 0 {
                    PasteResult::Failed
                } else {
                    PasteResult::Posted
                };
            }
        }
        let _ = x.conn.flush();
        PasteResult::Posted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cantrip_core::ports::FieldRef;

    fn target(win: &str, field: Option<&str>) -> Target {
        Target {
            app: AppIdentity {
                app_id: "t".into(),
                pid: std::process::id(),
                instance: x11::instance_of(std::process::id()).unwrap(),
            },
            window: WindowRef(win.into()),
            field: field.map(|f| FieldRef(f.into())),
        }
    }

    #[test]
    fn constructors_never_panic_with_or_without_a_display() {
        use crate::platform::Hotkeys as _;
        let _ = LinuxDesktop::new().focus_now();
        let c = crate::platform::linux::clipboard::LinuxClipboard::new();
        let _ = c.backend_name();
        let mut h = crate::platform::linux::hotkeys::LinuxHotkeys::new();
        h.set_escape_grab(false);
        let _ = crate::platform::linux::window::WindowPatcher::new().hud_anchor();
    }

    #[test]
    fn window_refs_parse_only_x11_ids() {
        assert_eq!(
            LinuxDesktop::x11_window(&target("x11:1a00007", None)),
            Some(0x1a00007)
        );
        assert_eq!(
            LinuxDesktop::x11_window(&target("atspi::1.2|/a", None)),
            None
        );
    }

    #[test]
    fn app_alive_tracks_instance_not_pid() {
        let d = LinuxDesktop {
            x: None,
            wayland: false,
            epoch: Mutex::new(Epoch::default()),
        };
        let mut t = target("x11:1", None);
        assert!(d.app_alive(&t.app));
        t.app.instance = format!("{}:1", t.app.pid);
        assert!(!d.app_alive(&t.app));
    }

    #[test]
    fn degrades_safely_without_display_or_bus() {
        let d = LinuxDesktop {
            x: None,
            wayland: false,
            epoch: Mutex::new(Epoch::default()),
        };
        assert!(d.capture_target().is_none());
        assert!(d.focus_now().is_none());
        assert!(!d.can_address_paste(&target("x", None).app));
        assert_eq!(
            d.post_paste(&target("x11:1", None)),
            PasteResult::Unaddressable
        );
        assert_eq!(
            d.field_support(&target("x11:1", None)),
            FieldSupport::NoDirectInsert
        );
        assert_eq!(
            d.insert_text(&target("x11:1", None), "x"),
            InsertResult::Refused
        );
        let _ = d.session_epoch();
    }
}

/// Live checks against a real GTK3 app on `$DISPLAY` with AT-SPI (run with
/// `cargo test -p cantrip -- --ignored --test-threads=1`).
#[cfg(test)]
mod live {
    use super::*;
    use crate::platform::linux::clipboard::LinuxClipboard;
    use cantrip_core::ports::Clipboard;
    use std::process::{Child, Command, Stdio};

    const GTK_APP: &str = r#"
import gi, sys
gi.require_version("Gtk", "3.0")
from gi.repository import Gtk, GLib
def win(title, text):
    w = Gtk.Window(title=title)
    w.set_default_size(420, 80)
    e = Gtk.Entry()
    e.set_text(text)
    w.add(e)
    w.connect("destroy", Gtk.main_quit)
    w.show_all()
    return w, e
a, ea = win("CantripTargetA", "abc")
ea.grab_focus()
ea.select_region(1, 2)
b, eb = win("CantripOtherB", "zzz")
def report():
    sys.stdout.write("A=" + ea.get_text() + "\tB=" + eb.get_text() + "\n"); sys.stdout.flush()
    return True
GLib.timeout_add(100, report)
Gtk.main()
"#;

    struct App(Child);
    impl Drop for App {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn xdotool(args: &[&str]) -> String {
        let o = Command::new("xdotool").args(args).output().unwrap();
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    fn window_id(name: &str) -> String {
        for _ in 0..100 {
            let id = xdotool(&["search", "--name", name]);
            if let Some(first) = id.lines().next() {
                return first.to_string();
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("window {name} never appeared");
    }

    fn activate(id: &str) {
        xdotool(&["windowactivate", "--sync", id]);
        std::thread::sleep(Duration::from_millis(400));
    }

    #[test]
    #[ignore]
    fn live_gtk_capture_tier1_focus_and_addressed_paste() {
        let dir = std::env::temp_dir().join(format!("cantrip-gtk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("app.py");
        std::fs::write(&script, GTK_APP).unwrap();
        let mut child = Command::new("python3")
            .arg(&script)
            .env_remove("NO_AT_BRIDGE")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let out = child.stdout.take().unwrap();
        let pid = child.id();
        let _app = App(child);
        let last_line = Arc::new(Mutex::new(String::new()));
        let ll = last_line.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for l in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                *ll.lock().unwrap() = l;
            }
        });
        let a = window_id("CantripTargetA");
        let b = window_id("CantripOtherB");
        activate(&a);
        std::thread::sleep(Duration::from_millis(800)); // AT-SPI registration
                                                        // Select "b" (test-only XTEST keys into the focused entry).
        xdotool(&["key", "End", "Left", "shift+Left"]);
        std::thread::sleep(Duration::from_millis(200));

        let d = LinuxDesktop::new();
        let t0 = Instant::now();
        let target = d.capture_target().expect("capture");
        let took = t0.elapsed();
        eprintln!(
            "capture: app_id={} pid={} window={} field={} in {took:?}",
            target.app.app_id,
            target.app.pid,
            target.window.0,
            target.field.is_some()
        );
        assert_eq!(target.app.pid, pid);
        assert_eq!(
            target.window.0,
            format!("x11:{:x}", a.parse::<u32>().unwrap())
        );
        assert!(target.field.is_some(), "AT-SPI field");
        assert!(
            took < Duration::from_millis(250),
            "capture within deadline: {took:?}"
        );
        assert!(d.app_alive(&target.app));
        // The native-Wayland focus source (AT-SPI STATE_ACTIVE) agrees.
        let at = Atspi::connect(d.x.as_deref(), CALL_TIMEOUT).unwrap();
        let t1 = Instant::now();
        let active = at.active_app(Instant::now() + Duration::from_millis(500));
        eprintln!(
            "atspi active app: {:?} in {:?}",
            active.as_ref().map(|a| a.pid),
            t1.elapsed()
        );
        assert_eq!(active.map(|a| a.pid), Some(pid));

        assert_eq!(d.field_support(&target), FieldSupport::DirectInsert);
        let before = d.read_field(&target).unwrap();
        eprintln!(
            "before: chars={} selection={:?} complete={}",
            before.text.chars().count(),
            before.selection,
            before.complete
        );
        assert_eq!(before.selection, (1, 2));
        let ins = "Hello Scry 👋 é e\u{301}";
        assert_eq!(d.insert_text(&target, ins), InsertResult::Done);
        let after = d.read_field(&target).unwrap();
        let expected = format!("a{ins}c");
        assert_eq!(after.text, expected, "exact edit delta");
        let caret = 1 + ins.chars().count();
        assert_eq!(after.selection, (caret, caret));
        assert!(after.complete);
        std::thread::sleep(Duration::from_millis(250));
        assert_eq!(*last_line.lock().unwrap(), format!("A={expected}\tB=zzz"));

        // Focus checks.
        let now = d.focus_now().unwrap();
        assert_eq!(
            (now.app.clone(), now.window.clone()),
            (target.app.clone(), target.window.clone())
        );
        activate(&b);
        let moved = d.focus_now().unwrap();
        eprintln!(
            "after switching: window={} (same app instance: {})",
            moved.window.0,
            moved.app == target.app
        );
        assert_eq!(moved.app, target.app, "same app instance");
        assert_ne!(
            moved.window, target.window,
            "second window of the same app differs"
        );

        // Tier 2: addressed XSendEvent paste to window A while B has focus.
        let clip = LinuxClipboard::new();
        let tok = clip.write_text(" PASTED", true).unwrap();
        activate(&a);
        let before = d.read_field(&target).unwrap();
        assert!(d.can_address_paste(&target.app));
        assert_eq!(d.post_paste(&target), PasteResult::Posted);
        std::thread::sleep(Duration::from_millis(600));
        let after = d.read_field(&target).unwrap();
        eprintln!(
            "addressed paste: field changed={} contains_paste={} clipboard_unchanged={}",
            after.text != before.text,
            after.text.contains("PASTED"),
            clip.unchanged_since(&tok)
        );
        // Paste addressed to A while B holds focus: B must receive nothing.
        activate(&b);
        let lb = d.read_field(&target).unwrap();
        assert_eq!(d.post_paste(&target), PasteResult::Posted);
        std::thread::sleep(Duration::from_millis(600));
        let la = d.read_field(&target).unwrap();
        let line = last_line.lock().unwrap().clone();
        eprintln!(
            "paste to unfocused A: A changed={} B={:?}",
            la.text != lb.text,
            line.split('\t').nth(1)
        );
        assert!(line.ends_with("\tB=zzz"), "B received nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
