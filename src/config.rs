//! User configuration for the cantrip daemon.

use crate::{inject::InjectionMode, models, paths};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::ErrorKind;

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub injection: InjectionMode,
    pub keep_warm: bool,
    pub audio_source: Option<String>,
    pub vocabulary: Vec<String>,
    pub stt: SttConfig,
    pub postproc: PostprocConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct SttConfig {
    pub model: String,
    pub endpoint: Option<String>,
    pub api_key_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct PostprocConfig {
    pub enabled: bool,
    pub endpoint: String,
    pub model: String,
    pub api_key_id: Option<String>,
    pub timeout_ms: u64,
    pub instructions: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            injection: InjectionMode::Auto,
            keep_warm: true,
            audio_source: None,
            vocabulary: Vec::new(),
            stt: SttConfig::default(),
            postproc: PostprocConfig::default(),
        }
    }
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            model: "parakeet-tdt-0.6b-v3-int8".to_owned(),
            endpoint: None,
            api_key_id: None,
        }
    }
}

impl Default for PostprocConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: "http://localhost:11434/v1".to_owned(),
            model: String::new(),
            api_key_id: None,
            timeout_ms: 30_000,
            instructions: "You are cleaning up a dictated transcript. Remove filler words and false starts (such as um, uh, like, you know) and repeated words. Add correct punctuation, capitalization, and spelling. Keep the speaker's meaning and all meaningful words. Output only the corrected text, with no preamble."
                .to_owned(),
        }
    }
}

impl Config {
    /// Load the user configuration, using defaults when it does not exist.
    pub fn load() -> Result<Self> {
        let path = paths::config_file().context("locating config file")?;
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", path.display()));
            }
        };
        let config: Self =
            toml::from_str(&contents).with_context(|| format!("parsing {}", path.display()))?;
        config
            .validate()
            .with_context(|| format!("validating {}", path.display()))?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.postproc.enabled && self.postproc.model.trim().is_empty() {
            bail!("postproc.enabled = true requires postproc.model");
        }
        if self.stt.endpoint.is_none() {
            models::require(&self.stt.model)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_toml_uses_defaults() {
        let config: Config = toml::from_str("").expect("empty TOML should parse");
        assert_eq!(config.injection, InjectionMode::Auto);
        assert!(config.keep_warm);
        assert_eq!(config.audio_source, None);
        assert!(config.vocabulary.is_empty());
        assert_eq!(config.stt, SttConfig::default());
        assert_eq!(config.postproc, PostprocConfig::default());
    }

    #[test]
    fn partial_toml_uses_defaults_for_missing_fields() {
        let config: Config = toml::from_str("injection = \"clipboard\"\nkeep_warm = false\n")
            .expect("partial TOML should parse");
        assert_eq!(config.injection, InjectionMode::Clipboard);
        assert!(!config.keep_warm);
        assert_eq!(config.audio_source, None);
        assert_eq!(config.stt, SttConfig::default());
        assert_eq!(config.postproc, PostprocConfig::default());
    }

    #[test]
    fn partial_postproc_table_uses_defaults() {
        let config: Config = toml::from_str("[postproc]\nenabled = true\nmodel = \"llama3\"")
            .expect("partial postproc table should parse");
        assert!(config.postproc.enabled);
        assert_eq!(config.postproc.model, "llama3");
        assert_eq!(config.postproc.endpoint, "http://localhost:11434/v1");
        assert_eq!(config.postproc.timeout_ms, 30_000);
        assert_eq!(config.postproc.api_key_id, None);
        assert_eq!(
            config.postproc.instructions,
            PostprocConfig::default().instructions
        );
    }

    #[test]
    fn validation_rejects_enabled_postproc_without_model() {
        let config = Config {
            postproc: PostprocConfig {
                enabled: true,
                ..PostprocConfig::default()
            },
            ..Config::default()
        };
        let error = config
            .validate()
            .expect_err("empty postproc model must fail");
        assert!(error
            .to_string()
            .contains("postproc.enabled = true requires postproc.model"));
    }

    #[test]
    fn validation_rejects_unknown_local_stt_model() {
        let config = Config {
            stt: SttConfig {
                model: "missing-model".to_owned(),
                ..SttConfig::default()
            },
            ..Config::default()
        };
        let error = config
            .validate()
            .expect_err("unknown local model must fail");
        let message = error.to_string();
        assert!(message.contains("missing-model"));
        assert!(message.contains("parakeet-tdt-0.6b-v3-int8"));
    }
}
