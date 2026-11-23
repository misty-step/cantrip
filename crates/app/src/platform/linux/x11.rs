//! Shared X11 helpers: connecting (never panicking without a display), the
//! atoms Cantrip uses, property reads, and `/proc` process identity.

use std::sync::Arc;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt as _, Window};
use x11rb::rust_connection::RustConnection;

x11rb::atom_manager! {
    pub Atoms: AtomsCookie {
        CLIPBOARD,
        TARGETS,
        MULTIPLE,
        TIMESTAMP,
        INCR,
        UTF8_STRING,
        TEXT,
        TEXT_PLAIN_UTF8: b"text/plain;charset=utf-8",
        TEXT_PLAIN: b"text/plain",
        KDE_PASSWORD_HINT: b"x-kde-passwordManagerHint",
        CANTRIP_SEL: b"CANTRIP_SELECTION",
        WM_PROTOCOLS,
        WM_TAKE_FOCUS,
        WM_HINTS,
        WM_CLASS,
        AT_SPI_BUS,
        RESOURCE_MANAGER,
        _NET_ACTIVE_WINDOW,
        _NET_WM_PID,
        _NET_WM_STATE,
        _NET_WM_STATE_ABOVE,
        _NET_WM_STATE_STICKY,
        _NET_WM_STATE_SKIP_TASKBAR,
        _NET_WM_STATE_SKIP_PAGER,
        _NET_WM_DESKTOP,
        _NET_WM_WINDOW_TYPE,
        _NET_WM_WINDOW_TYPE_NOTIFICATION,
        _NET_WM_WINDOW_TYPE_DOCK,
        _NET_WM_WINDOW_TYPE_UTILITY,
        _NET_WM_WINDOW_TYPE_DIALOG,
        _NET_WM_WINDOW_TYPE_NORMAL,
        _NET_WORKAREA,
    }
}

/// One X11 connection with its root window and interned atoms.
pub struct X11 {
    pub conn: RustConnection,
    pub root: Window,
    pub atoms: Atoms,
    pub screen_width: u16,
    pub screen_height: u16,
}

impl X11 {
    /// Connect to `$DISPLAY`. `None` without a display or on any failure.
    pub fn connect() -> Option<Arc<X11>> {
        std::env::var_os("DISPLAY").filter(|d| !d.is_empty())?;
        let (conn, screen) = x11rb::connect(None).ok()?;
        let s = conn.setup().roots.get(screen)?;
        let (root, w, h) = (s.root, s.width_in_pixels, s.height_in_pixels);
        let atoms = Atoms::new(&conn).ok()?.reply().ok()?;
        Some(Arc::new(X11 {
            conn,
            root,
            atoms,
            screen_width: w,
            screen_height: h,
        }))
    }

    /// 32-bit property values (`None` when absent or unreadable).
    pub fn prop32(&self, win: Window, prop: Atom) -> Option<Vec<u32>> {
        let r = self
            .conn
            .get_property(false, win, prop, AtomEnum::ANY, 0, 1024)
            .ok()?
            .reply()
            .ok()?;
        let v: Vec<u32> = r.value32()?.collect();
        (!v.is_empty()).then_some(v)
    }

    /// Raw 8-bit property bytes.
    pub fn prop_bytes(&self, win: Window, prop: Atom) -> Option<Vec<u8>> {
        let r = self
            .conn
            .get_property(false, win, prop, AtomEnum::ANY, 0, 1 << 20)
            .ok()?
            .reply()
            .ok()?;
        (r.format == 8 && !r.value.is_empty()).then_some(r.value)
    }

    /// The EWMH active window, `None` when the WM reports none.
    pub fn active_window(&self) -> Option<Window> {
        let w = *self
            .prop32(self.root, self.atoms._NET_ACTIVE_WINDOW)?
            .first()?;
        (w != 0).then_some(w)
    }

    /// `_NET_WM_PID` of a client window.
    pub fn window_pid(&self, win: Window) -> Option<u32> {
        self.prop32(win, self.atoms._NET_WM_PID)?.first().copied()
    }

    /// WM_CLASS class part (second string), else the instance part.
    pub fn window_class(&self, win: Window) -> Option<String> {
        let raw = self.prop_bytes(win, self.atoms.WM_CLASS)?;
        let parts: Vec<&[u8]> = raw.split(|b| *b == 0).filter(|p| !p.is_empty()).collect();
        let pick = parts.get(1).or_else(|| parts.first())?;
        Some(String::from_utf8_lossy(pick).into_owned())
    }

    /// Xft.dpi / 96 from RESOURCE_MANAGER, else 1.0.
    pub fn scale(&self) -> f32 {
        let Some(rm) = self.prop_bytes(self.root, self.atoms.RESOURCE_MANAGER) else {
            return 1.0;
        };
        String::from_utf8_lossy(&rm)
            .lines()
            .find_map(|l| l.strip_prefix("Xft.dpi:"))
            .and_then(|v| v.trim().parse::<f32>().ok())
            .map(|dpi| (dpi / 96.0).max(0.5))
            .unwrap_or(1.0)
    }

    /// First keycode producing `keysym` in the core keyboard mapping.
    pub fn keycode_for(&self, keysym: u32) -> Option<u8> {
        let setup = self.conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let map = self
            .conn
            .get_keyboard_mapping(min, max - min + 1)
            .ok()?
            .reply()
            .ok()?;
        let per = map.keysyms_per_keycode as usize;
        if per == 0 {
            return None;
        }
        map.keysyms
            .chunks(per)
            .position(|syms| syms.contains(&keysym))
            .map(|i| min + i as u8)
    }

    pub fn flush(&self) {
        let _ = self.conn.flush();
    }
}

/// Process start time (clock ticks since boot, `/proc/<pid>/stat` field 22):
/// distinguishes a recycled PID from the original process.
pub fn proc_starttime(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm (field 2) may contain spaces/parens: split after the last ')'.
    let rest = &stat[stat.rfind(')')? + 2..];
    // rest starts at field 3; field 22 is index 19.
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// `/proc/<pid>/comm`.
pub fn proc_comm(pid: u32) -> Option<String> {
    let c = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    let c = c.trim();
    (!c.is_empty()).then(|| c.to_string())
}

/// Instance discriminator surviving PID reuse.
pub fn instance_of(pid: u32) -> Option<String> {
    Some(format!("{pid}:{}", proc_starttime(pid)?))
}

/// True on a native Wayland session (X11 may still exist via XWayland).
pub fn session_is_wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|v| v == "wayland")
}

/// Whether an executable is on PATH.
pub fn on_path(exe: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(exe).is_file()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_process_has_a_stable_instance() {
        let pid = std::process::id();
        let a = instance_of(pid).unwrap();
        assert_eq!(a, instance_of(pid).unwrap());
        assert!(a.starts_with(&format!("{pid}:")));
        assert!(proc_comm(pid).is_some());
        assert!(proc_starttime(u32::MAX).is_none());
    }
}
