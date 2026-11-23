//! Linux `UiPlatform`: system fonts (fontconfig), light/dark appearance
//! (xdg-desktop-portal, then GNOME settings, then `GTK_THEME`), the native
//! folder picker (portal FileChooser, then zenity / kdialog), and window
//! behavior delegated to [`super::window::WindowPatcher`].

use super::dbus_util;
use super::window::WindowPatcher;
use crate::platform::raw_window::Raw;
use crate::platform::{SurfaceKind, UiPlatform};
use dbus::arg::{PropMap, RefArg, Variant};
use dbus::blocking::Connection;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
/// A person is choosing a folder; wait for them, within reason.
const PICKER_TIMEOUT: Duration = Duration::from_secs(600);

pub struct LinuxUi {
    windows: WindowPatcher,
}

impl LinuxUi {
    pub fn new() -> LinuxUi {
        LinuxUi {
            windows: WindowPatcher::new(),
        }
    }
}

impl Default for LinuxUi {
    fn default() -> Self {
        Self::new()
    }
}

impl UiPlatform for LinuxUi {
    fn configure_window(&self, kind: SurfaceKind, raw: Raw) {
        self.windows.configure_window(kind, raw)
    }

    fn show_hud(&self, raw: Raw, visible: bool) {
        self.windows.show_hud(raw, visible)
    }

    fn hud_anchor(&self) -> Option<(f32, f32)> {
        self.windows.hud_anchor()
    }

    fn prefers_dark(&self) -> bool {
        prefers_dark()
    }

    fn system_fonts(&self) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
        (font_bytes("sans-serif"), font_bytes("monospace"))
    }

    /// Blocks until the user answers; call off the UI thread.
    fn choose_folder(&self, start: &Path) -> Result<Option<PathBuf>, String> {
        match portal_choose_folder(start) {
            Ok(r) => return Ok(r),
            Err(PickerError::NoPortal) => {}
            Err(PickerError::Failed(e)) => return Err(e),
        }
        command_choose_folder(start)
    }
}

// ───────────────────────────── fonts ─────────────────────────────

/// Bytes of fontconfig's best match for `pattern` (TTF/OTF only).
pub fn font_bytes(pattern: &str) -> Option<Vec<u8>> {
    let out = Command::new("fc-match")
        .args(["-f", "%{file}", pattern])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let path = String::from_utf8(out.stdout).ok()?;
    let path = path.trim();
    let lower = path.to_ascii_lowercase();
    if !(lower.ends_with(".ttf") || lower.ends_with(".otf") || lower.ends_with(".ttc")) {
        return None;
    }
    std::fs::read(path).ok()
}

// ───────────────────────────── appearance ─────────────────────────────

/// The session's dark preference, by the first source that answers.
pub fn prefers_dark() -> bool {
    if let Some(v) = portal_color_scheme() {
        return v == 1;
    }
    if let Some(dark) = gsettings_dark() {
        return dark;
    }
    gtk_theme_dark(std::env::var("GTK_THEME").ok().as_deref())
}

/// `org.freedesktop.appearance color-scheme`: 0 none, 1 dark, 2 light.
fn portal_color_scheme() -> Option<u32> {
    let conn = Connection::new_session().ok()?;
    if !dbus_util::name_has_owner(&conn, PORTAL) {
        return None;
    }
    let p = conn.with_proxy(PORTAL, PORTAL_PATH, Duration::from_millis(500));
    let read = |method: &str| -> Option<u32> {
        let (v,): (Variant<Box<dyn RefArg>>,) = p
            .method_call(
                "org.freedesktop.portal.Settings",
                method,
                ("org.freedesktop.appearance", "color-scheme"),
            )
            .ok()?;
        variant_u32(&*v.0)
    };
    read("ReadOne").or_else(|| read("Read"))
}

/// Unwrap nested variants (the old `Read` returns `v` inside `v`).
fn variant_u32(v: &dyn RefArg) -> Option<u32> {
    if let Some(n) = v.as_u64() {
        return u32::try_from(n).ok();
    }
    let mut it = v.as_iter()?;
    variant_u32(it.next()?)
}

fn gsettings_dark() -> Option<bool> {
    let out = Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "color-scheme"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
    if v.contains("dark") {
        Some(true)
    } else if v.contains("light") {
        Some(false)
    } else {
        None // 'default': no explicit preference; fall through
    }
}

/// `GTK_THEME=Adwaita:dark` convention.
pub fn gtk_theme_dark(value: Option<&str>) -> bool {
    value
        .map(|v| v.to_ascii_lowercase())
        .is_some_and(|v| v.ends_with(":dark") || v.ends_with("-dark"))
}

// ───────────────────────────── folder picker ─────────────────────────────

enum PickerError {
    NoPortal,
    Failed(String),
}

