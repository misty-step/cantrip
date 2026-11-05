//! OS keyring access for cantrip API keys.

use anyhow::{bail, Context, Result};
use keyring::Entry;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

const SERVICE: &str = "cantrip";
pub(crate) const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
static READING: AtomicBool = AtomicBool::new(false);

fn entry(id: &str) -> Result<Entry> {
    Entry::new(SERVICE, id).with_context(|| format!("opening key '{id}' in OS keyring"))
}

/// Reject values that cannot travel in an HTTP header without ureq echoing
/// the full header value into a transport error (and from there into logs).
fn validate_secret(id: &str, secret: &str) -> Result<()> {
    if secret.is_empty() {
        bail!("key '{id}' is empty");
    }
    if !secret.chars().all(|c| ('!'..='~').contains(&c)) {
        bail!("key '{id}' contains whitespace, control, or non-ASCII characters");
    }
    Ok(())
}

pub fn get(id: &str) -> Result<String> {
    get_until(id, Instant::now() + LOOKUP_TIMEOUT, None)
}

pub(crate) fn get_until(
    id: &str,
    deadline: Instant,
    cancel: Option<&AtomicBool>,
) -> Result<String> {
    anyhow::ensure!(
        !crate::stt::is_cancelled(cancel),
        "OS keyring lookup cancelled"
    );
    #[cfg(target_os = "macos")]
    if READING.load(Ordering::Acquire) {
        return Err(AuthorizationRequired.into());
    }
    let id = id.to_owned();
    read_until(&READING, deadline, cancel, move || read_secret(&id, false))
}

#[derive(Debug)]
pub(crate) struct AuthorizationRequired;

impl std::fmt::Display for AuthorizationRequired {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Keychain access not yet allowed")
    }
}

impl std::error::Error for AuthorizationRequired {}

fn read_secret(id: &str, interactive: bool) -> Result<String> {
    // keyring uses the legacy login Keychain. Per-query authentication UI flags
    // do not suppress that store's prompts; serialize this process-wide guard.
    #[cfg(target_os = "macos")]
    let _interaction = if !interactive {
        Some(
            security_framework::os::macos::keychain::SecKeychain::disable_user_interaction()
                .map_err(|_| anyhow::anyhow!("OS keyring interaction guard unavailable"))?,
        )
    } else {
        None
    };
    #[cfg(not(target_os = "macos"))]
    let _ = interactive;
    let secret = entry(id)?.get_password().map_err(|error| {
        if matches!(error, keyring::Error::NoEntry) {
            return anyhow::Error::new(error);
        }
        #[cfg(target_os = "macos")]
        if let keyring::Error::PlatformFailure(source) | keyring::Error::NoStorageAccess(source) =
            &error
        {
            if source
                .downcast_ref::<security_framework::base::Error>()
                .is_some_and(|error| matches!(error.code(), -25293 | -25308 | -25315 | -128))
            {
                return anyhow::Error::new(AuthorizationRequired);
            }
        }
        let _ = error;
        anyhow::anyhow!("OS keyring credential unavailable")
    })?;
    validate_secret(id, &secret)?;
    Ok(secret)
}

/// The explicit approval path has no deadline, but its caller can abandon it.
/// Only the native worker may own the secret, and approval never retains it.
pub(crate) fn authorize(id: &str, cancel: &AtomicBool) -> Result<()> {
    let requested_id = id.to_owned();
    read_with_deadline(&READING, None, Some(cancel), move || {
        read_secret(&requested_id, true).map(drop)
    })?;
    // A one-time Allow may authorize only the discarded read. Ready means the
    // next take can read without a prompt, not merely that a prompt was answered.
    get_until(id, Instant::now() + LOOKUP_TIMEOUT, Some(cancel)).map(drop)
}

// Native keyring APIs cannot be interrupted. Leave at most one read outstanding,
// discard its late result, and let the existing caller fallback/cancellation run.
// No credentials are cached and a recovered store can serve subsequent takes.
fn read_until<T: Send + 'static>(
    reading: &'static AtomicBool,
    deadline: Instant,
    cancel: Option<&AtomicBool>,
    read: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    read_with_deadline(
        reading,
        Some(deadline.min(Instant::now() + LOOKUP_TIMEOUT)),
        cancel,
        read,
    )
}

