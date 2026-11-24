//! Cantrip v2 entry point.
//!
//! `cantrip`            run the tray app (a second launch shows the popover)
//! `cantrip popover`    show the popover (the launcher entry on trayless sessions)
//! `cantrip settings`   show Settings
//! `cantrip toggle`     start/stop dictation (bindable as a compositor shortcut)
//! `cantrip ipc …`      `hotkey-down` / `hotkey-up` / `cancel`: the lines Sway
//!                      bindings run (one socket write, before any UI setup)
//! `cantrip gallery …`  render UI evidence screenshots from fixture state
//! `cantrip e2e …`      headless end-to-end runs over recorded audio (verification)

mod cli;
mod ipc;
mod net;
mod platform;
mod ui;

use cantrip_core::app::{default_settings_path, AppCore, AppDeps};
use cantrip_core::dictation::DictationDeps;
use cantrip_core::meeting::MeetingDeps;
use cantrip_core::speech::OpenRouter;
use cantrip_core::staging::Staging;
use platform::{Hotkeys, UiRequest};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Compositor-bound shortcut lines: the fast path, nothing else starts.
    if args.first().map(String::as_str) == Some("ipc") {
        if let Err(e) = ipc::run_cli(&args[1..]) {
            eprintln!("cantrip: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    let code = match args.first().map(String::as_str) {
        None => run_app(None),
        Some("popover") => run_app(Some(UiRequest::ShowPopover)),
        Some("settings") => run_app(Some(UiRequest::ShowSettings)),
        Some("toggle") => match ipc::send("toggle") {
            Ok(true) => Ok(()),
            _ => Err(anyhow::anyhow!("Cantrip is not running")),
        },
        Some("gallery") => ui::gallery::run_gallery(&args[1..]),
        Some("e2e") => cli::run(&args[1..]),
        Some("--version" | "-V") => {
            println!("cantrip {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(other) => Err(anyhow::anyhow!(
            "unknown command `{other}` (try: popover, settings, toggle)"
        )),
    };
    if let Err(e) = code {
        eprintln!("cantrip: {e:#}");
        std::process::exit(1);
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/".into())
}

/// Run the app, or hand `request` to the instance that already runs.
fn run_app(request: Option<UiRequest>) -> anyhow::Result<()> {
    let cmd = match request {
        Some(UiRequest::ShowSettings) => "settings",
        _ => "popover",
    };
    if ipc::send(cmd).unwrap_or(false) {
        return Ok(());
    }

    // SIGTERM / SIGINT / SIGHUP quit like the Quit control (bindings and
    // grabs are released on the way out). Blocked before any thread starts
    // so only the waiter receives them; children get a clean mask.
    let quit_signals = block_quit_signals();

    let (req_tx, req_rx) = mpsc::channel();
    let (platform, tray) = platform::detect(req_tx.clone())?;
    eprintln!("[cantrip] session {}", platform.session);

    let staging = Staging::open(&Staging::default_root())?;
    let openrouter = OpenRouter::new(platform.transport.clone());
    let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(ui::wake);
    let core = Arc::new(AppCore::new(AppDeps {
        dictation: DictationDeps {
            recorder: platform.recorder.clone(),
            desktop: platform.desktop.clone(),
            clipboard: platform.clipboard.clone(),
            secrets: platform.secrets.clone(),
            local: platform.local.clone(),
            openrouter: openrouter.clone(),
            staging: staging.clone(),
            notify: notify.clone(),
        },
        meeting: MeetingDeps {
            capture: platform.meeting.clone(),
            secrets: platform.secrets.clone(),
            openrouter,
            staging,
            notify,
        },
        recorder: platform.recorder.clone(),
        secrets: platform.secrets.clone(),
        login: platform.login.clone(),
        settings_path: default_settings_path(&home()),
        home: home(),
    })?);

    // Shortcut events → the core's press-length state machine. Compositor-
    // bound shortcuts (Sway) arrive on the IPC socket into the same channel.
    let (key_tx, key_rx) = mpsc::channel();
    let _server = ipc::serve(
        &ipc::socket_path(),
        ipc::Sinks {
            ui: req_tx.clone(),
            keys: key_tx.clone(),
        },
    )?;
    if let Some(set) = quit_signals {
        let tx = req_tx.clone();
        std::thread::spawn(move || wait_for_quit_signal(set, tx));
    }
    {
        let core = core.clone();
        std::thread::spawn(move || {
            for ev in key_rx {
                core.on_key(ev);
            }
        });
    }
    let mut hotkeys: Box<dyn Hotkeys> = platform.hotkeys;
    let mut chord = core.settings().shortcut;
    match hotkeys.register(&chord, key_tx.clone()) {
        Ok(mode) => eprintln!(
            "[cantrip] shortcut {} via {}: {}",
            chord.display(),
            hotkeys.backend(),
            mode.describe()
        ),
        Err(e) => eprintln!("[cantrip] shortcut unavailable: {e}"),
    }
    {
        let core = core.clone();
        std::thread::spawn(move || core.refresh());
    }

    // Without a tray (stock GNOME, bare X11) the popover is the way in.
    if request.is_some() || tray.is_none() {
        let _ = req_tx.send(request.unwrap_or(UiRequest::ShowPopover));
    }

    // Sleep stops an active meeting with a message (best effort per OS).
    let sleep = {
        let core = core.clone();
        platform::watch_sleep(Arc::new(move || core.on_system_sleep()))
    };

    let tick_core = core.clone();
    let mut escape = false;
    let mut recording = false;
    let mut meeting = false;
    let tick = Box::new(move || {
        if let Some(next) = tick_core.take_shortcut_change() {
            match hotkeys.register(&next, key_tx.clone()) {
                Ok(mode) => {
                    eprintln!(
                        "[cantrip] shortcut {} via {}: {}",
                        next.display(),
                        hotkeys.backend(),
                        mode.describe()
                    );
                    chord = next;
                }
                Err(e) => {
                    let _ = hotkeys.register(&chord, key_tx.clone());
                    tick_core.revert_shortcut(chord.clone(), &e);
                }
            }
        }
        let want = tick_core.wants_escape();
        if want != escape {
            hotkeys.set_escape_grab(want);
            escape = want;
        }
        let in_meeting = tick_core.meetings().active();
        if in_meeting != meeting {
            sleep.set_meeting_active(in_meeting);
            meeting = in_meeting;
        }
        let rec = tick_core.dictation().recording() || in_meeting;
        if rec != recording {
            if let Some(t) = &tray {
                t.set_recording(rec);
            }
            recording = rec;
        }
    });
    ui::run(core.clone(), platform.ui, req_rx, tick)?;
    // Let a meeting stopped by Quit finalize its staged audio.
    core.meetings().wait_idle(std::time::Duration::from_secs(2));
    Ok(())
}

/// Block the quit signals in this (and every later) thread; `None` if the
/// mask could not be set (the default handlers then stay in place).
fn block_quit_signals() -> Option<libc::sigset_t> {
    // SAFETY: plain sigset manipulation on an owned, zeroed set.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for s in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::sigaddset(&mut set, s);
        }
        (libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) == 0).then_some(set)
    }
}

fn wait_for_quit_signal(set: libc::sigset_t, ui: mpsc::Sender<UiRequest>) {
    let mut sig = 0;
    // SAFETY: sigwait on a set blocked in every thread.
    if unsafe { libc::sigwait(&set, &mut sig) } == 0 {
        eprintln!("[cantrip] quitting on signal {sig}");
        let _ = ui.send(UiRequest::Quit);
        ui::wake();
    }
}
