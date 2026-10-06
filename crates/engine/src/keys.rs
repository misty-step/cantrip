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
    use super::validate_secret;

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
