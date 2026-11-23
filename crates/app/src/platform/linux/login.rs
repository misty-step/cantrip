//! Open at login via an XDG autostart entry owned by Cantrip:
//! `$XDG_CONFIG_HOME/autostart/cantrip.desktop`. Disabling removes only that
//! one file.

use cantrip_core::ports::LoginItem;
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub const FILE_NAME: &str = "cantrip.desktop";

pub struct XdgAutostart {
    dir: PathBuf,
    exe: PathBuf,
}

impl XdgAutostart {
    /// The user's autostart directory and the running executable.
    pub fn new() -> XdgAutostart {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| "/".into())
                    .join(".config")
            });
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("cantrip"));
        XdgAutostart::with_dir(&config.join("autostart"), &exe)
    }

    pub fn with_dir(dir: &Path, exe: &Path) -> XdgAutostart {
        XdgAutostart {
            dir: dir.to_path_buf(),
            exe: exe.to_path_buf(),
        }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(FILE_NAME)
    }

    /// The autostart entry's content.
    pub fn entry(&self) -> String {
        format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Cantrip\n\
             Comment=Dictation and meeting notes\n\
             Exec={}\n\
             Icon=cantrip\n\
             Terminal=false\n\
             NoDisplay=true\n\
             X-GNOME-Autostart-enabled=true\n\
             X-KDE-autostart-after=panel\n",
            exec_quote(&self.exe.to_string_lossy())
        )
    }
}

impl Default for XdgAutostart {
    fn default() -> Self {
        Self::new()
    }
}

impl LoginItem for XdgAutostart {
    fn enabled(&self) -> bool {
        self.path().is_file()
    }

    fn set_enabled(&self, on: bool) -> Result<(), String> {
        let path = self.path();
        if on {
            fs::create_dir_all(&self.dir)
                .map_err(|e| format!("Could not create autostart folder: {e}"))?;
            let write = || -> std::io::Result<()> {
                let mut f = fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o644)
                    .open(&path)?;
                f.write_all(self.entry().as_bytes())?;
                f.sync_all()
            };
            write().map_err(|e| format!("Could not write the autostart entry: {e}"))
        } else {
            match fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(format!("Could not remove the autostart entry: {e}")),
            }
        }
    }
}

/// Quote one argument per the Desktop Entry `Exec` rules.
pub fn exec_quote(arg: &str) -> String {
    let needs = arg.is_empty()
        || arg
            .chars()
            .any(|c| c.is_whitespace() || "\"'\\><~|&;$*?#()`%".contains(c));
    if !needs {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    for c in arg.chars() {
        match c {
            '"' | '`' | '$' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '%' => out.push_str("%%"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_names_the_executable_and_autostarts() {
        let dir = tempfile::tempdir().unwrap();
        let a = XdgAutostart::with_dir(
            &dir.path().join("autostart"),
            Path::new("/opt/My Apps/cantrip"),
        );
        let e = a.entry();
        assert!(e.starts_with("[Desktop Entry]\nType=Application\n"));
        assert!(e.contains("Exec=\"/opt/My Apps/cantrip\"\n"));
        assert!(e.contains("X-GNOME-Autostart-enabled=true"));
    }

    #[test]
    fn enable_then_disable_touches_only_cantrips_file() {
        let dir = tempfile::tempdir().unwrap();
        let auto = dir.path().join("autostart");
        fs::create_dir_all(&auto).unwrap();
        fs::write(auto.join("other.desktop"), "x").unwrap();
        let a = XdgAutostart::with_dir(&auto, Path::new("/usr/bin/cantrip"));
        assert!(!a.enabled());
        a.set_enabled(true).unwrap();
        assert!(a.enabled());
        assert!(fs::read_to_string(a.path())
            .unwrap()
            .contains("Exec=/usr/bin/cantrip\n"));
        a.set_enabled(false).unwrap();
        assert!(!a.enabled());
        a.set_enabled(false).unwrap();
        assert!(auto.join("other.desktop").exists());
    }

    #[test]
    fn exec_quoting() {
        assert_eq!(exec_quote("/usr/bin/cantrip"), "/usr/bin/cantrip");
        assert_eq!(exec_quote("/a b/$x"), "\"/a b/\\$x\"");
        assert_eq!(exec_quote("50%"), "\"50%%\"");
    }
}
