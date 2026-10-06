//! Conservative delivery permits for the compositor/session observed at stop.
//!
//! User-service compositors are associated through logind's root-owned session
//! controller record and its authenticated D-Bus PID, never inherited client env.
//!
//! Hyprland's `activewindow` is *not* a keyboard-surface query. Its Lua layer
//! metadata is therefore also required: any mapped keyboard-interactive layer
//! makes the destination ambiguous. No window title or transcript is retained.
//!
//! Verified against Hyprland 0.56.2 (HyprCtl.cpp, FocusState.cpp,
//! SessionLockManager.cpp and LuaBindingsQuery/LuaLayerSurface.cpp) and logind
//! 261.2. Native session locks clear surface focus and emit activewindowv2 even
//! when a third-party locker does not update logind's advisory LockedHint.
//! Reconnection starts a fresh epoch; it can never revalidate an existing permit.
//! Only a direct DRM desktop is supported: nested compositors do not expose
//! enough parent focus/lock history to authorize automatic delivery.

use dbus::{
    arg::PropMap,
    blocking::{stdintf::org_freedesktop_dbus::Properties, Connection},
    channel::MatchingReceiver,
    message::MatchRule,
    MessageType,
};
use parking_lot::Mutex;
use std::{
    env,
    io::{self, Read, Write},
    mem,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
            net::UnixStream,
        },
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, LazyLock,
    },
    time::{Duration, Instant},
};

const IO_TIMEOUT: Duration = Duration::from_millis(150);
const REQUEST_TIMEOUT: Duration = Duration::from_millis(900);
const TICK: Duration = Duration::from_millis(10);
const MANAGER_PATH: &str = "/org/freedesktop/login1";
const MANAGER: &str = "org.freedesktop.login1.Manager";
const SESSION: &str = "org.freedesktop.login1.Session";
const UNKNOWN: &str = "Desktop safety is unavailable; text was not sent. Use Copy from recordings.";
const CHANGED: &str =
    "The destination, lock state, or session changed since stop; text was not sent.";

// `repl` returns values; `eval` only returns "ok". Check configerrors first:
// Hyprland's eval implementation clears its diagnostics, even for a query.
// This is constant code, never interpolated with a transcript or window title.
const FOCUS_QUERY: &str = concat!(
    "/repl local w=hl.get_active_window(); ",
    "if not w or not w.mapped or w.hidden or not w.visible or not w.accepts_input then return 'none' end; ",
    "for _,l in ipairs(hl.get_layers()) do ",
    "if type(l.mapped)~='boolean' or type(l.interactivity)~='number' then return 'unknown' end; ",
    "if l.mapped and l.interactivity~=0 then return 'layer' end end; ",
    "if w.class=='cantrip-actions' or w.class=='cantrip-settings' or w.class=='cantrip' then return 'cantrip' end; ",
    "return string.format('focus|%s|%x|%d',w.address,w.stable_id,w.pid)"
);

type SafetyResult<T, E = &'static str> = Result<T, E>;
type Reply = mpsc::SyncSender<SafetyResult<Stamp>>;

struct Request {
    generation: u64,
    reply: Reply,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Focus {
    address: u64,
    stable_id: u64,
    pid: u32,
}

#[derive(Clone, Copy, Debug)]
struct Stamp {
    focus: Focus,
    generation: u64,
    suspend_offset: u64,
    compositor: i32,
}

static MONITOR: LazyLock<Option<Arc<Monitor>>> = LazyLock::new(|| Monitor::start().map(Arc::new));

#[derive(Clone)]
pub(crate) struct Guard {
    monitor: Option<Arc<Monitor>>,
    stamp: SafetyResult<Stamp>,
}

impl Guard {
    pub(crate) fn prepare() {
        LazyLock::force(&MONITOR);
    }

    pub(crate) fn capture() -> Self {
        let monitor = LazyLock::get(&MONITOR).and_then(Option::as_ref).cloned();
        let stamp = monitor
            .as_ref()
            .ok_or(UNKNOWN)
            .and_then(|monitor| monitor.cached_stamp());
        Self { monitor, stamp }
    }

    pub(crate) fn check(&self) -> SafetyResult<()> {
        self.check_history()?;
        let initial = self.stamp?;
        let current = self.monitor.as_ref().ok_or(UNKNOWN)?.request()?;
        if current.focus != initial.focus || current.generation != initial.generation {
            return Err(CHANGED);
        }
        self.check_history()
    }

    pub(crate) fn check_history(&self) -> SafetyResult<()> {
        self.monitor
            .as_ref()
            .ok_or(UNKNOWN)?
            .check_stamp(self.stamp?)
    }

    pub(crate) fn compositor(&self) -> SafetyResult<i32> {
        Ok(self.stamp?.compositor)
    }

