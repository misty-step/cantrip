//! Linux clipboard with identity-based change detection
//! (`cantrip_core::ports::Clipboard`).
//!
//! - **X11 (and XWayland fallback).** Cantrip owns the `CLIPBOARD` selection
//!   from a hidden window served by a dedicated thread. A [`ClipToken`] is a
//!   write generation; "unchanged since Cantrip wrote" means *we still own the
//!   selection for that generation* (`GetSelectionOwner` is authoritative) —
//!   never a text comparison, so a concurrent copy of the same text counts as
//!   a change. Snapshots read every target from the current owner (INCR
//!   included, bounded in time and size) and restore by serving them back.
//!   Transient writes add `x-kde-passwordManagerHint: secret` (Klipper).
//!   X11 limitation: data Cantrip serves disappears when Cantrip quits unless
//!   a clipboard manager adopts it.
//! - **Native Wayland with `wl-copy`/`wl-paste`.** Each write is one
//!   `wl-copy --foreground` child, which exits as soon as anything replaces
//!   the selection: alive ⇔ unchanged. `wl-copy` serves one MIME type, so a
//!   restore puts back the snapshot's best single type (image over text) —
//!   a documented Wayland limitation; transient marking is unavailable.
//!   Without those tools Cantrip falls back to the XWayland X11 clipboard,
//!   which compositors bridge to Wayland apps.

use super::x11::{self, X11};
use cantrip_core::ports::{ClipSnapshot, ClipToken, Clipboard};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux, EventMask,
    PropMode, Property, SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass,
    SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::wrapper::ConnectionExt as _;

/// Whole-snapshot and per-target read budgets.
const SNAPSHOT_BUDGET: Duration = Duration::from_millis(1500);
const TARGET_BUDGET: Duration = Duration::from_millis(400);
/// Largest snapshot Cantrip keeps in memory.
const SNAPSHOT_MAX_BYTES: usize = 64 << 20;
/// Larger values are sent with the INCR protocol.
const INCR_CHUNK: usize = 64 << 10;

/// Targets that are protocol machinery, not clipboard content.
const SKIP_TARGETS: &[&str] = &[
    "TARGETS",
    "MULTIPLE",
    "TIMESTAMP",
    "SAVE_TARGETS",
    "DELETE",
    "INSERT_PROPERTY",
    "INSERT_SELECTION",
];

pub struct LinuxClipboard {
    backend: Backend,
}

enum Backend {
    X11(X11Clip),
    Wl(WlClip),
    None,
}

impl Default for LinuxClipboard {
    fn default() -> Self {
        Self::new()
    }
}

impl LinuxClipboard {
    pub fn new() -> LinuxClipboard {
        let backend =
            if x11::session_is_wayland() && x11::on_path("wl-copy") && x11::on_path("wl-paste") {
                Backend::Wl(WlClip::default())
            } else {
                X11Clip::start().map(Backend::X11).unwrap_or(Backend::None)
            };
        LinuxClipboard { backend }
    }

    /// Which mechanism is in use (logs / release matrix).
    pub fn backend_name(&self) -> &'static str {
        match self.backend {
            Backend::X11(_) => "x11-selection",
            Backend::Wl(_) => "wl-copy",
            Backend::None => "none",
        }
    }
}

impl Clipboard for LinuxClipboard {
    fn snapshot(&self) -> Option<ClipSnapshot> {
        match &self.backend {
            Backend::X11(c) => c.snapshot(),
            Backend::Wl(c) => c.snapshot(),
            Backend::None => None,
        }
    }

    fn write_text(&self, text: &str, transient: bool) -> Result<ClipToken, String> {
        match &self.backend {
            Backend::X11(c) => c.write(text_items(&c.x, text, transient)),
            Backend::Wl(c) => c.write_text(text),
            Backend::None => Err("no clipboard on this session".into()),
        }
    }

    fn unchanged_since(&self, token: &ClipToken) -> bool {
        match &self.backend {
            Backend::X11(c) => c.unchanged_since(token),
            Backend::Wl(c) => c.unchanged_since(token),
            Backend::None => false,
        }
    }

