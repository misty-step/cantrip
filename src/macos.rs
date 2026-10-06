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
use anyhow::{anyhow, Context, Result};
use cantrip_engine::{
    config::Config,
    engine::{request_shutdown, shutdown_requested},
    ipc::{self, Command, Completeness, StateKind},
};
use mark::{Tone, CHASE_STEP};
use objc2::{
    define_class, msg_send,
    rc::{autoreleasepool, Retained},
    runtime::{AnyObject, ProtocolObject},
    sel, DefinedClass, MainThreadMarker, MainThreadOnly,
};
use objc2_app_kit::{
    NSAccessibility, NSApplication, NSApplicationActivationOptions, NSApplicationActivationPolicy,
    NSApplicationDelegate, NSApplicationTerminateReply, NSBeep, NSControlStateValueOn, NSMenu,
    NSMenuDelegate, NSMenuItem, NSRunningApplication, NSStatusBar, NSStatusBarButton, NSStatusItem,
    NSVariableStatusItemLength, NSWorkspace, NSWorkspaceOpenConfiguration,
};
use objc2_foundation::{
    ns_string, NSBundle, NSNotification, NSObject, NSObjectProtocol, NSRunLoop,
    NSRunLoopCommonModes, NSString, NSTimer, NSURL,
};
use objc2_service_management::{SMAppService, SMAppServiceStatus};
use shortcut::Registration;
use std::{
    cell::RefCell,
    fs,
    process::{Child, Command as Process, Stdio},
    sync::mpsc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// A just-started engine binds its socket within this window.
const STARTUP_GRACE: Duration = Duration::from_secs(15);
/// The configured shortcut and microphone access are re-read this often.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
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

/// How Cantrip answered one command.
enum Reply {
    Accepted,
    /// The engine's own sentence; a reply's `error` is a log class, not words.
    Refused(String),
    Unreachable,
}

/// One worker submits commands in order, so the AppKit thread never waits on IPC.
fn spawn_commands() -> Result<(mpsc::Sender<Command>, mpsc::Receiver<Reply>)> {
    let (commands, incoming) = mpsc::channel::<Command>();
    let (replies, results) = mpsc::channel();
    thread::Builder::new()
        .name("cantrip-commands".to_owned())
        .spawn(move || {
            for command in incoming {
                let reply = match ipc::command(command) {
                    Ok(reply) if reply.ok => Reply::Accepted,
                    Ok(reply) => Reply::Refused(
                        reply
                            .message
                            .unwrap_or_else(|| "Cantrip did not accept this action.".to_owned()),
                    ),
                    Err(_) => Reply::Unreachable,
                };
                if replies.send(reply).is_err() {
                    break;
                }
            }
        })
        .context("starting the command worker")?;
    Ok((commands, results))
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

/// What a menu item does, bound as the menu opens: an action never retargets
/// a later take or outcome.
#[derive(Clone)]
enum Choice {
    Send(Command),
    Settings,
    Recordings,
    CheckSetup,
    Microphone,
    StartEngine,
    Login,
    Quit,
}

/// The menu being filled as it opens; each enabled item's tag indexes its choice.
struct Items<'a> {
    menu: &'a NSMenu,
    target: &'a AnyObject,
    choices: Vec<Choice>,
}

impl Items<'_> {
    /// An item that performs `choice`, or a disabled one without a choice.
    fn add(&mut self, title: &str, choice: Option<Choice>) -> Retained<NSMenuItem> {
        // SAFETY: `choose:` is a method `AppDelegate` defines below.
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(self.menu.mtm()),
                &NSString::from_str(title),
                choice.is_some().then_some(sel!(choose:)),
                ns_string!(""),
            )
        };
        match choice {
            Some(choice) => {
                item.setTag(self.choices.len() as isize);
                self.choices.push(choice);
                // SAFETY: the delegate outlives the menu for the process lifetime.
                unsafe { item.setTarget(Some(self.target)) };
            }
            None => item.setEnabled(false),
        }
        self.menu.addItem(&item);
        item
    }

    fn item(&mut self, title: &str, choice: Choice) -> Retained<NSMenuItem> {
        self.add(title, Some(choice))
    }

    fn send(&mut self, title: &str, command: Command) {
        self.add(title, Some(Choice::Send(command)));
    }

    /// An informational line: absent when empty, shortened to the menu's width
    /// with the full text in its tooltip.
    fn line(&mut self, text: Option<&str>) {
        let Some(text) = text.filter(|text| !text.is_empty()) else {
            return;
        };
        if text.chars().count() <= LINE_LIMIT {
            self.add(text, None);
        } else {
            let short: String = text.chars().take(LINE_LIMIT - 1).chain(['…']).collect();
            self.add(&short, None)
                .setToolTip(Some(&NSString::from_str(text)));
        }
    }

    fn separator(&self) {
        self.menu
            .addItem(&NSMenuItem::separatorItem(self.menu.mtm()));
    }
}

