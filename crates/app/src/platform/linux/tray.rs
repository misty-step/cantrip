//! StatusNotifierItem tray icon (KDE Plasma, wlroots bars, GNOME with the
//! AppIndicator extension). Present only when a StatusNotifierWatcher exists;
//! trayless sessions (stock GNOME) use the launcher entry instead
//! (`cantrip popover`). Activation opens the popover — the full control
//! surface — so the tray is never an icon-only control.

use super::dbus_util;
use crate::platform::{Tray, UiRequest};
use std::sync::mpsc::Sender;
use std::sync::Mutex;
use std::time::Duration;

pub const WATCHER: &str = "org.kde.StatusNotifierWatcher";

struct CantripTray {
    requests: Mutex<Sender<UiRequest>>,
    recording: bool,
}

impl ksni::Tray for CantripTray {
    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.requests.lock().unwrap().send(UiRequest::ShowPopover);
    }

    fn id(&self) -> String {
        "cantrip".into()
    }

    fn title(&self) -> String {
        "Cantrip".into()
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::ApplicationStatus
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: if self.recording {
                "Cantrip — recording".into()
            } else {
                "Cantrip".into()
            },
            ..Default::default()
        }
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        [22, 44]
            .iter()
            .map(|&size| ksni::Icon {
                width: size,
                height: size,
                data: icon_argb(size as usize, self.recording),
            })
            .collect()
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        vec![ksni::menu::StandardItem {
            label: "Open Cantrip".into(),
            activate: Box::new(|t: &mut CantripTray| {
                let _ = t.requests.lock().unwrap().send(UiRequest::ShowPopover);
            }),
            ..Default::default()
        }
        .into()]
    }
}

pub struct LinuxTray {
    handle: ksni::Handle<CantripTray>,
}

impl Tray for LinuxTray {
    fn set_recording(&self, recording: bool) {
        self.handle.update(|t| t.recording = recording);
    }
}

/// Start the tray if this session has a StatusNotifierWatcher.
pub fn start(requests: Sender<UiRequest>) -> Option<LinuxTray> {
    let conn = dbus::blocking::Connection::new_session().ok()?;
    if !dbus_util::name_has_owner(&conn, WATCHER) {
        return None;
    }
    drop(conn);
    let service = ksni::TrayService::new(CantripTray {
        requests: Mutex::new(requests),
        recording: false,
    });
    let handle = service.handle();
    std::thread::Builder::new()
        .name("cantrip-tray".into())
        .spawn(move || {
            if let Err(e) = service.run() {
                eprintln!(
                    "[Tray] status notifier stopped: {}",
                    e.name().unwrap_or("dbus error")
                );
            }
        })
        .ok()?;
    std::thread::sleep(Duration::from_millis(10));
    Some(LinuxTray { handle })
}