    fn restore(&self, snapshot: &ClipSnapshot, token: &ClipToken) -> bool {
        match &self.backend {
            Backend::X11(c) => c.restore(snapshot, token),
            Backend::Wl(c) => c.restore(snapshot, token),
            Backend::None => false,
        }
    }
}

// ───────────────────────────── X11 ─────────────────────────────

/// One served target: (target atom, property type atom, bytes).
type Item = (Atom, Atom, Vec<u8>);

#[derive(Default)]
struct Served {
    generation: u64,
    /// `None` when Cantrip does not (believe it) own CLIPBOARD.
    items: Option<Vec<Item>>,
}

struct IncrSend {
    ty: Atom,
    data: Vec<u8>,
    offset: usize,
}

struct X11Clip {
    x: Arc<X11>,
    win: Window,
    served: Arc<Mutex<Served>>,
}

fn intern(x: &X11, name: &str) -> Option<Atom> {
    Some(
        x.conn
            .intern_atom(false, name.as_bytes())
            .ok()?
            .reply()
            .ok()?
            .atom,
    )
}

fn atom_name(x: &X11, atom: Atom) -> Option<String> {
    let r = x.conn.get_atom_name(atom).ok()?.reply().ok()?;
    Some(String::from_utf8_lossy(&r.name).into_owned())
}

fn text_items(x: &X11, text: &str, transient: bool) -> Vec<Item> {
    let a = &x.atoms;
    let b = text.as_bytes().to_vec();
    let mut items = vec![
        (a.UTF8_STRING, a.UTF8_STRING, b.clone()),
        (a.TEXT_PLAIN_UTF8, a.TEXT_PLAIN_UTF8, b.clone()),
        (a.TEXT_PLAIN, a.TEXT_PLAIN, b.clone()),
        (a.TEXT, a.UTF8_STRING, b.clone()),
        (u32::from(AtomEnum::STRING), u32::from(AtomEnum::STRING), b),
    ];
    if transient {
        items.push((a.KDE_PASSWORD_HINT, a.KDE_PASSWORD_HINT, b"secret".to_vec()));
    }
    items
}

impl X11Clip {
    fn start() -> Option<X11Clip> {
        let x = X11::connect()?;
        let win = x.conn.generate_id().ok()?;
        x.conn
            .create_window(
                0,
                win,
                x.root,
                -10,
                -10,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                0,
                &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )
            .ok()?
            .check()
            .ok()?;
        let served = Arc::new(Mutex::new(Served::default()));
        let (tx, tserved) = (x.clone(), served.clone());
        std::thread::Builder::new()
            .name("cantrip-clipboard".into())
            .spawn(move || serve_loop(&tx, win, &tserved))
            .ok()?;
        Some(X11Clip { x, win, served })
    }

