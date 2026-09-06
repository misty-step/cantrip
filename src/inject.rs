//! Cancellable native Wayland keyboard delivery and bounded clipboard ownership.
//!
//! A successful result acknowledges the compositor/helper, not an application's
//! receipt of text. Once keys or a clipboard handoff may have happened, errors
//! are Uncertain and never start a fallback that could duplicate the payload.

use serde::{Deserialize, Serialize};
use std::{
    borrow::Cow,
    env, fmt,
    fs::File,
    io::{self, Write},
    os::{
        fd::{AsFd, AsRawFd, FromRawFd},
        unix::{fs::PermissionsExt, net::UnixStream, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use wayland_client::{
    backend::WaylandError,
    delegate_noop,
    protocol::{wl_callback, wl_keyboard, wl_registry, wl_seat},
    Connection, Dispatch, EventQueue, QueueHandle, WEnum,
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

const INJECT_TIMEOUT: Duration = Duration::from_secs(5);
const DISCOVERY_TIMEOUT: Duration = Duration::from_millis(500);
const TICK: Duration = Duration::from_millis(10);
const TYPE_CHUNK: usize = 128;
const CTRL_SHIFT: u32 = (1 << 2) | 1;

type Result<T, E = InjectionFailure> = std::result::Result<T, E>;

/// Intended destination and uninterrupted session history, captured at stop.
#[derive(Clone)]
pub struct DeliveryGuard {
    desktop: crate::desktop::Guard,
}

impl DeliveryGuard {
    /// Start desktop observation during daemon startup, without waiting for it.
    pub fn prepare() {
        crate::desktop::Guard::prepare();
    }

    /// Capture cached destination history immediately; an unready cache defers.
    pub fn capture() -> Self {
        Self {
            desktop: crate::desktop::Guard::capture(),
        }
    }

    fn check(&self, cancel: &AtomicBool) -> Result<()> {
        cancelled(cancel)?;
        self.desktop
            .check()
            .map_err(|message| failure(InjectionFailureKind::Deferred, message))?;
        cancelled(cancel)
    }

    fn check_history(&self, cancel: &AtomicBool) -> Result<()> {
        cancelled(cancel)?;
        self.desktop
            .check_history()
            .map_err(|message| failure(InjectionFailureKind::Deferred, message))
    }
}

/// Select the delivery policy. Type never reads or writes the clipboard.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum InjectionMode {
    #[default]
    Auto,
    /// Copy verbatim, then send Ctrl+Shift+V to the verified destination.
    Paste,
    Type,
    Clipboard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectionOutcome {
    Typed(&'static str),
    Pasted,
    Clipboard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectionFailureKind {
    Cancelled,
    Deferred,
    Failed,
    Uncertain,
}

#[derive(Debug)]
pub struct InjectionFailure {
    pub kind: InjectionFailureKind,
    pub message: String,
}

impl fmt::Display for InjectionFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for InjectionFailure {}

fn failure(kind: InjectionFailureKind, message: impl Into<String>) -> InjectionFailure {
    InjectionFailure {
        kind,
        message: message.into(),
    }
}

fn failed(message: &'static str) -> InjectionFailure {
    failure(InjectionFailureKind::Failed, message)
}

fn cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        Err(failure(
            InjectionFailureKind::Cancelled,
            "Delivery was cancelled before any further input.",
        ))
    } else {
        Ok(())
    }
}

fn check_deadline(deadline: Instant) -> Result<()> {
    if Instant::now() >= deadline {
        Err(failed(
            "Delivery exceeded its time limit; no further input was sent.",
        ))
    } else {
        Ok(())
    }
}

fn after_keys(error: InjectionFailure) -> InjectionFailure {
    failure(
        InjectionFailureKind::Uncertain,
        format!(
            "Keyboard delivery was interrupted and may be partial. No fallback was attempted. {}",
            error.message
        ),
    )
}

/// Deliver according to the snapshotted policy. Unknown/stale desktop safety is
/// always Deferred; the caller may explicitly choose clipboard recovery.
/// Explicit Clipboard ignores destination focus but still honors cancellation.
pub fn inject(
    text: &str,
    mode: InjectionMode,
    guard: &DeliveryGuard,
    cancel: &AtomicBool,
) -> Result<InjectionOutcome> {
    let deadline = Instant::now() + INJECT_TIMEOUT;
    cancelled(cancel)?;
    if text.is_empty() {
        return Err(failed("There is no text to deliver."));
    }
    if mode == InjectionMode::Clipboard {
        copy_text(text, cancel, None, deadline)?;
        return Ok(InjectionOutcome::Clipboard);
    }
    guard.check(cancel)?;
    let mut keyboard = match NativeKeyboard::open(guard, cancel, deadline) {
        Ok(keyboard) => keyboard,
        Err(error) if mode == InjectionMode::Auto && error.kind == InjectionFailureKind::Failed => {
            guard.check(cancel)?;
            copy_text(text, cancel, Some(guard), deadline)?;
            return Ok(InjectionOutcome::Clipboard);
        }
        Err(error) => return Err(error),
    };
    if mode == InjectionMode::Paste
        || (mode == InjectionMode::Auto && executable_in_path("wl-copy"))
    {
        keyboard.set_keymap(&['v'], cancel, guard)?;
        guard.check(cancel)?;
        match copy_text(text, cancel, Some(guard), deadline) {
            Ok(()) => {
                // Clipboard ownership can itself change focus on compositors
                // without data-control. Never dispatch from a pre-copy permit.
                guard.check(cancel).map_err(|error| {
                    failure(
                        error.kind,
                        format!(
                            "Text is on the clipboard, but no paste keys were sent. {}",
                            error.message
                        ),
                    )
                })?;
                keyboard.pair(1, CTRL_SHIFT, cancel, guard)?;
                guard.check(cancel).map_err(after_keys)?;
                return Ok(InjectionOutcome::Pasted);
            }
            Err(error)
                if mode == InjectionMode::Auto && error.kind == InjectionFailureKind::Failed =>
            {
                // Only a failure *before EOF/handoff* is safe to fall back from.
                // Neither a possibly changed selection nor possible keys retry.
            }
            Err(error) => return Err(error),
        }
    }
    let typed = normalize_for_typing(text);
    keyboard.type_text(&typed, cancel, guard)?;
    guard.check(cancel).map_err(after_keys)?;
    Ok(InjectionOutcome::Typed("virtual-keyboard"))
}

/// Policy for doctor, given actual protocol discovery and helper availability.
/// This does not imply that a destination or its lock history is safe.
pub fn planned_backend_names(
    mode: InjectionMode,
    virtual_keyboard: bool,
    wl_copy: bool,
) -> Vec<&'static str> {
    match mode {
        InjectionMode::Clipboard => {
            if wl_copy {
                vec!["clipboard"]
            } else {
                vec![]
            }
        }
        InjectionMode::Type => {
            if virtual_keyboard {
                vec!["virtual-keyboard"]
            } else {
                vec![]
            }
        }
        InjectionMode::Paste => {
            if virtual_keyboard && wl_copy {
                vec!["paste"]
            } else {
                vec![]
            }
        }
        InjectionMode::Auto => {
            let mut backends = Vec::with_capacity(3);
            if virtual_keyboard && wl_copy {
                backends.push("paste");
            }
            if virtual_keyboard {
                backends.push("virtual-keyboard");
            }
            if wl_copy {
                backends.push("clipboard");
            }
            backends
        }
    }
}

/// Bounded, read-only registry discovery. Does not create a virtual keyboard or
/// issue input, and deliberately refuses ambiguous multi-seat connections.
pub fn virtual_keyboard_available() -> bool {
    let Ok(path) = crate::desktop::wayland_socket_path() else {
        return false;
    };
    WaylandSession::connect(&path, None, DISCOVERY_TIMEOUT, &mut || Ok(())).is_ok()
}

/// True when `name` resolves to an executable regular file on PATH.
pub fn executable_in_path(name: &str) -> bool {
    let Some(path_var) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&path_var).any(|directory| {
        let directory = if directory.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            directory
        };
        directory
            .join(name)
            .metadata()
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    })
}

/// Controls must not become Return/Tab/Escape in a live application. Unicode
/// prose is preserved; consecutive controls collapse into one literal space.
fn normalize_for_typing(text: &str) -> Cow<'_, str> {
    if !text.chars().any(char::is_control) {
        return Cow::Borrowed(text);
    }
    let mut result = String::with_capacity(text.len());
    let mut pending_space = false;
    for character in text.chars() {
        if character.is_control() {
            pending_space = true;
        } else {
            if pending_space {
                result.push(' ');
                pending_space = false;
            }
            result.push(character);
        }
    }
    if pending_space {
        result.push(' ');
    }
    Cow::Owned(result)
}

