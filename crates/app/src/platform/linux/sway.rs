//! Sway (wlroots) compositor IPC: the native global shortcut on Sway, where
//! `xdg-desktop-portal-wlr` exports no `GlobalShortcuts` portal.
//!
//! Speaks the i3/Sway IPC protocol on `$SWAYSOCK` directly (no `swaymsg`):
//! `"i3-ipc"` + payload length (u32) + message type (u32), native endian.
//!
//! [`SwayKeys`] binds the chord's press (`bindsym --no-repeat`) and release
//! (`bindsym --release`) to Sway's no-op command with a marker,
//! `nop cantrip-hotkey-down|up`, and plain Escape to `nop cantrip-cancel` only
//! while a take is active. A press therefore spawns nothing: one persistent
//! `SUBSCRIBE ["binding", "workspace", "shutdown"]` connection receives the
//! `binding` event Sway emits for every binding it runs, and a listener
//! thread hands the marker straight to the core's key channel.
//!
//! `swaymsg reload` drops every runtime binding and then emits a `workspace`
//! event with `change: "reload"`; on it the listener re-checks the reloaded
//! config and re-installs the chord (and Escape, during a take). A lost
//! subscription is reconnected and re-asserted the same way.
//!
//! Bindings in the user's Sway config are never overwritten or removed. The
//! config is read as Sway loads it: the main file (the text `GET_CONFIG`
//! returns, located through the compositor's `-c` argument or Sway's search
//! path) plus every `include`d file, resolved like Sway does (`$variables`,
//! `~`, environment variables and globs, relative to the including file's
//! directory, each file once). A chord the config binds is reported as taken;
//! an Escape the config binds is left to the config. Cantrip only ever
//! `unbindsym`s a binding it installed itself (never at startup).

use crate::ipc::{KeyLine, KeyRelay};
use crate::platform::HotkeyMode;
use cantrip_core::shortcut::{Chord, KeyEvent};
use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const MAGIC: &[u8; 6] = b"i3-ipc";
const RUN_COMMAND: u32 = 0;
const SUBSCRIBE: u32 = 2;
const GET_TREE: u32 = 4;
const GET_VERSION: u32 = 7;
const GET_CONFIG: u32 = 9;
const EVENT_WORKSPACE: u32 = 0x8000_0000;
const EVENT_BINDING: u32 = 0x8000_0005;
const EVENT_SHUTDOWN: u32 = 0x8000_0006;
const IO_TIMEOUT: Duration = Duration::from_secs(2);
/// Wait between attempts to re-subscribe after the event connection broke.
const RESUBSCRIBE: Duration = Duration::from_millis(500);
/// Largest IPC message accepted (a GET_TREE of a busy session is far below).
const MAX_MESSAGE: usize = 64 << 20;

/// The binding commands Cantrip installs (Sway's `nop` runs nothing).
pub const NOP_DOWN: &str = "nop cantrip-hotkey-down";
pub const NOP_UP: &str = "nop cantrip-hotkey-up";
pub const NOP_CANCEL: &str = "nop cantrip-cancel";

/// One Sway IPC endpoint (a fresh connection per request: requests are rare).
#[derive(Clone, Debug)]
pub struct SwayIpc {
    path: PathBuf,
}

fn write_msg(s: &mut UnixStream, kind: u32, payload: &str) -> io::Result<()> {
    let mut msg = Vec::with_capacity(14 + payload.len());
    msg.extend_from_slice(MAGIC);
    msg.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
    msg.extend_from_slice(&kind.to_ne_bytes());
    msg.extend_from_slice(payload.as_bytes());
    s.write_all(&msg)
}

/// One message: (type, body).
fn read_msg(s: &mut UnixStream) -> io::Result<(u32, Vec<u8>)> {
    let mut head = [0u8; 14];
    s.read_exact(&mut head)?;
    if &head[..6] != MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not sway ipc"));
    }
    let len = u32::from_ne_bytes([head[6], head[7], head[8], head[9]]) as usize;
    let kind = u32::from_ne_bytes([head[10], head[11], head[12], head[13]]);
    if len > MAX_MESSAGE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "oversized"));
    }
    let mut body = vec![0u8; len];
    s.read_exact(&mut body)?;
    Ok((kind, body))
}

impl SwayIpc {
    /// `$SWAYSOCK`, if set and answering.
    pub fn from_env() -> Option<SwayIpc> {
        let path = std::env::var_os("SWAYSOCK").filter(|p| !p.is_empty())?;
        let ipc = SwayIpc { path: path.into() };
        ipc.request(GET_VERSION, "").ok()?;
        Some(ipc)
    }

    fn connect(&self) -> io::Result<UnixStream> {
        let s = UnixStream::connect(&self.path)?;
        s.set_read_timeout(Some(IO_TIMEOUT))?;
        s.set_write_timeout(Some(IO_TIMEOUT))?;
        Ok(s)
    }