    fn owner_is_us(&self) -> bool {
        self.x
            .conn
            .get_selection_owner(self.x.atoms.CLIPBOARD)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|r| r.owner == self.win)
    }

    fn write(&self, items: Vec<Item>) -> Result<ClipToken, String> {
        let generation = {
            let mut s = self.served.lock().unwrap_or_else(|p| p.into_inner());
            s.generation += 1;
            s.items = Some(items);
            s.generation
        };
        self.x
            .conn
            .set_selection_owner(self.win, self.x.atoms.CLIPBOARD, x11rb::CURRENT_TIME)
            .map_err(|e| e.to_string())?;
        self.x.flush();
        if self.owner_is_us() {
            Ok(ClipToken(generation))
        } else {
            let mut s = self.served.lock().unwrap_or_else(|p| p.into_inner());
            if s.generation == generation {
                s.items = None;
            }
            Err("clipboard ownership was refused".into())
        }
    }

    fn unchanged_since(&self, token: &ClipToken) -> bool {
        let ours = {
            let s = self.served.lock().unwrap_or_else(|p| p.into_inner());
            s.generation == token.0 && s.items.is_some()
        };
        ours && self.owner_is_us()
    }

    fn snapshot(&self) -> Option<ClipSnapshot> {
        // Our own content (an earlier dictation) is snapshotted from memory.
        {
            let s = self.served.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(items) = &s.items {
                if self.owner_is_us() {
                    let item = items
                        .iter()
                        .filter_map(|(t, _, d)| Some((atom_name(&self.x, *t)?, d.clone())))
                        .collect();
                    return Some(ClipSnapshot { items: vec![item] });
                }
            }
        }
        read_foreign_snapshot()
    }

    fn restore(&self, snapshot: &ClipSnapshot, token: &ClipToken) -> bool {
        if !self.unchanged_since(token) {
            return false;
        }
        let items: Vec<Item> = snapshot
            .items
            .first()
            .map(|item| {
                item.iter()
                    .filter_map(|(name, data)| {
                        let target = intern(&self.x, name)?;
                        let ty = if name == "TEXT" {
                            self.x.atoms.UTF8_STRING
                        } else {
                            target
                        };
                        Some((target, ty, data.clone()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        {
            let mut s = self.served.lock().unwrap_or_else(|p| p.into_inner());
            if s.generation != token.0 || s.items.is_none() {
                return false;
            }
            s.generation += 1;
            s.items = (!items.is_empty()).then_some(items.clone());
        }
        if items.is_empty() {
            // The clipboard was empty before: give the selection up.
            let _ = self.x.conn.set_selection_owner(
                x11rb::NONE,
                self.x.atoms.CLIPBOARD,
                x11rb::CURRENT_TIME,
            );
            self.x.flush();
            return true;
        }
        self.owner_is_us()
    }
}

fn serve_loop(x: &X11, win: Window, served: &Mutex<Served>) {
    let mut incr: HashMap<(Window, Atom), IncrSend> = HashMap::new();
    let max_req = x.conn.maximum_request_bytes();
    let chunk = INCR_CHUNK.min(max_req.saturating_sub(1024).max(4096));
    loop {
        let Ok(ev) = x.conn.wait_for_event() else {
            return; // connection closed
        };
        match ev {
            Event::SelectionRequest(req) => {
                let items = served
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .items
                    .clone();
                let prop = answer(x, &req, items.as_deref(), chunk, &mut incr);
                let notify = SelectionNotifyEvent {
                    response_type: SELECTION_NOTIFY_EVENT,
                    sequence: 0,
                    time: req.time,
                    requestor: req.requestor,
                    selection: req.selection,
                    target: req.target,
                    property: prop,
                };
                let _ = x
                    .conn
                    .send_event(false, req.requestor, EventMask::NO_EVENT, notify);
                x.flush();
            }
            Event::SelectionClear(c) if c.selection == x.atoms.CLIPBOARD => {
                // Stale clears (we re-acquired meanwhile) are ignored: the
                // server's current owner is authoritative.
                let still_ours = x
                    .conn
                    .get_selection_owner(x.atoms.CLIPBOARD)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .is_some_and(|r| r.owner == win);
                if !still_ours {
                    served.lock().unwrap_or_else(|p| p.into_inner()).items = None;
                }
            }
            Event::PropertyNotify(p) if p.state == Property::DELETE => {
                let key = (p.window, p.atom);
                if let Some(t) = incr.get_mut(&key) {
                    let end = (t.offset + chunk).min(t.data.len());
                    let part = &t.data[t.offset..end];
                    let _ =
                        x.conn
                            .change_property8(PropMode::REPLACE, p.window, p.atom, t.ty, part);
                    x.flush();
                    if part.is_empty() {
                        incr.remove(&key);
                    } else {
                        t.offset = end;
                    }
                }
            }
            _ => {}
        }
    }
}

/// Answer one SelectionRequest; returns the property written or NONE.
fn answer(
    x: &X11,
    req: &SelectionRequestEvent,
    items: Option<&[Item]>,
    chunk: usize,
    incr: &mut HashMap<(Window, Atom), IncrSend>,
) -> Atom {
    let a = &x.atoms;
    let Some(items) = items.filter(|_| req.selection == a.CLIPBOARD) else {
        return x11rb::NONE;
    };
    let prop = if req.property == x11rb::NONE {
        req.target
    } else {
        req.property
    };
    if req.target == a.TARGETS {
        let mut targets = vec![a.TARGETS];
        targets.extend(items.iter().map(|i| i.0));
        let ok = x
            .conn
            .change_property32(
                PropMode::REPLACE,
                req.requestor,
                prop,
                AtomEnum::ATOM,
                &targets,
            )
            .is_ok();
        return if ok { prop } else { x11rb::NONE };
    }
    let Some((_, ty, data)) = items.iter().find(|i| i.0 == req.target) else {
        return x11rb::NONE;
    };
    if data.len() > chunk {
        let _ = x.conn.change_window_attributes(
            req.requestor,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        );
        let len = data.len() as u32;
        if x.conn
            .change_property32(PropMode::REPLACE, req.requestor, prop, a.INCR, &[len])
            .is_err()
        {
            return x11rb::NONE;
        }
        incr.insert(
            (req.requestor, prop),
            IncrSend {
                ty: *ty,
                data: data.clone(),
                offset: 0,
            },
        );
        return prop;
    }
    match x
        .conn
        .change_property8(PropMode::REPLACE, req.requestor, prop, *ty, data)
    {
        Ok(_) => prop,
        Err(_) => x11rb::NONE,
    }
}

/// Read every content target from the current (foreign) owner on a private
/// connection, so its events never mix with the serving thread's.
fn read_foreign_snapshot() -> Option<ClipSnapshot> {
    let x = X11::connect()?;
    let owner = x
        .conn
        .get_selection_owner(x.atoms.CLIPBOARD)
        .ok()?
        .reply()
        .ok()?
        .owner;
    if owner == x11rb::NONE {
        return Some(ClipSnapshot::default());
    }
    let win = x.conn.generate_id().ok()?;
    x.conn
        .create_window(
            0,
            win,
            x.root,
            -10,
            -10,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .ok()?;
    let deadline = Instant::now() + SNAPSHOT_BUDGET;
    let result = (|| {
        let (_, targets) = convert(&x, win, x.atoms.TARGETS, deadline)?;
        let atoms: Vec<Atom> = targets
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_ne_bytes(*c))
            .collect();
        let mut item = Vec::new();
        let mut total = 0usize;
        for atom in atoms {
            if Instant::now() > deadline {
                break;
            }
            let Some(name) = atom_name(&x, atom) else {
                continue;
            };
            if SKIP_TARGETS.contains(&name.as_str()) || item.iter().any(|(n, _)| n == &name) {
                continue;
            }
            let per = (Instant::now() + TARGET_BUDGET).min(deadline);
            if let Some((_, data)) = convert(&x, win, atom, per) {
                total += data.len();
                if total > SNAPSHOT_MAX_BYTES {
                    break;
                }
                item.push((name, data));
            }
        }
        Some(ClipSnapshot {
            items: if item.is_empty() { vec![] } else { vec![item] },
        })
    })();
    let _ = x.conn.destroy_window(win);
    x.flush();
    result
}

/// ConvertSelection and collect the value (INCR supported). Returns
/// (type, bytes). For 32-bit formats the bytes are native-endian u32s.
fn convert(x: &X11, win: Window, target: Atom, deadline: Instant) -> Option<(Atom, Vec<u8>)> {
    let prop = x.atoms.CANTRIP_SEL;
    x.conn
        .convert_selection(win, x.atoms.CLIPBOARD, target, prop, x11rb::CURRENT_TIME)
        .ok()?;
    x.flush();
    // Wait for our SelectionNotify.
    loop {
        match next_event(x, deadline)? {
            Event::SelectionNotify(n) if n.requestor == win && n.target == target => {
                if n.property == x11rb::NONE {
                    return None;
                }
                break;
            }
            _ => continue,
        }
    }
    let r = x
        .conn
        .get_property(true, win, prop, AtomEnum::ANY, 0, u32::MAX / 4)
        .ok()?
        .reply()
        .ok()?;
    if r.type_ != x.atoms.INCR {
        return Some((r.type_, r.value));
    }
    // INCR: the delete above asked for the first chunk.
    x.flush();
    let mut out = Vec::new();
    let mut ty = x11rb::NONE;
    loop {
        match next_event(x, deadline)? {
            Event::PropertyNotify(p)
                if p.window == win && p.atom == prop && p.state == Property::NEW_VALUE =>
            {
                let part = x
                    .conn
                    .get_property(true, win, prop, AtomEnum::ANY, 0, u32::MAX / 4)
                    .ok()?
                    .reply()
                    .ok()?;
                x.flush();
                if part.value.is_empty() {
                    return Some((ty, out));
                }
                ty = part.type_;
                out.extend_from_slice(&part.value);
                if out.len() > SNAPSHOT_MAX_BYTES {
                    return None;
                }
            }
            _ => continue,
        }
    }
}

fn next_event(x: &X11, deadline: Instant) -> Option<Event> {
    loop {
        match x.conn.poll_for_event() {
            Ok(Some(e)) => return Some(e),
            Ok(None) => {
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(_) => return None,
        }
    }
}

// ───────────────────────────── Wayland (wl-copy) ─────────────────────────────

#[derive(Default)]
struct WlClip {
    state: Mutex<WlState>,
}

#[derive(Default)]
struct WlState {
    generation: u64,
    child: Option<Child>,
}

impl WlClip {
    fn spawn(&self, mime: &str, data: &[u8]) -> Result<u64, String> {
        let mut child = Command::new("wl-copy")
            .args(["--foreground", "--type", mime])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("wl-copy: {e}"))?;
        child
            .stdin
            .take()
            .ok_or("wl-copy stdin")?
            .write_all(data)
            .map_err(|e| format!("wl-copy: {e}"))?;
        // Give wl-copy a moment to take the selection; an immediate exit is a
        // failure.
        std::thread::sleep(Duration::from_millis(30));
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("wl-copy exited: {status}"));
        }
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(mut old) = s.child.take() {
            let _ = old.kill();
            let _ = old.wait();
        }
        s.generation += 1;
        s.child = Some(child);
        Ok(s.generation)
    }

    fn write_text(&self, text: &str) -> Result<ClipToken, String> {
        self.spawn("text/plain;charset=utf-8", text.as_bytes())
            .map(ClipToken)
    }

    fn unchanged_since(&self, token: &ClipToken) -> bool {
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        s.generation == token.0
            && s.child
                .as_mut()
                .is_some_and(|c| matches!(c.try_wait(), Ok(None)))
    }

    fn snapshot(&self) -> Option<ClipSnapshot> {
        let types = run_capture(&["--list-types"], Duration::from_millis(500))?;
        let types = String::from_utf8_lossy(&types).into_owned();
        let mut item = Vec::new();
        for ty in types.lines().map(str::trim).filter(|t| !t.is_empty()) {
            if let Some(data) = run_capture(&["--no-newline", "--type", ty], TARGET_BUDGET) {
                item.push((ty.to_string(), data));
            }
        }
        Some(ClipSnapshot {
            items: if item.is_empty() { vec![] } else { vec![item] },
        })
    }

    fn restore(&self, snapshot: &ClipSnapshot, token: &ClipToken) -> bool {
        if !self.unchanged_since(token) {
            return false;
        }
        let Some(item) = snapshot.items.first() else {
            // Empty before: stop serving Cantrip's text.
            let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(mut c) = s.child.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
            s.generation += 1;
            return true;
        };
        let best = item
            .iter()
            .find(|(t, _)| t.starts_with("image/"))
            .or_else(|| item.iter().find(|(t, _)| t.starts_with("text/plain")))
            .or_else(|| item.first());
        match best {
            Some((ty, data)) => self.spawn(ty, data).is_ok(),
            None => false,
        }
    }
}

/// Run `wl-paste <args>` with a deadline; stdout on success.
fn run_capture(args: &[&str], budget: Duration) -> Option<Vec<u8>> {
    let mut child = Command::new("wl-paste")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut out = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out.read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + budget;
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                let buf = reader.join().ok()?;
                return st.success().then_some(buf);
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_backend_fails_closed() {
        let c = LinuxClipboard {
            backend: Backend::None,
        };
        assert!(c.snapshot().is_none());
        assert!(c.write_text("x", true).is_err());
        assert!(!c.unchanged_since(&ClipToken(1)));
        assert!(!c.restore(&ClipSnapshot::default(), &ClipToken(1)));
    }
}

/// Live checks against the real X server on `$DISPLAY` (run with
/// `cargo test -p cantrip -- --ignored --test-threads=1`).
#[cfg(test)]
mod live {
    use super::*;
    use std::process::Command;

    fn xclip_out(target: &str) -> Vec<u8> {
        Command::new("xclip")
            .args(["-o", "-selection", "clipboard", "-t", target])
            .output()
            .unwrap()
            .stdout
    }

    fn xclip_in(target: &str, data: &[u8]) {
        let mut c = Command::new("xclip")
            .args(["-i", "-selection", "clipboard", "-t", target])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        c.stdin.take().unwrap().write_all(data).unwrap();
        assert!(c.wait().unwrap().success());
        std::thread::sleep(Duration::from_millis(150));
    }

    fn x11(c: &LinuxClipboard) -> &X11Clip {
        match &c.backend {
            Backend::X11(x) => x,
            _ => panic!("expected the X11 backend"),
        }
    }

    #[test]
    #[ignore]
    fn live_x11_ownership_is_identity_not_text() {
        let c = LinuxClipboard::new();
        assert_eq!(c.backend_name(), "x11-selection");
        let t = c.write_text("cantrip live text ✓", true).unwrap();
        assert_eq!(xclip_out("UTF8_STRING"), "cantrip live text ✓".as_bytes());
        let targets = String::from_utf8(xclip_out("TARGETS")).unwrap();
        assert!(targets.contains("x-kde-passwordManagerHint"), "{targets}");
        assert!(c.unchanged_since(&t));
        // Another app copies the *same* text: that is a change.
        xclip_in("UTF8_STRING", "cantrip live text ✓".as_bytes());
        assert!(!c.unchanged_since(&t));
        // A newer Cantrip write invalidates the older token too.
        let t1 = c.write_text("one", false).unwrap();
        let t2 = c.write_text("two", false).unwrap();
        assert!(!c.unchanged_since(&t1));
        assert!(c.unchanged_since(&t2));
        let targets = String::from_utf8(xclip_out("TARGETS")).unwrap();
        assert!(!targets.contains("x-kde-passwordManagerHint"));
    }

    #[test]
    #[ignore]
    fn live_x11_snapshot_restores_every_type_including_incr_images() {
        // A foreign owner (a second, independent clipboard window) serving
        // an image and text together.
        let png: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let other = LinuxClipboard::new();
        let ox = x11(&other);
        let img = intern(&ox.x, "image/png").unwrap();
        ox.write(vec![
            (img, img, png.clone()),
            (
                ox.x.atoms.UTF8_STRING,
                ox.x.atoms.UTF8_STRING,
                b"caption".to_vec(),
            ),
        ])
        .unwrap();

        let c = LinuxClipboard::new();
        let t0 = Instant::now();
        let snap = c.snapshot().unwrap();
        eprintln!("snapshot took {:?}", t0.elapsed());
        let item = &snap.items[0];
        assert!(
            item.iter().any(|(n, d)| n == "image/png" && *d == png),
            "INCR image read"
        );
        assert!(item
            .iter()
            .any(|(n, d)| n == "UTF8_STRING" && d == b"caption"));

        let tok = c.write_text("dictation", true).unwrap();
        assert!(!other.unchanged_since(&ClipToken(1)));
        assert!(c.restore(&snap, &tok));
        assert!(!c.unchanged_since(&tok));
        assert_eq!(xclip_out("image/png"), png, "image served back via INCR");
        assert_eq!(xclip_out("UTF8_STRING"), b"caption");

        // Restore refuses after a foreign change.
        let tok = c.write_text("dictation 2", false).unwrap();
        xclip_in("text/plain", b"someone else");
        assert!(!c.restore(&snap, &tok));
        assert_eq!(xclip_out("text/plain"), b"someone else");
    }
}
