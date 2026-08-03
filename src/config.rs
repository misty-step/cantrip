//! User configuration for the cantrip daemon.

use crate::{inject::InjectionMode, paths};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::ErrorKind;

const DEFAULT_MODEL: &str = "parakeet-tdt-0.6b-v3-int8";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    pub injection: InjectionMode,
    pub keep_warm: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_source: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: DEFAULT_MODEL.to_owned(),
            language: None,
            injection: InjectionMode::Auto,
            keep_warm: true,
            audio_source: None,
        }
    }
}

impl Config {
    /// Load the user configuration, using defaults when it does not exist.
    pub fn load() -> Result<Self> {
        let path = paths::config_file().context("locating config file")?;
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", path.display()));
            }
        };
        toml::from_str(&contents).with_context(|| format!("parsing {}", path.display()))
    }

    /// Write a commented default configuration when no configuration exists.
    pub fn write_default_if_missing() -> Result<()> {
        write_default_if_missing()
    }
}

/// Write a commented default configuration when no configuration exists.
pub fn write_default_if_missing() -> Result<()> {
    let path = paths::config_file().context("locating config file")?;
    if path.exists() {
        return Ok(());
    }

    let parent = path
        .parent()
        .context("config file has no parent directory")?
        .to_path_buf();
    paths::ensure_dir(parent).context("creating config directory")?;
    let body = toml::to_string_pretty(&Config::default()).context("serializing default config")?;
    let contents =
        format!("# cantrip configuration\n# Edit this file, then restart the daemon.\n\n{body}");

    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            use std::io::Write;
            file.write_all(contents.as_bytes())
                .with_context(|| format!("writing {}", path.display()))?;
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(error).with_context(|| format!("creating {}", path.display()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_toml_uses_defaults() {
        let config: Config = toml::from_str("").expect("empty TOML should parse");
        assert_eq!(config.model, DEFAULT_MODEL);
        assert_eq!(config.language, None);
        assert_eq!(config.injection, InjectionMode::Auto);
        assert!(config.keep_warm);
        assert_eq!(config.audio_source, None);
    }

    #[test]
    fn partial_toml_uses_defaults_for_missing_fields() {
        let config: Config = toml::from_str("injection = \"clipboard\"\nkeep_warm = false\n")
            .expect("partial TOML should parse");
        assert_eq!(config.model, DEFAULT_MODEL);
        assert_eq!(config.injection, InjectionMode::Clipboard);
        assert!(!config.keep_warm);
        assert_eq!(config.language, None);
        assert_eq!(config.audio_source, None);
    }
}