    fn request(&self, kind: u32, payload: &str) -> io::Result<String> {
        let mut s = self.connect()?;
        write_msg(&mut s, kind, payload)?;
        let (_, body) = read_msg(&mut s)?;
        String::from_utf8(body).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))
    }

    /// A connection subscribed to `events` (JSON array), blocking reads.
    pub fn subscribe(&self, events: &str) -> io::Result<UnixStream> {
        let mut s = self.connect()?;
        write_msg(&mut s, SUBSCRIBE, events)?;
        let (_, body) = read_msg(&mut s)?;
        let v: serde_json::Value = serde_json::from_slice(&body)?;
        if v.get("success").and_then(|b| b.as_bool()) != Some(true) {
            return Err(io::Error::other("Sway refused the subscription"));
        }
        s.set_read_timeout(None)?;
        Ok(s)
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

    /// The loaded main config text (`GET_CONFIG`; included files are not
    /// part of it).
    pub fn config(&self) -> Result<String, String> {
        let reply = self.request(GET_CONFIG, "").map_err(|e| e.to_string())?;
        let v: serde_json::Value = serde_json::from_str(&reply).map_err(|e| e.to_string())?;
        Ok(v.get("config")
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .to_string())
    }

    /// The compositor's process id (the socket peer).
    fn peer_pid(&self) -> Option<u32> {
        let s = self.connect().ok()?;
        let mut cred = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: SO_PEERCRED fills one ucred on a connected Unix socket.
        let rc = unsafe {
            libc::getsockopt(
                s.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast(),
                &mut len,
            )
        };
        (rc == 0 && cred.pid > 0).then_some(cred.pid as u32)
    }

    /// Every binding in the user's config (main file plus includes).
    pub fn user_bindings(&self) -> Result<UserBindings, String> {
        let text = self.config()?;
        let proc_ = self.peer_pid().map(|p| PathBuf::from(format!("/proc/{p}")));
        let env = proc_
            .as_ref()
            .and_then(|d| std::fs::read(d.join("environ")).ok())
            .map(|b| parse_environ(&b))
            .unwrap_or_else(|| std::env::vars().collect());
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        let cmdline = proc_
            .as_ref()
            .and_then(|d| std::fs::read(d.join("cmdline")).ok())
            .unwrap_or_default();
        let cwd = proc_
            .as_ref()
            .and_then(|d| std::fs::read_link(d.join("cwd")).ok());
        let main = main_config_path(&cmdline, cwd.as_deref(), &get);
        Ok(UserBindings::parse(&text, main.as_deref(), &get))
    }
}