/// The Cantrip glyph: a calm monochrome microphone capsule with a stand, plus
/// a red dot while recording. ARGB32, network byte order.
pub fn icon_argb(size: usize, recording: bool) -> Vec<u8> {
    let mut px = vec![0u8; size * size * 4];
    let s = size as f32;
    let fg = [0xff, 0xe8, 0xe8, 0xe8];
    let edge = [0xff, 0x30, 0x30, 0x30];
    let red = [0xff, 0xe5, 0x48, 0x4d];
    let mut put = |x: usize, y: usize, c: [u8; 4], a: f32| {
        let i = (y * size + x) * 4;
        let alpha = (a.clamp(0.0, 1.0) * c[0] as f32) as u8;
        if alpha >= px[i] {
            px[i..i + 4].copy_from_slice(&[alpha, c[1], c[2], c[3]]);
        }
    };
    for y in 0..size {
        for x in 0..size {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            // Capsule: centre column, from 0.14s to 0.62s, radius 0.17s.
            let cx = s * 0.5;
            let r = s * 0.17;
            let (top, bottom) = (s * 0.14 + r, s * 0.62 - r);
            let dy = if fy < top {
                top - fy
            } else if fy > bottom {
                fy - bottom
            } else {
                0.0
            };
            let d = ((fx - cx).powi(2) + dy.powi(2)).sqrt() - r;
            // Stand: arc below the capsule plus a short stem.
            let arc_r = s * 0.27;
            let ad = ((fx - cx).powi(2) + (fy - s * 0.48).powi(2)).sqrt();
            let on_arc = fy > s * 0.48 && (ad - arc_r).abs() < s * 0.045;
            let on_stem = (fx - cx).abs() < s * 0.045 && fy > s * 0.75 && fy < s * 0.88;
            let on_base = (fy - s * 0.88).abs() < s * 0.045 && (fx - cx).abs() < s * 0.17;
            if d < -0.8 {
                put(x, y, fg, 1.0);
            } else if d < 0.6 {
                put(x, y, edge, 1.0);
            }
            if on_arc || on_stem || on_base {
                put(x, y, fg, 1.0);
            }
            if recording {
                let rd = ((fx - s * 0.8).powi(2) + (fy - s * 0.8).powi(2)).sqrt();
                if rd < s * 0.19 {
                    put(x, y, red, 1.0);
                }
            }
        }
    }
    px
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_is_argb_sized_and_recording_adds_red() {
        let idle = icon_argb(22, false);
        let rec = icon_argb(22, true);
        assert_eq!(idle.len(), 22 * 22 * 4);
        let reds = |p: &[u8]| {
            p.chunks(4)
                .filter(|c| c[0] > 0 && c[1] == 0xe5 && c[2] == 0x48)
                .count()
        };
        assert_eq!(reds(&idle), 0);
        assert!(reds(&rec) > 10);
        assert!(idle.chunks(4).filter(|c| c[0] > 0).count() > 40);
    }

    #[test]
    #[ignore]
    fn tray_absent_without_watcher() {
        let (tx, _rx) = std::sync::mpsc::channel();
        assert!(start(tx).is_none());
    }

    /// Run via scripts/test-linux-tray (private bus + fake watcher).
    #[test]
    #[ignore]
    fn tray_exported_with_watcher() {
        use dbus::arg::{RefArg, Variant};
        let (tx, rx) = std::sync::mpsc::channel();
        let tray = start(tx).expect("tray with a watcher present");
        let conn = dbus::blocking::Connection::new_session().unwrap();
        let bus = conn.with_proxy(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            Duration::from_secs(2),
        );
        let mut name = None;
        for _ in 0..50 {
            let (names,): (Vec<String>,) = bus
                .method_call("org.freedesktop.DBus", "ListNames", ())
                .unwrap();
            name = names
                .into_iter()
                .find(|n| n.starts_with("org.kde.StatusNotifierItem-"));
            if name.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let name = name.expect("item name on the bus");
        let item = conn.with_proxy(name.as_str(), "/StatusNotifierItem", Duration::from_secs(2));
        let get = |p: &str| -> Variant<Box<dyn RefArg>> {
            let (v,): (Variant<Box<dyn RefArg>>,) = item
                .method_call(
                    "org.freedesktop.DBus.Properties",
                    "Get",
                    ("org.kde.StatusNotifierItem", p),
                )
                .unwrap();
            v
        };
        assert_eq!(get("Title").0.as_str(), Some("Cantrip"));
        assert_eq!(get("Id").0.as_str(), Some("cantrip"));
        let _: () = item
            .method_call("org.kde.StatusNotifierItem", "Activate", (0i32, 0i32))
            .unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            UiRequest::ShowPopover
        );
        let menu = conn.with_proxy(name.as_str(), "/MenuBar", Duration::from_secs(2));
        type Layout = (i32, dbus::arg::PropMap, Vec<Variant<Box<dyn RefArg>>>);
        let (_rev, layout): (u32, Layout) = menu
            .method_call(
                "com.canonical.dbusmenu",
                "GetLayout",
                (0i32, -1i32, Vec::<String>::new()),
            )
            .unwrap();
        let dump = format!("{:?}", layout.2);
        assert!(dump.contains("Open Cantrip"), "{dump}");
        tray.set_recording(true);
        eprintln!("tray item {name}: Title=Cantrip, Activate→ShowPopover, menu has Open Cantrip");
    }
}
