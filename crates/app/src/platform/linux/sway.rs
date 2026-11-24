//! Sway (wlroots) compositor IPC: the native global shortcut on Sway, where
//! `xdg-desktop-portal-wlr` exports no `GlobalShortcuts` portal.
//!
//! Speaks the i3/Sway IPC protocol on `$SWAYSOCK` directly (no `swaymsg`):
//! `"i3-ipc"` + payload length (u32) + message type (u32), native endian.
//! [`SwayKeys`] binds the chord's press (`bindsym --no-repeat`) and release
//! (`bindsym --release`) to `cantrip ipc hotkey-down|hotkey-up`, which write
//! one line to the single-instance socket (see `crate::ipc`), and binds plain
//! Escape to `cantrip ipc cancel` only while a take is active. Every binding
//! is removed again (`unbindsym`) on rebind, when the take ends, and on exit.
//! Bindings already in the user's Sway config are never overwritten: such a
//! chord is reported as taken, and Escape is then left to the config.

use crate::platform::HotkeyMode;
use cantrip_core::shortcut::Chord;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

const MAGIC: &[u8; 6] = b"i3-ipc";
const RUN_COMMAND: u32 = 0;
const GET_TREE: u32 = 4;
const GET_VERSION: u32 = 7;
const GET_CONFIG: u32 = 9;
const IO_TIMEOUT: Duration = Duration::from_secs(2);

/// One Sway IPC endpoint (a fresh connection per request: requests are rare).
#[derive(Clone, Debug)]
pub struct SwayIpc {
    path: PathBuf,
}

impl SwayIpc {
    /// `$SWAYSOCK`, if set and answering.
    pub fn from_env() -> Option<SwayIpc> {
        let path = std::env::var_os("SWAYSOCK").filter(|p| !p.is_empty())?;
        let ipc = SwayIpc { path: path.into() };
        ipc.request(GET_VERSION, "").ok()?;
        Some(ipc)
    }

    fn request(&self, kind: u32, payload: &str) -> io::Result<String> {
        let mut s = UnixStream::connect(&self.path)?;
        s.set_read_timeout(Some(IO_TIMEOUT))?;
        s.set_write_timeout(Some(IO_TIMEOUT))?;
        let mut msg = Vec::with_capacity(14 + payload.len());
        msg.extend_from_slice(MAGIC);
        msg.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
        msg.extend_from_slice(&kind.to_ne_bytes());
        msg.extend_from_slice(payload.as_bytes());
        s.write_all(&msg)?;
        let mut head = [0u8; 14];
        s.read_exact(&mut head)?;
        if &head[..6] != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "not sway ipc"));
        }
        let len = u32::from_ne_bytes([head[6], head[7], head[8], head[9]]) as usize;
        let mut body = vec![0u8; len];
        s.read_exact(&mut body)?;
        String::from_utf8(body).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))
    }

    /// Run one Sway command; `Err` with Sway's message when it fails.
    pub fn run(&self, command: &str) -> Result<(), String> {
        let reply = self
            .request(RUN_COMMAND, command)
            .map_err(|e| e.to_string())?;
        let v: serde_json::Value = serde_json::from_str(&reply).map_err(|e| e.to_string())?;
        let results = v.as_array().cloned().unwrap_or_default();
        if results.is_empty() {
            return Err("Sway gave no result".into());
        }
        for r in results {
            if r.get("success").and_then(|b| b.as_bool()) != Some(true) {
                return Err(r
                    .get("error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("Sway refused the command")
                    .to_string());
            }
        }
        Ok(())
    }

    /// The layout tree (`get_tree`).
    pub fn tree(&self) -> Result<serde_json::Value, String> {
        let reply = self.request(GET_TREE, "").map_err(|e| e.to_string())?;
        serde_json::from_str(&reply).map_err(|e| e.to_string())
    }

    /// The loaded config text (what the user's config binds).
    pub fn config(&self) -> Result<String, String> {
        let reply = self.request(GET_CONFIG, "").map_err(|e| e.to_string())?;
        let v: serde_json::Value = serde_json::from_str(&reply).map_err(|e| e.to_string())?;
        Ok(v.get("config")
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .to_string())
    }
}

