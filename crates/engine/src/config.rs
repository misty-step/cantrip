//! User configuration for the cantrip daemon.

pub use crate::vocabulary::VocabularyEntry;
use crate::{delivery::InjectionMode, models, paths};
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
    /// macOS global toggle shortcut; None uses the native client's default.
    /// Linux shortcuts remain owned by the compositor.
    pub hotkey: Option<String>,
    #[serde(deserialize_with = "crate::vocabulary::deserialize_entries")]
    pub vocabulary: Vec<VocabularyEntry>,
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
    /// What the HUD and status call the target, e.g. "Kaylee"; defaults to its name.
    #[serde(default)]
    pub label: Option<String>,
    /// Keeps this target's takes on this computer: the installed local model transcribes
    /// them, LLM cleanup never runs, and a missing local model refuses capture.
    #[serde(default)]
    pub local_only: bool,
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

/// The selected dictation transformation. This is the sole mode vocabulary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Raw,
    Clean,
    Elevate,
}

impl Mode {
    pub const ALL: [Self; 3] = [Self::Raw, Self::Clean, Self::Elevate];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Clean => "clean",
            Self::Elevate => "elevate",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Raw => "Raw",
            Self::Clean => "Clean",
            Self::Elevate => "Elevate",
        }
    }
}

impl std::str::FromStr for Mode {
    type Err = &'static str;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|mode| mode.as_str() == value)
            .ok_or("expected raw, clean, or elevate")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct ModeModelConfig {
    pub model: String,
    /// OpenRouter reasoning effort; omitted from requests when unset.
    #[serde(
        deserialize_with = "optional_nonempty_string",
        serialize_with = "empty_string_for_none"
    )]
    pub reasoning_effort: Option<String>,
}