    pub(crate) fn socket_path(&self) -> SafetyResult<&Path> {
        Ok(&self.monitor.as_ref().ok_or(UNKNOWN)?.paths.wayland)
    }
}

struct Paths {
    control: PathBuf,
    events: PathBuf,
    wayland: PathBuf,
}

impl Paths {
    fn current() -> Option<Self> {
        let runtime = PathBuf::from(env::var_os("XDG_RUNTIME_DIR")?);
        let instance = env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
        if !runtime.is_absolute()
            || instance.is_empty()
            || !instance
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_')
        {
            return None;
        }
        let directory = runtime.join("hypr").join(instance);
        Some(Self {
            control: directory.join(".socket.sock"),
            events: directory.join(".socket2.sock"),
            wayland: wayland_socket_path().ok()?,
        })
    }
}

pub(crate) fn wayland_socket_path() -> io::Result<PathBuf> {
    // Do not steal a caller-owned WAYLAND_SOCKET fd or mutate environment from
    // a worker thread. Forwarded/pre-opened connections have no verified target.
    if env::var_os("WAYLAND_SOCKET").is_some() {
        return Err(io::Error::other(
            "Pre-opened Wayland sockets are not supported",
        ));
    }
    let display = PathBuf::from(
        env::var_os("WAYLAND_DISPLAY").ok_or_else(|| io::Error::other("No Wayland display"))?,
    );
    if display.is_absolute() {
        return Ok(display);
    }
    let runtime = PathBuf::from(
        env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| io::Error::other("No runtime directory"))?,
    );
    if !runtime.is_absolute() || display.components().count() != 1 {
        return Err(io::Error::other("Invalid Wayland display path"));
    }
    Ok(runtime.join(display))
}

struct Monitor {
    paths: Arc<Paths>,
    requests: mpsc::SyncSender<Request>,
    generation: Arc<AtomicU64>,
    alive: Arc<AtomicBool>,
    last_poll: Arc<AtomicU64>,
    cached: Arc<Mutex<SafetyResult<Stamp>>>,
    refresh_requested: Arc<AtomicBool>,
}

impl Monitor {
    fn start() -> Option<Self> {
        let paths = Arc::new(Paths::current()?);
        let generation = Arc::new(AtomicU64::new(0));
        let alive = Arc::new(AtomicBool::new(false));
        let last_poll = Arc::new(AtomicU64::new(0));
        let cached = Arc::new(Mutex::new(Err(UNKNOWN)));
        let refresh_requested = Arc::new(AtomicBool::new(false));
        let (requests, receiver) = mpsc::sync_channel(4);
        let thread_paths = paths.clone();
        let thread_generation = generation.clone();
        let thread_alive = alive.clone();
        let thread_poll = last_poll.clone();
        let thread_cached = cached.clone();
        let thread_refresh = refresh_requested.clone();
        std::thread::Builder::new()
            .name("cantrip-desktop".into())
            .spawn(move || {
                let mut delay = Duration::from_millis(100);
                loop {
                    let result = monitor_loop(
                        &thread_paths,
                        &thread_generation,
                        &thread_alive,
                        &thread_poll,
                        &thread_cached,
                        &thread_refresh,
                        &receiver,
                    );
                    let was_alive = thread_alive.load(Ordering::Acquire);
                    invalidate_history(&thread_generation, &thread_alive, &thread_poll);
                    if result.is_ok() {
                        break;
                    }
                    if was_alive {
                        delay = Duration::from_millis(100);
                    }
                    // Reconnect does not queue or retry delivery. Requests arriving
                    // while offline fail, and every connection gets a new epoch.
                    let deadline = Instant::now() + delay;
                    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
                        match receiver.recv_timeout(remaining) {
                            Ok(request) => {
                                let _ = request.reply.send(Err(UNKNOWN));
                            }
                            Err(mpsc::RecvTimeoutError::Timeout) => break,
                            Err(mpsc::RecvTimeoutError::Disconnected) => return,
                        }
                    }
                    delay = (delay * 2).min(Duration::from_secs(1));
                }
            })
            .ok()?;
        Some(Self {
            paths,
            requests,
            generation,
            alive,
            last_poll,
            cached,
            refresh_requested,
        })
    }

    fn cached_stamp(&self) -> SafetyResult<Stamp> {
        // No IPC, channel wait, sleep, or initialization on the command loop.
        // The producer never holds this mutex while querying the desktop.
        let result = (|| {
            let stamp = (*self.cached.try_lock().ok_or(UNKNOWN)?)?;
            self.check_stamp(stamp)?;
            Ok(stamp)
        })();
        if result.is_err() {
            self.refresh_requested.store(true, Ordering::Release);
        }
        result
    }

    fn check_stamp(&self, stamp: Stamp) -> SafetyResult<()> {
        if !self.alive.load(Ordering::Acquire)
            || clock_ns(libc::CLOCK_MONOTONIC)
                .ok_or(UNKNOWN)?
                .saturating_sub(self.last_poll.load(Ordering::Acquire))
                > 250_000_000
        {
            return Err(UNKNOWN);
        }
        if self.generation.load(Ordering::Acquire) != stamp.generation
            || suspend_offset()
                .ok_or(UNKNOWN)?
                .abs_diff(stamp.suspend_offset)
                > 10_000_000
        {
            return Err(CHANGED);
        }
        Ok(())
    }

    fn request(&self) -> SafetyResult<Stamp> {
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        let generation = self.generation.load(Ordering::Acquire);
        if !self.alive.load(Ordering::Acquire) {
            return Err(UNKNOWN);
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        self.requests
            .try_send(Request {
                generation,
                reply: sender,
            })
            .map_err(|_| UNKNOWN)?;
        receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| UNKNOWN)?
    }
}

