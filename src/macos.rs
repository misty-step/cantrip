//! The macOS menu-bar app: the one owner of the shared engine, the native HUD,
//! the global dictation shortcut and the deliberately opened windows.
//!
//! The engine runs on its own std thread through the same `daemon::run` host as
//! Linux, and this process reaches it only through its IPC contract: the menu,
//! the shortcut and the HUD hold no second dictation state. Settings and
//! Recordings and recovery are the shared windows, opened as their own
//! processes, so closing one never stops dictation. macOS delivery is explicit
//! clipboard copy and manual paste; nothing here requests Accessibility, Input
//! Monitoring or Screen Recording access.

mod mark;
pub(crate) mod shortcut;

use crate::capture::{self, MicrophonePermission};
use crate::hud::macos::{runtime_dir, LiveHud};
use anyhow::{Context, Result};
use cantrip_engine::{
    config::Config,
    ipc::{self, Command, Completeness, StateKind},
    paths,
};
use mark::{Mark, Tone, CHASE_STEP};
use objc2::{
    define_class, msg_send,
    rc::{autoreleasepool, Retained},
    runtime::{AnyObject, ProtocolObject, Sel},
    sel, DefinedClass, MainThreadMarker, MainThreadOnly,
};
use objc2_app_kit::{
    NSAccessibility, NSApplication, NSApplicationActivationOptions, NSApplicationActivationPolicy,
    NSApplicationDelegate, NSApplicationTerminateReply, NSBeep, NSControlStateValueOff,
    NSControlStateValueOn, NSMenu, NSMenuDelegate, NSMenuItem, NSRunningApplication, NSStatusBar,
    NSStatusBarButton, NSStatusItem, NSVariableStatusItemLength, NSWorkspace,
    NSWorkspaceOpenConfiguration,
};
use objc2_foundation::{
    ns_string, NSBundle, NSNotification, NSObject, NSObjectProtocol, NSRunLoop,
    NSRunLoopCommonModes, NSString, NSTimer, NSURL,
};
use objc2_service_management::{SMAppService, SMAppServiceStatus};
use shortcut::{Registration, Shortcut};
use std::{
    cell::RefCell,
    fs,
    process::{Child, Command as Process, Stdio},
    sync::mpsc,
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime},
};

/// A just-started engine binds its socket within this window.
const STARTUP_GRACE: Duration = Duration::from_secs(15);
/// Configuration, microphone access and the login item are re-read this often.
const SETTINGS_INTERVAL: Duration = Duration::from_secs(2);
/// Menu lines stay readable at the menu's default width.
const LINE_LIMIT: usize = 72;
const MICROPHONE_SETTINGS: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone";

/// Run the menu-bar app until Quit, logout or a termination signal. A second
/// copy exits at once and leaves the running app in charge.
pub fn run() -> Result<()> {
    let mtm = MainThreadMarker::new().context("the Cantrip app must start on the main thread")?;
    let Some(lock) = crate::hud::acquire_lock_on(&runtime_dir()?.join("app.lock"))? else {
        eprintln!("Cantrip is already running in the menu bar.");
        return Ok(());
    };
    let app = NSApplication::sharedApplication(mtm);
    // A menu-bar app, like LSUIElement in the bundle: no Dock icon or app menu.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let delegate = AppDelegate::new(mtm, lock);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    app.run();
    Ok(())
}

