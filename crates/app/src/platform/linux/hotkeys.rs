//! The one global shortcut on Linux (`crate::platform::Hotkeys`).
//!
//! - **X11** (`$DISPLAY`, not a native Wayland session): passive `XGrabKey`
//!   on the root window for the chord (with Caps/Num-Lock variants) and XKB
//!   *detectable auto-repeat*, so a hold yields exactly one press and one
//!   release; repeat pairs from servers without it are filtered too. Plain
//!   Escape is grabbed only while a take is active, under the exact lock
//!   masks (none, CapsLock, NumLock, both) and never `AnyModifier`, so it
//!   cannot collide with window-manager chords such as Openbox's stock
//!   `Alt+Escape`. Press and release are reported: hold-to-talk and
//!   tap-to-toggle.
//! - **Native Wayland**: xdg-desktop-portal `GlobalShortcuts` (the compositor
//!   may ask the user to approve the chord). `Activated`/`Deactivated` map to
//!   press/release on desktops whose portals report release (KDE Plasma,
//!   GNOME, Hyprland); elsewhere every activation is a tap (ActivationOnly).
//!   Escape cannot be grabbed through the portal, so on Wayland a take is
//!   cancelled from the popover's Stop control rather than Escape — a
//!   documented session limit. No portal → `Err`.

use super::x11::{self, X11};
use crate::platform::{HotkeyMode, Hotkeys};
use cantrip_core::shortcut::{Chord, KeyEvent};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use x11rb::connection::Connection as _;
use x11rb::protocol::xkb::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{ConnectionExt as _, GrabMode, ModMask};
use x11rb::protocol::Event;

const KEYSYM_ESCAPE: u32 = 0xff1b;

/// Modifier bits compared for Escape: Shift, Lock, Control, Mod1–Mod5 (not
/// pointer-button bits).
const ESCAPE_STATE_MASK: u16 = 0x00ff;

/// X11 keysym for a `Chord::key` name.
pub fn keysym_for(key: &str) -> Option<u32> {
    let k = match key {
        "Space" => 0x20,
        "Enter" | "Return" => 0xff0d,
        "Tab" => 0xff09,
        "Backspace" => 0xff08,
        "Insert" => 0xff63,
        "Delete" => 0xffff,
        "Home" => 0xff50,
        "End" => 0xff57,
        "PageUp" => 0xff55,
        "PageDown" => 0xff56,
        "Left" => 0xff51,
        "Up" => 0xff52,
        "Right" => 0xff53,
        "Down" => 0xff54,
        _ => {
            if let Some(n) = key.strip_prefix('F').and_then(|n| n.parse::<u32>().ok()) {
                return (1..=24).contains(&n).then_some(0xffbe + n - 1);
            }
            let mut cs = key.chars();
            let (Some(c), None) = (cs.next(), cs.next()) else {
                return None;
            };
            if c.is_ascii_alphabetic() {
                c.to_ascii_lowercase() as u32
            } else if c.is_ascii_graphic() {
                c as u32
            } else {
                return None;
            }
        }
    };
    Some(k)
}

/// Core modifier mask for a chord.
pub fn mods_for(chord: &Chord) -> u16 {
    let mut m = 0u16;
    if chord.shift {
        m |= u16::from(ModMask::SHIFT);
    }
    if chord.ctrl {
        m |= u16::from(ModMask::CONTROL);
    }
    if chord.alt {
        m |= u16::from(ModMask::M1);
    }
    if chord.meta {
        m |= u16::from(ModMask::M4);
    }
    m
}

/// xdg-desktop-portal trigger string, e.g. `CTRL+ALT+space`.
pub fn portal_trigger(chord: &Chord) -> String {
    let mut parts: Vec<String> = Vec::new();
    if chord.ctrl {
        parts.push("CTRL".into());
    }
    if chord.alt {
        parts.push("ALT".into());
    }
    if chord.shift {
        parts.push("SHIFT".into());
    }
    if chord.meta {
        parts.push("LOGO".into());
    }
    let key = match chord.key.as_str() {
        "Space" => "space".to_string(),
        "Enter" => "Return".to_string(),
        k if k.chars().count() == 1 => k.to_ascii_lowercase(),
        k => k.to_string(),
    };
    parts.push(key);
    parts.join("+")
}

