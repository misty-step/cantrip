//! Which Linux desktop session Cantrip is running in, for logs, the release
//! matrix and per-session behavior (tray presence, addressable paste…).

/// The session kinds the release matrix distinguishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Session {
    X11,
    WaylandKde,
    WaylandGnome,
    WaylandWlroots,
    WaylandOther,
}

impl Session {
    /// Detect from the process environment.
    pub fn detect() -> Session {
        Session::from_env(|k| std::env::var(k).ok())
    }

    /// Detect from an environment lookup (pure; unit-tested).
    pub fn from_env(env: impl Fn(&str) -> Option<String>) -> Session {
        let nonempty = |k: &str| env(k).filter(|v| !v.trim().is_empty());
        let kind = nonempty("XDG_SESSION_TYPE").map(|v| v.to_ascii_lowercase());
        let wayland = match kind.as_deref() {
            Some("wayland") => true,
            Some("x11") => false,
            _ => nonempty("WAYLAND_DISPLAY").is_some(),
        };
        if !wayland {
            return Session::X11;
        }
        let desktop = nonempty("XDG_CURRENT_DESKTOP")
            .or_else(|| nonempty("XDG_SESSION_DESKTOP"))
            .or_else(|| nonempty("DESKTOP_SESSION"))
            .unwrap_or_default()
            .to_ascii_lowercase();
        let parts: Vec<&str> = desktop.split([':', ';']).map(str::trim).collect();
        let has = |names: &[&str]| parts.iter().any(|p| names.contains(p));
        if has(&["kde", "plasma"]) {
            Session::WaylandKde
        } else if has(&["gnome", "gnome-classic", "ubuntu", "pop", "unity"]) && !has(&["sway"]) {
            Session::WaylandGnome
        } else if has(&[
            "sway", "river", "wayfire", "labwc", "hyprland", "dwl", "qtile", "niri", "cage",
            "wlroots", "phosh",
        ]) || nonempty("SWAYSOCK").is_some()
        {
            Session::WaylandWlroots
        } else {
            Session::WaylandOther
        }
    }

    /// Release-matrix name, e.g. `linux-wayland-kde`.
    pub fn name(self) -> &'static str {
        match self {
            Session::X11 => "linux-x11",
            Session::WaylandKde => "linux-wayland-kde",
            Session::WaylandGnome => "linux-wayland-gnome",
            Session::WaylandWlroots => "linux-wayland-wlroots",
            Session::WaylandOther => "linux-wayland-other",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detect(pairs: &[(&str, &str)]) -> Session {
        Session::from_env(|k| {
            pairs
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.to_string())
        })
    }

    #[test]
    fn x11_by_session_type_or_absence_of_wayland() {
        assert_eq!(
            detect(&[
                ("XDG_SESSION_TYPE", "x11"),
                ("WAYLAND_DISPLAY", "wayland-0")
            ]),
            Session::X11
        );
        assert_eq!(detect(&[("DISPLAY", ":0")]), Session::X11);
        assert_eq!(Session::X11.name(), "linux-x11");
    }

    #[test]
    fn wayland_desktops() {
        let w = |d: &str| detect(&[("XDG_SESSION_TYPE", "wayland"), ("XDG_CURRENT_DESKTOP", d)]);
        assert_eq!(w("KDE"), Session::WaylandKde);
        assert_eq!(w("ubuntu:GNOME"), Session::WaylandGnome);
        assert_eq!(w("GNOME"), Session::WaylandGnome);
        assert_eq!(w("sway"), Session::WaylandWlroots);
        assert_eq!(w("Hyprland"), Session::WaylandWlroots);
        assert_eq!(w("COSMIC"), Session::WaylandOther);
        assert_eq!(w("GNOME").name(), "linux-wayland-gnome");
    }

    #[test]
    fn wayland_display_without_session_type_counts() {
        assert_eq!(
            detect(&[
                ("WAYLAND_DISPLAY", "wayland-1"),
                ("SWAYSOCK", "/run/sway.sock")
            ]),
            Session::WaylandWlroots
        );
    }
}