/// Open Cantrip through LaunchServices: launch the app bundle, or send the
/// running app its reopen event, which starts a stopped engine. An unbundled
/// build starts its menu-bar host directly; a second host exits by itself.
pub(crate) fn open_app() -> Result<()> {
    if let Some(bundle) = autoreleasepool(|_| app_bundle()) {
        autoreleasepool(|_| {
            NSWorkspace::sharedWorkspace().openApplicationAtURL_configuration_completionHandler(
                &bundle,
                &NSWorkspaceOpenConfiguration::configuration(),
                None,
            );
        });
        return Ok(());
    }
    let mut child = Process::new(std::env::current_exe()?)
        .arg("app")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("Starting Cantrip")?;
    thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Open System Settings at Privacy & Security › Microphone. Allowing access
/// stays the operator's decision there; this changes no permission.
pub(crate) fn open_microphone_settings() -> Result<()> {
    autoreleasepool(|_| {
        let url = NSURL::URLWithString(&NSString::from_str(MICROPHONE_SETTINGS))
            .context("composing the Privacy & Security link")?;
        anyhow::ensure!(
            NSWorkspace::sharedWorkspace().openURL(&url),
            "System Settings did not open"
        );
        Ok(())
    })
}

/// The running `.app` bundle, if this executable lives in one.
fn app_bundle() -> Option<Retained<NSURL>> {
    let bundle = NSBundle::mainBundle();
    (bundle.bundleIdentifier().is_some() && bundle.bundlePath().to_string().ends_with(".app"))
        .then(|| bundle.bundleURL())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    Shortcut,
    Menu,
}

struct Request {
    command: Command,
    origin: Origin,
}

struct Reply {
    origin: Origin,
    /// None when Cantrip accepted the request.
    problem: Option<String>,
    reachable: bool,
}

/// One worker submits requests in order, so the AppKit thread never waits on IPC.
fn spawn_commands() -> Result<(mpsc::Sender<Request>, mpsc::Receiver<Reply>)> {
    let (requests, incoming) = mpsc::channel::<Request>();
    let (replies, results) = mpsc::channel();
    thread::Builder::new()
        .name("cantrip-commands".to_owned())
        .spawn(move || {
            for Request { command, origin } in incoming {
                let (problem, reachable) =
                    match ipc::command(command) {
                        Ok(reply) if reply.ok => (None, true),
                        Ok(reply) => (
                            Some(reply.error.or(reply.message).unwrap_or_else(|| {
                                "Cantrip did not accept this action.".to_owned()
                            })),
                            true,
                        ),
                        Err(_) => (
                            Some("Cantrip is not running. Choose Start Cantrip.".to_owned()),
                            false,
                        ),
                    };
                let reply = Reply {
                    origin,
                    problem,
                    reachable,
                };
                if replies.send(reply).is_err() {
                    break;
                }
            }
        })
        .context("starting the command worker")?;
    Ok((requests, results))
}

enum EngineExit {
    Stopped,
    External,
}

enum Engine {
    /// This app's engine thread.
    Owned {
        thread: JoinHandle<Result<EngineExit>>,
        started: Instant,
    },
    /// Another Cantrip process already served dictation; this app never starts a second.
    External,
    /// Not running here, with the reason when it ended in an error.
    Stopped(Option<String>),
}

#[derive(Clone, Copy)]
enum Window {
    Settings,
    Recordings,
    CheckSetup,
}

impl Window {
    fn arguments(self) -> &'static [&'static str] {
        match self {
            Self::Settings => &["settings"],
            Self::Recordings => &["actions"],
            Self::CheckSetup => &["actions", "--doctor"],
        }
    }
}

/// The shared Settings and Actions windows, one process each.
#[derive(Default)]
struct Windows {
    settings: Option<Child>,
    actions: Option<Child>,
}

impl Windows {
    /// Bring an open window forward, or open it.
    fn open(&mut self, window: Window) -> Result<()> {
        let slot = match window {
            Window::Settings => &mut self.settings,
            Window::Recordings | Window::CheckSetup => &mut self.actions,
        };
        if let Some(child) = slot.as_mut() {
            if matches!(child.try_wait(), Ok(None)) {
                if let Some(app) =
                    NSRunningApplication::runningApplicationWithProcessIdentifier(child.id() as _)
                {
                    app.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows);
                }
                return Ok(());
            }
        }
        *slot = Some(
            Process::new(std::env::current_exe()?)
                .args(window.arguments())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .spawn()
                .context("Opening the window")?,
        );
        Ok(())
    }

    /// Reap closed windows; dictation continues regardless.
    fn reap(&mut self) {
        for slot in [&mut self.settings, &mut self.actions] {
            if slot
                .as_mut()
                .is_some_and(|child| !matches!(child.try_wait(), Ok(None)))
            {
                *slot = None;
            }
        }
    }
}

/// The global toggle and why the configured one is not active, if it is not.
struct Shortcuts {
    registration: Option<Registration>,
    active: Option<Shortcut>,
    problem: Option<String>,
    /// The configuration file's modification time when last applied.
    seen: Option<Option<SystemTime>>,
}

impl Shortcuts {
    /// Register the configured shortcut when the configuration file changed.
    fn refresh(&mut self) {
        let path = paths::config_file().ok();
        let stamp = path
            .and_then(|path| fs::metadata(path).ok())
            .and_then(|metadata| metadata.modified().ok());
        if self.seen == Some(stamp) {
            return;
        }
        self.seen = Some(stamp);
        let Some(registration) = self.registration.as_mut() else {
            return;
        };
        let configured = match Config::load() {
            Ok(config) => Shortcut::configured(config.hotkey.as_deref()),
            Err(_) => {
                Err("The configuration needs repair before the shortcut can change.".to_owned())
            }
        };
        self.problem = match configured {
            Ok(shortcut) => match registration.register(&shortcut) {
                Ok(()) => {
                    self.active = Some(shortcut);
                    None
                }
                Err(problem) => Some(problem),
            },
            Err(problem) => Some(problem),
        };
    }

    fn title(&self) -> String {
        match (&self.active, &self.problem) {
            (Some(active), None) => format!("Dictation shortcut: {}", active.label()),
            (Some(active), Some(_)) => {
                format!("Shortcut {} kept; new one refused…", active.label())
            }
            (None, _) => "Shortcut unavailable; open Settings…".to_owned(),
        }
    }
}