fn read_with_deadline<T: Send + 'static>(
    reading: &'static AtomicBool,
    deadline: Option<Instant>,
    cancel: Option<&AtomicBool>,
    read: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    let remaining = || -> Result<Duration> {
        anyhow::ensure!(
            !crate::stt::is_cancelled(cancel),
            "OS keyring lookup cancelled"
        );
        match deadline {
            Some(deadline) => deadline
                .checked_duration_since(Instant::now())
                .filter(|time| !time.is_zero())
                .ok_or_else(|| anyhow::anyhow!("OS keyring lookup timed out")),
            None => Ok(Duration::from_millis(25)),
        }
    };
    loop {
        remaining()?;
        if reading
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            break;
        }
        std::thread::sleep(remaining()?.min(Duration::from_millis(25)));
    }
    let (send, receive) = mpsc::sync_channel(1);
    let worker = std::thread::Builder::new()
        .name("cantrip-keyring".into())
        .spawn(move || {
            struct Reading(&'static AtomicBool);
            impl Drop for Reading {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::Release);
                }
            }
            let result = {
                let _reading = Reading(reading);
                read()
            };
            let _ = send.send(result);
        });
    if worker.is_err() {
        reading.store(false, Ordering::Release);
        bail!("OS keyring lookup could not start");
    }
    loop {
        match receive.recv_timeout(remaining()?.min(Duration::from_millis(25))) {
            Ok(result) => {
                remaining()?;
                return result;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => bail!("OS keyring lookup failed"),
        }
    }
}

pub fn set(id: &str, secret: &str) -> Result<()> {
    validate_secret(id, secret)?;
    entry(id)?
        .set_password(secret)
        .with_context(|| format!("storing key '{id}' in OS keyring"))
}

pub fn delete(id: &str) -> Result<()> {
    entry(id)?
        .delete_credential()
        .with_context(|| format!("deleting key '{id}' from OS keyring"))
}

pub fn exists(id: &str) -> Result<bool> {
    match get(id) {
        Ok(_) => Ok(true),
        Err(error)
            if error
                .downcast_ref::<keyring::Error>()
                .is_some_and(|error| matches!(error, keyring::Error::NoEntry)) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stalled_reads_bound_waits_and_workers_then_recover_without_late_results() {
        static BUSY: AtomicBool = AtomicBool::new(false);
        let (release, blocked) = mpsc::channel();
        let started = Instant::now();
        let result = read_until(
            &BUSY,
            started + Duration::from_millis(60),
            None,
            move || {
                blocked.recv().unwrap();
                Ok("late synthetic secret")
            },
        );
        assert!(result.unwrap_err().to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(BUSY.load(Ordering::Acquire));
        let result = read_until(
            &BUSY,
            Instant::now() + Duration::from_millis(40),
            None,
            || -> Result<()> { panic!("must not accumulate another native lookup") },
        );
        assert!(result.unwrap_err().to_string().contains("timed out"));

        let cancel = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(40));
                cancel.store(true, Ordering::Release);
            });
            let started = Instant::now();
            let result = read_until(
                &BUSY,
                started + LOOKUP_TIMEOUT,
                Some(&cancel),
                || -> Result<()> { panic!("cancelled wait must not start another native lookup") },
            );
            assert!(result.unwrap_err().to_string().contains("cancelled"));
            assert!(started.elapsed() < Duration::from_secs(1));
        });

        release.send(()).unwrap();
        let fresh = read_until(&BUSY, Instant::now() + LOOKUP_TIMEOUT, None, || Ok("fresh"));
        assert_eq!(fresh.unwrap(), "fresh");
        let expired = read_until(&BUSY, Instant::now(), None, || -> Result<()> {
            panic!("expired lookup must not start")
        });
        assert!(expired.unwrap_err().to_string().contains("timed out"));
    }

    #[test]
    fn explicit_authorization_can_wait_without_a_deadline_and_still_cancel() {
        static BUSY: AtomicBool = AtomicBool::new(false);
        let (release, blocked) = mpsc::channel();
        let (entered, native_started) = mpsc::channel();
        let cancel = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let waiter = scope.spawn(|| {
                read_with_deadline(&BUSY, None, Some(&cancel), move || {
                    entered.send(()).unwrap();
                    blocked.recv().unwrap();
                    Ok("discard this late value")
                })
            });
            native_started.recv().unwrap();
            assert!(BUSY.load(Ordering::Acquire));
            cancel.store(true, Ordering::Release);
            assert!(waiter
                .join()
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("cancelled"));
        });
        assert!(BUSY.load(Ordering::Acquire));
        release.send(()).unwrap();
        let fresh = read_until(&BUSY, Instant::now() + LOOKUP_TIMEOUT, None, || Ok(()));
        assert!(fresh.is_ok());
    }

    #[test]
    fn credentials_require_nonempty_visible_ascii() {
        let visible_ascii: String = (b'!'..=b'~').map(char::from).collect();
        assert!(validate_secret("test-key", &visible_ascii).is_ok());

        for secret in [
            "",
            "token value",
            "token\tvalue",
            "token\r\nvalue",
            "token\0value",
            "token\u{7f}",
            "tokené",
        ] {
            assert!(validate_secret("test-key", secret).is_err());
        }
    }

    #[test]
    fn rejected_credentials_identify_the_key_without_disclosing_the_secret() {
        let error = validate_secret("cleanup-key", "synthetic-private-credential\n")
            .unwrap_err()
            .to_string();
        assert!(error.contains("cleanup-key"));
        assert!(!error.contains("synthetic-private-credential"));
    }
}
