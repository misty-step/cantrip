//! Small blocking D-Bus helpers shared by the Secret Service, portal and tray
//! code: wait for one signal on one object path, with a bound.

use dbus::blocking::Connection;
use dbus::message::MatchRule;
use dbus::Message;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Subscribe to `interface.member` signals on `path`. Call *before* the method
/// that triggers the signal, then [`Pending::wait`].
pub struct Pending<'c> {
    conn: &'c Connection,
    slot: Arc<Mutex<Option<Message>>>,
    token: Option<dbus::channel::Token>,
}

pub fn subscribe<'c>(
    conn: &'c Connection,
    path: &str,
    interface: &'static str,
    member: &'static str,
) -> Result<Pending<'c>, dbus::Error> {
    let slot: Arc<Mutex<Option<Message>>> = Arc::new(Mutex::new(None));
    let s = slot.clone();
    let rule = MatchRule::new_signal(interface, member).with_path(path.to_string());
    let token = conn.add_match(rule, move |_: (), _, msg: &Message| {
        if let Ok(copy) = msg.duplicate() {
            *s.lock().unwrap() = Some(copy);
        }
        false
    })?;
    Ok(Pending {
        conn,
        slot,
        token: Some(token),
    })
}

impl Pending<'_> {
    /// Process the connection until the signal arrives or `limit` elapses.
    pub fn wait(mut self, limit: Duration) -> Option<Message> {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(m) = self.slot.lock().unwrap().take() {
                self.token = None; // callback returned false: match removed
                return Some(m);
            }
            let left = deadline.checked_duration_since(Instant::now())?;
            if self
                .conn
                .process(left.min(Duration::from_millis(200)))
                .is_err()
            {
                return None;
            }
        }
    }
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if let Some(t) = self.token.take() {
            let _ = self.conn.remove_match(t);
        }
    }
}

/// Whether `name` currently has an owner on the session bus.
pub fn name_has_owner(conn: &Connection, name: &str) -> bool {
    let proxy = conn.with_proxy(
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        Duration::from_millis(500),
    );
    proxy
        .method_call::<(bool,), _, _, _>("org.freedesktop.DBus", "NameHasOwner", (name,))
        .map(|(b,)| b)
        .unwrap_or(false)
}
