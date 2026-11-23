//! X11 window patching for the egui surfaces (also XWayland windows).
//!
//! The HUD becomes a non-activating, click-through overlay on every
//! workspace: `override_redirect` (applied on the next map, so the window
//! manager never manages, focuses or decorates it), plus EWMH hints for the
//! window that is already mapped (`NOTIFICATION`/`DOCK` type, above, sticky,
//! all desktops, skip taskbar/pager), `WM_HINTS.input = False`, no
//! `WM_TAKE_FOCUS`, and an empty XShape *input* region so pointer events pass
//! through to whatever is beneath. [`WindowPatcher::show_hud`] maps/unmaps it
//! and raises it, which never moves keyboard focus.
//!
//! On native Wayland windows (`Raw::Wayland`) nothing can be patched; the app
//! runs its windows through XWayland so this module applies.

use super::x11::X11;
use crate::platform::raw_window::Raw;
use std::sync::Arc;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ClipOrdering,
    ConfigureWindowAux, ConnectionExt as _, EventMask, PropMode, StackMode, Window,
};
use x11rb::wrapper::ConnectionExt as _;

/// WM_HINTS flag: the `input` field is valid.
const INPUT_HINT: u32 = 1;
const NET_WM_STATE_ADD: u32 = 1;

pub struct WindowPatcher {
    x: Option<Arc<X11>>,
}

impl Default for WindowPatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowPatcher {
    pub fn new() -> WindowPatcher {
        WindowPatcher { x: X11::connect() }
    }

    fn window(raw: Raw) -> Option<Window> {
        match raw {
            Raw::X11(id) => u32::try_from(id).ok(),
            _ => None,
        }
    }

    pub fn configure_hud(&self, raw: Raw) {
        let (Some(x), Some(win)) = (self.x.as_ref(), Self::window(raw)) else {
            return;
        };
        let a = &x.atoms;
        {
            {
                let _ = x.conn.change_window_attributes(
                    win,
                    &ChangeWindowAttributesAux::new().override_redirect(1),
                );
                self.set_types(
                    x,
                    win,
                    &[
                        a._NET_WM_WINDOW_TYPE_NOTIFICATION,
                        a._NET_WM_WINDOW_TYPE_DOCK,
                    ],
                );
                self.set_states(
                    x,
                    win,
                    &[
                        a._NET_WM_STATE_ABOVE,
                        a._NET_WM_STATE_STICKY,
                        a._NET_WM_STATE_SKIP_TASKBAR,
                        a._NET_WM_STATE_SKIP_PAGER,
                    ],
                );
                self.all_desktops(x, win);
                self.no_input_focus(x, win);
                // Empty input region: clicks pass through.
                let _ = x.conn.shape_rectangles(
                    shape::SO::SET,
                    shape::SK::INPUT,
                    ClipOrdering::UNSORTED,
                    win,
                    0,
                    0,
                    &[],
                );
            }
        }
        x.flush();
    }