/// The first window in `tree` owned by `pid` with title `title`:
/// (con_id, container x, y, width, height).
pub fn find_window(
    tree: &serde_json::Value,
    pid: u32,
    title: &str,
) -> Option<(i64, i64, i64, i64, i64)> {
    let int = |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_i64());
    if int(tree, "pid") == Some(i64::from(pid))
        && tree.get("name").and_then(|n| n.as_str()) == Some(title)
    {
        let r = tree.get("rect")?;
        return Some((
            int(tree, "id")?,
            int(r, "x")?,
            int(r, "y")?,
            int(r, "width")?,
            int(r, "height")?,
        ));
    }
    ["nodes", "floating_nodes"]
        .iter()
        .filter_map(|k| tree.get(*k).and_then(|n| n.as_array()))
        .flatten()
        .find_map(|n| find_window(n, pid, title))
}

/// Sway `bindsym` combo for a chord, e.g. `Control+Mod1+space`.
pub fn combo(chord: &Chord) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if chord.ctrl {
        parts.push("Control".into());
    }
    if chord.alt {
        parts.push("Mod1".into());
    }
    if chord.shift {
        parts.push("Shift".into());
    }
    if chord.meta {
        parts.push("Mod4".into());
    }
    parts.push(keysym_name(&chord.key)?);
    Some(parts.join("+"))
}

/// XKB keysym name for a `Chord::key` name.
pub fn keysym_name(key: &str) -> Option<String> {
    let named = match key {
        "Space" => "space",
        "Enter" | "Return" => "Return",
        "Tab" => "Tab",
        "Backspace" => "BackSpace",
        "Insert" => "Insert",
        "Delete" => "Delete",
        "Home" => "Home",
        "End" => "End",
        "PageUp" => "Prior",
        "PageDown" => "Next",
        "Left" => "Left",
        "Up" => "Up",
        "Right" => "Right",
        "Down" => "Down",
        "-" => "minus",
        "=" => "equal",
        "," => "comma",
        "." => "period",
        "/" => "slash",
        ";" => "semicolon",
        "'" => "apostrophe",
        "[" => "bracketleft",
        "]" => "bracketright",
        "\\" => "backslash",
        "`" => "grave",
        _ => {
            if let Some(n) = key.strip_prefix('F').and_then(|n| n.parse::<u32>().ok()) {
                return (1..=24).contains(&n).then(|| format!("F{n}"));
            }
            let mut cs = key.chars();
            return match (cs.next(), cs.next()) {
                (Some(c), None) if c.is_ascii_alphanumeric() => {
                    Some(c.to_ascii_lowercase().to_string())
                }
                _ => None,
            };
        }
    };
    Some(named.to_string())
}

/// Normalized (sorted modifiers, lower-case key) form of a bindsym combo, so
/// `Ctrl+Alt+space`, `$mod+…` (after expansion) and `Mod1+Control+Space`
/// compare equal.
fn normalize(combo: &str) -> String {
    let mut mods: Vec<&str> = Vec::new();
    let mut key = String::new();
    let parts: Vec<&str> = combo.split('+').collect();
    for (i, p) in parts.iter().enumerate() {
        let m = match p.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => Some("control"),
            "alt" | "mod1" => Some("mod1"),
            "shift" => Some("shift"),
            "mod4" | "super" | "logo" => Some("mod4"),
            "mod2" => Some("mod2"),
            "mod3" => Some("mod3"),
            "mod5" => Some("mod5"),
            _ => None,
        };
        match m {
            Some(m) if i + 1 < parts.len() => mods.push(m),
            _ => key = p.to_ascii_lowercase(),
        }
    }
    mods.sort_unstable();
    mods.dedup();
    mods.push(&key);
    mods.join("+")
}

/// Whether the user's config binds `combo` in the default mode (top level
/// `bindsym` lines, `$variables` expanded). Bindings inside `mode { }` blocks
/// only apply in that mode and do not collide.
pub fn config_binds(config: &str, combo: &str) -> bool {
    let want = normalize(combo);
    let mut vars: Vec<(String, String)> = Vec::new();
    let mut depth = 0usize;
    for raw in config.lines() {
        let line = raw.trim();
        if line.starts_with('#') {
            continue;
        }
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if depth == 0 {
            match tokens.as_slice() {
                ["set", name, value, ..] if name.starts_with('$') => {
                    vars.push((name.to_string(), value.to_string()));
                    // Longest names first so `$mod2` never expands as `$mod`.
                    vars.sort_by_key(|(n, _)| std::cmp::Reverse(n.len()));
                }
                ["bindsym", rest @ ..] => {
                    if let Some(c) = rest.iter().find(|t| !t.starts_with("--")) {
                        let mut c = c.to_string();
                        for (n, v) in &vars {
                            c = c.replace(n.as_str(), v);
                        }
                        if normalize(&c) == want {
                            return true;
                        }
                    }
                }
                _ => {}
            }
        }
        depth += line.matches('{').count();
        depth = depth.saturating_sub(line.matches('}').count());
    }
    false
}