/// The take and outcome the open menu describes; actions never retarget a later take.
#[derive(Default)]
struct Targets {
    take: Option<String>,
    event: Option<u64>,
}

struct Menu {
    menu: Retained<NSMenu>,
    state: Retained<NSMenuItem>,
    detail: Retained<NSMenuItem>,
    outcome: Retained<NSMenuItem>,
    problem: Retained<NSMenuItem>,
    start: Retained<NSMenuItem>,
    stop: Retained<NSMenuItem>,
    cancel: Retained<NSMenuItem>,
    shortcut: Retained<NSMenuItem>,
    copy: Retained<NSMenuItem>,
    recover_local: Retained<NSMenuItem>,
    recover_provider: Retained<NSMenuItem>,
    install_model: Retained<NSMenuItem>,
    dismiss: Retained<NSMenuItem>,
    microphone: Retained<NSMenuItem>,
    start_engine: Retained<NSMenuItem>,
    login: Retained<NSMenuItem>,
}

impl Menu {
    fn new(
        mtm: MainThreadMarker,
        target: &AnyObject,
        delegate: &ProtocolObject<dyn NSMenuDelegate>,
    ) -> Self {
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        menu.setDelegate(Some(delegate));
        let add = |title: &str, action: Option<Sel>, key: &str| {
            // SAFETY: every action names a method `AppDelegate` defines below.
            let item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &NSString::from_str(title),
                    action,
                    &NSString::from_str(key),
                )
            };
            if action.is_some() {
                // SAFETY: the delegate outlives the menu for the process lifetime.
                unsafe { item.setTarget(Some(target)) };
            } else {
                item.setEnabled(false);
            }
            menu.addItem(&item);
            item
        };
        let separator = || menu.addItem(&NSMenuItem::separatorItem(mtm));
        let state = add("Starting Cantrip…", None, "");
        let detail = add("", None, "");
        let outcome = add("", None, "");
        let problem = add("", None, "");
        separator();
        let start = add("Start Dictation", Some(sel!(startDictation:)), "");
        let stop = add("Stop Recording", Some(sel!(stopRecording:)), "");
        let cancel = add("Cancel Without Delivery", Some(sel!(cancelTake:)), "");
        let shortcut = add("", Some(sel!(openSettings:)), "");
        separator();
        let copy = add("Copy This Transcript", Some(sel!(copyTranscript:)), "");
        let recover_local = add(
            "Recover Locally to Clipboard",
            Some(sel!(recoverLocally:)),
            "",
        );
        let recover_provider = add(
            "Recover with Configured Provider to Clipboard",
            Some(sel!(recoverWithProvider:)),
            "",
        );
        let install_model = add("Install Local Model…", Some(sel!(checkSetup:)), "");
        let dismiss = add("Dismiss Outcome", Some(sel!(dismissOutcome:)), "");
        add("Recordings and Recovery…", Some(sel!(openRecordings:)), "");
        add("Check Setup…", Some(sel!(checkSetup:)), "");
        add("Settings…", Some(sel!(openSettings:)), ",");
        separator();
        let microphone = add("", Some(sel!(microphoneAccess:)), "");
        let start_engine = add("Start Cantrip", Some(sel!(startEngine:)), "");
        let login = add("Open at Login", Some(sel!(toggleLogin:)), "");
        separator();
        add("Quit Cantrip", Some(sel!(quit:)), "q");
        Self {
            menu,
            state,
            detail,
            outcome,
            problem,
            start,
            stop,
            cancel,
            shortcut,
            copy,
            recover_local,
            recover_provider,
            install_model,
            dismiss,
            microphone,
            start_engine,
            login,
        }
    }
}

/// An informational line: hidden when empty, shortened to the menu's width
/// with the full text in its tooltip.
fn line(item: &NSMenuItem, text: Option<&str>) {
    let text = text.filter(|text| !text.is_empty());
    item.setHidden(text.is_none());
    let Some(text) = text else {
        return;
    };
    let short = if text.chars().count() > LINE_LIMIT {
        let mut short: String = text.chars().take(LINE_LIMIT - 1).collect();
        short.push('…');
        short
    } else {
        text.to_owned()
    };
    item.setTitle(&NSString::from_str(&short));
    item.setToolTip((short != text).then(|| NSString::from_str(text)).as_deref());
}

fn sentence(text: &str) -> String {
    let mut characters = text.chars();
    characters.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(characters).collect()
    })
}

/// Every piece of app state the AppKit thread owns, created at launch.
struct Host {
    hud: LiveHud,
    engine: Engine,
    /// Quit was chosen; the engine is finishing its take before the app exits.
    quitting: bool,
    _item: Retained<NSStatusItem>,
    button: Retained<NSStatusBarButton>,
    menu: Menu,
    mark: Mark,
    tooltip: String,
    shortcuts: Shortcuts,
    commands: mpsc::Sender<Request>,
    replies: mpsc::Receiver<Reply>,
    microphone: MicrophonePermission,
    microphone_request: Option<mpsc::Receiver<Result<MicrophonePermission, String>>>,
    /// The latest failed action, shown until a later action succeeds.
    problem: Option<String>,
    targets: Targets,
    windows: Windows,
    timer: Option<(Retained<NSTimer>, Duration)>,
    settings_at: Option<Instant>,
    processing_since: Option<Instant>,
}