fn parse_environ(b: &[u8]) -> Vec<(String, String)> {
    b.split(|c| *c == 0)
        .filter_map(|kv| {
            let kv = String::from_utf8_lossy(kv);
            let (k, v) = kv.split_once('=')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

/// The main config file Sway loaded: its `-c`/`--config` argument (relative
/// to its working directory), else the first file on Sway's search path.
pub fn main_config_path(
    cmdline: &[u8],
    cwd: Option<&Path>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<PathBuf> {
    let args: Vec<String> = cmdline
        .split(|c| *c == 0)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    let mut given = None;
    for (i, a) in args.iter().enumerate().skip(1) {
        let v = match a.as_str() {
            "-c" | "--config" => args.get(i + 1).cloned(),
            _ => a
                .strip_prefix("--config=")
                .or_else(|| a.strip_prefix("-c").filter(|v| !v.is_empty()))
                .map(str::to_string),
        };
        if v.is_some() {
            given = v;
        }
    }
    if let Some(g) = given {
        let p = PathBuf::from(g);
        return Some(match (p.is_relative(), cwd) {
            (true, Some(c)) => c.join(p),
            _ => p,
        });
    }
    let home = env("HOME").unwrap_or_default();
    let xdg = env("XDG_CONFIG_HOME")
        .filter(|x| !x.is_empty())
        .unwrap_or_else(|| format!("{home}/.config"));
    [
        format!("{home}/.sway/config"),
        format!("{xdg}/sway/config"),
        format!("{home}/.i3/config"),
        format!("{xdg}/i3/config"),
        "/etc/sway/config".to_string(),
        "/etc/i3/config".to_string(),
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|p| p.is_file())
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

/// Standard (evdev + 8) keycode of a keysym name, for `bindcode` lines.
fn keycode_of(keysym: &str) -> Option<u32> {
    const ROWS: [(&str, u32); 4] = [
        ("1234567890", 10),
        ("qwertyuiop", 24),
        ("asdfghjkl", 38),
        ("zxcvbnm", 52),
    ];
    let k = keysym.to_ascii_lowercase();
    let mut cs = k.chars();
    if let (Some(c), None) = (cs.next(), cs.next()) {
        for (row, first) in ROWS {
            if let Some(i) = row.find(c) {
                return Some(first + i as u32);
            }
        }
    }
    if let Some(n) = k.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()) {
        return match n {
            1..=10 => Some(66 + n),
            11 => Some(95),
            12 => Some(96),
            _ => None,
        };
    }
    Some(match k.as_str() {
        "escape" => 9,
        "minus" => 20,
        "equal" => 21,
        "backspace" => 22,
        "tab" => 23,
        "bracketleft" => 34,
        "bracketright" => 35,
        "return" => 36,
        "semicolon" => 47,
        "apostrophe" => 48,
        "grave" => 49,
        "backslash" => 51,
        "comma" => 59,
        "period" => 60,
        "slash" => 61,
        "space" => 65,
        "home" => 110,
        "up" => 111,
        "prior" => 112,
        "left" => 113,
        "right" => 114,
        "end" => 115,
        "down" => 116,
        "next" => 117,
        "insert" => 118,
        "delete" => 119,
        _ => return None,
    })
}

/// Normalized (sorted modifiers, lower-case key) form of a binding combo, so
/// `Ctrl+Alt+space`, `$mod+…` (after expansion) and `Mod1+Control+Space`
/// compare equal. `bindcode` keys are `code:N`.
fn normalize(combo: &str, code: bool) -> String {
    let mut mods: Vec<&str> = Vec::new();
    let mut key = String::new();
    let parts: Vec<&str> = combo.split('+').collect();
    for (i, p) in parts.iter().enumerate() {
        let m = match p.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => Some("control"),
            "alt" | "mod1" => Some("mod1"),
            "shift" => Some("shift"),
            "caps" | "lock" => Some("lock"),
            "mod4" | "super" | "logo" => Some("mod4"),
            "mod2" => Some("mod2"),
            "mod3" => Some("mod3"),
            "mod5" => Some("mod5"),
            _ => None,
        };
        match m {
            Some(m) if i + 1 < parts.len() => mods.push(m),
            _ if code => key = format!("code:{}", p.trim()),
            _ => key = p.to_ascii_lowercase(),
        }
    }
    mods.sort_unstable();
    mods.dedup();
    mods.push(&key);
    mods.join("+")
}

/// The default-mode key bindings of a Sway config, includes resolved.
#[derive(Debug, Default)]
pub struct UserBindings {
    combos: HashSet<String>,
    /// Every file read (the main file first, when known).
    pub files: Vec<PathBuf>,
}

impl UserBindings {
    /// Parse `main` (the loaded main config text, located at `main_path`)
    /// and every file it includes; `env` is the compositor's environment.
    pub fn parse(
        main: &str,
        main_path: Option<&Path>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> UserBindings {
        let dir = main_path
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| {
                let xdg = env("XDG_CONFIG_HOME")
                    .filter(|x| !x.is_empty())
                    .unwrap_or_else(|| format!("{}/.config", env("HOME").unwrap_or_default()));
                PathBuf::from(xdg).join("sway")
            });
        let mut scan = Scan {
            vars: Vec::new(),
            seen: HashSet::new(),
            out: UserBindings::default(),
            env,
        };
        if let Some(p) = main_path {
            let c = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
            scan.seen.insert(c.clone());
            scan.out.files.push(c);
        }
        scan.text(main, &dir, 0);
        scan.out
    }

    /// Whether `combo` (a `bindsym` combo) is bound in the default mode,
    /// by `bindsym` or by the `bindcode` of its standard keycode.
    pub fn binds(&self, combo: &str) -> bool {
        if self.combos.contains(&normalize(combo, false)) {
            return true;
        }
        let (mods, key) = combo.rsplit_once('+').unwrap_or(("", combo));
        keycode_of(key).is_some_and(|k| {
            let c = if mods.is_empty() {
                k.to_string()
            } else {
                format!("{mods}+{k}")
            };
            self.combos.contains(&normalize(&c, true))
        })
    }
}

/// Whether a Sway config text alone (no includes) binds `combo` in the
/// default mode.
#[cfg(test)]
fn config_binds(config: &str, combo: &str) -> bool {
    UserBindings::parse(config, None, &|_| None).binds(combo)
}

/// Include nesting / file count limits (Sway itself refuses cycles).
const MAX_INCLUDE_DEPTH: usize = 16;
const MAX_FILES: usize = 256;

struct Scan<'a> {
    /// `set $name value`, longest names first (`$mod2` never expands as `$mod`).
    vars: Vec<(String, String)>,
    /// Canonical paths already read (Sway includes each file once).
    seen: HashSet<PathBuf>,
    out: UserBindings,
    env: &'a dyn Fn(&str) -> Option<String>,
}

impl Scan<'_> {
    fn expand_vars(&self, s: &str) -> String {
        let mut s = s.to_string();
        for (n, v) in &self.vars {
            s = s.replace(n.as_str(), v);
        }
        s
    }

    fn text(&mut self, text: &str, dir: &Path, depth: usize) {
        // Join `\` line continuations first (Sway concatenates them as-is).
        let mut lines: Vec<String> = Vec::new();
        let mut cont = String::new();
        for raw in text.lines() {
            let t = raw.trim_end();
            if let Some(head) = t.strip_suffix('\\') {
                cont.push_str(head);
                continue;
            }
            cont.push_str(t);
            lines.push(std::mem::take(&mut cont));
        }
        if !cont.is_empty() {
            lines.push(cont);
        }
        // Block prefixes (`mode "x" {`, `bindsym --release {`): Sway prepends
        // them to every line inside the block.
        let mut blocks: Vec<String> = Vec::new();
        let mut prev: Option<String> = None;
        for line in lines {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.starts_with('}') {
                blocks.pop();
                prev = None;
                continue;
            }
            if line == "{" {
                blocks.push(prev.take().unwrap_or_default());
                continue;
            }
            if let Some(head) = line.strip_suffix('{') {
                blocks.push(head.trim().to_string());
                prev = None;
                continue;
            }
            prev = Some(line.to_string());
            let full = if blocks.is_empty() {
                line.to_string()
            } else {
                format!("{} {line}", blocks.join(" "))
            };
            self.command(&full, dir, depth);
        }
    }

    fn command(&mut self, line: &str, dir: &Path, depth: usize) {
        let tokens = words(line);
        let mut t: &[String] = &tokens;
        // `mode default …` is the default mode; any other mode never collides.
        if t.first().map(String::as_str) == Some("mode") {
            let rest: Vec<&String> = t[1..].iter().filter(|w| !w.starts_with("--")).collect();
            if rest.first().map(|w| w.as_str()) != Some("default") {
                return;
            }
            let skip = t[1..]
                .iter()
                .position(|w| w == "default")
                .map_or(t.len(), |i| i + 2);
            t = &t[skip.min(t.len())..];
        }
        match t.first().map(String::as_str) {
            Some("set") if t.len() >= 3 && t[1].starts_with('$') => {
                let value = self.expand_vars(&t[2..].join(" "));
                self.vars.retain(|(n, _)| n != &t[1]);
                self.vars.push((t[1].clone(), value));
                self.vars.sort_by_key(|(n, _)| std::cmp::Reverse(n.len()));
            }
            Some(kind @ ("bindsym" | "bindcode")) => {
                if let Some(c) = t[1..].iter().find(|w| !w.starts_with("--")) {
                    let c = self.expand_vars(c);
                    self.out.combos.insert(normalize(&c, kind == "bindcode"));
                }
            }
            Some("include") if t.len() >= 2 => {
                let pattern = self.expand_vars(&t[1..].join(" "));
                self.include(&pattern, dir, depth);
            }
            _ => {}
        }
    }

    fn include(&mut self, pattern: &str, dir: &Path, depth: usize) {
        if depth >= MAX_INCLUDE_DEPTH {
            return;
        }
        for word in words(pattern) {
            let word = expand_shell(&word, self.env);
            for path in glob_paths(&word, dir) {
                if self.out.files.len() >= MAX_FILES {
                    return;
                }
                let Ok(canon) = std::fs::canonicalize(&path) else {
                    continue;
                };
                if !canon.is_file() || !self.seen.insert(canon.clone()) {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&canon) else {
                    continue;
                };
                self.out.files.push(canon.clone());
                let sub = canon.parent().map(Path::to_path_buf).unwrap_or_default();
                self.text(&text, &sub, depth + 1);
            }
        }
    }
}

/// Whitespace-separated words; double or single quotes group (and are
/// removed), as Sway's argument splitting does.
fn words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut any = false;
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                any = true;
            }
            (None, c) if c.is_whitespace() => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                any = false;
            }
            (None, c) => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// `~`, `$NAME` and `${NAME}` expansion (the parts of `wordexp(3)` a config