/// Characters a path may use to be placed in a Sway `exec` unquoted (Sway
/// re-parses binding commands, so quoting cannot be relied on).
fn exec_safe(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-+@%:~".contains(&b))
}

/// The global shortcut and Escape-during-a-take through Sway bindings.
pub struct SwayKeys {
    ipc: SwayIpc,
    /// `exec …` prefix: `<cantrip> ipc` + the line + `--socket <path>`.
    exe: String,
    socket: String,
    /// The bound combo (press and release bindings).
    bound: Option<String>,
    escape: bool,
}

impl SwayKeys {
    /// The Sway backend for this session, if Sway answers and this binary's
    /// path can be bound. Also clears a plain-Escape binding a crashed
    /// Cantrip may have left (only when the user's config binds none).
    pub fn new(ipc: SwayIpc, socket: &Path) -> Result<SwayKeys, String> {
        let exe = std::env::current_exe()
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .into_owned();
        let socket = socket.to_string_lossy().into_owned();
        if !exec_safe(&exe) || !exec_safe(&socket) {
            return Err("Cantrip's path cannot be bound by Sway".into());
        }
        let keys = SwayKeys {
            ipc,
            exe,
            socket,
            bound: None,
            escape: false,
        };
        if !keys.user_binds("Escape") {
            let _ = keys.ipc.run("unbindsym Escape");
        }
        Ok(keys)
    }

    fn exec(&self, line: &str) -> String {
        format!("exec {} ipc {line} --socket {}", self.exe, self.socket)
    }

    fn user_binds(&self, combo: &str) -> bool {
        self.ipc
            .config()
            .map(|c| config_binds(&c, combo))
            .unwrap_or(false)
    }

    fn unbind_chord(&mut self) {
        if let Some(c) = self.bound.take() {
            let _ = self.ipc.run(&format!("unbindsym {c}"));
            let _ = self.ipc.run(&format!("unbindsym --release {c}"));
        }
    }

    /// Bind `chord`'s press and release (replacing the previous chord).
    pub fn register(&mut self, chord: &Chord) -> Result<HotkeyMode, String> {
        let c = combo(chord).ok_or_else(|| format!("Unsupported key {}", chord.key))?;
        if self.bound.as_deref() == Some(c.as_str()) {
            return Ok(HotkeyMode::PressRelease);
        }
        if self.user_binds(&c) {
            return Err(format!(
                "{} is already used by another app",
                chord.display_linux()
            ));
        }
        self.unbind_chord();
        let press = format!("bindsym --no-repeat {c} {}", self.exec("hotkey-down"));
        let release = format!("bindsym --release {c} {}", self.exec("hotkey-up"));
        self.ipc.run(&press)?;
        if let Err(e) = self.ipc.run(&release) {
            let _ = self.ipc.run(&format!("unbindsym {c}"));
            return Err(e);
        }
        self.bound = Some(c);
        Ok(HotkeyMode::PressRelease)
    }

    /// Bind plain Escape to cancel while a take is active; unbind otherwise.
    pub fn set_escape(&mut self, active: bool) {
        if active == self.escape {
            return;
        }
        if active {
            if self.user_binds("Escape") {
                eprintln!("[cantrip] escape bind skipped: the Sway config binds plain Escape");
                return;
            }
            let bind = format!("bindsym --no-repeat Escape {}", self.exec("cancel"));
            match self.ipc.run(&bind) {
                Ok(()) => self.escape = true,
                Err(_) => eprintln!("[cantrip] escape bind unavailable on this Sway session"),
            }
        } else {
            let _ = self.ipc.run("unbindsym Escape");
            self.escape = false;
        }
    }
}