fn portal_choose_folder(start: &Path) -> Result<Option<PathBuf>, PickerError> {
    let conn = Connection::new_session().map_err(|_| PickerError::NoPortal)?;
    if !dbus_util::name_has_owner(&conn, PORTAL) {
        return Err(PickerError::NoPortal);
    }
    let token = format!("cantrip{}", std::process::id());
    let sender = conn.unique_name().to_string();
    let request_path = request_path(&sender, &token);
    let pending = dbus_util::subscribe(
        &conn,
        &request_path,
        "org.freedesktop.portal.Request",
        "Response",
    )
    .map_err(|_| PickerError::NoPortal)?;
    let mut opts: PropMap = HashMap::new();
    opts.insert("handle_token".into(), Variant(Box::new(token)));
    opts.insert("directory".into(), Variant(Box::new(true)));
    opts.insert("modal".into(), Variant(Box::new(true)));
    let mut start_bytes = start.to_string_lossy().as_bytes().to_vec();
    start_bytes.push(0);
    opts.insert("current_folder".into(), Variant(Box::new(start_bytes)));
    let p = conn.with_proxy(PORTAL, PORTAL_PATH, Duration::from_secs(5));
    let res: Result<(dbus::Path<'static>,), _> = p.method_call(
        "org.freedesktop.portal.FileChooser",
        "OpenFile",
        ("", "Choose Meeting folder", opts),
    );
    if let Err(e) = res {
        // No FileChooser backend behind the portal: fall back to commands.
        if e.name() == Some("org.freedesktop.DBus.Error.UnknownMethod")
            || e.name() == Some("org.freedesktop.DBus.Error.ServiceUnknown")
        {
            return Err(PickerError::NoPortal);
        }
        return Err(PickerError::Failed(
            "The folder picker could not open".into(),
        ));
    }
    let msg = pending
        .wait(PICKER_TIMEOUT)
        .ok_or_else(|| PickerError::Failed("The folder picker did not answer".into()))?;
    let (code, results): (u32, PropMap) = msg
        .read2()
        .map_err(|_| PickerError::Failed("The folder picker answered unexpectedly".into()))?;
    if code != 0 {
        return Ok(None); // cancelled
    }
    let uri = results
        .get("uris")
        .and_then(|v| v.0.as_iter())
        .and_then(|mut it| it.next().and_then(|u| u.as_str().map(str::to_string)));
    Ok(uri.and_then(|u| file_uri_to_path(&u)))
}

/// `/org/freedesktop/portal/desktop/request/<sender>/<token>`.
pub fn request_path(unique_name: &str, token: &str) -> String {
    let sender = unique_name.trim_start_matches(':').replace('.', "_");
    format!("{PORTAL_PATH}/request/{sender}/{token}")
}

/// Decode a `file://` URI (percent-encoding) to a path.
pub fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    use std::os::unix::ffi::OsStringExt;
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

fn command_choose_folder(start: &Path) -> Result<Option<PathBuf>, String> {
    let start_s = format!("{}/", start.display());
    let candidates: [(&str, Vec<String>); 2] = [
        (
            "zenity",
            vec![
                "--file-selection".into(),
                "--directory".into(),
                "--title=Choose Meeting folder".into(),
                format!("--filename={start_s}"),
            ],
        ),
        (
            "kdialog",
            vec![
                "--getexistingdirectory".into(),
                start.display().to_string(),
                "--title".into(),
                "Choose Meeting folder".into(),
            ],
        ),
    ];
    for (prog, args) in candidates {
        match Command::new(prog)
            .args(&args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
        {
            Ok(out) if out.status.success() => {
                let p = String::from_utf8_lossy(&out.stdout)
                    .trim_end_matches('\n')
                    .to_string();
                return Ok((!p.is_empty()).then(|| PathBuf::from(p)));
            }
            // Exit 1 = the user cancelled.
            Ok(out) if out.status.code() == Some(1) => return Ok(None),
            Ok(_) => return Err("The folder picker failed".into()),
            Err(_) => continue, // not installed
        }
    }
    Err("no folder picker".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gtk_theme_convention() {
        assert!(gtk_theme_dark(Some("Adwaita:dark")));
        assert!(gtk_theme_dark(Some("Breeze-Dark")));
        assert!(!gtk_theme_dark(Some("Adwaita")));
        assert!(!gtk_theme_dark(None));
    }

    #[test]
    fn portal_request_path() {
        assert_eq!(
            request_path(":1.42", "cantrip7"),
            "/org/freedesktop/portal/desktop/request/1_42/cantrip7"
        );
    }

    #[test]
    fn file_uris_decode() {
        assert_eq!(
            file_uri_to_path("file:///home/u/My%20Notes").unwrap(),
            PathBuf::from("/home/u/My Notes")
        );
        assert_eq!(file_uri_to_path("https://x/y"), None);
    }

    #[test]
    fn system_fonts_resolve_here() {
        // fontconfig is present in this environment; tolerate its absence.
        if Command::new("fc-match").arg("--version").output().is_ok() {
            assert!(font_bytes("sans-serif").is_some_and(|b| b.len() > 1000));
        }
    }
}