/// path uses); unknown variables expand to nothing, like the shell.
fn expand_shell(word: &str, env: &dyn Fn(&str) -> Option<String>) -> String {
    let mut s = word.to_string();
    if s == "~" || s.starts_with("~/") {
        s = format!("{}{}", env("HOME").unwrap_or_default(), &s[1..]);
    }
    let mut out = String::new();
    let mut cs = s.chars().peekable();
    while let Some(c) = cs.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let mut name = String::new();
        if cs.peek() == Some(&'{') {
            cs.next();
            for c in cs.by_ref() {
                if c == '}' {
                    break;
                }
                name.push(c);
            }
        } else {
            while let Some(&c) = cs.peek() {
                if c.is_ascii_alphanumeric() || c == '_' {
                    name.push(c);
                    cs.next();
                } else {
                    break;
                }
            }
        }
        if name.is_empty() {
            out.push('$');
        } else {
            out.push_str(&env(&name).unwrap_or_default());
        }
    }
    out
}

/// The files `pattern` names, relative to `dir` (`glob(3)`, sorted). A
/// pattern without matches stands for itself (like `wordexp`).
fn glob_paths(pattern: &str, dir: &Path) -> Vec<PathBuf> {
    let full = if Path::new(pattern).is_absolute() {
        pattern.to_string()
    } else {
        let mut d = String::new();
        for c in dir.to_string_lossy().chars() {
            if matches!(c, '*' | '?' | '[' | '\\') {
                d.push('\\');
            }
            d.push(c);
        }
        format!("{d}/{pattern}")
    };
    let Ok(c) = CString::new(full.clone()) else {
        return Vec::new();
    };
    // SAFETY: glob(3) on a valid C string into a zeroed glob_t, freed below.
    unsafe {
        let mut g: libc::glob_t = std::mem::zeroed();
        let rc = libc::glob(c.as_ptr(), 0, None, &mut g);
        let mut out = Vec::new();
        if rc == 0 {
            for i in 0..g.gl_pathc {
                let p = *g.gl_pathv.add(i);
                if !p.is_null() {
                    out.push(PathBuf::from(
                        CStr::from_ptr(p).to_string_lossy().into_owned(),
                    ));
                }
            }
        }
        libc::globfree(&mut g);
        if rc == libc::GLOB_NOMATCH {
            out.push(PathBuf::from(full));
        }
        out
    }
}