/// Lock / NumLock combinations a passive grab must cover.
fn lock_variants(mods: u16) -> [u16; 4] {
    let lock = u16::from(ModMask::LOCK);
    let num = u16::from(ModMask::M2);
    [mods, mods | lock, mods | num, mods | lock | num]
}

#[derive(Default)]
struct Shared {
    sink: Option<Sender<KeyEvent>>,
    /// (keycode, modifier mask) of the registered chord.
    chord: Option<(u8, u16)>,
    escape: Option<u8>,
    escape_active: bool,
    /// The exact masks plain Escape is currently grabbed under.
    escape_masks: Vec<u16>,
    down: bool,
}

impl Shared {
    fn send(&self, ev: KeyEvent) {
        if let Some(s) = &self.sink {
            let _ = s.send(ev);
        }
    }
}

struct X11Keys {
    x: Arc<X11>,
    shared: Arc<Mutex<Shared>>,
    thread: bool,
}

enum Inner {
    X11(X11Keys),
    Portal(portal::PortalKeys),
    None,
}

pub struct LinuxHotkeys {
    inner: Inner,
}

impl Default for LinuxHotkeys {
    fn default() -> Self {
        Self::new()
    }
}

impl LinuxHotkeys {
    pub fn new() -> LinuxHotkeys {
        let inner = if x11::session_is_wayland() {
            Inner::Portal(portal::PortalKeys::default())
        } else {
            match X11::connect() {
                Some(x) => Inner::X11(X11Keys {
                    x,
                    shared: Arc::default(),
                    thread: false,
                }),
                None => Inner::None,
            }
        };
        LinuxHotkeys { inner }
    }
}

impl Hotkeys for LinuxHotkeys {
    fn register(&mut self, chord: &Chord, sink: Sender<KeyEvent>) -> Result<HotkeyMode, String> {
        match &mut self.inner {
            Inner::X11(k) => k.register(chord, sink),
            Inner::Portal(p) => p.register(chord, sink),
            Inner::None => Err("Global shortcuts are not available on this session".into()),
        }
    }

    fn set_escape_grab(&mut self, active: bool) {
        if let Inner::X11(k) = &mut self.inner {
            k.set_escape(active);
        }
    }
}

impl X11Keys {
    fn register(&mut self, chord: &Chord, sink: Sender<KeyEvent>) -> Result<HotkeyMode, String> {
        let keysym =
            keysym_for(&chord.key).ok_or_else(|| format!("Unsupported key {}", chord.key))?;
        let keycode = self
            .x
            .keycode_for(keysym)
            .ok_or_else(|| format!("No key produces {} on this keyboard", chord.key))?;
        let mods = mods_for(chord);
        let conn = &self.x.conn;
        // Release the old chord first.
        let old = self
            .shared
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .chord
            .take();
        if let Some((code, m)) = old {
            for v in lock_variants(m) {
                let _ = conn.ungrab_key(code, self.x.root, ModMask::from(v));
            }
        }
        for v in lock_variants(mods) {
            let grab = conn
                .grab_key(
                    false,
                    self.x.root,
                    ModMask::from(v),
                    keycode,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                )
                .map_err(|e| e.to_string())?;
            if grab.check().is_err() {
                for u in lock_variants(mods) {
                    let _ = conn.ungrab_key(keycode, self.x.root, ModMask::from(u));
                }
                self.x.flush();
                return Err(format!(
                    "{} is already used by another app",
                    chord.display_linux()
                ));
            }
        }
        // One press + one release per hold.
        if let Ok(c) = conn.xkb_use_extension(1, 0) {
            let _ = c.reply();
            let flag = xkb::PerClientFlag::DETECTABLE_AUTO_REPEAT;
            let _ = conn
                .xkb_per_client_flags(
                    xkb::ID::USE_CORE_KBD.into(),
                    flag,
                    flag,
                    xkb::BoolCtrl::from(0u32),
                    xkb::BoolCtrl::from(0u32),
                    xkb::BoolCtrl::from(0u32),
                )
                .map(|c| c.reply());
        }
        self.x.flush();
        {
            let mut s = self.shared.lock().unwrap_or_else(|p| p.into_inner());
            s.sink = Some(sink);
            s.chord = Some((keycode, mods));
            s.escape = self.x.keycode_for(KEYSYM_ESCAPE);
            s.down = false;
        }
        if !self.thread {
            let (x, shared) = (self.x.clone(), self.shared.clone());
            std::thread::Builder::new()
                .name("cantrip-hotkeys".into())
                .spawn(move || x11_loop(&x, &shared))
                .map_err(|e| e.to_string())?;
            self.thread = true;
        }
        Ok(HotkeyMode::PressRelease)
    }