    /// Map (and raise) or unmap the HUD. Mapping an override-redirect window
    /// never changes keyboard focus.
    pub fn show_hud(&self, raw: Raw, visible: bool) {
        let (Some(x), Some(win)) = (self.x.as_ref(), Self::window(raw)) else {
            return;
        };
        if visible {
            let _ = x.conn.map_window(win);
            let _ = x
                .conn
                .configure_window(win, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE));
        }
        // Hidden = parked off-screen by the UI, never unmapped (see trait).
        x.flush();
    }

    /// Clip the HUD to its pill shape when no compositing manager runs (no
    /// compositor → no per-pixel transparency, so the transparent corners
    /// would show as black squares). With a compositor this is a no-op.
    pub fn shape_hud(&self, raw: Raw, width: u32, height: u32) {
        let (Some(x), Some(win)) = (self.x.as_ref(), Self::window(raw)) else {
            return;
        };
        let composited = x
            .conn
            .get_selection_owner(x.atoms._NET_WM_CM_S0)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|r| r.owner != x11rb::NONE);
        if composited || width == 0 || height == 0 {
            return;
        }
        let rects = pill_rows(width, height);
        let _ = x.conn.shape_rectangles(
            shape::SO::SET,
            shape::SK::BOUNDING,
            ClipOrdering::YX_BANDED,
            win,
            0,
            0,
            &rects,
        );
        x.flush();
    }

    /// Bottom-centre (physical px) of the monitor under the pointer, inside the
    /// EWMH work area when the WM publishes one. The UI converts with its own
    /// scale factor and subtracts its size and margin.
    pub fn hud_anchor(&self) -> Option<(f32, f32)> {
        let x = self.x.as_ref()?;
        let p = x.conn.query_pointer(x.root).ok()?.reply().ok()?;
        let (px, py) = (p.root_x as i32, p.root_y as i32);
        let mut rect = (0i32, 0i32, x.screen_width as i32, x.screen_height as i32);
        if let Some(mons) = x
            .conn
            .randr_get_monitors(x.root, true)
            .ok()
            .and_then(|c| c.reply().ok())
        {
            if let Some(m) = mons.monitors.iter().find(|m| {
                let (mx, my) = (m.x as i32, m.y as i32);
                px >= mx && px < mx + m.width as i32 && py >= my && py < my + m.height as i32
            }) {
                rect = (m.x as i32, m.y as i32, m.width as i32, m.height as i32);
            }
        }
        // Clip to the work area (panels), if published.
        if let Some(wa) = x.prop32(x.root, x.atoms._NET_WORKAREA) {
            if wa.len() >= 4 {
                let (wx, wy, ww, wh) = (wa[0] as i32, wa[1] as i32, wa[2] as i32, wa[3] as i32);
                let left = rect.0.max(wx);
                let top = rect.1.max(wy);
                let right = (rect.0 + rect.2).min(wx + ww);
                let bottom = (rect.1 + rect.3).min(wy + wh);
                if right > left && bottom > top {
                    rect = (left, top, right - left, bottom - top);
                }
            }
        }
        Some(((rect.0 + rect.2 / 2) as f32, (rect.1 + rect.3) as f32))
    }

    fn set_types(&self, x: &X11, win: Window, types: &[Atom]) {
        let _ = x.conn.change_property32(
            PropMode::REPLACE,
            win,
            x.atoms._NET_WM_WINDOW_TYPE,
            AtomEnum::ATOM,
            types,
        );
    }

    /// Set `_NET_WM_STATE` directly (unmapped) and ask the WM (mapped).
    fn set_states(&self, x: &X11, win: Window, states: &[Atom]) {
        let _ = x.conn.change_property32(
            PropMode::REPLACE,
            win,
            x.atoms._NET_WM_STATE,
            AtomEnum::ATOM,
            states,
        );
        for pair in states.chunks(2) {
            let data = [
                NET_WM_STATE_ADD,
                pair[0],
                pair.get(1).copied().unwrap_or(0),
                1,
                0,
            ];
            self.root_message(x, win, x.atoms._NET_WM_STATE, data);
        }
    }

    fn all_desktops(&self, x: &X11, win: Window) {
        let all = 0xFFFF_FFFFu32;
        let _ = x.conn.change_property32(
            PropMode::REPLACE,
            win,
            x.atoms._NET_WM_DESKTOP,
            AtomEnum::CARDINAL,
            &[all],
        );
        self.root_message(x, win, x.atoms._NET_WM_DESKTOP, [all, 1, 0, 0, 0]);
    }

    /// WM_HINTS.input = False and drop WM_TAKE_FOCUS from WM_PROTOCOLS.
    fn no_input_focus(&self, x: &X11, win: Window) {
        let mut hints = x
            .prop32(win, x.atoms.WM_HINTS)
            .unwrap_or_else(|| vec![0; 9]);
        hints.resize(9, 0);
        hints[0] |= INPUT_HINT;
        hints[1] = 0;
        let _ = x.conn.change_property32(
            PropMode::REPLACE,
            win,
            x.atoms.WM_HINTS,
            x.atoms.WM_HINTS,
            &hints,
        );
        if let Some(protocols) = x.prop32(win, x.atoms.WM_PROTOCOLS) {
            let kept: Vec<u32> = protocols
                .into_iter()
                .filter(|p| *p != x.atoms.WM_TAKE_FOCUS)
                .collect();
            let _ = x.conn.change_property32(
                PropMode::REPLACE,
                win,
                x.atoms.WM_PROTOCOLS,
                AtomEnum::ATOM,
                &kept,
            );
        }
    }

    fn root_message(&self, x: &X11, win: Window, ty: Atom, data: [u32; 5]) {
        let ev = ClientMessageEvent::new(32, win, ty, data);
        let _ = x.conn.send_event(
            false,
            x.root,
            EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
            ev,
        );
    }
}