/// The key line a `binding` event carries, if it is one of Cantrip's.
pub fn binding_line(body: &[u8]) -> Option<KeyLine> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    if v.get("change").and_then(|c| c.as_str()) != Some("run") {
        return None;
    }
    match v.pointer("/binding/command")?.as_str()?.trim() {
        NOP_DOWN => Some(KeyLine::Down),
        NOP_UP => Some(KeyLine::Up),
        NOP_CANCEL => Some(KeyLine::Cancel),
        _ => None,
    }
}

/// Whether a `workspace` event is Sway's "config reloaded".
fn is_reload(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("change")
                .and_then(|c| c.as_str())
                .map(|c| c == "reload")
        })
        .unwrap_or(false)
}

/// What Cantrip has bound (and wants bound) on this Sway session.
struct Bindings {
    ipc: SwayIpc,
    /// The registered chord's combo (what should be bound).
    chord: Option<String>,
    /// Cantrip's own press and release bindings for `chord` are installed.
    chord_owned: bool,
    /// A take is active: Escape should cancel.
    escape_wanted: bool,
    /// Cantrip's own Escape binding is installed (only then is it removed).
    escape_owned_by_cantrip: bool,
    /// The backend was dropped (graceful exit): install nothing more.
    closed: bool,
}

impl Bindings {
    fn user(&self) -> UserBindings {
        self.ipc.user_bindings().unwrap_or_default()
    }

    fn install_chord(&self, c: &str) -> Result<(), String> {
        self.ipc
            .run(&format!("bindsym --no-repeat {c} {NOP_DOWN}"))?;
        if let Err(e) = self.ipc.run(&format!("bindsym --release {c} {NOP_UP}")) {
            let _ = self.ipc.run(&format!("unbindsym {c}"));
            return Err(e);
        }
        Ok(())
    }

    /// Remove Cantrip's chord bindings (only if Cantrip installed them and a
    /// reloaded config has not since taken the combo).
    fn remove_chord(&mut self, user: &UserBindings) {
        if !std::mem::take(&mut self.chord_owned) {
            return;
        }
        if let Some(c) = &self.chord {
            if !user.binds(c) {
                let _ = self.ipc.run(&format!("unbindsym {c}"));
                let _ = self.ipc.run(&format!("unbindsym --release {c}"));
            }
        }
    }

    fn bind_escape(&mut self, user: &UserBindings) {
        if user.binds("Escape") {
            eprintln!("[cantrip] escape bind skipped: the Sway config binds plain Escape");
            return;
        }
        match self
            .ipc
            .run(&format!("bindsym --no-repeat Escape {NOP_CANCEL}"))
        {
            Ok(()) => self.escape_owned_by_cantrip = true,
            Err(_) => eprintln!("[cantrip] escape bind unavailable on this Sway session"),
        }
    }

    fn unbind_escape(&mut self, user: &UserBindings) {
        if std::mem::take(&mut self.escape_owned_by_cantrip) && !user.binds("Escape") {
            let _ = self.ipc.run("unbindsym Escape");
        }
    }