    fn set_escape(&mut self, active: bool) {
        let mut s = self.shared.lock().unwrap_or_else(|p| p.into_inner());
        if s.escape_active == active {
            return;
        }
        let Some(code) = s.escape else { return };
        let conn = &self.x.conn;
        if active {
            // Plain Escape only: each lock variant is its own passive grab,
            // so one taken elsewhere (BadAccess) never costs the others.
            let mut grabbed = Vec::new();
            for v in lock_variants(0) {
                let ok = conn
                    .grab_key(
                        false,
                        self.x.root,
                        ModMask::from(v),
                        code,
                        GrabMode::ASYNC,
                        GrabMode::ASYNC,
                    )
                    .ok()
                    .is_some_and(|c| c.check().is_ok());
                if ok {
                    grabbed.push(v);
                }
            }
            s.escape_active = grabbed.contains(&0);
            if !s.escape_active {
                for v in grabbed.drain(..) {
                    let _ = conn.ungrab_key(code, self.x.root, ModMask::from(v));
                }
                eprintln!(
                    "[cantrip] escape grab unavailable: plain Escape is taken by another client"
                );
            }
            s.escape_masks = grabbed;
        } else {
            for v in std::mem::take(&mut s.escape_masks) {
                let _ = conn.ungrab_key(code, self.x.root, ModMask::from(v));
            }
            s.escape_active = false;
        }
        self.x.flush();
    }
}

impl Drop for X11Keys {
    /// Release every passive grab (the event thread keeps the connection).
    fn drop(&mut self) {
        self.set_escape(false);
        let mut s = self.shared.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((code, m)) = s.chord.take() {
            for v in lock_variants(m) {
                let _ = self.x.conn.ungrab_key(code, self.x.root, ModMask::from(v));
            }
        }
        s.sink = None;
        drop(s);
        self.x.flush();
    }
}

fn x11_loop(x: &X11, shared: &Mutex<Shared>) {
    let mut pending: Option<Event> = None;
    loop {
        let ev = match pending.take() {
            Some(e) => e,
            None => match x.conn.wait_for_event() {
                Ok(e) => e,
                Err(_) => return,
            },
        };
        let now = Instant::now();
        let mut s = shared.lock().unwrap_or_else(|p| p.into_inner());
        let Some((code, _mods)) = s.chord else {
            continue;
        };
        match ev {
            Event::KeyPress(e) if e.detail == code => {
                if !s.down {
                    s.down = true;
                    s.send(KeyEvent::Pressed(now));
                }
            }
            Event::KeyRelease(e) if e.detail == code => {
                // Without detectable auto-repeat, a repeat is a release
                // immediately followed by a press with the same timestamp.
                drop(s);
                std::thread::sleep(Duration::from_millis(2));
                if let Ok(Some(next)) = x.conn.poll_for_event() {
                    if let Event::KeyPress(n) = &next {
                        if n.detail == code && n.time == e.time {
                            continue;
                        }
                    }
                    pending = Some(next);
                }
                let mut s = shared.lock().unwrap_or_else(|p| p.into_inner());
                if s.down {
                    s.down = false;
                    s.send(KeyEvent::Released(now));
                }
            }
            // Only the exact grabbed masks reach us; check anyway so a stray
            // modified Escape is never treated as cancel.
            Event::KeyPress(e)
                if Some(e.detail) == s.escape
                    && s.escape_active
                    && s.escape_masks
                        .contains(&(u16::from(e.state) & ESCAPE_STATE_MASK)) =>
            {
                s.send(KeyEvent::Escape);
            }
            _ => {}
        }
    }
}

mod portal {
    //! xdg-desktop-portal GlobalShortcuts client (blocking D-Bus, own thread).

    use super::*;
    use dbus::arg::{PropMap, Variant};
    use dbus::blocking::Connection;
    use dbus::message::MatchRule;
    use std::sync::mpsc;

    const DEST: &str = "org.freedesktop.portal.Desktop";
    const PATH: &str = "/org/freedesktop/portal/desktop";
    const IFACE: &str = "org.freedesktop.portal.GlobalShortcuts";
    const SHORTCUT_ID: &str = "dictate";

