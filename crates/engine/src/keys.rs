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
    let id = id.to_owned();
    read_until(&READING, deadline, cancel, move || {
        let secret = entry(&id)?
            .get_password()
            .map_err(|_| anyhow::anyhow!("OS keyring credential unavailable"))?;
        validate_secret(&id, &secret)?;
        Ok(secret)
    })
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
    let deadline = deadline.min(Instant::now() + LOOKUP_TIMEOUT);
    let remaining = || -> Result<Duration> {
        anyhow::ensure!(
            !crate::stt::is_cancelled(cancel),
            "OS keyring lookup cancelled"
        );
        deadline
            .checked_duration_since(Instant::now())
            .filter(|time| !time.is_zero())
            .ok_or_else(|| anyhow::anyhow!("OS keyring lookup timed out"))
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
    let id = id.to_owned();
    read_until(
        &READING,
        Instant::now() + LOOKUP_TIMEOUT,
        None,
        move || match entry(&id)?.get_password() {
            Ok(_) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(_) => bail!("OS keyring credential unavailable"),
        },
    )
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
