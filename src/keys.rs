//! OS keyring access for cantrip API keys.

use anyhow::{bail, Context, Result};
use keyring::Entry;

const SERVICE: &str = "cantrip";

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
    let secret = entry(id)?
        .get_password()
        .with_context(|| format!("reading key '{id}' from OS keyring"))?;
    validate_secret(id, &secret)?;
    Ok(secret)
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
    let entry = entry(id)?;
    match entry.get_password() {
        Ok(_) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(error) => Err(error).with_context(|| format!("checking key '{id}' in OS keyring")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_empty_secret() {
        let result = validate_secret("empty", "");
        assert!(result.is_err(), "expected rejection for empty secret");
    }

    #[test]
    fn reject_whitespace_only_secret() {
        for secret in [" ", "\t", "\n", " \t\n ", "\u{2003}"] {
            let result = validate_secret("ws", secret);
            assert!(result.is_err(), "expected rejection for {secret:?}");
        }
    }

    #[test]
    fn reject_non_ascii_secret() {
        for secret in ["émoji", "🔑", "caf\u{e9}", "tok\u{2028}en"] {
            let result = validate_secret("non-ascii", secret);
            assert!(result.is_err(), "expected rejection for {secret:?}");
        }
    }

    #[test]
    fn accept_normal_token() {
        assert!(validate_secret("ok", "ghtu_AbC123").is_ok());
        assert!(validate_secret("ok", "a~zA-Z0-9!#$%&'()*+-./:;<=>?@[\\]^_`{|}").is_ok());
        assert!(validate_secret("ok", "single").is_ok());
    }

    #[test]
    fn error_message_mentions_key_id() {
        let err = validate_secret("mykey", " ").unwrap_err().to_string();
        assert!(err.contains("mykey"), "error should name the key: {err}");
    }
}
