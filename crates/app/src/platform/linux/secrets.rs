//! The OpenRouter key in the freedesktop Secret Service (GNOME Keyring,
//! KWallet, KeePassXC…), over blocking D-Bus. The key is never written to a
//! file, a log or Cantrip's settings.
//!
//! Any failure to talk to the service is `SecretError::Unavailable` — never
//! `Ok(None)`, which is reserved for "the service answered: no such item".

use super::dbus_util;
use cantrip_core::ports::{SecretError, SecretStore};
use dbus::arg::{PropMap, RefArg, Variant};
use dbus::blocking::Connection;
use dbus::Path;
use std::collections::HashMap;
use std::time::Duration;

const SERVICE: &str = "org.freedesktop.secrets";
const SERVICE_PATH: &str = "/org/freedesktop/secrets";
const IFACE_SERVICE: &str = "org.freedesktop.Secret.Service";
const IFACE_COLLECTION: &str = "org.freedesktop.Secret.Collection";
const IFACE_ITEM: &str = "org.freedesktop.Secret.Item";
const IFACE_PROMPT: &str = "org.freedesktop.Secret.Prompt";
const CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// An unlock prompt waits for a person typing a password, but not forever.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(120);

/// Attributes identifying Cantrip's one item.
pub fn attributes() -> HashMap<String, String> {
    HashMap::from([
        ("application".to_string(), "cantrip".to_string()),
        ("key".to_string(), "openrouter".to_string()),
    ])
}

type Secret = (Path<'static>, Vec<u8>, Vec<u8>, String);

#[derive(Default)]
pub struct SecretService;

impl SecretService {
    pub fn new() -> Self {
        SecretService
    }
}

fn unavailable(what: &str, e: impl std::fmt::Display) -> SecretError {
    // dbus error names/messages carry no secret material.
    SecretError::Unavailable(format!("Secret store {what} failed: {e}"))
}

struct Session<'c> {
    conn: &'c Connection,
    path: Path<'static>,
}