#[derive(Default)]
struct Registry {
    seat: Option<wl_seat::WlSeat>,
    seats: usize,
    manager: Option<ZwpVirtualKeyboardManagerV1>,
    seat_global: Option<u32>,
    manager_global: Option<u32>,
    keyboard_capability: bool,
    removed: bool,
    completed: u64,
}

impl Dispatch<wl_registry::WlRegistry, ()> for Registry {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        queue: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                if interface == "wl_seat" {
                    state.seats += 1;
                    if state.seats != 1 {
                        state.removed = true;
                    }
                    if state.seat.is_none() {
                        state.seat = Some(registry.bind(name, version.min(7), queue, ()));
                        state.seat_global = Some(name);
                    }
                } else if interface == "zwp_virtual_keyboard_manager_v1" {
                    state.manager = Some(registry.bind(name, 1, queue, ()));
                    state.manager_global = Some(name);
                }
            }
            wl_registry::Event::GlobalRemove { name }
                if Some(name) == state.seat_global || Some(name) == state.manager_global =>
            {
                state.removed = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for Registry {
    fn event(
        state: &mut Self,
        _: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities } = event {
            let keyboard = matches!(capabilities, WEnum::Value(value) if value.contains(wl_seat::Capability::Keyboard));
            if state.keyboard_capability && !keyboard {
                state.removed = true;
            }
            state.keyboard_capability = keyboard;
        }
    }
}

