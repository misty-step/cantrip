//! Sleep handling on Linux through systemd-logind on the system bus
//! (DESIGN.md › Meeting pipeline: idle sleep is prevented; sleep anyway stops
//! the meeting with a message).
//!
//! While a meeting records, Cantrip holds two logind inhibitor locks:
//! `idle` in `block` mode (logind's idle action never suspends mid-meeting)
//! and `sleep` in `delay` mode (an explicit or forced suspend waits, at most
//! logind's `InhibitDelayMaxSec`, until Cantrip has stopped the meeting and
//! finalized its audio). `PrepareForSleep(true)`, accepted only from the
//! `org.freedesktop.login1` owner, stops the meeting; then both locks are
//! released so the suspend proceeds.
//!
//! Best effort by design: without a system bus or logind (containers, some
//! minimal sessions) nothing is held and nothing is reported as an error;
//! sleep then simply cannot be anticipated (the session epoch still turns
//! any take that spans it copy-only).

use crate::platform::SleepWatch;
use dbus::arg::OwnedFd;
use dbus::blocking::Connection;
use dbus::message::MatchRule;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

const DEST: &str = "org.freedesktop.login1";
const PATH: &str = "/org/freedesktop/login1";
const IFACE: &str = "org.freedesktop.login1.Manager";

pub struct LogindSleep {
    tx: mpsc::Sender<bool>,
}

impl SleepWatch for LogindSleep {
    fn set_meeting_active(&self, active: bool) {
        let _ = self.tx.send(active);
    }
}

/// Start watching; `on_sleep` runs on the watcher thread when the system is
/// about to sleep and must return once the meeting's audio is finalized.
pub fn watch(on_sleep: Arc<dyn Fn() + Send + Sync>) -> Box<dyn SleepWatch> {
    let (tx, rx) = mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("cantrip-sleep".into())
        .spawn(move || run(&rx, on_sleep.as_ref()));
    Box::new(LogindSleep { tx })
}

/// The inhibitor locks held while a meeting records; dropping closes them.
struct Locks {
    _sleep: Option<OwnedFd>,
    _idle: Option<OwnedFd>,
}

fn inhibit(conn: &Connection, what: &str, why: &str, mode: &str) -> Option<OwnedFd> {
    let proxy = conn.with_proxy(DEST, PATH, Duration::from_secs(2));
    proxy
        .method_call::<(OwnedFd,), _, _, _>(IFACE, "Inhibit", (what, "Cantrip", why, mode))
        .ok()
        .map(|(fd,)| fd)
}

fn take_locks(conn: &Connection) -> Locks {
    let locks = Locks {
        _sleep: inhibit(
            conn,
            "sleep",
            "Stopping the meeting recording before sleep",
            "delay",
        ),
        _idle: inhibit(conn, "idle", "Recording a meeting", "block"),
    };
    eprintln!(
        "[cantrip] meeting sleep locks: sleep-delay={} idle-block={}",
        locks._sleep.is_some(),
        locks._idle.is_some()
    );
    locks
}

fn run(rx: &Receiver<bool>, on_sleep: &(dyn Fn() + Send + Sync)) {
    let conn = match Connection::new_system() {
        Ok(c) => c,
        Err(_) => {
            eprintln!("[cantrip] sleep watch unavailable: no system bus");
            return;
        }
    };
    let going = Arc::new(AtomicBool::new(false));
    let g = going.clone();
    let rule = MatchRule::new_signal(IFACE, "PrepareForSleep")
        .with_path(PATH)
        .with_sender(DEST);
    if conn
        .add_match(rule, move |(start,): (bool,), _, _| {
            if start {
                g.store(true, Ordering::SeqCst);
            }
            true
        })
        .is_err()
    {
        eprintln!("[cantrip] sleep watch unavailable: cannot subscribe");
        return;
    }
    let mut meeting = false;
    let mut locks: Option<Locks> = None;
    loop {
        if conn.process(Duration::from_millis(200)).is_err() {
            return;
        }
        loop {
            match rx.try_recv() {
                Ok(a) => meeting = a,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        if going.swap(false, Ordering::SeqCst) {
            eprintln!("[cantrip] system is going to sleep");
            on_sleep();
            // Audio finalized: let the suspend go on.
            locks = None;
            meeting = false;
            // Drop updates queued before the stop took effect.
            while rx.try_recv().is_ok() {}
            continue;
        }
        match (meeting, locks.is_some()) {
            (true, false) => locks = Some(take_locks(&conn)),
            (false, true) => locks = None,
            _ => {}
        }
    }
}