/// What the AppKit thread does once no host state is borrowed.
enum After {
    Continue,
    /// A termination signal stopped the engine: quit the app as well.
    Quit,
    /// Quit was waiting for the engine, which has now finished.
    FinishQuit,
}

impl Host {
    fn start(delegate: &AppDelegate) -> Result<Self> {
        let mtm = delegate.mtm();
        let target: &AnyObject = delegate;
        let menu = Menu::new(mtm, target, ProtocolObject::from_ref(delegate));
        let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
        item.setMenu(Some(&menu.menu));
        let button = item
            .button(mtm)
            .context("the menu bar did not provide a status item button")?;
        button.setAccessibilityLabel(Some(ns_string!("Cantrip")));
        let (commands, replies) = spawn_commands()?;
        let shortcut_requests = commands.clone();
        let (registration, problem) = match Registration::new(move || {
            let _ = shortcut_requests.send(Request {
                command: Command::Toggle {
                    postproc: None,
                    handoff: None,
                },
                origin: Origin::Shortcut,
            });
        }) {
            Ok(registration) => (Some(registration), None),
            Err(problem) => (None, Some(problem)),
        };
        let mut host = Self {
            hud: LiveHud::start(mtm)?,
            engine: Engine::Stopped(None),
            quitting: false,
            _item: item,
            button,
            menu,
            mark: Mark::new(),
            tooltip: String::new(),
            shortcuts: Shortcuts {
                registration,
                active: None,
                problem,
                seen: None,
            },
            commands,
            replies,
            microphone: capture::microphone_permission(),
            microphone_request: None,
            problem: None,
            targets: Targets::default(),
            windows: Windows::default(),
            timer: None,
            settings_at: None,
            processing_since: None,
        };
        host.start_engine();
        Ok(host)
    }

    fn start_engine(&mut self) {
        if self.quitting
            || cantrip_engine::engine::shutdown_requested()
            || matches!(self.engine, Engine::Owned { .. })
        {
            return;
        }
        let thread = thread::Builder::new()
            .name("cantrip-engine".to_owned())
            .spawn(|| {
                // A Cantrip process that already answers owns dictation.
                if ipc::status().is_ok() {
                    return Ok(EngineExit::External);
                }
                let config = Config::load().context("loading configuration")?;
                crate::daemon::run(config, false).map(|()| EngineExit::Stopped)
            });
        self.engine = match thread {
            Ok(thread) => Engine::Owned {
                thread,
                started: Instant::now(),
            },
            Err(error) => {
                Engine::Stopped(Some(format!("The engine thread did not start: {error}")))
            }
        };
        tracing::info!("[Daemon] app host starting the engine");
    }

    /// Start dictation again when nothing serves it, as Start Cantrip does.
    fn start_if_stopped(&mut self) {
        if self.hud.status().is_none() && !matches!(self.engine, Engine::Owned { .. }) {
            self.start_engine();
        }
    }

    /// Join a finished engine thread. True for a clean stop this app did not
    /// request: a termination signal reached the engine.
    fn reap_engine(&mut self) -> bool {
        if !matches!(&self.engine, Engine::Owned { thread, .. } if thread.is_finished()) {
            return false;
        }
        let Engine::Owned { thread, .. } =
            std::mem::replace(&mut self.engine, Engine::Stopped(None))
        else {
            return false;
        };
        self.engine = match thread.join() {
            Ok(Ok(EngineExit::External)) => Engine::External,
            Ok(Ok(EngineExit::Stopped)) => Engine::Stopped(None),
            Ok(Err(error)) => {
                tracing::warn!("[Daemon] engine stopped: {error:#}");
                Engine::Stopped(Some(format!("{error:#}")))
            }
            Err(_) => {
                tracing::warn!("[Daemon] engine thread panicked");
                Engine::Stopped(Some("The engine stopped unexpectedly.".to_owned()))
            }
        };
        matches!(self.engine, Engine::Stopped(None)) && !self.quitting
    }

    fn send(&mut self, command: Command, origin: Origin) {
        if self.commands.send(Request { command, origin }).is_err() {
            self.problem = Some("The command worker stopped; quit and reopen Cantrip.".to_owned());
        }
    }