impl<'c> Session<'c> {
    fn open(conn: &'c Connection) -> Result<Session<'c>, SecretError> {
        if !dbus_util::name_has_owner(conn, SERVICE) && !activatable(conn) {
            return Err(SecretError::Unavailable(
                "No secret store (GNOME Keyring, KWallet…) is running".into(),
            ));
        }
        let svc = conn.with_proxy(SERVICE, SERVICE_PATH, CALL_TIMEOUT);
        let (_out, path): (Variant<Box<dyn RefArg>>, Path<'static>) = svc
            .method_call(
                IFACE_SERVICE,
                "OpenSession",
                ("plain", Variant(Box::new(String::new()) as Box<dyn RefArg>)),
            )
            .map_err(|e| unavailable("session", e))?;
        Ok(Session { conn, path })
    }

    fn service(&self) -> dbus::blocking::Proxy<'_, &'c Connection> {
        self.conn.with_proxy(SERVICE, SERVICE_PATH, CALL_TIMEOUT)
    }

    /// Unlock `objects`, completing a prompt if the service needs one.
    fn unlock(&self, objects: Vec<Path<'static>>) -> Result<(), SecretError> {
        let (_unlocked, prompt): (Vec<Path<'static>>, Path<'static>) = self
            .service()
            .method_call(IFACE_SERVICE, "Unlock", (objects,))
            .map_err(|e| unavailable("unlock", e))?;
        self.complete_prompt(prompt)
    }

    fn complete_prompt(&self, prompt: Path<'static>) -> Result<(), SecretError> {
        if &*prompt == "/" {
            return Ok(());
        }
        let pending = dbus_util::subscribe(self.conn, &prompt, IFACE_PROMPT, "Completed")
            .map_err(|e| unavailable("prompt", e))?;
        self.conn
            .with_proxy(SERVICE, prompt.clone(), CALL_TIMEOUT)
            .method_call::<(), _, _, _>(IFACE_PROMPT, "Prompt", ("",))
            .map_err(|e| unavailable("prompt", e))?;
        let msg = pending
            .wait(PROMPT_TIMEOUT)
            .ok_or_else(|| SecretError::Unavailable("Secret store prompt timed out".into()))?;
        let dismissed: bool = msg.read1().unwrap_or(true);
        if dismissed {
            Err(SecretError::Unavailable(
                "Secret store unlock was dismissed".into(),
            ))
        } else {
            Ok(())
        }
    }

    fn find(&self) -> Result<Vec<Path<'static>>, SecretError> {
        let (mut unlocked, locked): (Vec<Path<'static>>, Vec<Path<'static>>) = self
            .service()
            .method_call(IFACE_SERVICE, "SearchItems", (attributes(),))
            .map_err(|e| unavailable("search", e))?;
        if !locked.is_empty() {
            self.unlock(locked.clone())?;
            unlocked.extend(locked);
        }
        Ok(unlocked)
    }

    fn get_secret(&self, item: &Path<'static>) -> Result<Vec<u8>, SecretError> {
        let (secret,): (Secret,) = self
            .conn
            .with_proxy(SERVICE, item.clone(), CALL_TIMEOUT)
            .method_call(IFACE_ITEM, "GetSecret", (self.path.clone(),))
            .map_err(|e| unavailable("read", e))?;
        Ok(secret.2)
    }

    fn default_collection(&self) -> Result<Path<'static>, SecretError> {
        let (alias,): (Path<'static>,) = self
            .service()
            .method_call(IFACE_SERVICE, "ReadAlias", ("default",))
            .map_err(|e| unavailable("collection lookup", e))?;
        if &*alias != "/" {
            return Ok(alias);
        }
        let mut props: PropMap = HashMap::new();
        props.insert(
            "org.freedesktop.Secret.Collection.Label".into(),
            Variant(Box::new("Login".to_string())),
        );
        let (collection, prompt): (Path<'static>, Path<'static>) = self
            .service()
            .method_call(IFACE_SERVICE, "CreateCollection", (props, "default"))
            .map_err(|e| unavailable("collection create", e))?;
        self.complete_prompt(prompt)?;
        if &*collection != "/" {
            return Ok(collection);
        }
        let (alias,): (Path<'static>,) = self
            .service()
            .method_call(IFACE_SERVICE, "ReadAlias", ("default",))
            .map_err(|e| unavailable("collection lookup", e))?;
        if &*alias == "/" {
            Err(SecretError::Unavailable(
                "The secret store has no default keyring".into(),
            ))
        } else {
            Ok(alias)
        }
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        let _ = self
            .conn
            .with_proxy(SERVICE, self.path.clone(), CALL_TIMEOUT)
            .method_call::<(), _, _, _>("org.freedesktop.Secret.Session", "Close", ());
    }
}

fn activatable(conn: &Connection) -> bool {
    let proxy = conn.with_proxy(
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        CALL_TIMEOUT,
    );
    proxy
        .method_call::<(Vec<String>,), _, _, _>("org.freedesktop.DBus", "ListActivatableNames", ())
        .map(|(names,)| names.iter().any(|n| n == SERVICE))
        .unwrap_or(false)
}

fn connect() -> Result<Connection, SecretError> {
    Connection::new_session().map_err(|e| unavailable("connection", e))
}

impl SecretStore for SecretService {
    fn get_key(&self) -> Result<Option<String>, SecretError> {
        let conn = connect()?;
        let s = Session::open(&conn)?;
        let Some(item) = s.find()?.into_iter().next() else {
            return Ok(None);
        };
        let bytes = s.get_secret(&item)?;
        let key = String::from_utf8(bytes)
            .map_err(|_| SecretError::Unavailable("Stored key is not text".into()))?;
        let key = key.trim().to_string();
        Ok((!key.is_empty()).then_some(key))
    }

    fn set_key(&self, key: &str) -> Result<(), SecretError> {
        let conn = connect()?;
        let s = Session::open(&conn)?;
        let collection = s.default_collection()?;
        s.unlock(vec![collection.clone()])?;
        let mut props: PropMap = HashMap::new();
        props.insert(
            "org.freedesktop.Secret.Item.Label".into(),
            Variant(Box::new("Cantrip OpenRouter key".to_string())),
        );
        props.insert(
            "org.freedesktop.Secret.Item.Attributes".into(),
            Variant(Box::new(attributes())),
        );
        let secret: Secret = (
            s.path.clone(),
            Vec::new(),
            key.trim().as_bytes().to_vec(),
            "text/plain; charset=utf8".to_string(),
        );
        let (_item, prompt): (Path<'static>, Path<'static>) = conn
            .with_proxy(SERVICE, collection, CALL_TIMEOUT)
            .method_call(IFACE_COLLECTION, "CreateItem", (props, secret, true))
            .map_err(|e| unavailable("write", e))?;
        s.complete_prompt(prompt)
    }

    fn delete_key(&self) -> Result<(), SecretError> {
        let conn = connect()?;
        let s = Session::open(&conn)?;
        for item in s.find()? {
            let (prompt,): (Path<'static>,) = conn
                .with_proxy(SERVICE, item, CALL_TIMEOUT)
                .method_call(IFACE_ITEM, "Delete", ())
                .map_err(|e| unavailable("delete", e))?;
            s.complete_prompt(prompt)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributes_identify_one_cantrip_item() {
        let a = attributes();
        assert_eq!(a.len(), 2);
        assert_eq!(a["application"], "cantrip");
        assert_eq!(a["key"], "openrouter");
    }

    /// Against a real gnome-keyring-daemon on a private session bus. Run via
    /// `scripts/test-linux-secret-service` (sets DBUS_SESSION_BUS_ADDRESS).
    #[test]
    #[ignore]
    fn round_trip_against_a_real_secret_service() {
        let s = SecretService::new();
        s.delete_key().unwrap();
        assert_eq!(s.get_key().unwrap(), None);
        s.set_key("sk-or-test-not-a-real-key").unwrap();
        assert_eq!(
            s.get_key().unwrap().as_deref(),
            Some("sk-or-test-not-a-real-key")
        );
        s.set_key("sk-or-test-replaced").unwrap();
        assert_eq!(s.get_key().unwrap().as_deref(), Some("sk-or-test-replaced"));
        s.delete_key().unwrap();
        assert_eq!(s.get_key().unwrap(), None);
    }

    /// An unreachable secret store is an error, never "no key".
    #[test]
    #[ignore]
    fn missing_service_is_unavailable_not_none() {
        let r = SecretService::new().get_key();
        assert!(matches!(r, Err(SecretError::Unavailable(_))), "{r:?}");
    }
}