/// Bring a shared window forward, or open it as its own process, so closing it
/// never stops dictation.
fn open_window(window: &mut Option<Child>, arguments: &[&str]) -> Result<()> {
    if let Some(child) = window.as_mut() {
        if matches!(child.try_wait(), Ok(None)) {
            if let Some(app) =
                NSRunningApplication::runningApplicationWithProcessIdentifier(child.id() as _)
            {
                app.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows);
            }
            return Ok(());
        }
    }
    let child = Process::new(std::env::current_exe()?)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .context("Opening the window")?;
    *window = Some(child);
    Ok(())
}

/// Register or unregister Cantrip.app's own login item, or open its approval
/// page in System Settings.
fn toggle_login() -> Result<()> {
    autoreleasepool(|_| {
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
        changed.map_err(|error| {
            anyhow!(
                "Open at Login was not changed: {}",
                error.localizedDescription()
            )
        })
    })
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
    /// The mark shown, redrawn only when its tone changes.
    tone: Option<Tone>,
    tooltip: String,
    shortcut: Registration,
    commands: mpsc::Sender<Command>,
    replies: mpsc::Receiver<Reply>,
    microphone: MicrophonePermission,
    microphone_request: Option<JoinHandle<Result<MicrophonePermission>>>,
    /// The latest failed action, shown until a later action succeeds.
    problem: Option<String>,
    /// What the open menu's items do, by tag.
    choices: Vec<Choice>,
    /// The shared Settings and Actions windows, one process each.
    settings: Option<Child>,
    actions: Option<Child>,
    timer: Option<(Retained<NSTimer>, Duration)>,
    /// When the shortcut and microphone access were last re-read.
    refreshed: Instant,
    /// The processing chase's clock.
    launched: Instant,
}

/// What the AppKit thread does once no host state is borrowed.
enum After {
    Continue,
    /// A termination signal reached Cantrip: quit the app as well.
    Quit,
    /// Quit was waiting for the engine, which has now finished.
    FinishQuit,
}

impl Host {
    fn start(delegate: &AppDelegate) -> Result<Self> {
        let mtm = delegate.mtm();
        let target: &AnyObject = delegate;
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        menu.setDelegate(Some(ProtocolObject::from_ref(delegate)));
        let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
        item.setMenu(Some(&menu));
        let button = item
            .button(mtm)
            .context("the menu bar did not provide a status item button")?;
        button.setAccessibilityLabel(Some(ns_string!("Cantrip")));
        let (commands, replies) = spawn_commands()?;
        let toggles = commands.clone();
        let now = Instant::now();
        let mut host = Self {
            hud: LiveHud::start(mtm)?,
            engine: Engine::Stopped(None),
            quitting: false,
            _item: item,
            button,
            tone: None,
            tooltip: String::new(),
            shortcut: Registration::new(move || {
                let _ = toggles.send(Command::Toggle {
                    postproc: None,
                    handoff: None,
                });
            }),
            commands,
            replies,
            microphone: capture::microphone_permission(),
            microphone_request: None,
            problem: None,
            choices: Vec::new(),
            settings: None,
            actions: None,
            timer: None,
            refreshed: now,
            launched: now,
        };
        host.shortcut.refresh();
        host.start_engine();
        host.fill(&menu, target);
        Ok(host)
    }

    /// Start one engine while nothing serves dictation. A Cantrip process that
    /// already answers is attached instead, never duplicated.
    fn start_engine(&mut self) {
        if self.hud.status().is_some()
            || matches!(self.engine, Engine::Owned { .. })
            || shutdown_requested()
        {
            return;
        }
        let thread = thread::Builder::new()
            .name("cantrip-engine".to_owned())
            .spawn(|| {
                if ipc::status().is_ok() {
                    return Ok(EngineExit::External);
                }
                let config = Config::load().context("loading configuration")?;
                crate::daemon::run(config, false).map(|()| EngineExit::Stopped)
            });
        self.engine = match thread {
            Ok(thread) => {
                tracing::info!("[Daemon] app host starting the engine");
                Engine::Owned {
                    thread,
                    started: Instant::now(),
                }
            }
            Err(error) => {
                Engine::Stopped(Some(format!("The engine thread did not start: {error}")))
            }
        };
    }