impl Drop for SwayKeys {
    /// Graceful exit: leave no binding behind.
    fn drop(&mut self) {
        self.set_escape(false);
        self.unbind_chord();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chords_translate_to_sway_keysyms() {
        assert_eq!(
            combo(&Chord::platform_default()).as_deref(),
            Some("Control+Mod1+space")
        );
        let c = Chord {
            ctrl: false,
            alt: false,
            shift: true,
            meta: true,
            key: "F9".into(),
        };
        assert_eq!(combo(&c).as_deref(), Some("Shift+Mod4+F9"));
        assert_eq!(keysym_name("D").as_deref(), Some("d"));
        assert_eq!(keysym_name("PageUp").as_deref(), Some("Prior"));
        assert_eq!(keysym_name(",").as_deref(), Some("comma"));
        assert_eq!(keysym_name("F25"), None);
        assert_eq!(keysym_name("Fn"), None);
    }

    #[test]
    fn config_conflicts_respect_variables_aliases_and_modes() {
        let cfg = "set $mod Mod4\nset $mod2 Mod1\n\
                   bindsym $mod+Return exec foot\n\
                   bindsym --to-code Ctrl+$mod2+d exec x\n\
                   # bindsym Control+Mod1+space exec commented\n\
                   mode \"resize\" {\n    bindsym Escape mode \"default\"\n}\n";
        assert!(config_binds(cfg, "Mod4+Return"));
        assert!(config_binds(cfg, "Mod1+Control+d"));
        assert!(!config_binds(cfg, "Control+Mod1+space"), "comment");
        assert!(!config_binds(cfg, "Escape"), "only inside a mode block");
        assert!(config_binds("bindsym Escape nop\n", "Escape"));
        // The stock Sway config never binds Cantrip's default chord.
        if let Ok(stock) = std::fs::read_to_string("/etc/sway/config") {
            assert!(!config_binds(&stock, "Control+Mod1+space"));
            assert!(!config_binds(&stock, "Escape"));
            assert!(config_binds(&stock, "Mod4+Return"));
        }
    }

    /// Live (on a Sway session; skipped elsewhere): rebinding replaces the old chord's press
    /// and release bindings, Escape is bound only while asked, and dropping
    /// the backend (graceful exit) leaves no binding. `unbindsym` fails when
    /// nothing is bound, which is the probe.
    #[test]
    #[ignore]
    fn live_sway_rebind_escape_and_exit_leave_no_stale_binding() {
        let Some(ipc) = SwayIpc::from_env() else {
            eprintln!("skip: no Sway session ($SWAYSOCK)");
            return;
        };
        let bound = |spec: &str| {
            let ok = ipc.run(&format!("unbindsym {spec}")).is_ok();
            ok // a probe that succeeds also removed it
        };
        let sock = std::env::temp_dir().join("cantrip-test.sock");
        let mut keys = SwayKeys::new(ipc.clone(), &sock).unwrap();
        let mut d = Chord::platform_default();
        assert_eq!(
            keys.register(&d).unwrap(),
            HotkeyMode::PressRelease,
            "hold-to-talk + tap-to-toggle"
        );
        d.key = "D".into();
        keys.register(&d).unwrap();
        assert!(!bound("Control+Mod1+space"), "old press binding removed");
        assert!(
            !bound("--release Control+Mod1+space"),
            "old release removed"
        );
        keys.set_escape(true);
        assert!(
            bound("Escape"),
            "positive control: the probe sees a binding"
        );
        keys.set_escape(false);
        keys.set_escape(true);
        keys.set_escape(false);
        assert!(!bound("Escape"), "Escape unbound when the take ends");
        keys.set_escape(true);
        drop(keys);
        for spec in ["Escape", "Control+Mod1+d", "--release Control+Mod1+d"] {
            assert!(!bound(spec), "{spec} left bound after exit");
        }
    }

    #[test]
    fn windows_are_found_by_pid_and_title() {
        let tree = serde_json::json!({"id": 1, "nodes": [{"id": 2, "nodes": [],
            "floating_nodes": [{"id": 7, "pid": 42, "name": "Cantrip HUD",
            "rect": {"x": 0, "y": 23, "width": 75, "height": 50}}]}]});
        assert_eq!(
            find_window(&tree, 42, "Cantrip HUD"),
            Some((7, 0, 23, 75, 50))
        );
        assert_eq!(find_window(&tree, 43, "Cantrip HUD"), None);
        assert_eq!(find_window(&tree, 42, "Cantrip"), None);
    }

    #[test]
    fn exec_paths_must_survive_sway_parsing() {
        assert!(exec_safe("/usr/bin/cantrip"));
        assert!(exec_safe("/run/user/1000/cantrip-v2.sock"));
        assert!(!exec_safe("/home/a b/cantrip"));
        assert!(!exec_safe("/x;y"));
        assert!(!exec_safe("/x,y"));
    }
}