    /// After `reload` (which dropped every runtime binding) or a lost event
    /// connection: install the chord and, during a take, Escape again.
    fn reassert(&mut self, why: &str) {
        if self.closed {
            return;
        }
        self.chord_owned = false;
        self.escape_owned_by_cantrip = false;
        let user = self.user();
        if let Some(c) = self.chord.clone() {
            if user.binds(&c) {
                eprintln!(
                    "[cantrip] shortcut not restored after {why}: the Sway config now binds it"
                );
            } else {
                match self.install_chord(&c) {
                    Ok(()) => {
                        self.chord_owned = true;
                        eprintln!("[cantrip] shortcut restored after {why}");
                    }
                    Err(e) => eprintln!("[cantrip] shortcut not restored after {why}: {e}"),
                }
            }
        }
        if self.escape_wanted {
            self.bind_escape(&user);
        }
    }
}

type Sink = Arc<Mutex<Option<Sender<KeyEvent>>>>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The global shortcut and Escape-during-a-take through Sway bindings.
pub struct SwayKeys {
    state: Arc<Mutex<Bindings>>,
    sink: Sink,
}

impl SwayKeys {
    /// The Sway backend for this session: subscribes to binding and reload
    /// events and starts the listener. Binds and unbinds nothing yet.
    pub fn new(ipc: SwayIpc) -> Result<SwayKeys, String> {
        let events = ipc.subscribe(EVENTS).map_err(|e| e.to_string())?;
        let state = Arc::new(Mutex::new(Bindings {
            ipc,
            chord: None,
            chord_owned: false,
            escape_wanted: false,
            escape_owned_by_cantrip: false,
            closed: false,
        }));
        let sink: Sink = Arc::default();
        let (st, sk) = (state.clone(), sink.clone());
        std::thread::Builder::new()
            .name("cantrip-sway-events".into())
            .spawn(move || listen(events, st, sk))
            .map_err(|e| e.to_string())?;
        Ok(SwayKeys { state, sink })
    }

    /// Bind `chord`'s press and release (replacing the previous chord);
    /// its events go to `sink`.
    pub fn register(
        &mut self,
        chord: &Chord,
        sink: Sender<KeyEvent>,
    ) -> Result<HotkeyMode, String> {
        *lock(&self.sink) = Some(sink);
        let c = combo(chord).ok_or_else(|| format!("Unsupported key {}", chord.key))?;
        let mut st = lock(&self.state);
        if st.chord.as_deref() == Some(c.as_str()) && st.chord_owned {
            return Ok(HotkeyMode::PressRelease);
        }
        let user = st.user();
        if user.binds(&c) {
            return Err(format!(
                "{} is already used by another app",
                chord.display_linux()
            ));
        }
        st.remove_chord(&user);
        st.install_chord(&c)?;
        st.chord = Some(c);
        st.chord_owned = true;
        Ok(HotkeyMode::PressRelease)
    }

    /// Bind plain Escape to cancel while a take is active; unbind otherwise
    /// (only Cantrip's own binding, never the config's).
    pub fn set_escape(&mut self, active: bool) {
        let mut st = lock(&self.state);
        st.escape_wanted = active;
        if active == st.escape_owned_by_cantrip {
            return;
        }
        let user = st.user();
        if active {
            st.bind_escape(&user);
        } else {
            st.unbind_escape(&user);
        }
    }
}

impl Drop for SwayKeys {
    /// Graceful exit: leave none of Cantrip's bindings behind.
    fn drop(&mut self) {
        let mut st = lock(&self.state);
        st.closed = true;
        st.escape_wanted = false;
        let user = st.user();
        st.unbind_escape(&user);
        st.remove_chord(&user);
    }
}

const EVENTS: &str = r#"["binding","workspace","shutdown"]"#;

