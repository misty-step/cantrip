//! User configuration for the cantrip daemon.

use crate::{inject::InjectionMode, models, paths};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub injection: InjectionMode,
    pub keep_warm: bool,
    pub audio_source: Option<String>,
    pub vocabulary: Vec<String>,
    pub stt: SttConfig,
    pub postproc: PostprocConfig,
    pub telemetry: TelemetryConfig,
    pub hud: HudConfig,
    pub handoff: BTreeMap<String, HandoffTarget>,
    #[serde(skip)]
    pub selected_handoff: Option<(String, HandoffTarget)>,
}

/// A local process receiving a completed take on standard input, without a shell.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct HandoffTarget {
    pub command: Vec<String>,
    #[serde(
        default = "default_handoff_timeout",
        deserialize_with = "clamped_handoff_timeout"
    )]
    pub timeout_seconds: u64,
}

const fn default_handoff_timeout() -> u64 {
    15
}
fn clamped_handoff_timeout<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<u64, D::Error> {
    u64::deserialize(deserializer).map(|value| value.clamp(1, 120))
}

impl HandoffTarget {
    pub fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.timeout_seconds.clamp(1, 120))
    }
}

pub fn valid_handoff_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte))
}

/// Accessibility preferences for the passive dictation HUD.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct HudConfig {
    pub labels: bool,
    /// None follows the desktop; an explicit value overrides that preference.
    pub reduced_motion: Option<bool>,
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
    /// Reasoning effort for providers that support it (OpenRouter
    /// `reasoning.effort`): e.g. low, medium, high. Omitted from the
    /// request when unset; local endpoints ignore unknown fields.
    pub reasoning_effort: Option<String>,
    pub timeout_ms: u64,
    pub passes: u8,
    /// Skip cleanup when the raw transcript has fewer than this many chars.
    /// `0` disables the skip. Default 40 covers short commands without a cloud round-trip.
    pub min_chars: usize,
    pub instructions: String,
    /// Optional TypeSafe System One decision model (e.g. "typesafe/jev-1.13" or "jev-latest")
    /// for fast pre-cleanup triage and post-cleanup watchdog validation.
    /// Disabled when None or empty.
    pub decision_model: Option<String>,
    /// Optional endpoint override for the decision model. Defaults to
    /// OpenRouter decisions endpoint for "typesafe/*" models or TypeSafe native otherwise.
    pub decision_endpoint: Option<String>,
    /// Optional keyring ID for decision authentication. Defaults to "openrouter"
    /// for OpenRouter models or "typesafe" otherwise, falling back to api_key_id.
    pub decision_api_key_id: Option<String>,
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
            telemetry: TelemetryConfig::default(),
            hud: HudConfig::default(),
            handoff: BTreeMap::new(),
            selected_handoff: None,
        }
    }
}