fn monitor_loop(
    paths: &Paths,
    generation: &Arc<AtomicU64>,
    alive: &AtomicBool,
    last_poll: &AtomicU64,
    cached: &Mutex<SafetyResult<Stamp>>,
    refresh_requested: &AtomicBool,
    requests: &mpsc::Receiver<Request>,
) -> SafetyResult<()> {
    let mut events = connect_unix(&paths.events, IO_TIMEOUT).map_err(|_| UNKNOWN)?;
    let compositor = peer_pid(&events).map_err(|_| UNKNOWN)?;
    let control = connect_unix(&paths.control, IO_TIMEOUT).map_err(|_| UNKNOWN)?;
    if peer_pid(&control).map_err(|_| UNKNOWN)? != compositor {
        return Err(UNKNOWN);
    }
    // Never leave an idle Hyprland command connection open: its server handles
    // commands synchronously and waits for a request on every accepted socket.
    drop(control);
    let wayland = connect_unix(&paths.wayland, IO_TIMEOUT).map_err(|_| UNKNOWN)?;
    if peer_pid(&wayland).map_err(|_| UNKNOWN)? != compositor {
        return Err(UNKNOWN);
    }
    drop(wayland);
    check_direct_desktop(&query(&paths.control, compositor, b"j/status")?)?;
    let logind = Logind::connect(compositor, generation.clone())?;
    let mut pending = Vec::with_capacity(2048);
    subscription_barrier(paths, compositor, &mut events, &mut pending, generation)?;
    last_poll.store(
        clock_ns(libc::CLOCK_MONOTONIC).ok_or(UNKNOWN)?,
        Ordering::Release,
    );
    alive.store(true, Ordering::Release);
    let mut last_suspend = suspend_offset().ok_or(UNKNOWN)?;
    let mut sampled_generation = None;
    let mut refresh_after = Instant::now();
    loop {
        drain_history(&mut events, &mut pending, &logind.connection, generation)?;
        if logind.owner_changed.load(Ordering::Acquire) {
            return Err(UNKNOWN);
        }
        logind.check_controller()?;
        let now = clock_ns(libc::CLOCK_MONOTONIC).ok_or(UNKNOWN)?;
        let previous = last_poll.swap(now, Ordering::AcqRel);
        if previous != 0 && now.saturating_sub(previous) > 250_000_000 {
            generation.fetch_add(1, Ordering::AcqRel);
        }
        let suspend = suspend_offset().ok_or(UNKNOWN)?;
        if suspend.abs_diff(last_suspend) > 10_000_000 {
            generation.fetch_add(1, Ordering::AcqRel);
            last_suspend = suspend;
        }
        let current_generation = generation.load(Ordering::Acquire);
        // Healthy unchanged history needs no Lua polling. Failed captures may
        // request a background retry, coalesced to at most once per 250ms.
        if sampled_generation != Some(current_generation)
            || (refresh_requested.load(Ordering::Acquire) && Instant::now() >= refresh_after)
        {
            refresh_requested.store(false, Ordering::Release);
            let result = take_snapshot(
                paths,
                compositor,
                &mut events,
                &mut pending,
                &logind,
                generation,
            );
            sampled_generation = Some(generation.load(Ordering::Acquire));
            *cached.lock() = result;
            refresh_after = Instant::now() + Duration::from_millis(250);
        }
        match requests.recv_timeout(TICK) {
            Ok(request) => {
                if request.generation != generation.load(Ordering::Acquire) {
                    let _ = request.reply.send(Err(CHANGED));
                    continue;
                }
                let result = take_snapshot(
                    paths,
                    compositor,
                    &mut events,
                    &mut pending,
                    &logind,
                    generation,
                );
                sampled_generation = Some(generation.load(Ordering::Acquire));
                *cached.lock() = result;
                refresh_after = Instant::now() + Duration::from_millis(250);
                let result = result.and_then(|stamp| {
                    if stamp.generation == request.generation {
                        Ok(stamp)
                    } else {
                        Err(CHANGED)
                    }
                });
                let _ = request.reply.send(result);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

fn take_snapshot(
    paths: &Paths,
    compositor: i32,
    events: &mut UnixStream,
    pending: &mut Vec<u8>,
    logind: &Logind,
    generation: &AtomicU64,
) -> SafetyResult<Stamp> {
    let result = (|| {
        drain_history(events, pending, &logind.connection, generation)?;
        if !pending.is_empty() {
            return Err(UNKNOWN);
        }
        let before = generation.load(Ordering::Acquire);
        let suspend = suspend_offset().ok_or(UNKNOWN)?;
        let focus = query_focus(paths, compositor)?;
        let locked: serde_json::Value =
            serde_json::from_slice(&query(&paths.control, compositor, b"j/locked")?)
                .map_err(|_| UNKNOWN)?;
        if locked.get("locked").and_then(serde_json::Value::as_bool) != Some(false) {
            return Err("The desktop is locked or lock safety is unknown; text was not sent.");
        }
        logind.check()?;
        drain_history(events, pending, &logind.connection, generation)?;
        if !pending.is_empty()
            || before != generation.load(Ordering::Acquire)
            || suspend.abs_diff(suspend_offset().ok_or(UNKNOWN)?) > 10_000_000
        {
            return Err(CHANGED);
        }
        Ok(Stamp {
            focus,
            generation: before,
            suspend_offset: suspend,
            compositor,
        })
    })();
    if result.is_err() {
        // An unknown snapshot is a break in authority, not just a cache miss.
        // In particular, mapped layers can become keyboard-interactive without
        // an openlayer or activewindowv2 event. Old permits never recover.
        generation.fetch_add(1, Ordering::AcqRel);
    }
    result
}

fn check_direct_desktop(bytes: &[u8]) -> SafetyResult<()> {
    let status: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| UNKNOWN)?;
    // Hyprland 0.56.2 deliberately skips headless backends in this reply;
    // "error" is unknown, never proof of a controlled headless desktop.
    if status.get("backend").and_then(serde_json::Value::as_str) != Some("drm") {
        return Err("Nested or unknown desktops cannot verify parent focus and lock state; use Copy from recordings.");
    }
    if status
        .get("configProvider")
        .and_then(serde_json::Value::as_str)
        != Some("lua")
    {
        return Err("This desktop cannot provide the layer metadata required for safe delivery; use Copy from recordings.");
    }
    Ok(())
}

fn invalidate_history(generation: &AtomicU64, alive: &AtomicBool, last_poll: &AtomicU64) {
    alive.store(false, Ordering::Release);
    generation.fetch_add(1, Ordering::AcqRel);
    last_poll.store(0, Ordering::Release);
}

fn check_config_errors(paths: &Paths, compositor: i32) -> SafetyResult<()> {
    let errors: Vec<String> =
        serde_json::from_slice(&query(&paths.control, compositor, b"j/configerrors")?)
            .map_err(|_| UNKNOWN)?;
    if errors.iter().any(|error| !error.trim().is_empty()) {
        return Err("Desktop configuration has errors; focus safety cannot be checked without clearing them.");
    }
    Ok(())
}

fn query_focus(paths: &Paths, compositor: i32) -> SafetyResult<Focus> {
    check_config_errors(paths, compositor)?;
    parse_focus(&query(&paths.control, compositor, FOCUS_QUERY.as_bytes())?)
}

fn subscription_barrier(
    paths: &Paths,
    compositor: i32,
    events: &mut UnixStream,
    pending: &mut Vec<u8>,
    generation: &AtomicU64,
) -> SafetyResult<()> {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    check_config_errors(paths, compositor)?;
    let nonce = format!(
        "cantrip-delivery-ready:{}:{:x}:{:x}",
        std::process::id(),
        clock_ns(libc::CLOCK_BOOTTIME).ok_or(UNKNOWN)?,
        SERIAL.fetch_add(1, Ordering::Relaxed)
    );
    // Hyprland 0.56.2's Lua dispatch evaluates hl.dispatch(hl.dsp.event(...)).
    // ConfigActions::event only posts custom>>DATA: no notification or focus
    // change. The matching event, not a command reply, proves subscription.
    query(
        &paths.control,
        compositor,
        format!("/dispatch hl.dsp.event('{nonce}')").as_bytes(),
    )?;
    let expected = format!("custom>>{nonce}");
    let deadline = Instant::now() + IO_TIMEOUT;
    loop {
        if drain_focus_events(events, pending, generation, Some(expected.as_bytes()))? {
            return Ok(());
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(UNKNOWN)?;
        let mut fd = libc::pollfd {
            fd: events.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut fd, 1, remaining.min(TICK).as_millis() as i32) } < 0
            && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
        {
            return Err(UNKNOWN);
        }
    }
}

fn parse_focus(bytes: &[u8]) -> SafetyResult<Focus> {
    let text = std::str::from_utf8(bytes).map_err(|_| UNKNOWN)?.trim();
    match text {
        "layer" => return Err("A keyboard-interactive desktop layer is open; text was not sent."),
        "cantrip" => {
            return Err("Cantrip's own window is focused; text was saved instead of sent.")
        }
        "none" => return Err("There is no verified text destination; text was not sent."),
        _ => {}
    }
    let mut fields = text.split('|');
    if fields.next() != Some("focus") {
        return Err(UNKNOWN);
    }
    let address = u64::from_str_radix(
        fields
            .next()
            .and_then(|s| s.strip_prefix("0x"))
            .ok_or(UNKNOWN)?,
        16,
    )
    .map_err(|_| UNKNOWN)?;
    let stable_id = u64::from_str_radix(fields.next().ok_or(UNKNOWN)?, 16).map_err(|_| UNKNOWN)?;
    let pid = fields
        .next()
        .ok_or(UNKNOWN)?
        .parse::<u32>()
        .map_err(|_| UNKNOWN)?;
    if address == 0 || stable_id == 0 || pid == 0 || fields.next().is_some() {
        return Err(UNKNOWN);
    }
    Ok(Focus {
        address,
        stable_id,
        pid,
    })
}

fn query(path: &Path, compositor: i32, request: &[u8]) -> SafetyResult<Vec<u8>> {
    let mut socket = connect_unix(path, IO_TIMEOUT).map_err(|_| UNKNOWN)?;
    if peer_pid(&socket).map_err(|_| UNKNOWN)? != compositor {
        return Err(UNKNOWN);
    }
    let deadline = Instant::now() + IO_TIMEOUT;
    socket.set_nonblocking(false).map_err(|_| UNKNOWN)?;
    socket
        .set_write_timeout(Some(IO_TIMEOUT))
        .map_err(|_| UNKNOWN)?;
    socket.write_all(request).map_err(|_| UNKNOWN)?;
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(UNKNOWN)?;
        socket
            .set_read_timeout(Some(remaining))
            .map_err(|_| UNKNOWN)?;
        match socket.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => bytes.extend_from_slice(&buffer[..count]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(UNKNOWN),
        }
        if bytes.len() > 16_384 {
            return Err(UNKNOWN);
        }
    }
    Ok(bytes)
}

fn invalidates_focus(event: &[u8]) -> bool {
    let name = event.split(|c| *c == b'>').next().unwrap_or_default();
    matches!(
        name,
        b"activewindowv2"
            | b"openlayer"
            | b"closelayer"
            | b"workspacev2"
            | b"focusedmonv2"
            | b"activespecial"
            | b"monitorremoved"
            | b"configreloaded"
    )
}

fn drain_history(
    events: &mut UnixStream,
    pending: &mut Vec<u8>,
    connection: &Connection,
    generation: &AtomicU64,
) -> SafetyResult<()> {
    drain_focus_events(events, pending, generation, None)?;
    for _ in 0..256 {
        if !connection.process(Duration::ZERO).map_err(|_| UNKNOWN)? {
            return Ok(());
        }
    }
    Err(UNKNOWN)
}

fn drain_focus_events(
    events: &mut UnixStream,
    pending: &mut Vec<u8>,
    generation: &AtomicU64,
    barrier: Option<&[u8]>,
) -> SafetyResult<bool> {
    let mut bytes = [0; 4096];
    let mut matched = false;
    for _ in 0..64 {
        match events.read(&mut bytes) {
            Ok(0) => return Err(UNKNOWN),
            Ok(count) => {
                pending.extend_from_slice(&bytes[..count]);
                while let Some(end) = pending.iter().position(|c| *c == b'\n') {
                    if invalidates_focus(&pending[..end]) {
                        generation.fetch_add(1, Ordering::AcqRel);
                    }
                    matched |= barrier.is_some_and(|expected| &pending[..end] == expected);
                    pending.drain(..=end);
                }
                if pending.len() > 4096 {
                    return Err(UNKNOWN);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                // A split event is not evidence of unchanged focus. Invalidate
                // old permits now, even before its remaining bytes arrive.
                if !pending.is_empty() {
                    generation.fetch_add(1, Ordering::AcqRel);
                }
                return Ok(matched);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(UNKNOWN),
        }
    }
    Err(UNKNOWN)
}

struct SessionController {
    state_path: PathBuf,
    bus_name: String,
}

fn compositor_session_id(compositor: i32) -> SafetyResult<String> {
    // This is only a locator. The root-owned controller record and D-Bus peer
    // PID below provide the authority; never use the daemon's environment.
    let mut bytes = Vec::new();
    std::fs::File::open(format!("/proc/{compositor}/environ"))
        .map_err(|_| UNKNOWN)?
        .take(65_537)
        .read_to_end(&mut bytes)
        .map_err(|_| UNKNOWN)?;
    if bytes.len() > 65_536 {
        return Err(UNKNOWN);
    }
    let mut candidates = bytes
        .split(|byte| *byte == 0)
        .filter_map(|entry| entry.strip_prefix(b"XDG_SESSION_ID="));
    let id = std::str::from_utf8(candidates.next().ok_or(UNKNOWN)?).map_err(|_| UNKNOWN)?;
    if candidates.next().is_some()
        || id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(UNKNOWN);
    }
    Ok(id.to_owned())
}

fn parse_controller(bytes: &[u8]) -> SafetyResult<&str> {
    if !bytes.ends_with(b"\n") {
        return Err(UNKNOWN);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| UNKNOWN)?;
    let mut controllers = text
        .lines()
        .filter_map(|line| line.strip_prefix("CONTROLLER="));
    let controller = controllers.next().ok_or(UNKNOWN)?;
    let unique_name = controller.strip_prefix(':').ok_or(UNKNOWN)?;
    if controllers.next().is_some()
        || !unique_name.contains('.')
        || unique_name.split('.').any(|part| {
            part.is_empty()
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
    {
        return Err(UNKNOWN);
    }
    Ok(controller)
}

fn read_controller<'a>(path: &Path, bytes: &'a mut [u8]) -> SafetyResult<&'a str> {
    // logind 261.2 saves CONTROLLER in this private state format. Unknown or
    // changed formats fail closed; no writable or symlink component is trusted.
    for directory in ["/run", "/run/systemd", "/run/systemd/sessions"] {
        let metadata = std::fs::symlink_metadata(directory).map_err(|_| UNKNOWN)?;
        if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err(UNKNOWN);
        }
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| UNKNOWN)?;
    let metadata = file.metadata().map_err(|_| UNKNOWN)?;
    if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        return Err(UNKNOWN);
    }
    let mut length = 0;
    loop {
        if length == bytes.len() {
            return Err(UNKNOWN);
        }
        match file.read(&mut bytes[length..]) {
            Ok(0) => return parse_controller(&bytes[..length]),
            Ok(count) => length += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(UNKNOWN),
        }
    }
}

struct Logind {
    connection: Connection,
    owner: String,
    session: dbus::Path<'static>,
    owner_changed: Arc<AtomicBool>,
    controller: Option<SessionController>,
}

impl Logind {
    fn connect(compositor: i32, generation: Arc<AtomicU64>) -> SafetyResult<Self> {
        let connection = Connection::new_system().map_err(|_| UNKNOWN)?;
        let bus =
            connection.with_proxy("org.freedesktop.DBus", "/org/freedesktop/DBus", IO_TIMEOUT);
        let (owner,): (String,) = bus
            .method_call(
                "org.freedesktop.DBus",
                "GetNameOwner",
                ("org.freedesktop.login1",),
            )
            .map_err(|_| UNKNOWN)?;
        let manager = connection.with_proxy(owner.as_str(), MANAGER_PATH, IO_TIMEOUT);
        let by_pid: dbus::Result<(dbus::Path<'static>,)> =
            manager.method_call(MANAGER, "GetSessionByPID", (compositor as u32,));
        let (session, controller) = match by_pid {
            Ok((session,)) => (session, None),
            Err(_) => {
                // UWSM starts Hyprland outside the login session's cgroup.
                // It must still be logind's authenticated session controller.
                let id = compositor_session_id(compositor)?;
                let (session,): (dbus::Path<'static>,) = manager
                    .method_call(MANAGER, "GetSession", (id.as_str(),))
                    .map_err(|_| UNKNOWN)?;
                let state_path = PathBuf::from("/run/systemd/sessions").join(id);
                let mut bytes = [0; 16_384];
                let bus_name = read_controller(&state_path, &mut bytes)?;
                let (pid,): (u32,) = bus
                    .method_call(
                        "org.freedesktop.DBus",
                        "GetConnectionUnixProcessID",
                        (bus_name,),
                    )
                    .map_err(|_| UNKNOWN)?;
                if pid != compositor as u32 {
                    return Err(UNKNOWN);
                }
                (
                    session,
                    Some(SessionController {
                        state_path,
                        bus_name: bus_name.to_owned(),
                    }),
                )
            }
        };
        let session_proxy = connection.with_proxy(owner.as_str(), session.clone(), IO_TIMEOUT);
        let (uid, _): (u32, dbus::Path<'static>) =
            session_proxy.get(SESSION, "User").map_err(|_| UNKNOWN)?;
        if uid != unsafe { libc::geteuid() } {
            return Err(UNKNOWN);
        }
        let session_path = session.to_string();
        let rule = MatchRule::new()
            .with_type(MessageType::Signal)
            .with_strict_sender(owner.clone())
            .with_namespaced_path(MANAGER_PATH);
        let _: () = bus
            .method_call("org.freedesktop.DBus", "AddMatch", (rule.match_str(),))
            .map_err(|_| UNKNOWN)?;
        let signals_generation = generation.clone();
        connection.start_receive(
            rule,
            Box::new(move |message, _| {
                let path = message.path();
                let member = message.member();
                let member = member.as_deref().unwrap_or("");
                let path = path.as_deref().unwrap_or("");
                let changed = if path == MANAGER_PATH {
                    matches!(
                        member,
                        "PrepareForSleep" | "PrepareForShutdown" | "SessionRemoved"
                    )
                } else if path == session_path {
                    if matches!(member, "Lock" | "Unlock" | "PauseDevice" | "ResumeDevice") {
                        true
                    } else if member == "PropertiesChanged" {
                        match message.read3::<String, PropMap, Vec<String>>() {
                            Ok((interface, changed, invalidated)) if interface == SESSION => {
                                ["Active", "LockedHint", "State", "Type", "Remote"]
                                    .iter()
                                    .any(|name| {
                                        changed.contains_key(*name)
                                            || invalidated.iter().any(|field| field == *name)
                                    })
                            }
                            Ok(_) => false,
                            Err(_) => true,
                        }
                    } else {
                        false
                    }
                } else {
                    false
                };
                if changed {
                    signals_generation.fetch_add(1, Ordering::AcqRel);
                }
                true
            }),
        );
        let owner_rule = MatchRule::new_signal("org.freedesktop.DBus", "NameOwnerChanged")
            .with_strict_sender("org.freedesktop.DBus");
        let _: () = bus
            .method_call(
                "org.freedesktop.DBus",
                "AddMatch",
                (owner_rule.match_str(),),
            )
            .map_err(|_| UNKNOWN)?;
        let owner_changed = Arc::new(AtomicBool::new(false));
        let changed = owner_changed.clone();
        let controller_name = controller
            .as_ref()
            .map(|controller| controller.bus_name.clone());
        connection.start_receive(
            owner_rule,
            Box::new(move |message, _| {
                if message
                    .read3::<String, String, String>()
                    .map_or(true, |(name, _, _)| {
                        name == "org.freedesktop.login1" || controller_name.as_ref() == Some(&name)
                    })
                {
                    changed.store(true, Ordering::Release);
                    generation.fetch_add(1, Ordering::AcqRel);
                }
                true
            }),
        );
        Ok(Self {
            connection,
            owner,
            session,
            owner_changed,
            controller,
        })
    }

    fn check_controller(&self) -> SafetyResult<()> {
        if let Some(controller) = &self.controller {
            let mut bytes = [0; 16_384];
            if read_controller(&controller.state_path, &mut bytes)? != controller.bus_name {
                return Err(UNKNOWN);
            }
        }
        Ok(())
    }

    fn check(&self) -> SafetyResult<()> {
        if self.owner_changed.load(Ordering::Acquire) {
            return Err(UNKNOWN);
        }
        let session =
            self.connection
                .with_proxy(self.owner.as_str(), self.session.clone(), IO_TIMEOUT);
        let props = session.get_all(SESSION).map_err(|_| UNKNOWN)?;
        let boolean = |name: &str| props.get(name).and_then(|v| v.0.as_i64()).map(|v| v != 0);
        if boolean("Active") != Some(true)
            || boolean("LockedHint") != Some(false)
            || boolean("Remote") != Some(false)
            || props.get("Type").and_then(|v| v.0.as_str()) != Some("wayland")
            || props.get("State").and_then(|v| v.0.as_str()) != Some("active")
        {
            return Err(
                "The local Wayland session is inactive, locked, or unknown; text was not sent.",
            );
        }
        let manager = self
            .connection
            .with_proxy(self.owner.as_str(), MANAGER_PATH, IO_TIMEOUT);
        let sleeping: bool = manager
            .get(MANAGER, "PreparingForSleep")
            .map_err(|_| UNKNOWN)?;
        if sleeping {
            return Err("The system is preparing to sleep; text was not sent.");
        }
        self.check_controller()
    }
}

// CLOCK_BOOTTIME includes suspend; CLOCK_MONOTONIC does not. This additionally
// rejects a stale permit immediately after resume, before queued D-Bus signals
// have been dispatched. Ten milliseconds allow syscall/scheduling jitter.
fn suspend_offset() -> Option<u64> {
    Some(clock_ns(libc::CLOCK_BOOTTIME)?.saturating_sub(clock_ns(libc::CLOCK_MONOTONIC)?))
}

fn clock_ns(id: libc::clockid_t) -> Option<u64> {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(id, &mut value) } != 0 {
        return None;
    }
    Some(
        (value.tv_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(value.tv_nsec as u64),
    )
}

pub(crate) fn peer_pid(socket: &UnixStream) -> io::Result<i32> {
    let mut credentials: libc::ucred = unsafe { mem::zeroed() };
    let mut size = mem::size_of::<libc::ucred>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut size,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if credentials.uid != unsafe { libc::geteuid() } || credentials.pid <= 0 {
        return Err(io::Error::other("Untrusted desktop socket"));
    }
    Ok(credentials.pid)
}

pub(crate) fn connect_unix(path: &Path, timeout: Duration) -> io::Result<UnixStream> {
    let mut address: libc::sockaddr_un = unsafe { mem::zeroed() };
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() || bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        return Err(io::Error::other("Invalid desktop socket path"));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (output, input) in address.sun_path.iter_mut().zip(bytes) {
        *output = *input as libc::c_char;
    }
    let raw = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: socket returned a new owned fd; OwnedFd closes every error path.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    if unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            mem::size_of_val(&address) as libc::socklen_t,
        )
    } != 0
    {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(error);
        }
        let deadline = Instant::now() + timeout;
        loop {
            if Instant::now() >= deadline {
                return Err(io::ErrorKind::TimedOut.into());
            }
            let mut poll = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            let result = unsafe { libc::poll(&mut poll, 1, 10) };
            if result < 0 {
                return Err(io::Error::last_os_error());
            }
            if result > 0 {
                break;
            }
        }
        let mut error = 0i32;
        let mut size = mem::size_of_val(&error) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&mut error as *mut i32).cast(),
                &mut size,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error));
        }
    }
    Ok(UnixStream::from(fd))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_diagnostic_entries_are_not_errors_but_real_diagnostics_still_block() {
        use std::os::unix::net::UnixListener;

        for (reply, allowed) in [
            (br#"["", "  \n"]"#.as_slice(), true),
            (
                br#"["", "invalid desktop configuration"]"#.as_slice(),
                false,
            ),
        ] {
            let path = env::temp_dir().join(format!(
                "cantrip-diagnostics-{}-{}.sock",
                std::process::id(),
                clock_ns(libc::CLOCK_MONOTONIC).unwrap(),
            ));
            let listener = UnixListener::bind(&path).unwrap();
            let responder = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; b"j/configerrors".len()];
                stream.read_exact(&mut request).unwrap();
                stream.write_all(reply).unwrap();
            });
            let paths = Paths {
                control: path.clone(),
                events: PathBuf::new(),
                wayland: PathBuf::new(),
            };
            assert_eq!(
                check_config_errors(&paths, std::process::id() as i32).is_ok(),
                allowed
            );
            responder.join().unwrap();
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn controller_authority_requires_one_complete_unique_bus_name() {
        assert_eq!(
            parse_controller(b"UID=1000\nCONTROLLER=:1.27\nACTIVE=1\n").unwrap(),
            ":1.27"
        );
        for record in [
            b"UID=1000\n".as_slice(),
            b"CONTROLLER=org.freedesktop.login1\n",
            b"CONTROLLER=:1.27\nCONTROLLER=:1.28\n",
            b"CONTROLLER=:1.27",
            b"CONTROLLER=:\n",
            b"CONTROLLER=:1..27\n",
            b"CONTROLLER=:1.27/other\n",
        ] {
            assert!(parse_controller(record).is_err());
        }
    }

    #[test]
    fn focus_identity_rejects_unknown_layers_and_reused_window_addresses() {
        let first = parse_focus(b"focus|0x42|100|123").unwrap();
        let reused = parse_focus(b"focus|0x42|101|123").unwrap();
        assert_ne!(first, reused);
        for reply in [
            b"layer".as_slice(),
            b"none",
            b"cantrip",
            b"unknown",
            b"ok",
            b"focus|0x0|1|2",
            b"focus|0x42|1|0",
            b"focus|0x42|1|2|extra",
        ] {
            assert!(parse_focus(reply).is_err());
        }
    }

    #[test]
    fn nested_or_unknown_backends_do_not_authorize_automatic_delivery() {
        assert!(check_direct_desktop(br#"{"backend":"drm","configProvider":"lua"}"#).is_ok());
        for status in [
            br#"{"backend":"wayland","configProvider":"lua"}"#.as_slice(),
            br#"{"backend":"error","configProvider":"lua"}"#,
            br#"{"backend":"headless","configProvider":"lua"}"#,
            br#"{"backend":"drm","configProvider":"hyprlang"}"#,
            br#"{}"#,
        ] {
            assert!(check_direct_desktop(status).is_err());
        }
    }

    fn captured_monitor() -> (Guard, mpsc::Receiver<Request>) {
        let generation = Arc::new(AtomicU64::new(4));
        let (sender, receiver) = mpsc::sync_channel(1);
        let monitor = Arc::new(Monitor {
            paths: Arc::new(Paths {
                control: PathBuf::new(),
                events: PathBuf::new(),
                wayland: PathBuf::new(),
            }),
            requests: sender,
            generation,
            alive: Arc::new(AtomicBool::new(true)),
            last_poll: Arc::new(AtomicU64::new(clock_ns(libc::CLOCK_MONOTONIC).unwrap())),
            cached: Arc::new(Mutex::new(Err(UNKNOWN))),
            refresh_requested: Arc::new(AtomicBool::new(false)),
        });
        let guard = Guard {
            monitor: Some(monitor),
            stamp: Ok(Stamp {
                focus: parse_focus(b"focus|0x42|100|123").unwrap(),
                generation: 4,
                suspend_offset: suspend_offset().unwrap(),
                compositor: 123,
            }),
        };
        *guard.monitor.as_ref().unwrap().cached.lock() = guard.stamp;
        (guard, receiver)
    }

    #[test]
    fn lock_and_unlock_history_rejects_the_original_destination_permit() {
        let (guard, _receiver) = captured_monitor();
        assert!(guard.check_history().is_ok());
        let (mut consumer, mut publisher) = UnixStream::pair().unwrap();
        consumer.set_nonblocking(true).unwrap();
        publisher
            .write_all(b"activewindowv2>>\nactivewindowv2>>42\n")
            .unwrap();
        drain_focus_events(
            &mut consumer,
            &mut Vec::new(),
            &guard.monitor.as_ref().unwrap().generation,
            None,
        )
        .unwrap();
        // Same destination after unlock is not permission to replay old input.
        assert!(guard.check_history().is_err());
    }

    #[test]
    fn a_reconnected_monitor_accepts_fresh_captures_but_never_old_permits() {
        let (old, _receiver) = captured_monitor();
        let monitor = old.monitor.as_ref().unwrap();
        assert!(old.check_history().is_ok());
        invalidate_history(&monitor.generation, &monitor.alive, &monitor.last_poll);
        assert!(old.check_history().is_err());
        assert!(monitor.cached_stamp().is_err());
        let mut baseline = old.stamp.unwrap();
        baseline.generation = monitor.generation.load(Ordering::Acquire);
        baseline.suspend_offset = suspend_offset().unwrap();
        monitor
            .last_poll
            .store(clock_ns(libc::CLOCK_MONOTONIC).unwrap(), Ordering::Release);
        monitor.alive.store(true, Ordering::Release);
        *monitor.cached.lock() = Ok(baseline);
        let fresh = Guard {
            monitor: old.monitor.clone(),
            stamp: monitor.cached_stamp(),
        };
        assert!(fresh.check_history().is_ok());
        assert!(old.check_history().is_err());
    }

    #[test]
    fn cached_capture_defers_immediately_when_a_snapshot_is_being_published() {
        let (guard, _requests) = captured_monitor();
        let monitor = guard.monitor.unwrap();
        let publishing = monitor.cached.lock();
        let reader = monitor.clone();
        let (done, completion) = mpsc::sync_channel(1);
        let capture = std::thread::spawn(move || {
            done.send(reader.cached_stamp()).unwrap();
        });
        // Releasing the writer even on failure keeps a blocking-regression
        // failure finite instead of deadlocking the full test process.
        let result = completion.recv_timeout(Duration::from_millis(250));
        drop(publishing);
        capture.join().unwrap();
        assert!(result
            .expect("capture waited for the snapshot producer")
            .is_err());
    }

    #[test]
    fn cached_destinations_reject_stale_generation_suspend_and_observer_gaps() {
        let (guard, _requests) = captured_monitor();
        let monitor = guard.monitor.unwrap();
        assert!(monitor.cached_stamp().is_ok());
        monitor.generation.fetch_add(1, Ordering::AcqRel);
        assert!(monitor.cached_stamp().is_err());

        let mut baseline = guard.stamp.unwrap();
        baseline.generation = monitor.generation.load(Ordering::Acquire);
        baseline.suspend_offset = suspend_offset().unwrap().saturating_add(20_000_000);
        *monitor.cached.lock() = Ok(baseline);
        assert!(monitor.cached_stamp().is_err());

        baseline.suspend_offset = suspend_offset().unwrap();
        *monitor.cached.lock() = Ok(baseline);
        monitor.last_poll.store(0, Ordering::Release);
        assert!(monitor.cached_stamp().is_err());

        monitor
            .last_poll
            .store(clock_ns(libc::CLOCK_MONOTONIC).unwrap(), Ordering::Release);
        monitor.alive.store(false, Ordering::Release);
        assert!(monitor.cached_stamp().is_err());
    }

    #[test]
    fn subscription_requires_the_complete_matching_nonce_without_losing_focus_events() {
        let generation = AtomicU64::new(4);
        let (mut consumer, mut publisher) = UnixStream::pair().unwrap();
        consumer.set_nonblocking(true).unwrap();
        let expected = b"custom>>cantrip-delivery-ready:42";
        publisher
            .write_all(b"custom>>cantrip-delivery-ready:41\ncustom>>cantrip-delivery-ready:4")
            .unwrap();
        let mut pending = Vec::new();
        assert!(
            !drain_focus_events(&mut consumer, &mut pending, &generation, Some(expected)).unwrap()
        );
        publisher.write_all(b"2\nactivewindowv2>>\n").unwrap();
        assert!(
            drain_focus_events(&mut consumer, &mut pending, &generation, Some(expected)).unwrap()
        );
        assert_ne!(generation.load(Ordering::Acquire), 4);
    }

    #[test]
    fn incomplete_focus_event_is_not_treated_as_unchanged_history() {
        let generation = AtomicU64::new(4);
        let (mut consumer, mut publisher) = UnixStream::pair().unwrap();
        consumer.set_nonblocking(true).unwrap();
        publisher.write_all(b"activewindowv2>").unwrap();
        let mut pending = Vec::new();
        drain_focus_events(&mut consumer, &mut pending, &generation, None).unwrap();
        assert_ne!(generation.load(Ordering::Acquire), 4);
        publisher.write_all(b">42\n").unwrap();
        drain_focus_events(&mut consumer, &mut pending, &generation, None).unwrap();
        assert!(pending.is_empty());
    }
}