/// Horizontal bands covering a full pill (radius = height / 2), one band per
/// run of rows with the same inset, top to bottom.
fn pill_rows(width: u32, height: u32) -> Vec<x11rb::protocol::xproto::Rectangle> {
    let r = height as f32 / 2.0;
    let mut out: Vec<x11rb::protocol::xproto::Rectangle> = Vec::new();
    for y in 0..height {
        let dy = (y as f32 + 0.5 - r).abs();
        let inset = (r - (r * r - dy * dy).max(0.0).sqrt()).round() as u32;
        let w = width.saturating_sub(2 * inset);
        match out.last_mut() {
            Some(last) if last.x as u32 == inset && last.width as u32 == w => last.height += 1,
            _ => out.push(x11rb::protocol::xproto::Rectangle {
                x: inset as i16,
                y: y as i16,
                width: w as u16,
                height: 1,
            }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn pill_rows_cover_the_middle_fully_and_round_the_ends() {
        let rows = super::pill_rows(200, 40);
        assert_eq!(rows.iter().map(|r| r.height as u32).sum::<u32>(), 40);
        let mid = rows
            .iter()
            .find(|r| r.y <= 20 && r.y as u32 + r.height as u32 > 20)
            .unwrap();
        assert_eq!((mid.x, mid.width), (0, 200));
        assert!(rows[0].x > 10, "top row is inset by the curve");
    }

    use super::*;

    #[test]
    fn non_x11_handles_are_ignored_without_panicking() {
        let p = WindowPatcher { x: None };
        p.configure_hud(Raw::Wayland(1));
        p.configure_hud(Raw::X11(42));
        p.show_hud(Raw::AppKit(1), true);
        assert_eq!(p.hud_anchor(), None);
        assert_eq!(
            WindowPatcher::window(Raw::X11(0x1a0_0007)),
            Some(0x1a0_0007)
        );
        assert_eq!(WindowPatcher::window(Raw::Unknown), None);
    }
}

/// Live check on `$DISPLAY` (run with `cargo test -p cantrip -- --ignored`).
#[cfg(test)]
mod live {
    use super::*;
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{CreateWindowAux, WindowClass};

    #[test]
    #[ignore]
    fn live_hud_is_override_redirect_click_through_and_takes_no_focus() {
        let x = X11::connect().expect("display");
        let win = x.conn.generate_id().unwrap();
        x.conn
            .create_window(
                0,
                win,
                x.root,
                100,
                100,
                240,
                32,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new(),
            )
            .unwrap()
            .check()
            .unwrap();
        let focus_before = x.conn.get_input_focus().unwrap().reply().unwrap().focus;
        let p = WindowPatcher::new();
        p.configure_hud(Raw::X11(win as u64));
        p.show_hud(Raw::X11(win as u64), true);
        std::thread::sleep(std::time::Duration::from_millis(300));
        let attrs = x.conn.get_window_attributes(win).unwrap().reply().unwrap();
        assert!(attrs.override_redirect, "override-redirect");
        assert_eq!(attrs.map_state, x11rb::protocol::xproto::MapState::VIEWABLE);
        let rects = x
            .conn
            .shape_get_rectangles(win, shape::SK::INPUT)
            .unwrap()
            .reply()
            .unwrap();
        assert!(
            rects.rectangles.is_empty(),
            "empty input region = click-through"
        );
        let focus_after = x.conn.get_input_focus().unwrap().reply().unwrap().focus;
        assert_eq!(
            focus_before, focus_after,
            "showing the HUD never moves focus"
        );
        let hints = x.prop32(win, x.atoms.WM_HINTS).unwrap();
        assert_eq!((hints[0] & INPUT_HINT, hints[1]), (INPUT_HINT, 0));
        let anchor = p.hud_anchor().unwrap();
        eprintln!("hud anchor (bottom-centre of pointer's monitor): {anchor:?}; focus {focus_before:x} unchanged");
        p.show_hud(Raw::X11(win as u64), false);
        x.flush();
        let attrs = x.conn.get_window_attributes(win).unwrap().reply().unwrap();
        assert_eq!(attrs.map_state, x11rb::protocol::xproto::MapState::UNMAPPED);
        let _ = x.conn.destroy_window(win);
        x.flush();
    }
}