impl Default for ModeModelConfig {
    fn default() -> Self {
        Self {
            model: "openai/gpt-6.1-sol".to_owned(),
            reasoning_effort: Some("low".to_owned()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(from = "PostprocInput")]
pub struct PostprocConfig {
    pub mode: Mode,
    pub clean: ModeModelConfig,
    pub elevate: ModeModelConfig,
    pub endpoint: String,
    #[serde(serialize_with = "empty_string_for_none")]
    pub api_key_id: Option<String>,
    pub timeout_ms: u64,
    pub passes: u8,
    /// Clean only: skip LLM cleanup below this character count; 0 never skips.
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

/// Read old flat fields only at the configuration boundary. Serialization has
/// no compatibility aliases, so saving a migrated config completes the cutover.
#[derive(Deserialize, Default)]
#[serde(default)]
struct PostprocInput {
    mode: Option<Mode>,
    enabled: Option<bool>,
    clean: Option<ModeModelInput>,
    elevate: Option<ModeModelInput>,
    model: Option<String>,
    reasoning_effort: Option<String>,
    endpoint: Option<String>,
    #[serde(deserialize_with = "present_optional_string")]
    api_key_id: Option<Option<String>>,
    timeout_ms: Option<u64>,
    passes: Option<u8>,
    min_chars: Option<usize>,
    instructions: Option<String>,
    decision_model: Option<String>,
    decision_endpoint: Option<String>,
    decision_api_key_id: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ModeModelInput {
    model: Option<String>,
    #[serde(deserialize_with = "present_optional_string")]
    reasoning_effort: Option<Option<String>>,
}

impl ModeModelInput {
    fn apply_to(self, config: &mut ModeModelConfig) {
        if let Some(model) = self.model {
            config.model = model;
        }
        if let Some(effort) = self.reasoning_effort {
            config.reasoning_effort = effort;
        }
    }
}

fn present_optional_string<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(deserializer)
        .map(|value| Some(value.filter(|s| !s.trim().is_empty())))
}

fn optional_nonempty_string<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer).map(|value| value.filter(|s| !s.trim().is_empty()))
}

fn empty_string_for_none<S: serde::Serializer>(
    value: &Option<String>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.serialize_str(value.as_deref().unwrap_or_default())
}

impl From<PostprocInput> for PostprocConfig {
    fn from(input: PostprocInput) -> Self {
        let mut config = Self::default();
        let legacy = input.mode.is_none()
            && (input.enabled.is_some()
                || input.model.is_some()
                || input.reasoning_effort.is_some());
        let legacy_model = input.model.is_some();
        config.mode = input.mode.unwrap_or_else(|| {
            if input.enabled.unwrap_or(false) {
                Mode::Clean
            } else {
                Mode::Raw
            }
        });
        if let Some(clean) = input.clean {
            clean.apply_to(&mut config.clean);
        } else if let Some(model) = input.model {
            config.clean = ModeModelConfig {
                model,
                reasoning_effort: input.reasoning_effort,
            };
        } else if legacy {
            // Enabling aliases in an old config never silently opts into a
            // newly supplied cloud model.
            config.clean.model.clear();
            config.clean.reasoning_effort = None;
        }
        if let Some(elevate) = input.elevate {
            elevate.apply_to(&mut config.elevate);
        }
        if let Some(endpoint) = input.endpoint {
            config.endpoint = endpoint;
        } else if legacy_model {
            config.endpoint = "http://localhost:11434/v1".to_owned();
        }
        if let Some(key) = input.api_key_id {
            config.api_key_id = key;
        } else if legacy || config.endpoint.trim_end_matches('/') != "https://openrouter.ai/api/v1"
        {
            config.api_key_id = None;
        }
        if let Some(timeout) = input.timeout_ms {
            config.timeout_ms = timeout;
        }
        if let Some(passes) = input.passes {
            config.passes = passes;
        }
        if let Some(min_chars) = input.min_chars {
            config.min_chars = min_chars;
        }
        if let Some(instructions) = input.instructions {
            config.instructions = instructions;
        }
        config.decision_model = input.decision_model;
        config.decision_endpoint = input.decision_endpoint;
        config.decision_api_key_id = input.decision_api_key_id;
        config
    }
}

impl PostprocConfig {
    /// Raw callers bypass post-processing before consulting a model.
    pub fn model_config(&self) -> &ModeModelConfig {
        self.model_for(self.mode)
    }

    /// `mode`'s own model and effort; Raw callers bypass post-processing first.
    pub fn model_for(&self, mode: Mode) -> &ModeModelConfig {
        match mode {
            Mode::Raw | Mode::Clean => &self.clean,
            Mode::Elevate => &self.elevate,
        }
    }
}

/// What a configured or snapshotted take runs, for clients to present: the
/// mode, each stage's model, and the hosts each stage sends the take to.
/// Fields an older engine omits read as unknown, never as staying local.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PipelineDisplay {
    pub mode: Mode,
    pub stt_model: String,
    pub stt_local: bool,
    /// The host a cloud recognizer receives the audio at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stt_host: Option<String>,
    pub llm_model: Option<String>,
    /// The hosts that receive the text while it is refined: the shared
    /// endpoint's, then an opted-in decision model's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub llm_hosts: Vec<String>,
    /// Vocabulary terms with aliases, whose spellings Clean and Elevate fix.
    #[serde(default)]
    pub spellings: usize,
}

/// The host an endpoint URL names, lowercased, without its scheme,
/// credentials, port or path.
fn endpoint_host(endpoint: &str) -> Option<String> {
    let endpoint = endpoint.trim();
    let rest = endpoint
        .split_once("://")
        .map_or(endpoint, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let address = authority
        .rsplit_once('@')
        .map_or(authority, |(_, address)| address);
    let host = match address.strip_prefix('[') {
        Some(bracketed) => bracketed.split_once(']').map(|(host, _)| host)?,
        None => address.split(':').next().unwrap_or_default(),
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

impl Default for Config {
    fn default() -> Self {
        Self {
            injection: if cfg!(target_os = "macos") {
                InjectionMode::Clipboard
            } else {
                InjectionMode::Auto
            },
            keep_warm: true,
            audio_source: None,
            hotkey: None,
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
            model: models::PARAKEET_V3_INT8.dir_name.to_owned(),
            endpoint: None,
            api_key_id: None,
        }
    }
}

impl Default for PostprocConfig {
    fn default() -> Self {
        Self {
            mode: Mode::Raw,
            clean: ModeModelConfig {
                model: "deepseek/deepseek-v4.1-flash-1017:nitro".to_owned(),
                reasoning_effort: Some("low".to_owned()),
            },
            elevate: ModeModelConfig::default(),
            endpoint: "https://openrouter.ai/api/v1".to_owned(),
            api_key_id: Some("openrouter".to_owned()),
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

    /// Preserve selection while preventing STT, cleanup, and decision provider calls.
    pub fn restrict_to_local(&mut self) {
        self.stt = SttConfig::default();
        self.postproc.clean.model.clear();
        self.postproc.elevate.model.clear();
    }

    /// What a take in `mode` runs with this configuration.
    pub fn pipeline_display(&self, mode: Mode) -> PipelineDisplay {
        let llm_model = (mode != Mode::Raw)
            .then(|| self.postproc.model_for(mode).model.trim())
            .filter(|model| !model.is_empty())
            .map(str::to_owned);
        let mut llm_hosts = Vec::new();
        if llm_model.is_some() {
            let decision = self
                .postproc
                .decision_model
                .as_deref()
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .map(|model| {
                    crate::typesafe::resolve_endpoint(
                        self.postproc.decision_endpoint.as_deref(),
                        model,
                    )
                });
            for host in std::iter::once(self.postproc.endpoint.as_str())
                .chain(decision.as_deref())
                .filter_map(endpoint_host)
            {
                if !llm_hosts.contains(&host) {
                    llm_hosts.push(host);
                }
            }
        }
        PipelineDisplay {
            mode,
            stt_model: self.stt.model.clone(),
            stt_local: self.stt.endpoint.is_none(),
            stt_host: self.stt.endpoint.as_deref().and_then(endpoint_host),
            llm_model,
            llm_hosts,
            spellings: self
                .vocabulary
                .iter()
                .filter(|entry| !entry.aliases().is_empty())
                .count(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        crate::vocabulary::validate(&self.vocabulary)?;
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
            if let Some(label) = &target.label {
                if label.trim().is_empty()
                    || label.chars().count() > 32
                    || label.chars().any(char::is_control)
                {
                    bail!("handoff.{name}.label must be 1-32 printable characters");
                }
            }
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
        assert_eq!(config.postproc.mode, Mode::Clean);
        assert_eq!(config.postproc.clean.model, "llama3");
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
    fn partial_mode_tables_keep_their_own_model_defaults() {
        let defaults = PostprocConfig::default();
        assert_eq!(
            defaults.clean.model,
            "deepseek/deepseek-v4.1-flash-1017:nitro"
        );
        assert_eq!(defaults.clean.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(defaults.elevate.model, "openai/gpt-6.1-sol");
        assert_eq!(defaults.elevate.reasoning_effort.as_deref(), Some("low"));
        let config: Config = toml::from_str(
            r#"[postproc]
mode = "clean"
[postproc.clean]
reasoning_effort = "medium"
[postproc.elevate]
model = "explicit-elevate"
"#,
        )
        .unwrap();
        assert_eq!(config.postproc.clean.model, defaults.clean.model);
        assert_eq!(
            config.postproc.clean.reasoning_effort.as_deref(),
            Some("medium")
        );
        assert_eq!(config.postproc.elevate.model, "explicit-elevate");
        assert_eq!(
            config.postproc.elevate.reasoning_effort,
            defaults.elevate.reasoning_effort
        );
    }

    #[test]
    fn clean_without_a_cleanup_model_is_valid_for_local_aliases() {
        let config = Config {
            postproc: PostprocConfig {
                mode: Mode::Clean,
                clean: ModeModelConfig {
                    model: String::new(),
                    reasoning_effort: None,
                },
                ..PostprocConfig::default()
            },
            ..Config::default()
        };
        config
            .validate()
            .expect("Clean can apply aliases without an LLM");
    }

    #[test]
    fn legacy_modes_preserve_opt_in_without_silently_enabling_a_cloud_model() {
        for (enabled, mode) in [(true, Mode::Clean), (false, Mode::Raw)] {
            let config: Config =
                toml::from_str(&format!("[postproc]\nenabled = {enabled}\n")).unwrap();
            assert_eq!(config.postproc.mode, mode);
            assert!(config.postproc.clean.model.is_empty());
            assert_eq!(config.postproc.api_key_id, None);
            let saved = toml::to_string(&config).unwrap();
            assert_eq!(toml::from_str::<Config>(&saved).unwrap(), config);
            let postproc = serde_json::to_value(&config.postproc).unwrap();
            assert!(postproc.get("enabled").is_none());
            assert!(postproc.get("model").is_none());
            assert!(postproc.get("reasoning_effort").is_none());
        }
    }

    #[test]
    fn explicit_mode_and_nested_model_win_and_modes_keep_independent_models() {
        let config: Config = toml::from_str(
            r#"[postproc]
mode = "elevate"
enabled = false
model = "legacy"
reasoning_effort = "high"
endpoint = "https://openrouter.ai/api/v1"
[postproc.clean]
model = "clean-model"
reasoning_effort = ""
[postproc.elevate]
model = "elevate-model"
reasoning_effort = "medium"
"#,
        )
        .unwrap();
        assert_eq!(config.postproc.mode, Mode::Elevate);
        assert_eq!(config.postproc.clean.model, "clean-model");
        assert_eq!(config.postproc.clean.reasoning_effort, None);
        assert_eq!(config.postproc.model_config().model, "elevate-model");
        assert_eq!(
            config.postproc.elevate.reasoning_effort.as_deref(),
            Some("medium")
        );
        let saved = toml::to_string(&config).unwrap();
        assert_eq!(toml::from_str::<Config>(&saved).unwrap(), config);
        let value = serde_json::to_value(&config.postproc).unwrap();
        assert_eq!(value["mode"], "elevate");
        for old in ["enabled", "model", "reasoning_effort"] {
            assert!(value.get(old).is_none(), "obsolete owner: {old}");
        }
    }

    #[test]
    fn legacy_model_and_effort_migrate_without_changing_its_implicit_local_route() {
        let config: Config = toml::from_str(
            "[postproc]\nenabled = true\nmodel = \"old-local\"\nreasoning_effort = \"high\"\n",
        )
        .unwrap();
        assert_eq!(config.postproc.clean.model, "old-local");
        assert_eq!(
            config.postproc.clean.reasoning_effort.as_deref(),
            Some("high")
        );
        assert_eq!(config.postproc.endpoint, "http://localhost:11434/v1");
        assert_eq!(config.postproc.elevate, ModeModelConfig::default());
        let saved = toml::to_string(&config).unwrap();
        assert_eq!(toml::from_str::<Config>(&saved).unwrap(), config);
    }

    #[test]
    fn only_the_default_provider_gets_an_implicit_keyring_id() {
        for (endpoint, key) in [
            ("https://openrouter.ai/api/v1", Some("openrouter")),
            ("http://localhost:11434/v1", None),
            ("https://other.example/v1", None),
        ] {
            let config: Config = toml::from_str(&format!(
                "[postproc]\nmode = \"clean\"\nendpoint = \"{endpoint}\"\n",
            ))
            .unwrap();
            assert_eq!(config.postproc.api_key_id.as_deref(), key);
            let saved = toml::to_string(&config).unwrap();
            assert_eq!(toml::from_str::<Config>(&saved).unwrap(), config);
        }
        let keyless: Config = toml::from_str("[postproc]\napi_key_id = \"\"\n").unwrap();
        assert_eq!(keyless.postproc.api_key_id, None);
    }

    #[test]
    fn a_pipeline_names_each_modes_model_and_every_host_that_receives_the_take() {
        let mut config: Config = toml::from_str(
            r#"vocabulary = ["PipeWire", { term = "Cantrip", aliases = ["Gantrip"] }, { term = "HUD", aliases = [] }]"#,
        )
        .unwrap();
        let raw = config.pipeline_display(Mode::Raw);
        assert_eq!(raw.mode, Mode::Raw);
        assert!(raw.stt_local && raw.stt_host.is_none());
        assert_eq!((raw.llm_model, raw.llm_hosts), (None, Vec::new()));
        // Only entries that own an alias have a spelling to fix.
        assert_eq!(raw.spellings, 1);
        let clean = config.pipeline_display(Mode::Clean);
        assert_eq!(
            clean.llm_model.as_deref(),
            Some("deepseek/deepseek-v4.1-flash-1017:nitro")
        );
        assert_eq!(clean.llm_hosts, ["openrouter.ai"]);
        // The selected mode is only a default; each mode names its own model.
        config.postproc.elevate.model = "provider/articulate".into();
        config.stt.endpoint = Some("https://token@API.OpenAI.com:443/v1".into());
        config.postproc.decision_model = Some("jev-latest".into());
        let elevate = config.pipeline_display(Mode::Elevate);
        assert_eq!(elevate.mode, Mode::Elevate);
        assert_eq!(elevate.llm_model.as_deref(), Some("provider/articulate"));
        assert!(!elevate.stt_local);
        assert_eq!(elevate.stt_host.as_deref(), Some("api.openai.com"));
        assert_eq!(elevate.llm_hosts, ["openrouter.ai", "api.typesafe.ai"]);
        // A decision endpoint on the same host is named once.
        config.postproc.decision_model = Some("typesafe/jev-1.13".into());
        assert_eq!(
            config.pipeline_display(Mode::Clean).llm_hosts,
            ["openrouter.ai"]
        );
        config.postproc.endpoint = "http://[::1]:11434/v1".into();
        config.postproc.decision_model = None;
        assert_eq!(config.pipeline_display(Mode::Clean).llm_hosts, ["::1"]);
        config.restrict_to_local();
        let local = config.pipeline_display(Mode::Elevate);
        assert!(local.stt_local && local.stt_host.is_none());
        assert_eq!((local.llm_model, local.llm_hosts), (None, Vec::new()));
    }

    #[test]
    fn an_older_engines_pipeline_reads_its_destinations_as_unknown() {
        let older: PipelineDisplay = serde_json::from_str(
            r#"{"mode":"clean","stt_model":"gpt-4o-mini-transcribe","stt_local":false,"llm_model":"openai/gpt-6.1-sol"}"#,
        )
        .unwrap();
        assert_eq!(older.stt_host, None);
        assert!(older.llm_hosts.is_empty());
        assert_eq!(older.spellings, 0);
    }

    #[test]
    fn vocabulary_accepts_plain_and_mixed_entries_and_round_trips() {
        for text in [
            r#"vocabulary = ["PipeWire", "Cantrip"]"#,
            r#"vocabulary = ["PipeWire", { term = "Cantrip", aliases = ["Gantrip", "can trip"] }]"#,
            r#"vocabulary = ["PipeWire", { term = "Cantrip", aliases = ["cantrip"] }]"#,
        ] {
            let config: Config = toml::from_str(text).unwrap();
            config.validate().unwrap();
            assert_eq!(
                config
                    .vocabulary
                    .iter()
                    .map(VocabularyEntry::term)
                    .collect::<Vec<_>>(),
                ["PipeWire", "Cantrip"]
            );
            let saved = toml::to_string(&config).unwrap();
            assert_eq!(toml::from_str::<Config>(&saved).unwrap(), config);
        }
    }

    #[test]
    fn vocabulary_rejects_empty_conflicting_and_duplicate_aliases() {
        for (entries, expected) in [
            (r#"[{ term = "Cantrip", aliases = ["  "] }]"#, "empty alias"),
            (
                r#"[{ term = "  ", aliases = ["Gantrip"] }]"#,
                "empty canonical term",
            ),
            (
                r#"["PipeWire", { term = "Cantrip", aliases = ["PIPEWIRE"] }]"#,
                "a canonical term",
            ),
            (
                r#"[{ term = "Cantrip", aliases = ["Gantrip"] }, { term = "Other", aliases = ["GANTRIP"] }]"#,
                "duplicates an alias",
            ),
        ] {
            let config: Config = toml::from_str(&format!("vocabulary = {entries}")).unwrap();
            let error = config.validate().unwrap_err().to_string();
            assert!(error.contains(expected), "{error}");
            assert!(!error.contains("Gantrip") && !error.contains("PIPEWIRE"));
        }
    }

    #[test]
    fn vocabulary_shape_errors_name_the_entry_and_shape_without_echoing_values() {
        for (text, index) in [
            (r#"vocabulary = ["PRIVATE-CANONICAL", 7]"#, 2),
            (
                r#"vocabulary = ["PRIVATE-CANONICAL", { term = "PRIVATE-TERM", aliases = "PRIVATE-ALIAS" }]"#,
                2,
            ),
            (
                r#"vocabulary = [{ term = 7, aliases = ["PRIVATE-ALIAS"] }]"#,
                1,
            ),
            (
                r#"vocabulary = ["PRIVATE-CANONICAL", { term = "PRIVATE-TERM", alias = ["PRIVATE-ALIAS"] }]"#,
                2,
            ),
        ] {
            let error = toml::from_str::<Config>(text).unwrap_err();
            let message = error.message();
            assert!(
                message.contains(&format!("vocabulary entry {index}")),
                "{message}"
            );
            assert!(
                message.contains("string or { term = string, aliases = [string, ...] }"),
                "{message}"
            );
            assert!(!message.contains("PRIVATE-"), "{message}");
            assert!(!message.contains("untagged enum"), "{message}");
        }
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
             local_only = true\n\
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
        assert!(config.handoff["pepper"].local_only);
        assert!(!config.handoff["default"].local_only);
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

#[cfg(test)]
mod handoff_label_tests {
    use super::Config;

    #[test]
    fn handoff_label_is_optional_and_must_be_short_printable_text() {
        let config: Config = toml::from_str(
            "[handoff.pepper]\ncommand = [\"/bin/true\"]\nlabel = \"Kaylee\"\n\
             [handoff.other]\ncommand = [\"/bin/true\"]\n",
        )
        .unwrap();
        config.validate().unwrap();
        assert_eq!(config.handoff["pepper"].label.as_deref(), Some("Kaylee"));
        assert_eq!(config.handoff["other"].label, None);
        for label in [
            "\"\"",
            "\"   \"",
            "\"a\\nb\"",
            "\"123456789012345678901234567890123\"",
        ] {
            let config: Config = toml::from_str(&format!(
                "[handoff.pepper]\ncommand = [\"/bin/true\"]\nlabel = {label}"
            ))
            .unwrap();
            assert!(config.validate().is_err(), "accepted label {label}");
        }
    }
}