/// The event listener: Cantrip's binding markers go straight to the key
/// channel; a reload (or a lost connection) re-asserts the bindings.
fn listen(mut events: UnixStream, state: Arc<Mutex<Bindings>>, sink: Sink) {
    let mut relay = KeyRelay::default();
    loop {
        match read_msg(&mut events) {
            Ok((EVENT_BINDING, body)) => {
                let now = Instant::now();
                if let Some(ev) = binding_line(&body).and_then(|l| relay.event(l, now)) {
                    if let Some(s) = lock(&sink).as_ref() {
                        let _ = s.send(ev);
                    }
                }
            }
            Ok((EVENT_WORKSPACE, body)) if is_reload(&body) => {
                relay = KeyRelay::default();
                lock(&state).reassert("swaymsg reload");
            }
            Ok((EVENT_SHUTDOWN, _)) => return,
            Ok(_) => {}
            Err(_) => loop {
                std::thread::sleep(RESUBSCRIBE);
                let ipc = {
                    let st = lock(&state);
                    if st.closed {
                        return;
                    }
                    st.ipc.clone()
                };
                if let Ok(s) = ipc.subscribe(EVENTS) {
                    events = s;
                    relay = KeyRelay::default();
                    lock(&state).reassert("a lost Sway connection");
                    break;
                }
            },
        }
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

    #[test]
    fn config_flags_blocks_bindcode_and_continuations() {
        let flagged = "bindsym --locked --no-repeat --inhibited Ctrl+Alt+space exec a\n";
        assert!(config_binds(flagged, "Control+Mod1+space"));
        assert!(config_binds(
            "bindsym --release --whole-window Escape nop\n",
            "Escape"
        ));
        assert!(config_binds(
            "bindsym --input-device=1:1:kbd Super+F9 exec x\n",
            "Mod4+F9"
        ));
        // `bindcode` with the standard keycodes of space (65) and Escape (9).
        assert!(config_binds(
            "bindcode Control+Mod1+65 exec x\n",
            "Control+Mod1+space"
        ));
        assert!(config_binds("bindcode --release 9 exec x\n", "Escape"));
        assert!(!config_binds(
            "bindcode Control+65 exec x\n",
            "Control+Mod1+space"
        ));
        // A `bindsym { … }` block and a line continuation.
        let block = "bindsym {\n  Mod4+a exec a\n  Escape exec b\n}\n\
                     bindsym Control+Mod1+\\\nspace \\\n  exec c\n";
        assert!(config_binds(block, "Escape"));
        assert!(config_binds(block, "Mod4+a"));
        assert!(config_binds(block, "Control+Mod1+space"));
        // A mode block with the brace on its own line, and `mode default`.
        let modes = "mode \"x\"\n{\n  bindsym Escape mode default\n}\n\
                     mode default bindsym Mod4+z exec z\n";
        assert!(!config_binds(modes, "Escape"));
        assert!(config_binds(modes, "Mod4+z"));
        // Nested blocks close in order.
        let nested = "bar {\n  colors {\n    background #000000\n  }\n}\nbindsym Escape nop\n";
        assert!(config_binds(nested, "Escape"));
        // Indented comments and `$vars` set from other `$vars`.
        let vars =
            "set $a Mod1\nset $chord Control+$a+space\n  # bindsym Escape x\nbindsym $chord nop\n";
        assert!(config_binds(vars, "Ctrl+Alt+space"));
        assert!(!config_binds(vars, "Escape"));
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cantrip-sway-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn includes_resolve_globs_vars_env_home_and_cycles() {
        let root = tmpdir("inc");
        let cfg = root.join("sway");
        std::fs::create_dir_all(cfg.join("config.d")).unwrap();
        std::fs::create_dir_all(root.join("home/.sway-extra")).unwrap();
        let main = "set $mod Mod4\nset $dir config.d\n\
                    include $dir/*.conf\n\
                    include ~/.sway-extra/keys\n\
                    include $EXTRA_DIR/env.conf\n\
                    include config.d/missing-*.conf\n";
        std::fs::write(cfg.join("config"), main).unwrap();
        // Relative to the including file's own directory; includes itself
        // and the main file (cycles are read once).
        std::fs::write(
            cfg.join("config.d/10-user.conf"),
            "# bindsym Mod4+q kill\nbindsym $mod+Escape exec x\ninclude ../config\ninclude 10-user.conf\ninclude sub/*.conf\n",
        )
        .unwrap();
        std::fs::create_dir_all(cfg.join("config.d/sub")).unwrap();
        std::fs::write(
            cfg.join("config.d/sub/deep.conf"),
            "bindsym --no-repeat Control+Mod1+space exec y\n",
        )
        .unwrap();
        std::fs::write(
            cfg.join("config.d/notes.txt"),
            "bindsym F1 exec ignored-by-glob\n",
        )
        .unwrap();
        std::fs::write(
            root.join("home/.sway-extra/keys"),
            "bindsym Escape exec z\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("env")).unwrap();
        std::fs::write(root.join("env/env.conf"), "bindsym Mod4+F7 exec w\n").unwrap();
        let home = root.join("home").to_string_lossy().into_owned();
        let extra = root.join("env").to_string_lossy().into_owned();
        let env = move |k: &str| match k {
            "HOME" => Some(home.clone()),
            "EXTRA_DIR" => Some(extra.clone()),
            _ => None,
        };
        let u = UserBindings::parse(main, Some(&cfg.join("config")), &env);
        assert!(u.binds("Mod4+Escape"), "glob include with a $var");
        assert!(u.binds("Control+Mod1+space"), "nested relative include");
        assert!(u.binds("Escape"), "~ include");
        assert!(u.binds("Mod4+F7"), "environment variable include");
        assert!(!u.binds("F1"), "glob matched only *.conf");
        assert!(!u.binds("Mod4+q"), "commented in an included file");
        assert_eq!(u.files.len(), 5, "each file once: {:?}", u.files);
        // Without the include the main text alone binds none of them.
        assert!(!config_binds(main, "Escape"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn main_config_comes_from_sway_args_or_search_path() {
        let none = |_: &str| None;
        let cmd = b"sway\0-c\0cfg/my config\0";
        assert_eq!(
            main_config_path(cmd, Some(Path::new("/w")), &none),
            Some(PathBuf::from("/w/cfg/my config"))
        );
        assert_eq!(
            main_config_path(b"sway\0--config=/etc/x\0", None, &none),
            Some(PathBuf::from("/etc/x"))
        );
        assert_eq!(
            main_config_path(b"sway\0-c/abs\0-d\0", None, &none),
            Some(PathBuf::from("/abs"))
        );
        let root = tmpdir("search");
        std::fs::create_dir_all(root.join("x/sway")).unwrap();
        std::fs::write(root.join("x/sway/config"), "").unwrap();
        let x = root.join("x").to_string_lossy().into_owned();
        let h = root.to_string_lossy().into_owned();
        let env = move |k: &str| match k {
            "XDG_CONFIG_HOME" => Some(x.clone()),
            "HOME" => Some(h.clone()),
            _ => None,
        };
        assert_eq!(
            main_config_path(b"sway\0", None, &env),
            Some(root.join("x/sway/config"))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn shell_words_and_expansion() {
        assert_eq!(
            words(r#"include "a b/*.conf" c"#),
            ["include", "a b/*.conf", "c"]
        );
        let env = |k: &str| (k == "HOME").then(|| "/h".to_string());
        assert_eq!(expand_shell("~/x/${HOME}/$NOPE/$", &env), "/h/x//h//$");
        assert_eq!(expand_shell("a~b", &env), "a~b");
    }

    #[test]
    fn binding_events_map_to_key_lines() {
        let ev = |cmd: &str| {
            format!(r#"{{"change":"run","binding":{{"command":"{cmd}","symbol":"space"}}}}"#)
        };
        assert_eq!(binding_line(ev(NOP_DOWN).as_bytes()), Some(KeyLine::Down));
        assert_eq!(binding_line(ev(NOP_UP).as_bytes()), Some(KeyLine::Up));
        assert_eq!(
            binding_line(ev(NOP_CANCEL).as_bytes()),
            Some(KeyLine::Cancel)
        );
        assert_eq!(binding_line(ev("exec foot").as_bytes()), None);
        assert_eq!(
            binding_line(ev("nop cantrip-hotkey-downx").as_bytes()),
            None
        );
        assert!(is_reload(
            br#"{"change":"reload","old":null,"current":null}"#
        ));
        assert!(!is_reload(br#"{"change":"focus"}"#));
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
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut keys = SwayKeys::new(ipc.clone()).unwrap();
        let mut d = Chord::platform_default();
        assert_eq!(
            keys.register(&d, tx.clone()).unwrap(),
            HotkeyMode::PressRelease,
            "hold-to-talk + tap-to-toggle"
        );
        d.key = "D".into();
        keys.register(&d, tx).unwrap();
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

    /// Live (on a Sway session whose config binds neither the chord nor
    /// Escape; skipped elsewhere): `reload` drops runtime bindings and the
    /// listener re-installs the chord (and Escape during a take) on its
    /// `workspace` "reload" event; idle, a reload brings back no Escape.
    #[test]
    #[ignore]
    fn live_sway_reload_reinstalls_cantrips_bindings() {
        let Some(ipc) = SwayIpc::from_env() else {
            eprintln!("skip: no Sway session ($SWAYSOCK)");
            return;
        };
        let bound = |spec: &str| ipc.run(&format!("unbindsym {spec}")).is_ok();
        let wait = || std::thread::sleep(Duration::from_millis(400));
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut keys = SwayKeys::new(ipc.clone()).unwrap();
        keys.register(&Chord::platform_default(), tx).unwrap();
        keys.set_escape(true);
        ipc.run("reload").unwrap();
        wait();
        assert!(bound("Escape"), "Escape re-installed during a take");
        assert!(bound("Control+Mod1+space"), "press re-installed");
        assert!(
            bound("--release Control+Mod1+space"),
            "release re-installed"
        );
        keys.set_escape(false);
        ipc.run("reload").unwrap();
        wait();
        assert!(!bound("Escape"), "idle: no Escape after reload");
        assert!(bound("Control+Mod1+space"), "press re-installed again");
        drop(keys);
        ipc.run("reload").unwrap();
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
}