impl Dispatch<wl_callback::WlCallback, u64> for Registry {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        serial: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.completed = state.completed.max(*serial);
    }
}

delegate_noop!(Registry: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(Registry: ignore ZwpVirtualKeyboardV1);

struct WaylandSession {
    connection: Connection,
    queue: EventQueue<Registry>,
    registry: Registry,
    socket: UnixStream,
    serial: u64,
}

impl WaylandSession {
    fn connect(
        path: &Path,
        compositor: Option<i32>,
        timeout: Duration,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Self> {
        let deadline = Instant::now() + timeout;
        check()?;
        check_deadline(deadline)?;
        let socket =
            crate::desktop::connect_unix(path, deadline.saturating_duration_since(Instant::now()))
                .map_err(|_| failed("Cannot connect to the Wayland compositor."))?;
        if compositor.is_some_and(|pid| crate::desktop::peer_pid(&socket).ok() != Some(pid)) {
            return Err(failure(
                InjectionFailureKind::Deferred,
                "The input connection does not belong to the captured compositor.",
            ));
        }
        let owned = socket
            .try_clone()
            .map_err(|_| failed("Cannot retain the Wayland connection."))?;
        let connection = Connection::from_socket(owned)
            .map_err(|_| failed("Cannot initialize the Wayland connection."))?;
        let queue = connection.new_event_queue();
        connection.display().get_registry(&queue.handle(), ());
        let mut session = Self {
            connection,
            queue,
            registry: Registry::default(),
            socket,
            serial: 0,
        };
        session.sync(deadline, check)?;
        // The first roundtrip discovers globals; binds made by those callbacks
        // require a second roundtrip for the seat's capabilities.
        session.sync(deadline, check)?;
        if session.registry.seats != 1
            || session.registry.seat.is_none()
            || !session.registry.keyboard_capability
            || session.registry.manager.is_none()
            || session.registry.removed
        {
            return Err(failed(
                "A single keyboard seat and Wayland virtual-keyboard protocol are required.",
            ));
        }
        Ok(session)
    }

    fn sync(&mut self, deadline: Instant, check: &mut impl FnMut() -> Result<()>) -> Result<()> {
        self.serial += 1;
        let serial = self.serial;
        self.connection.display().sync(&self.queue.handle(), serial);
        loop {
            check()?;
            if Instant::now() >= deadline {
                return Err(failed(
                    "The Wayland compositor did not acknowledge delivery within its time limit.",
                ));
            }
            match self.connection.flush() {
                Ok(()) => {}
                Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => return Err(failed("The Wayland input connection failed.")),
            }
            self.queue
                .dispatch_pending(&mut self.registry)
                .map_err(|_| failed("The Wayland input protocol failed."))?;
            if self.registry.removed {
                return Err(failed(
                    "The compositor changed the input seat or keyboard protocol.",
                ));
            }
            if self.registry.completed >= serial {
                return check();
            }
            if let Some(read) = self.connection.prepare_read() {
                let mut poll = libc::pollfd {
                    fd: read.connection_fd().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let ready = unsafe { libc::poll(&mut poll, 1, 10) };
                if ready < 0 {
                    if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(failed("Cannot monitor the Wayland input connection."));
                }
                if ready > 0 {
                    match read.read() {
                        Ok(_) => {}
                        Err(WaylandError::Io(error))
                            if error.kind() == io::ErrorKind::WouldBlock => {}
                        Err(_) => return Err(failed("The Wayland input connection closed.")),
                    }
                }
            }
        }
    }
}

struct NativeKeyboard {
    session: WaylandSession,
    keyboard: ZwpVirtualKeyboardV1,
    started: Instant,
    deadline: Instant,
    keys_possible: bool,
    poisoned: bool,
    keymap_ready: bool,
}

impl NativeKeyboard {
    fn open(guard: &DeliveryGuard, cancel: &AtomicBool, deadline: Instant) -> Result<Self> {
        check_deadline(deadline)?;
        let path = guard
            .desktop
            .socket_path()
            .map_err(|message| failure(InjectionFailureKind::Deferred, message))?;
        let compositor = guard
            .desktop
            .compositor()
            .map_err(|message| failure(InjectionFailureKind::Deferred, message))?;
        let session = WaylandSession::connect(
            path,
            Some(compositor),
            deadline
                .saturating_duration_since(Instant::now())
                .min(DISCOVERY_TIMEOUT),
            &mut || guard.check_history(cancel),
        )?;
        let registry = &session.registry;
        let manager = registry
            .manager
            .as_ref()
            .ok_or_else(|| failed("Virtual keyboard disappeared."))?;
        let seat = registry
            .seat
            .as_ref()
            .ok_or_else(|| failed("Keyboard seat disappeared."))?;
        let keyboard = manager.create_virtual_keyboard(seat, &session.queue.handle(), ());
        Ok(Self {
            session,
            keyboard,
            started: Instant::now(),
            deadline,
            keys_possible: false,
            poisoned: false,
            keymap_ready: false,
        })
    }

    fn set_keymap(
        &mut self,
        characters: &[char],
        cancel: &AtomicBool,
        guard: &DeliveryGuard,
    ) -> Result<()> {
        guard
            .check_history(cancel)
            .map_err(|error| self.classify(error))?;
        check_deadline(self.deadline).map_err(|error| self.classify(error))?;
        let keymap = keymap_file(characters).map_err(|error| self.classify(error))?;
        let length = keymap
            .metadata()
            .map_err(|_| self.classify(failed("Cannot read the input keymap size.")))?
            .len();
        self.keyboard.keymap(
            wl_keyboard::KeymapFormat::XkbV1 as u32,
            keymap.as_fd(),
            length as u32,
        );
        self.keymap_ready = true;
        self.session
            .sync(
                self.deadline.min(Instant::now() + DISCOVERY_TIMEOUT),
                &mut || guard.check_history(cancel),
            )
            .map_err(|error| self.classify(error))
    }

    fn pair(
        &mut self,
        code: u32,
        modifiers: u32,
        cancel: &AtomicBool,
        guard: &DeliveryGuard,
    ) -> Result<()> {
        // Layer interactivity can change without a Hyprland window event.
        // History alone cannot authorize another key, even within one chunk.
        guard.check(cancel).map_err(|error| self.classify(error))?;
        check_deadline(self.deadline).map_err(|error| self.classify(error))?;
        // Commit one pair, never a whole transcript. Once committed, releases
        // and the modifier reset must be queued even if cancellation arrives.
        // No waits occur between press/release/reset. A successful flush sends
        // the entire pair; failed/backpressured flushes never flush queued keys
        // again during cleanup or fallback.
        self.keys_possible = true;
        let time = self.started.elapsed().as_millis() as u32;
        self.keyboard.modifiers(modifiers, 0, 0, 0);
        self.keyboard
            .key(time, code, wl_keyboard::KeyState::Pressed as u32);
        self.keyboard
            .key(time, code, wl_keyboard::KeyState::Released as u32);
        self.keyboard.modifiers(0, 0, 0, 0);
        if self.session.connection.flush().is_err() {
            self.poisoned = true;
            let _ = self.session.socket.shutdown(std::net::Shutdown::Both);
            return Err(after_keys(failed(
                "Input flush failed; key release could not be confirmed.",
            )));
        }
        self.session
            .sync(
                self.deadline.min(Instant::now() + DISCOVERY_TIMEOUT),
                &mut || guard.check_history(cancel),
            )
            .map_err(after_keys)
    }

    fn type_text(&mut self, text: &str, cancel: &AtomicBool, guard: &DeliveryGuard) -> Result<()> {
        let mut characters = text.chars();
        let mut chunk = ['\0'; TYPE_CHUNK];
        loop {
            let mut length = 0;
            for destination in &mut chunk {
                let Some(character) = characters.next() else {
                    break;
                };
                *destination = character;
                length += 1;
            }
            if length == 0 {
                return Ok(());
            }
            // A bounded keymap supports arbitrary Unicode without an unbounded
            // all-transcript character/index copy or assumptions about layout.
            self.set_keymap(&chunk[..length], cancel, guard)?;
            for index in 0..length {
                self.pair(index as u32 + 1, 0, cancel, guard)?;
            }
        }
    }

    fn classify(&self, error: InjectionFailure) -> InjectionFailure {
        if self.keys_possible {
            after_keys(error)
        } else {
            error
        }
    }
}

impl Drop for NativeKeyboard {
    fn drop(&mut self) {
        if !self.poisoned && self.keymap_ready {
            // This is our own virtual device, not a new helper modifying the
            // user's physical modifiers. There are no queued press requests on
            // this path. The failed-flush path shuts down without flushing.
            self.keyboard.modifiers(0, 0, 0, 0);
            self.keyboard.destroy();
            let _ = self.session.connection.flush();
        }
        let _ = self.session.socket.shutdown(std::net::Shutdown::Both);
    }
}

fn keymap_file(characters: &[char]) -> Result<File> {
    use std::fmt::Write as _;
    let mut keymap = String::with_capacity(characters.len() * 80 + 256);
    writeln!(
        keymap,
        "xkb_keymap {{\nxkb_keycodes \"cantrip\" {{ minimum=8; maximum={};",
        characters.len() + 9
    )
    .map_err(|_| failed("Cannot prepare the input keymap."))?;
    for index in 0..characters.len() {
        writeln!(keymap, "<C{index}>={};", index + 9)
            .map_err(|_| failed("Cannot prepare the input keymap."))?;
    }
    keymap.push_str("};\nxkb_types \"cantrip\" { include \"complete\" };\nxkb_compatibility \"cantrip\" { include \"complete\" };\nxkb_symbols \"cantrip\" {\n");
    for (index, character) in characters.iter().enumerate() {
        // XKB's Uxxxx syntax represents a Unicode scalar, including non-Latin
        // and supplementary-plane characters, independent of the user's layout.
        writeln!(
            keymap,
            "key <C{index}> {{ type=\"ONE_LEVEL\", [ U{:04X} ] }};",
            *character as u32
        )
        .map_err(|_| failed("Cannot prepare the input keymap."))?;
    }
    keymap.push_str("};\n};\n\0");
    // SAFETY: the constant name is NUL terminated; a successful call returns a
    // new owned anonymous fd. The keymap never becomes a named transcript file.
    let fd = unsafe { libc::memfd_create(c"cantrip-keymap".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(failed("Cannot create an anonymous input keymap."));
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(keymap.as_bytes())
        .map_err(|_| failed("Cannot write the input keymap."))?;
    Ok(file)
}

fn copy_text(
    text: &str,
    cancel: &AtomicBool,
    guard: Option<&DeliveryGuard>,
    deadline: Instant,
) -> Result<()> {
    let mut command = Command::new("wl-copy");
    // Explicit text MIME avoids wl-copy spawning xdg-mime and prevents format
    // guessing from changing paragraph-preserving clipboard text.
    command
        .args(["--type", "text/plain;charset=utf-8"])
        .env_remove("WAYLAND_DEBUG");
    let mut check = || {
        cancelled(cancel)?;
        if let Some(guard) = guard {
            // Recheck current layer/session state at EOF and acknowledgement,
            // not only the event history used while native input is in flight.
            guard.check(cancel)?;
        }
        Ok(())
    };
    run_writer_backend(
        &mut command,
        text.as_bytes(),
        deadline.saturating_duration_since(Instant::now()),
        &mut check,
    )
}

/// Own exactly one process group. WNOWAIT observes exit without reaping the
/// leader, so its PID/group cannot be reused before we terminate an aborted
/// handoff. wl-copy 2.3 forks a selection owner only after setting selection;
/// on success that owner intentionally remains alive to serve future pastes.
struct OwnedHelper {
    child: Child,
    armed: bool,
}

impl OwnedHelper {
    fn spawn(command: &mut Command) -> Result<Self> {
        let child = command
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| {
                failed("Cannot start wl-copy; install wl-clipboard for clipboard delivery.")
            })?;
        Ok(Self { child, armed: true })
    }

    fn exited(&self) -> io::Result<Option<bool>> {
        let mut status: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: status is valid writable storage; WNOWAIT keeps ownership of
        // this exact child until the following wait/kill path completes.
        if unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.id(),
                &mut status,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        if unsafe { status.si_pid() } == 0 {
            return Ok(None);
        }
        Ok(Some(
            status.si_code == libc::CLD_EXITED && unsafe { status.si_status() } == 0,
        ))
    }

    fn commit(mut self) -> Result<()> {
        self.armed = false;
        self.child
            .wait()
            .map_err(|_| failed("Cannot reap the clipboard helper."))?;
        Ok(())
    }
}

impl Drop for OwnedHelper {
    fn drop(&mut self) {
        if self.armed {
            // Only the group created for this child. Never signal ydotoold,
            // another wl-copy, or any process found through a name/PID scan.
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn run_writer_backend(
    command: &mut Command,
    bytes: &[u8],
    timeout: Duration,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    check()?;
    check_deadline(deadline)?;
    let mut helper = OwnedHelper::spawn(command)?;
    // Keep stdin inside the owned child: on early return Drop must kill/reap
    // the helper *before* stdin closes, otherwise cancellation itself sends EOF
    // and could let wl-copy install a late selection.
    set_nonblocking(
        helper
            .child
            .stdin
            .as_ref()
            .ok_or_else(|| failed("Cannot open clipboard helper input."))?,
    )
    .map_err(|_| failed("Cannot make clipboard input interruptible."))?;
    let mut offset = 0;
    while offset < bytes.len() {
        check()?;
        if Instant::now() >= deadline {
            return Err(failed(
                "Clipboard helper did not drain its input within the time limit.",
            ));
        }
        match helper
            .child
            .stdin
            .as_mut()
            .ok_or_else(|| failed("Clipboard helper input disappeared."))?
            .write(&bytes[offset..])
        {
            Ok(0) => return Err(failed("Clipboard helper stopped accepting text.")),
            Ok(written) => offset += written,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(TICK),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(failed("Cannot write to the clipboard helper.")),
        }
    }
    check()?;
    check_deadline(deadline)?;
    // Source-verified wl-copy reads all stdin before creating its selection.
    // Dropping EOF is the handoff boundary, after which effects are ambiguous.
    drop(helper.child.stdin.take());
    let uncertain = |error: InjectionFailure| {
        failure(
            InjectionFailureKind::Uncertain,
            format!(
                "Clipboard handoff may have changed the selection. No keys were sent. {}",
                error.message
            ),
        )
    };
    loop {
        check().map_err(uncertain)?;
        if Instant::now() >= deadline {
            return Err(uncertain(failed(
                "Clipboard helper did not acknowledge the handoff within the time limit.",
            )));
        }
        match helper
            .exited()
            .map_err(|_| uncertain(failed("Cannot monitor the clipboard helper.")))?
        {
            Some(true) => {
                check().map_err(uncertain)?;
                check_deadline(deadline).map_err(uncertain)?;
                return helper.commit().map_err(uncertain);
            }
            Some(false) => {
                return Err(uncertain(failed(
                    "Clipboard helper exited without a successful handoff.",
                )))
            }
            None => std::thread::sleep(TICK),
        }
    }
}

fn set_nonblocking(stream: &impl AsRawFd) -> io::Result<()> {
    let fd = stream.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = env::temp_dir().join(format!(
                "cantrip-delivery-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn helper(&self, body: &str) -> Command {
            let mut command = Command::new("/bin/sh");
            command
                .args(["-c", body, "clipboard-test"])
                .arg(self.0.join("pid"))
                .arg(self.0.join("ready"));
            command
        }
        fn assert_reaped(&self) {
            let pid = std::fs::read_to_string(self.0.join("pid"))
                .unwrap()
                .trim()
                .parse::<i32>()
                .unwrap();
            let mut status = 0;
            assert_eq!(
                unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
                -1
            );
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn type_policy_never_uses_a_clipboard_even_without_keyboard_support() {
        assert!(planned_backend_names(InjectionMode::Type, false, true).is_empty());
        assert_eq!(
            planned_backend_names(InjectionMode::Type, true, true),
            ["virtual-keyboard"]
        );
        assert!(planned_backend_names(InjectionMode::Paste, true, false).is_empty());
    }

    #[test]
    fn typing_preserves_unicode_but_cannot_submit_or_navigate() {
        assert_eq!(
            normalize_for_typing("Καλημέρα\n\t世界\r\n🙂\u{1b}fin"),
            "Καλημέρα 世界 🙂 fin"
        );
        assert_eq!(normalize_for_typing("\nstart\0\x7fend\t"), " start end ");
        assert_eq!(
            normalize_for_typing("Écriture — café, 中文, 🦀"),
            "Écriture — café, 中文, 🦀"
        );
    }

    #[test]
    fn cancellation_before_spawn_does_not_run_the_helper() {
        let scratch = Scratch::new();
        let mut command = scratch.helper("printf '%s' $$ > \"$1\"");
        let error = run_writer_backend(
            &mut command,
            b"private text",
            Duration::from_secs(1),
            &mut || cancelled(&AtomicBool::new(true)),
        )
        .unwrap_err();
        assert_eq!(error.kind, InjectionFailureKind::Cancelled);
        assert!(!scratch.0.join("pid").exists());
        assert!(!error.to_string().contains("private text"));
    }

    #[test]
    fn a_preflight_check_cannot_restart_the_helper_deadline() {
        let scratch = Scratch::new();
        let mut command =
            scratch.helper("printf '%s' $$ > \"$1\"; cat > /dev/null; printf ready > \"$2\"");
        let mut first_check = true;
        let error = run_writer_backend(
            &mut command,
            b"private text",
            Duration::from_millis(10),
            &mut || {
                if std::mem::take(&mut first_check) {
                    std::thread::sleep(Duration::from_millis(30));
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.kind, InjectionFailureKind::Failed);
        assert!(!scratch.0.join("pid").exists());
        assert!(!scratch.0.join("ready").exists());
    }

    #[test]
    fn cancellation_interrupts_a_full_pipe_and_reaps_the_owned_helper() {
        let scratch = Scratch::new();
        let mut command = scratch.helper("printf '%s' $$ > \"$1\"; exec sleep 30");
        let payload = vec![b'x'; 1 << 20];
        let error = run_writer_backend(&mut command, &payload, Duration::from_secs(2), &mut || {
            if std::fs::read_to_string(scratch.0.join("pid"))
                .ok()
                .and_then(|text| text.parse::<u32>().ok())
                .is_some()
            {
                Err(failure(InjectionFailureKind::Cancelled, "cancelled"))
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert_eq!(error.kind, InjectionFailureKind::Cancelled);
        scratch.assert_reaped();
    }

    #[test]
    fn cancellation_after_eof_is_uncertain_not_safe_to_retry() {
        let scratch = Scratch::new();
        let mut command = scratch.helper(
            "printf '%s' $$ > \"$1\"; cat > /dev/null; printf ready > \"$2\"; exec sleep 30",
        );
        let error = run_writer_backend(
            &mut command,
            b"private text",
            Duration::from_secs(2),
            &mut || {
                if scratch.0.join("ready").exists() {
                    Err(failure(InjectionFailureKind::Cancelled, "cancelled"))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();
        assert_eq!(error.kind, InjectionFailureKind::Uncertain);
        assert!(!error.to_string().contains("private text"));
        scratch.assert_reaped();
    }

    #[test]
    fn failed_exit_after_possible_selection_cannot_trigger_a_typing_fallback() {
        let scratch = Scratch::new();
        let mut command = scratch.helper("printf '%s' $$ > \"$1\"; cat > /dev/null; exit 7");
        let error = run_writer_backend(
            &mut command,
            b"private text",
            Duration::from_secs(2),
            &mut || Ok(()),
        )
        .unwrap_err();
        assert_eq!(error.kind, InjectionFailureKind::Uncertain);
        scratch.assert_reaped();
    }

    #[test]
    fn cancellation_terminates_the_helpers_owned_process_group() {
        let scratch = Scratch::new();
        let mut command =
            scratch.helper("printf '%s' $$ > \"$1\"; sleep 30 & printf '%s' $! > \"$2\"; wait");
        let error = run_writer_backend(
            &mut command,
            &vec![b'x'; 1 << 20],
            Duration::from_secs(2),
            &mut || {
                if std::fs::read_to_string(scratch.0.join("ready"))
                    .ok()
                    .and_then(|text| text.parse::<u32>().ok())
                    .is_some()
                {
                    Err(failure(InjectionFailureKind::Cancelled, "cancelled"))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();
        assert_eq!(error.kind, InjectionFailureKind::Cancelled);
        scratch.assert_reaped();
        let descendant = std::fs::read_to_string(scratch.0.join("ready"))
            .unwrap()
            .parse::<u32>()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let state = std::fs::read_to_string(format!("/proc/{descendant}/stat"))
                .ok()
                .and_then(|stat| {
                    stat.rsplit_once(')')
                        .map(|(_, fields)| fields.trim_start().starts_with('Z'))
                });
            if state.is_none() || state == Some(true) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "owned descendant survived helper cancellation"
            );
            std::thread::sleep(TICK);
        }
    }

    #[test]
    fn helper_timeout_covers_the_write_not_only_the_wait() {
        let scratch = Scratch::new();
        let mut command = scratch.helper("printf '%s' $$ > \"$1\"; exec sleep 30");
        let started = Instant::now();
        let error = run_writer_backend(
            &mut command,
            &vec![b'x'; 1 << 20],
            Duration::from_millis(100),
            &mut || Ok(()),
        )
        .unwrap_err();
        assert_eq!(error.kind, InjectionFailureKind::Failed);
        assert!(started.elapsed() < Duration::from_secs(2));
        scratch.assert_reaped();
    }
}