    /// Join a finished engine thread, keeping why it ended.
    fn reap_engine(&mut self) {
        if !matches!(&self.engine, Engine::Owned { thread, .. } if thread.is_finished()) {
            return;
        }
        let Engine::Owned { thread, .. } =
            std::mem::replace(&mut self.engine, Engine::Stopped(None))
        else {
            return;
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
    }

    fn send(&self, command: Command) -> Result<()> {
        self.commands
            .send(command)
            .map_err(|_| anyhow!("The command worker stopped; quit and reopen Cantrip."))
    }

    fn tick(&mut self, target: &AnyObject) -> After {
        let now = Instant::now();
        let interval = self.hud.tick(self.button.mtm(), now);
        self.reap_engine();
        while let Ok(reply) = self.replies.try_recv() {
            self.problem = match reply {
                Reply::Accepted => None,
                Reply::Refused(message) => Some(message),
                Reply::Unreachable => {
                    NSBeep();
                    Some("Cantrip did not answer this action.".to_owned())
                }
            };
        }
        match self
            .microphone_request
            .take_if(|request| request.is_finished())
            .map(JoinHandle::join)
        {
            Some(Ok(Ok(permission))) => self.microphone = permission,
            Some(Ok(Err(error))) => self.problem = Some(format!("{error:#}")),
            Some(Err(_)) => {
                self.problem = Some("The microphone request stopped unexpectedly.".to_owned());
            }
            None => {}
        }
        if now.duration_since(self.refreshed) >= REFRESH_INTERVAL {
            self.refreshed = now;
            self.microphone = capture::microphone_permission();
            self.shortcut.refresh();
            // Reap closed windows; dictation continues regardless.
            for window in [&mut self.settings, &mut self.actions] {
                window.take_if(|child| !matches!(child.try_wait(), Ok(None)));
            }
        }
        self.show_mark(now);
        self.schedule(interval, target);
        if self.quitting && !matches!(self.engine, Engine::Owned { .. }) {
            After::FinishQuit
        } else if !self.quitting && shutdown_requested() {
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
        let needs_operator = status.is_some_and(|status| status.attention)
            || self.microphone != MicrophonePermission::Authorized
            || self.shortcut.active().is_none();
        let tone = match status.map(|status| &status.state) {
            None | Some(StateKind::Unknown(_)) => Tone::Unknown,
            Some(StateKind::Recording) => Tone::Recording(route),
            Some(StateKind::Processing) if self.hud.reduced_motion() => Tone::Processing(route, 0),
            Some(StateKind::Processing) => Tone::Processing(
                route,
                (now.duration_since(self.launched).as_millis() / CHASE_STEP.as_millis()) as usize,
            ),
            Some(StateKind::Idle) if needs_operator => Tone::Attention(palette.attention),
            Some(StateKind::Idle) => Tone::Rest,
        };
        if self.tone != Some(tone) {
            tone.show(&self.button);
            self.tone = Some(tone);
        }
        let tooltip = format!("Cantrip — {}", self.describe(now).0);
        if tooltip != self.tooltip {
            self.button.setToolTip(Some(&NSString::from_str(&tooltip)));
            self.tooltip = tooltip;
        }
    }

    /// The menu's first line and its explanation, from live status only.
    fn describe(&self, now: Instant) -> (String, Option<&str>) {
        if self.quitting {
            let detail = "Finishing the current take before Cantrip exits.";
            return ("Quitting…".to_owned(), Some(detail));
        }
        let Some(status) = self.hud.status() else {
            return match &self.engine {
                Engine::Owned { started, .. } if now.duration_since(*started) < STARTUP_GRACE => {
                    ("Starting Cantrip…".to_owned(), None)
                }
                Engine::Owned { .. } | Engine::External => (
                    "Cantrip is not responding".to_owned(),
                    Some("Recording and saved-audio status unknown."),
                ),
                Engine::Stopped(Some(reason)) => {
                    ("Cantrip stopped".to_owned(), Some(reason.as_str()))
                }
                Engine::Stopped(None) => ("Cantrip is not running".to_owned(), None),
            };
        };
        let mut state = match &status.state {
            StateKind::Idle => "Ready".to_owned(),
            StateKind::Recording if status.signal.is_none() => "Starting microphone…".to_owned(),
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
        let detail = matches!(self.engine, Engine::External)
            .then_some("Dictation runs in a Cantrip process started outside this app.");
        (state, detail)
    }

    /// Rebuild the menu from live status as it opens.
    fn fill(&mut self, menu: &NSMenu, target: &AnyObject) {
        self.microphone = capture::microphone_permission();
        let (state, detail) = self.describe(Instant::now());
        let status = self.hud.status();
        let capabilities = status.map(|status| status.capabilities).unwrap_or_default();
        let idle = status.is_some_and(|status| status.state == StateKind::Idle);
        let outcome = status
            .and_then(|status| status.outcome.as_ref())
            .filter(|outcome| !outcome.dismissed);
        let notice = status
            .and_then(|status| status.notice.as_ref())
            .map(|notice| notice.message.as_str());
        menu.removeAllItems();
        let mut items = Items {
            menu,
            target,
            choices: Vec::new(),
        };
        items.line(Some(&state));
        items.line(detail);
        items.line(
            outcome
                .map(|outcome| match &outcome.handoff {
                    Some(handoff) => format!("{} · to {}", outcome.message, handoff.label),
                    None => outcome.message.clone(),
                })
                .as_deref(),
        );
        items.line(self.problem.as_deref().or(notice));
        items.separator();
        if idle {
            let start = Command::Start {
                postproc: None,
                handoff: None,
            };
            items.send("Start Dictation", start);
        } else if status.is_none() {
            items.add("Start Dictation", None);
        }
        if capabilities.stop {
            items.send("Stop Recording", Command::Stop);
        }
        if capabilities.cancel {
            items.send("Cancel Without Delivery", Command::Cancel);
        }
        let shortcut = self.shortcut.active();
        let shortcut_problem = self.shortcut.problem();
        let title = match (shortcut, shortcut_problem) {
            (Some(shortcut), None) => format!("Dictation shortcut: {}", shortcut.label()),
            (Some(shortcut), Some(_)) => {
                format!("Shortcut {} kept; new one refused…", shortcut.label())
            }
            (None, _) => "Shortcut unavailable; open Settings…".to_owned(),
        };
        let repair = shortcut_problem.is_some() || shortcut.is_none();
        items
            .add(&title, repair.then_some(Choice::Settings))
            .setToolTip(shortcut_problem.map(NSString::from_str).as_deref());
        items.separator();
        if let Some(outcome) = outcome {
            // Deliberate actions for the outcome shown, bound to its own take.
            if let Some(id) = &outcome.artifacts.take_id {
                let failed_audio = !outcome.is_success() && outcome.artifacts.audio;
                let recover = |local| Command::Recover {
                    id: Some(id.clone()),
                    local,
                    clipboard: true,
                };
                if outcome.artifacts.text && capabilities.copy {
                    let copy = Command::Copy { id: id.clone() };
                    if outcome.completeness == Completeness::Partial {
                        items.send("Copy Partial Transcript", copy);
                    } else {
                        items.send("Copy This Transcript", copy);
                    }
                }
                if failed_audio && capabilities.recover && capabilities.local_model {
                    items.send("Recover Locally to Clipboard", recover(true));
                }
                if failed_audio && capabilities.recover && capabilities.remote_configured {
                    items.send(
                        "Recover with Configured Provider to Clipboard",
                        recover(false),
                    );
                }
                // The HUD names this item for saved audio while local recovery lacks its model.
                if failed_audio && !capabilities.local_model {
                    items.item("Install Local Model…", Choice::CheckSetup);
                }
            }
            if capabilities.dismiss && outcome.needs_attention() {
                let dismiss = Command::Dismiss {
                    event_id: Some(outcome.event_id),
                };
                items.send("Dismiss Outcome", dismiss);
            }
        }
        items.item("Recordings and Recovery…", Choice::Recordings);
        items.item("Check Setup…", Choice::CheckSetup);
        items
            .item("Settings…", Choice::Settings)
            .setKeyEquivalent(ns_string!(","));
        items.separator();
        match self.microphone {
            MicrophonePermission::Authorized => {}
            MicrophonePermission::NotDetermined if self.microphone_request.is_some() => {
                items.add("Waiting for your microphone decision…", None);
            }
            MicrophonePermission::NotDetermined => {
                items.item("Allow Microphone Access…", Choice::Microphone);
            }
            MicrophonePermission::Denied => {
                items.item(
                    "Microphone Access Denied — Open Privacy Settings…",
                    Choice::Microphone,
                );
            }
            MicrophonePermission::Restricted => {
                items.add("Microphone access is restricted on this Mac", None);
            }
        }
        if status.is_none() && !self.quitting && !matches!(self.engine, Engine::Owned { .. }) {
            items.item("Start Cantrip", Choice::StartEngine);
        }
        if autoreleasepool(|_| app_bundle()).is_none() {
            let login = items.add("Open at Login", None);
            login.setToolTip(Some(ns_string!(
                "Available when Cantrip runs from Cantrip.app."
            )));
        } else {
            // SAFETY: querying the main app's own login item has no preconditions.
            let (title, on, choice) = match unsafe { SMAppService::mainAppService().status() } {
                SMAppServiceStatus::Enabled => ("Open at Login", true, Some(Choice::Login)),
                SMAppServiceStatus::RequiresApproval => (
                    "Open at Login — Approve in System Settings…",
                    false,
                    Some(Choice::Login),
                ),
                SMAppServiceStatus::NotRegistered => ("Open at Login", false, Some(Choice::Login)),
                _ => ("Open at Login (unavailable for this copy)", false, None),
            };
            let login = items.add(title, choice);
            if on {
                login.setState(NSControlStateValueOn);
            }
        }
        items.separator();
        items
            .item("Quit Cantrip", Choice::Quit)
            .setKeyEquivalent(ns_string!("q"));
        self.choices = items.choices;
    }

    /// Perform a menu item's choice. True for Quit, which AppKit performs once
    /// no host state is borrowed.
    fn choose(&mut self, tag: isize) -> bool {
        let Some(choice) = usize::try_from(tag)
            .ok()
            .and_then(|index| self.choices.get(index))
            .cloned()
        else {
            return false;
        };
        // The problem line now reports this choice, or the reply to its command.
        self.problem = None;
        let done = match choice {
            Choice::Send(command) => self.send(command),
            Choice::Settings => open_window(&mut self.settings, &["settings"]),
            Choice::Recordings => open_window(&mut self.actions, &["actions"]),
            Choice::CheckSetup => open_window(&mut self.actions, &["actions", "--doctor"]),
            Choice::Microphone => self.microphone_access(),
            Choice::StartEngine => {
                self.start_engine();
                Ok(())
            }
            Choice::Login => toggle_login(),
            Choice::Quit => return true,
        };
        if let Err(error) = done {
            self.problem = Some(format!("{error:#}"));
        }
        false
    }

    /// Ask for microphone access when it was never requested, or open Privacy
    /// & Security when it was denied. Only the operator's answer grants it.
    fn microphone_access(&mut self) -> Result<()> {
        match capture::microphone_permission() {
            MicrophonePermission::NotDetermined if self.microphone_request.is_none() => {
                // The system prompt answers this explicit choice. The request
                // blocks until the operator decides, so it waits off this thread.
                let request = thread::Builder::new()
                    .name("cantrip-microphone".to_owned())
                    .spawn(|| autoreleasepool(|_| capture::request_microphone_permission()))
                    .context("Microphone access was not requested")?;
                self.microphone_request = Some(request);
                Ok(())
            }
            MicrophonePermission::Denied => open_microphone_settings(),
            _ => Ok(()),
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
            request_shutdown();
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
                host.start_engine();
            }
            false
        }
    }

    // SAFETY: the signature matches NSMenuDelegate.
    unsafe impl NSMenuDelegate for AppDelegate {
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &NSMenu) {
            let target: &AnyObject = self;
            if let Some(host) = self.ivars().host.borrow_mut().as_mut() {
                host.fill(menu, target);
            }
        }
    }

    // SAFETY: `tick:` takes its timer and `choose:` its menu item; both return nothing.
    impl AppDelegate {
        #[unsafe(method(tick:))]
        fn tick(&self, _timer: &NSTimer) {
            self.tick_now();
        }

        #[unsafe(method(choose:))]
        fn choose(&self, item: Option<&NSMenuItem>) {
            let quit = match (self.ivars().host.borrow_mut().as_mut(), item) {
                (Some(host), Some(item)) => host.choose(item.tag()),
                _ => false,
            };
            if quit {
                NSApplication::sharedApplication(self.mtm()).terminate(None);
            }
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
}