    fn tick(&mut self, target: &AnyObject) -> After {
        let mtm = self.button.mtm();
        let now = Instant::now();
        let interval = self.hud.tick(mtm, now);
        let signalled =
            self.reap_engine() || (!self.quitting && cantrip_engine::engine::shutdown_requested());
        while let Ok(reply) = self.replies.try_recv() {
            if reply.origin == Origin::Shortcut && !reply.reachable {
                NSBeep();
            }
            // Busy and similar refusals already reach the HUD as notices.
            if reply.origin == Origin::Menu || !reply.reachable {
                self.problem = reply.problem;
            } else if reply.problem.is_none() {
                self.problem = None;
            }
        }
        let decided = match self
            .microphone_request
            .as_ref()
            .map(mpsc::Receiver::try_recv)
        {
            Some(Ok(result)) => Some(result),
            Some(Err(mpsc::TryRecvError::Disconnected)) => Some(Err(
                "The microphone request stopped unexpectedly.".to_owned(),
            )),
            Some(Err(mpsc::TryRecvError::Empty)) | None => None,
        };
        if let Some(result) = decided {
            self.microphone_request = None;
            match result {
                Ok(permission) => {
                    self.microphone = permission;
                    self.problem = (permission != MicrophonePermission::Authorized).then(|| {
                        "Microphone access was not allowed. Change it in System Settings."
                            .to_owned()
                    });
                }
                Err(problem) => self.problem = Some(problem),
            }
        }
        self.windows.reap();
        if self
            .settings_at
            .is_none_or(|at| now.duration_since(at) >= SETTINGS_INTERVAL)
        {
            self.settings_at = Some(now);
            self.microphone = capture::microphone_permission();
            self.shortcuts.refresh();
        }
        self.show_mark(now);
        self.schedule(interval, target);
        if self.quitting && !matches!(self.engine, Engine::Owned { .. }) {
            After::FinishQuit
        } else if signalled {
            After::Quit
        } else {
            After::Continue
        }
    }