/// Opt-in Langfuse export. Disabled by default; when enabled it must point
/// at an OTLP endpoint and carry the project's public key (the secret half
/// lives in the OS keyring under `api_key_id`).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct TelemetryConfig {
    pub enabled: bool,
    pub endpoint: String,
    pub public_key: String,
    pub api_key_id: Option<String>,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: "https://us.cloud.langfuse.com/api/public/otel/v1/traces".to_owned(),
            public_key: String::new(),
            api_key_id: Some("langfuse".to_owned()),
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
            reasoning_effort: None,
            timeout_ms: 30_000,
            // One cleanup round is enough for modern instruct models; a second
            // pass doubles cloud latency for little gain on residual errors.
            passes: 1,
            min_chars: 40,
            instructions: String::new(),
            decision_model: None,
            decision_endpoint: None,
            decision_api_key_id: None,
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
        for (name, target) in &self.handoff {
            if !valid_handoff_name(name) {
                bail!("handoff target name must match [a-z0-9_-]{{1,32}}: {name}");
            }
            if target.command.is_empty()
                || target.command[0].is_empty()
                || !Path::new(&target.command[0]).is_absolute()
            {
                bail!("handoff.{name}.command must start with an absolute executable path");
            }
        }
        if self.postproc.enabled && self.postproc.model.trim().is_empty() {
            bail!("postproc.enabled = true requires postproc.model");
        }
        if !(1..=3).contains(&self.postproc.passes) {
            bail!(
                "postproc.passes must be between 1 and 3, got {}",
                self.postproc.passes
            );
        }
        if self.postproc.min_chars > 10_000 {
            bail!(
                "postproc.min_chars must be at most 10000, got {}",
                self.postproc.min_chars
            );
        }
        if !(1_000..=120_000).contains(&self.postproc.timeout_ms) {
            bail!(
                "postproc.timeout_ms must be between 1000 and 120000, got {}",
                self.postproc.timeout_ms
            );
        }
        if let Some(model) = &self.postproc.decision_model {
            if model.trim().is_empty() {
                bail!("postproc.decision_model must not be empty when set");
            }
            if let Some(endpoint) = &self.postproc.decision_endpoint {
                let endpoint = endpoint.trim();
                if endpoint.is_empty() {
                    bail!("postproc.decision_endpoint must not be empty when set");
                }
                if !endpoint.starts_with("http://") && !endpoint.starts_with("https://") {
                    bail!("postproc.decision_endpoint must be an http(s) URL");
                }
            }
        }
        if self.telemetry.enabled {
            if !self.telemetry.endpoint.starts_with("http://")
                && !self.telemetry.endpoint.starts_with("https://")
            {
                bail!("telemetry.endpoint must be an http(s) OTLP endpoint");
            }
            if self.telemetry.public_key.trim().is_empty() {
                bail!("telemetry.enabled = true requires telemetry.public_key");
            }
        }
        if let Some(endpoint) = &self.stt.endpoint {
            let endpoint = endpoint.trim();
            if endpoint.is_empty() {
                bail!("stt.endpoint must not be empty when set");
            }
            // Remote STT posts to `{endpoint}/audio/transcriptions`. The
            // value must be the API base (e.g. https://api.openai.com/v1),
            // not the full transcriptions path.
            let stripped = endpoint.trim_end_matches('/');
            if stripped.ends_with("/audio/transcriptions") {
                bail!(
                    "stt.endpoint must be the API base URL (e.g. https://api.openai.com/v1), not the full /audio/transcriptions path"
                );
            }
            if self.stt.model.trim().is_empty() {
                bail!("stt.endpoint requires a non-empty stt.model");
            }
        } else {
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
        assert_eq!(config.postproc.passes, 1);
        assert_eq!(config.postproc.min_chars, 40);
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
    fn validation_rejects_out_of_range_passes() {
        let config = Config {
            postproc: PostprocConfig {
                passes: 0,
                ..PostprocConfig::default()
            },
            ..Config::default()
        };
        let error = config.validate().expect_err("passes = 0 must fail");
        assert!(error
            .to_string()
            .contains("postproc.passes must be between 1 and 3"));
    }

    #[test]
    fn validation_rejects_huge_min_chars() {
        let config = Config {
            postproc: PostprocConfig {
                min_chars: 10_001,
                ..PostprocConfig::default()
            },
            ..Config::default()
        };
        let error = config.validate().expect_err("huge min_chars must fail");
        assert!(error
            .to_string()
            .contains("postproc.min_chars must be at most 10000"));
    }

    #[test]
    fn validation_rejects_zero_timeout_ms() {
        let config = Config {
            postproc: PostprocConfig {
                timeout_ms: 0,
                ..PostprocConfig::default()
            },
            ..Config::default()
        };
        let error = config.validate().expect_err("timeout_ms = 0 must fail");
        assert!(error
            .to_string()
            .contains("postproc.timeout_ms must be between 1000 and 120000"));
    }

    #[test]
    fn validation_rejects_oversized_timeout_ms() {
        let config = Config {
            postproc: PostprocConfig {
                timeout_ms: 120_001,
                ..PostprocConfig::default()
            },
            ..Config::default()
        };
        let error = config
            .validate()
            .expect_err("timeout_ms = 120001 must fail");
        assert!(error
            .to_string()
            .contains("postproc.timeout_ms must be between 1000 and 120000"));
    }

    #[test]
    fn validation_accepts_valid_decision_model() {
        let config = Config {
            postproc: PostprocConfig {
                decision_model: Some("typesafe/jev-1.13".to_string()),
                decision_endpoint: Some("https://openrouter.ai/api/alpha/decisions".to_string()),
                ..PostprocConfig::default()
            },
            ..Config::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validation_rejects_empty_decision_model() {
        let config = Config {
            postproc: PostprocConfig {
                decision_model: Some("   ".to_string()),
                ..PostprocConfig::default()
            },
            ..Config::default()
        };
        let error = config
            .validate()
            .expect_err("empty decision_model must fail");
        assert!(error
            .to_string()
            .contains("postproc.decision_model must not be empty"));
    }

    #[test]
    fn validation_accepts_timeout_ms_bounds() {
        for timeout_ms in [1_000, 120_000] {
            let config = Config {
                postproc: PostprocConfig {
                    timeout_ms,
                    ..PostprocConfig::default()
                },
                ..Config::default()
            };
            config.validate().expect("timeout_ms bound should validate");
        }
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

    #[test]
    fn stt_endpoint_rejects_transcriptions_path() {
        let config = Config {
            stt: SttConfig {
                endpoint: Some("https://api.openai.com/v1/audio/transcriptions".into()),
                model: "whisper-large-v3".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let err = config.validate().expect_err("full path must fail");
        let message = err.to_string();
        assert!(message.contains("API base"), "unexpected error: {message}");
        assert!(
            message.contains("/audio/transcriptions"),
            "error should name the offending path: {message}"
        );
        assert!(
            !message.contains("  "),
            "error should not contain a padded run of spaces: {message}"
        );
    }

    #[test]
    fn stt_endpoint_accepts_api_base() {
        let config = Config {
            stt: SttConfig {
                endpoint: Some("https://api.openai.com/v1".into()),
                model: "whisper-large-v3".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        config.validate().expect("base URL should validate");
    }

    #[test]
    fn stt_endpoint_rejects_empty() {
        let config = Config {
            stt: SttConfig {
                endpoint: Some("  ".into()),
                model: "whisper-large-v3".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }
    #[test]
    fn named_handoff_parses_and_clamps_timeout() {
        let config: Config = toml::from_str(
            "[handoff.pepper]\ncommand = [\"/bin/cat\", \"--number\"]\ntimeout_seconds = 500\n\
             [handoff.quiet_agent]\ncommand = [\"/bin/true\"]\ntimeout_seconds = 0\n\
             [handoff.default]\ncommand = [\"/bin/true\"]\n",
        )
        .unwrap();
        config.validate().unwrap();
        assert_eq!(config.handoff["pepper"].command[1], "--number");
        assert_eq!(config.handoff["pepper"].timeout_seconds, 120);
        assert_eq!(config.handoff["quiet_agent"].timeout_seconds, 1);
        assert_eq!(config.handoff["pepper"].timeout().as_secs(), 120);
        assert_eq!(config.handoff["quiet_agent"].timeout().as_secs(), 1);
        assert_eq!(config.handoff["default"].timeout().as_secs(), 15);
    }

    #[test]
    fn named_handoff_rejects_invalid_names_and_empty_argv() {
        for name in ["Pepper", "a.b", "123456789012345678901234567890123"] {
            let config: Config =
                toml::from_str(&format!("[handoff.\"{name}\"]\ncommand = [\"/bin/true\"]"))
                    .unwrap();
            assert!(config.validate().is_err(), "accepted invalid name {name}");
        }
        for command in ["[]", "[\"true\"]"] {
            let config: Config =
                toml::from_str(&format!("[handoff.pepper]\ncommand = {command}")).unwrap();
            assert!(config.validate().is_err(), "accepted command {command}");
        }
    }
}