    #[derive(Default)]
    pub struct PortalKeys {
        generation: Arc<Mutex<u64>>,
    }

    /// Desktops whose portals report `Deactivated` (release).
    fn reports_release() -> bool {
        let d = std::env::var("XDG_CURRENT_DESKTOP")
            .unwrap_or_default()
            .to_ascii_uppercase();
        ["KDE", "GNOME", "HYPRLAND"].iter().any(|n| d.contains(n))
    }

    impl PortalKeys {
        pub fn register(
            &mut self,
            chord: &Chord,
            sink: Sender<KeyEvent>,
        ) -> Result<HotkeyMode, String> {
            let generation = {
                let mut g = self.generation.lock().unwrap_or_else(|p| p.into_inner());
                *g += 1;
                *g
            };
            let mode = if reports_release() {
                HotkeyMode::PressRelease
            } else {
                HotkeyMode::ActivationOnly
            };
            let trigger = portal_trigger(chord);
            let (tx, rx) = mpsc::channel::<Result<(), String>>();
            let live = self.generation.clone();
            std::thread::Builder::new()
                .name("cantrip-portal-shortcuts".into())
                .spawn(move || {
                    if let Err(e) = run(&trigger, mode, sink, &tx, generation, &live) {
                        let _ = tx.send(Err(e));
                    }
                })
                .map_err(|e| e.to_string())?;
            // The compositor may show a consent dialog; do not block forever.
            match rx.recv_timeout(Duration::from_secs(3)) {
                Ok(Ok(())) | Err(mpsc::RecvTimeoutError::Timeout) => Ok(mode),
                Ok(Err(e)) => Err(e),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    Err("Global shortcuts are not available on this session".into())
                }
            }
        }
    }

    fn request_path(conn: &Connection, token: &str) -> String {
        let sender = conn.unique_name().to_string();
        let sender = sender.trim_start_matches(':').replace('.', "_");
        format!("/org/freedesktop/portal/desktop/request/{sender}/{token}")
    }

    fn run(
        trigger: &str,
        mode: HotkeyMode,
        sink: Sender<KeyEvent>,
        ready: &mpsc::Sender<Result<(), String>>,
        generation: u64,
        live: &Mutex<u64>,
    ) -> Result<(), String> {
        let unavailable = || "Global shortcuts are not available on this session".to_string();
        let conn = Connection::new_session().map_err(|_| unavailable())?;
        let proxy = conn.with_proxy(DEST, PATH, Duration::from_secs(5));
        use dbus::blocking::stdintf::org_freedesktop_dbus::Properties;
        let _version: u32 = proxy.get(IFACE, "version").map_err(|_| unavailable())?;

        let tok = format!("cantrip_{}_{generation}", std::process::id());
        let mut opts = PropMap::new();
        opts.insert("handle_token".into(), Variant(Box::new(format!("{tok}_c"))));
        opts.insert(
            "session_handle_token".into(),
            Variant(Box::new(tok.clone())),
        );
        let path = request_path(&conn, &format!("{tok}_c"));
        let pending = await_response_after(&conn, &path, || {
            proxy
                .method_call::<(dbus::Path<'static>,), _, _, _>(IFACE, "CreateSession", (opts,))
                .map(|_| ())
                .map_err(|_| unavailable())
        })?;
        let session: String = pending
            .get("session_handle")
            .and_then(|v| v.0.as_str().map(str::to_string))
            .ok_or_else(unavailable)?;
        let session_path = dbus::Path::new(session.clone()).map_err(|_| unavailable())?;

        let mut props = PropMap::new();
        props.insert(
            "description".into(),
            Variant(Box::new("Dictate with Cantrip".to_string())),
        );
        props.insert(
            "preferred_trigger".into(),
            Variant(Box::new(trigger.to_string())),
        );
        let shortcuts = vec![(SHORTCUT_ID.to_string(), props)];
        let mut bopts = PropMap::new();
        bopts.insert("handle_token".into(), Variant(Box::new(format!("{tok}_b"))));
        let bpath = request_path(&conn, &format!("{tok}_b"));
        await_response_after(&conn, &bpath, || {
            proxy
                .method_call::<(dbus::Path<'static>,), _, _, _>(
                    IFACE,
                    "BindShortcuts",
                    (session_path.clone(), shortcuts, "", bopts),
                )
                .map(|_| ())
                .map_err(|_| unavailable())
        })?;
        let _ = ready.send(Ok(()));

        let down = Arc::new(Mutex::new(false));
        let (s1, d1) = (sink.clone(), down.clone());
        let sess1 = session.clone();
        conn.add_match(
            MatchRule::new_signal(IFACE, "Activated"),
            move |(sess, id, _ts, _o): (dbus::Path<'static>, String, u64, PropMap), _, _| {
                if *sess == *sess1 && id == SHORTCUT_ID {
                    let now = Instant::now();
                    match mode {
                        HotkeyMode::ActivationOnly => {
                            let _ = s1.send(KeyEvent::Activated(now));
                        }
                        HotkeyMode::PressRelease => {
                            let mut d = d1.lock().unwrap_or_else(|p| p.into_inner());
                            if !*d {
                                *d = true;
                                let _ = s1.send(KeyEvent::Pressed(now));
                            }
                        }
                    }
                }
                true
            },
        )
        .map_err(|e| e.to_string())?;
        let sess2 = session.clone();
        conn.add_match(
            MatchRule::new_signal(IFACE, "Deactivated"),
            move |(sess, id, _ts, _o): (dbus::Path<'static>, String, u64, PropMap), _, _| {
                if *sess == *sess2 && id == SHORTCUT_ID && mode == HotkeyMode::PressRelease {
                    let mut d = down.lock().unwrap_or_else(|p| p.into_inner());
                    if *d {
                        *d = false;
                        let _ = sink.send(KeyEvent::Released(Instant::now()));
                    }
                }
                true
            },
        )
        .map_err(|e| e.to_string())?;
        // Serve until a newer registration replaces this one.
        while *live.lock().unwrap_or_else(|p| p.into_inner()) == generation {
            if conn.process(Duration::from_millis(500)).is_err() {
                break;
            }
        }
        let sp = conn.with_proxy(DEST, session_path, Duration::from_secs(2));
        let _: Result<(), _> = sp.method_call("org.freedesktop.portal.Session", "Close", ());
        Ok(())
    }

    /// Subscribe to the Request's Response first, then make the call, then
    /// wait (the user may be shown a consent dialog).
    fn await_response_after(
        conn: &Connection,
        path: &str,
        call: impl FnOnce() -> Result<(), String>,
    ) -> Result<PropMap, String> {
        let slot: Arc<Mutex<Option<(u32, PropMap)>>> = Arc::default();
        let s2 = slot.clone();
        let rule = MatchRule::new_signal("org.freedesktop.portal.Request", "Response")
            .with_path(path.to_string());
        let token = conn
            .add_match(rule, move |(code, results): (u32, PropMap), _, _| {
                *s2.lock().unwrap_or_else(|p| p.into_inner()) = Some((code, results));
                false
            })
            .map_err(|e| e.to_string())?;
        let outcome = call().and_then(|()| {
            let deadline = Instant::now() + Duration::from_secs(120);
            loop {
                if let Some((code, results)) = slot.lock().unwrap_or_else(|p| p.into_inner()).take()
                {
                    return match code {
                        0 => Ok(results),
                        1 => Err("The shortcut was not approved".into()),
                        _ => Err("Global shortcuts are not available on this session".into()),
                    };
                }
                if Instant::now() > deadline {
                    return Err("Timed out waiting for shortcut approval".into());
                }
                conn.process(Duration::from_millis(200))
                    .map_err(|e| e.to_string())?;
            }
        });
        let _ = conn.remove_match(token);
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keysyms_cover_supported_key_names() {
        assert_eq!(keysym_for("Space"), Some(0x20));
        assert_eq!(keysym_for("A"), Some(0x61));
        assert_eq!(keysym_for("7"), Some(0x37));
        assert_eq!(keysym_for("F1"), Some(0xffbe));
        assert_eq!(keysym_for("F12"), Some(0xffc9));
        assert_eq!(keysym_for("Enter"), Some(0xff0d));
        assert_eq!(keysym_for("Fn"), None);
        assert_eq!(keysym_for(""), None);
    }

    #[test]
    fn default_chord_maps_to_ctrl_alt() {
        let c = Chord::platform_default();
        assert_eq!(
            mods_for(&c),
            u16::from(ModMask::CONTROL) | u16::from(ModMask::M1)
        );
        assert_eq!(portal_trigger(&c), "CTRL+ALT+space");
        assert_eq!(lock_variants(4).len(), 4);
    }

    /// Live: grab Ctrl+Alt+Space on DISPLAY=:0 and drive it with XTEST
    /// (xdotool, test only). Holding 600 ms with auto-repeat must give one
    /// press and one release.
    #[test]
    #[ignore]
    fn live_x11_grab_reports_one_press_and_release() {
        let mut keys = LinuxHotkeys::new();
        let (tx, rx) = std::sync::mpsc::channel();
        assert_eq!(
            keys.register(&Chord::platform_default(), tx).unwrap(),
            HotkeyMode::PressRelease
        );
        let run = |args: &[&str]| {
            assert!(std::process::Command::new("xdotool")
                .args(args)
                .status()
                .unwrap()
                .success())
        };
        run(&["keydown", "ctrl+alt+space"]);
        std::thread::sleep(Duration::from_millis(900)); // > X autorepeat delay
        run(&["keyup", "space"]);
        run(&["keyup", "ctrl+alt"]);
        std::thread::sleep(Duration::from_millis(200));
        let evs: Vec<_> = rx.try_iter().collect();
        eprintln!("observed: {evs:?}");
        assert_eq!(evs.len(), 2, "{evs:?}");
        assert!(matches!(evs[0], KeyEvent::Pressed(_)));
        assert!(matches!(evs[1], KeyEvent::Released(_)));
        if let (KeyEvent::Pressed(a), KeyEvent::Released(b)) = (evs[0], evs[1]) {
            assert!(b.duration_since(a) >= Duration::from_millis(600));
        }
        // Escape only while grabbed.
        run(&["key", "Escape"]);
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(rx.try_iter().count(), 0);
        keys.set_escape_grab(true);
        run(&["key", "Escape"]);
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), vec![KeyEvent::Escape]);
        keys.set_escape_grab(false);
    }

    /// Live: a window manager that owns `Alt+Escape` (stock Openbox's
    /// `<keybind key="A-Escape">`) is simulated by a second client grabbing
    /// it. Cantrip's plain-Escape grab must still succeed and report plain
    /// Escape (also under CapsLock / NumLock), and never Alt+Escape.
    #[test]
    #[ignore]
    fn live_x11_escape_grab_coexists_with_a_wm_alt_escape_binding() {
        let wm = X11::connect().expect("DISPLAY");
        let esc = wm.keycode_for(KEYSYM_ESCAPE).unwrap();
        let alt = u16::from(ModMask::M1);
        let wm_grabs: Vec<u16> = lock_variants(alt)
            .into_iter()
            .filter(|v| {
                wm.conn
                    .grab_key(
                        false,
                        wm.root,
                        ModMask::from(*v),
                        esc,
                        GrabMode::ASYNC,
                        GrabMode::ASYNC,
                    )
                    .ok()
                    .is_some_and(|c| c.check().is_ok())
            })
            .collect();
        // Either the real WM already owns it, or our stand-in now does.
        let probe = X11::connect().unwrap();
        let taken = probe
            .conn
            .grab_key(
                false,
                probe.root,
                ModMask::ANY,
                esc,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )
            .ok()
            .is_some_and(|c| c.check().is_err());
        assert!(
            taken,
            "AnyModifier Escape must collide (the Pass 1 failure)"
        );
        drop(probe);

        let mut keys = LinuxHotkeys::new();
        let (tx, rx) = std::sync::mpsc::channel();
        keys.register(&Chord::platform_default(), tx).unwrap();
        keys.set_escape_grab(true);
        let run = |args: &[&str]| {
            assert!(std::process::Command::new("xdotool")
                .args(args)
                .status()
                .unwrap()
                .success())
        };
        run(&["key", "Escape"]);
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), vec![KeyEvent::Escape]);
        // Alt+Escape stays the window manager's.
        run(&["key", "alt+Escape"]);
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(rx.try_iter().count(), 0, "Alt+Escape is not a cancel");
        // CapsLock on: plain Escape still cancels.
        run(&["key", "Caps_Lock"]);
        run(&["key", "Escape"]);
        run(&["key", "Caps_Lock"]);
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), vec![KeyEvent::Escape]);
        keys.set_escape_grab(false);
        run(&["key", "Escape"]);
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(rx.try_iter().count(), 0, "released after the take");
        for v in wm_grabs {
            let _ = wm.conn.ungrab_key(esc, wm.root, ModMask::from(v));
        }
        wm.flush();
    }
}