    /// Keep one repeating timer at the HUD's cadence, in every run-loop mode so
    /// the instrument keeps moving while the menu is open or Quit is waiting.
    fn schedule(&mut self, interval: Duration, target: &AnyObject) {
        if self
            .timer
            .as_ref()
            .is_some_and(|(_, current)| *current == interval)
        {
            return;
        }
        if let Some((timer, _)) = self.timer.take() {
            timer.invalidate();
        }
        // SAFETY: `tick:` is defined by the target, which the timer retains.
        let timer = unsafe {
            NSTimer::timerWithTimeInterval_target_selector_userInfo_repeats(
                interval.as_secs_f64(),
                target,
                sel!(tick:),
                None,
                true,
            )
        };
        // SAFETY: the main run loop and its common modes outlive the timer.
        unsafe { NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
        self.timer = Some((timer, interval));
    }

    fn show_mark(&mut self, now: Instant) {
        let status = self.hud.status();
        let palette = self.hud.palette();
        let route = status
            .and_then(|status| status.handoff.as_ref())
            .map_or(palette.accent, |handoff| handoff.color);
        let processing = status.is_some_and(|status| status.state == StateKind::Processing);
        let since = if processing {
            *self.processing_since.get_or_insert(now)
        } else {
            self.processing_since = None;
            now
        };
        let needs_operator = status.is_some_and(|status| status.attention)
            || self.microphone != MicrophonePermission::Authorized
            || self.shortcuts.active.is_none();
        let tone = match status.map(|status| &status.state) {
            None | Some(StateKind::Unknown(_)) => Tone::Unknown,
            Some(StateKind::Recording) => Tone::Recording(route),
            Some(StateKind::Processing) => Tone::Processing(
                route,
                if self.hud.reduced_motion() {
                    0
                } else {
                    (now.duration_since(since).as_millis() / CHASE_STEP.as_millis()) as usize
                },
            ),
            Some(StateKind::Idle) if needs_operator => Tone::Attention(palette.attention),
            Some(StateKind::Idle) => Tone::Rest,
        };
        self.mark.show(&self.button, tone);
        let tooltip = format!("Cantrip — {}", self.describe(now).0);
        if tooltip != self.tooltip {
            self.button.setToolTip(Some(&NSString::from_str(&tooltip)));
            self.tooltip = tooltip;
        }
    }

    /// The menu's first line and its explanation, from live status only.
    fn describe(&self, now: Instant) -> (String, Option<String>) {
        if self.quitting {
            return (
                "Quitting…".to_owned(),
                Some("Finishing the current take before Cantrip exits.".to_owned()),
            );
        }
        if let Some(status) = self.hud.status() {
            let mut state = match &status.state {
                StateKind::Idle => "Ready".to_owned(),
                StateKind::Recording if status.signal.is_none() => {
                    "Starting microphone…".to_owned()
                }
                StateKind::Recording => {
                    format!("Recording · {}", crate::hud::format_elapsed(status.elapsed))
                }
                StateKind::Processing => status.stage.as_ref().map_or_else(
                    || "Working…".to_owned(),
                    |stage| sentence(&stage.to_string()),
                ),
                StateKind::Unknown(_) => "Cantrip status unavailable".to_owned(),
            };
            if let Some(handoff) = &status.handoff {
                state.push_str(" · to ");
                state.push_str(&handoff.label);
            }
            let detail = matches!(self.engine, Engine::External).then(|| {
                "Dictation runs in a Cantrip process started outside this app.".to_owned()
            });
            return (state, detail);
        }
        match &self.engine {
            Engine::Owned { started, .. } if now.duration_since(*started) < STARTUP_GRACE => {
                ("Starting Cantrip…".to_owned(), None)
            }
            Engine::Owned { .. } | Engine::External => (
                "Cantrip is not responding".to_owned(),
                Some("Recording and saved-audio status unknown.".to_owned()),
            ),
            Engine::Stopped(Some(reason)) => ("Cantrip stopped".to_owned(), Some(reason.clone())),
            Engine::Stopped(None) => ("Cantrip is not running".to_owned(), None),
        }
    }

    /// Rebuild every item from live status as the menu opens.
    fn update_menu(&mut self) {
        let now = Instant::now();
        self.microphone = capture::microphone_permission();
        let (state, detail) = self.describe(now);
        let status = self.hud.status().cloned();
        let menu = &self.menu;
        line(&menu.state, Some(&state));
        line(&menu.detail, detail.as_deref());
        let outcome = status
            .as_ref()
            .and_then(|status| status.outcome.as_ref())
            .filter(|outcome| !outcome.dismissed);
        line(
            &menu.outcome,
            outcome
                .map(|outcome| match &outcome.handoff {
                    Some(handoff) => format!("{} · to {}", outcome.message, handoff.label),
                    None => outcome.message.clone(),
                })
                .as_deref(),
        );
        let notice = status
            .as_ref()
            .and_then(|status| status.notice.as_ref())
            .map(|notice| notice.message.as_str());
        line(&menu.problem, self.problem.as_deref().or(notice));

        let capabilities = status
            .as_ref()
            .map(|status| status.capabilities)
            .unwrap_or_default();
        let idle = status
            .as_ref()
            .is_some_and(|status| status.state == StateKind::Idle);
        menu.start.setHidden(!idle && status.is_some());
        menu.start.setEnabled(idle);
        menu.stop.setHidden(!capabilities.stop);
        menu.cancel.setHidden(!capabilities.cancel);
        menu.shortcut
            .setTitle(&NSString::from_str(&self.shortcuts.title()));
        menu.shortcut.setToolTip(
            self.shortcuts
                .problem
                .as_deref()
                .map(NSString::from_str)
                .as_deref(),
        );
        menu.shortcut
            .setEnabled(self.shortcuts.problem.is_some() || self.shortcuts.active.is_none());

        // Deliberate actions for the outcome shown, bound to its own take.
        self.targets = Targets {
            take: outcome.and_then(|outcome| outcome.artifacts.take_id.clone()),
            event: outcome.map(|outcome| outcome.event_id),
        };
        let take = outcome.filter(|outcome| outcome.artifacts.take_id.is_some());
        let copy = take.is_some_and(|outcome| outcome.artifacts.text && capabilities.copy);
        let failed_audio =
            take.is_some_and(|outcome| !outcome.is_success() && outcome.artifacts.audio);
        let recover = failed_audio && capabilities.recover;
        menu.copy.setHidden(!copy);
        menu.copy.setTitle(
            if take.is_some_and(|outcome| outcome.completeness == Completeness::Partial) {
                ns_string!("Copy Partial Transcript")
            } else {
                ns_string!("Copy This Transcript")
            },
        );
        menu.recover_local
            .setHidden(!(recover && capabilities.local_model));
        menu.recover_provider
            .setHidden(!(recover && capabilities.remote_configured));
        // The HUD names this item for saved audio while local recovery lacks its model.
        menu.install_model
            .setHidden(!failed_audio || capabilities.local_model);
        menu.dismiss.setHidden(
            !(capabilities.dismiss && outcome.is_some_and(|outcome| outcome.needs_attention())),
        );

        let (title, enabled) = match self.microphone {
            MicrophonePermission::Authorized => ("", false),
            MicrophonePermission::NotDetermined if self.microphone_request.is_some() => {
                ("Waiting for your microphone decision…", false)
            }
            MicrophonePermission::NotDetermined => ("Allow Microphone Access…", true),
            MicrophonePermission::Denied => {
                ("Microphone Access Denied — Open Privacy Settings…", true)
            }
            MicrophonePermission::Restricted => {
                ("Microphone access is restricted on this Mac", false)
            }
        };
        line(&menu.microphone, Some(title));
        menu.microphone.setEnabled(enabled);
        menu.start_engine.setHidden(
            status.is_some() || matches!(self.engine, Engine::Owned { .. }) || self.quitting,
        );
        self.update_login_item();
    }

    fn update_login_item(&self) {
        let item = &self.menu.login;
        if autoreleasepool(|_| app_bundle()).is_none() {
            item.setTitle(ns_string!("Open at Login"));
            item.setState(NSControlStateValueOff);
            item.setEnabled(false);
            item.setToolTip(Some(ns_string!(
                "Available when Cantrip runs from Cantrip.app."
            )));
            return;
        }
        // SAFETY: querying the main app's own login item has no preconditions.
        let status = unsafe { SMAppService::mainAppService().status() };
        let (title, on, enabled) = match status {
            SMAppServiceStatus::Enabled => (ns_string!("Open at Login"), true, true),
            SMAppServiceStatus::RequiresApproval => (
                ns_string!("Open at Login — Approve in System Settings…"),
                false,
                true,
            ),
            SMAppServiceStatus::NotRegistered => (ns_string!("Open at Login"), false, true),
            _ => (
                ns_string!("Open at Login (unavailable for this copy)"),
                false,
                false,
            ),
        };
        item.setTitle(title);
        item.setState(if on {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        });
        item.setEnabled(enabled);
        item.setToolTip(None);
    }

    fn toggle_login(&mut self) {
        let result = autoreleasepool(|_| {
            // SAFETY: the main app registers and unregisters only its own login item.
            let changed = unsafe {
                let service = SMAppService::mainAppService();
                match service.status() {
                    SMAppServiceStatus::Enabled => service.unregisterAndReturnError(),
                    SMAppServiceStatus::RequiresApproval => {
                        SMAppService::openSystemSettingsLoginItems();
                        Ok(())
                    }
                    _ => service.registerAndReturnError(),
                }
            };
            changed.map_err(|error| error.localizedDescription().to_string())
        });
        self.problem = result
            .err()
            .map(|reason| format!("Open at Login was not changed: {reason}"));
    }

    fn microphone_access(&mut self) {
        match capture::microphone_permission() {
            MicrophonePermission::NotDetermined if self.microphone_request.is_none() => {
                // The system prompt answers this explicit choice. The request
                // blocks until the operator decides, so it waits off this thread.
                let (sender, receiver) = mpsc::channel();
                let spawned = thread::Builder::new()
                    .name("cantrip-microphone".to_owned())
                    .spawn(move || {
                        let result = autoreleasepool(|_| capture::request_microphone_permission())
                            .map_err(|error| format!("{error:#}"));
                        let _ = sender.send(result);
                    });
                match spawned {
                    Ok(_) => self.microphone_request = Some(receiver),
                    Err(error) => {
                        self.problem = Some(format!("Microphone access was not requested: {error}"))
                    }
                }
            }
            MicrophonePermission::Denied => {
                if let Err(error) = open_microphone_settings() {
                    self.problem = Some(format!("{error:#}"));
                }
            }
            _ => {}
        }
    }

    fn open(&mut self, window: Window) {
        if let Err(error) = self.windows.open(window) {
            self.problem = Some(format!("{error:#}"));
        }
    }
}

struct Ivars {
    /// Held for the process lifetime: one menu-bar owner per user.
    _lock: fs::File,
    host: RefCell<Option<Host>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; `AppDelegate` has no `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "CantripAppDelegate"]
    #[ivars = Ivars]
    struct AppDelegate;

    // SAFETY: NSObjectProtocol has no safety requirements.
    unsafe impl NSObjectProtocol for AppDelegate {}

    // SAFETY: the signatures match NSApplicationDelegate.
    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            match Host::start(self) {
                Ok(host) => *self.ivars().host.borrow_mut() = Some(host),
                Err(error) => {
                    tracing::error!("[Daemon] Cantrip app could not start: {error:#}");
                    NSApplication::sharedApplication(self.mtm()).terminate(None);
                    return;
                }
            }
            self.tick_now();
        }

        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, _sender: &NSApplication) -> NSApplicationTerminateReply {
            let mut host = self.ivars().host.borrow_mut();
            let Some(host) = host.as_mut() else {
                return NSApplicationTerminateReply::TerminateNow;
            };
            if !matches!(host.engine, Engine::Owned { .. }) {
                return NSApplicationTerminateReply::TerminateNow;
            }
            // Finish capture and settle cancellation first; recordings stay retained.
            host.quitting = true;
            cantrip_engine::engine::request_shutdown();
            tracing::info!("[Daemon] quit requested; waiting for the engine");
            NSApplicationTerminateReply::TerminateLater
        }

        #[unsafe(method(applicationWillTerminate:))]
        fn will_terminate(&self, _notification: &NSNotification) {
            // Stops the status reader and unregisters the shortcut.
            drop(self.ivars().host.borrow_mut().take());
        }

        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn should_handle_reopen(&self, _sender: &NSApplication, _visible: bool) -> bool {
            if let Some(host) = self.ivars().host.borrow_mut().as_mut() {
                host.start_if_stopped();
            }
            false
        }
    }

    // SAFETY: the signature matches NSMenuDelegate.
    unsafe impl NSMenuDelegate for AppDelegate {
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, _menu: &NSMenu) {
            if let Some(host) = self.ivars().host.borrow_mut().as_mut() {
                host.update_menu();
            }
        }
    }

    // SAFETY: each action takes the sender and returns nothing, as menu items expect.
    impl AppDelegate {
        #[unsafe(method(tick:))]
        fn tick(&self, _timer: &NSTimer) {
            self.tick_now();
        }

        #[unsafe(method(startDictation:))]
        fn start_dictation(&self, _sender: Option<&AnyObject>) {
            self.send(Command::Start {
                postproc: None,
                handoff: None,
            });
        }

        #[unsafe(method(stopRecording:))]
        fn stop_recording(&self, _sender: Option<&AnyObject>) {
            self.send(Command::Stop);
        }

        #[unsafe(method(cancelTake:))]
        fn cancel_take(&self, _sender: Option<&AnyObject>) {
            self.send(Command::Cancel);
        }

        #[unsafe(method(copyTranscript:))]
        fn copy_transcript(&self, _sender: Option<&AnyObject>) {
            self.with_take(|id| Command::Copy { id });
        }

        #[unsafe(method(recoverLocally:))]
        fn recover_locally(&self, _sender: Option<&AnyObject>) {
            self.with_take(|id| Command::Recover {
                id: Some(id),
                local: true,
                clipboard: true,
            });
        }

        #[unsafe(method(recoverWithProvider:))]
        fn recover_with_provider(&self, _sender: Option<&AnyObject>) {
            self.with_take(|id| Command::Recover {
                id: Some(id),
                local: false,
                clipboard: true,
            });
        }

        #[unsafe(method(dismissOutcome:))]
        fn dismiss_outcome(&self, _sender: Option<&AnyObject>) {
            let mut host = self.ivars().host.borrow_mut();
            if let Some(host) = host.as_mut() {
                if let Some(event_id) = host.targets.event {
                    host.send(
                        Command::Dismiss {
                            event_id: Some(event_id),
                        },
                        Origin::Menu,
                    );
                }
            }
        }

        #[unsafe(method(openRecordings:))]
        fn open_recordings(&self, _sender: Option<&AnyObject>) {
            self.open(Window::Recordings);
        }

        #[unsafe(method(checkSetup:))]
        fn check_setup(&self, _sender: Option<&AnyObject>) {
            self.open(Window::CheckSetup);
        }

        #[unsafe(method(openSettings:))]
        fn open_settings(&self, _sender: Option<&AnyObject>) {
            self.open(Window::Settings);
        }

        #[unsafe(method(microphoneAccess:))]
        fn microphone_access(&self, _sender: Option<&AnyObject>) {
            if let Some(host) = self.ivars().host.borrow_mut().as_mut() {
                host.microphone_access();
            }
        }

        #[unsafe(method(startEngine:))]
        fn start_engine(&self, _sender: Option<&AnyObject>) {
            if let Some(host) = self.ivars().host.borrow_mut().as_mut() {
                host.start_if_stopped();
            }
        }

        #[unsafe(method(toggleLogin:))]
        fn toggle_login(&self, _sender: Option<&AnyObject>) {
            if let Some(host) = self.ivars().host.borrow_mut().as_mut() {
                host.toggle_login();
            }
        }

        #[unsafe(method(quit:))]
        fn quit(&self, _sender: Option<&AnyObject>) {
            NSApplication::sharedApplication(self.mtm()).terminate(None);
        }
    }
);

impl AppDelegate {
    fn new(mtm: MainThreadMarker, lock: fs::File) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars {
            _lock: lock,
            host: RefCell::new(None),
        });
        // SAFETY: `init` is NSObject's designated initializer.
        unsafe { msg_send![super(this), init] }
    }

    fn tick_now(&self) {
        let target: &AnyObject = self;
        let after = match self.ivars().host.borrow_mut().as_mut() {
            Some(host) => autoreleasepool(|_| host.tick(target)),
            None => return,
        };
        // AppKit calls back into this delegate, so no host state is borrowed here.
        let app = NSApplication::sharedApplication(self.mtm());
        match after {
            After::Continue => {}
            After::Quit => app.terminate(None),
            After::FinishQuit => app.replyToApplicationShouldTerminate(true),
        }
    }

    fn send(&self, command: Command) {
        if let Some(host) = self.ivars().host.borrow_mut().as_mut() {
            host.send(command, Origin::Menu);
        }
    }

    fn with_take(&self, command: impl FnOnce(String) -> Command) {
        if let Some(host) = self.ivars().host.borrow_mut().as_mut() {
            if let Some(id) = host.targets.take.clone() {
                host.send(command(id), Origin::Menu);
            }
        }
    }

    fn open(&self, window: Window) {
        if let Some(host) = self.ivars().host.borrow_mut().as_mut() {
            host.open(window);
        }
    }
}
