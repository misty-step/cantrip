//! One WAV file through the full dictation backend: STT then optional
//! post-processing. Shared by the daemon worker and `cantrip transcribe`.

use crate::archive;
use crate::config::{Config, PostprocConfig, SttConfig};
use crate::models;
use crate::postproc;
use crate::stt::{self, Transcriber};
use anyhow::{Context, Result};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Result of the optional post-processing pass on a transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostprocStatus {
    /// Not enabled, or no transcript to clean.
    Off,
    /// LLM cleanup succeeded, taking `ms`.
    Applied { ms: u128 },
    /// Cleanup failed after `ms`; the raw transcript is preserved.
    Failed { ms: u128 },
    /// Enabled, but the transcript was under `min_chars`.
    SkippedShort { chars: usize },
}

/// Entry point that produced a transcript history record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Dictation,
    Recover,
    Transcribe,
}

impl Source {
    fn as_str(self) -> &'static str {
        match self {
            Self::Dictation => "dictation",
            Self::Recover => "recover",
            Self::Transcribe => "transcribe",
        }
    }
}

/// Result of saving the owner-private transcript history record.
#[derive(Debug)]
pub enum ArchiveStatus {
    Saved(PathBuf),
    Failed(String),
    /// STT failed, so there was no transcript to archive.
    NotApplicable,
}

/// Which sub-stage of a job is running right now, observable live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    /// Local multi-chunk STT progress (1-based).
    Transcribing {
        chunk: u32,
        total: u32,
    },
    CleaningUp,
    /// A stage added by a newer daemon or a malformed stage from the wire.
    Unknown(String),
}

impl Stage {
    /// Return validated measured chunk progress. Single-chunk transcription is
    /// intentionally indeterminate on the HUD.
    pub fn measured_progress(&self) -> Option<(u32, u32)> {
        match self {
            Self::Transcribing { chunk, total }
                if *total > 1 && *chunk >= 1 && *chunk <= *total =>
            {
                Some((*chunk, *total))
            }
            Self::Transcribing { .. } | Self::CleaningUp | Self::Unknown(_) => None,
        }
    }
}

impl fmt::Display for Stage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transcribing { chunk, total } if *total > 1 => {
                write!(formatter, "transcribing {chunk}/{total}")
            }
            Self::Transcribing { .. } => formatter.write_str("transcribing"),
            Self::CleaningUp => formatter.write_str("cleaning"),
            Self::Unknown(stage) => formatter.write_str(stage),
        }
    }
}

impl From<String> for Stage {
    fn from(stage: String) -> Self {
        match stage.as_str() {
            "transcribing" => Self::Transcribing { chunk: 1, total: 1 },
            "cleaning" => Self::CleaningUp,
            _ => {
                if let Some((chunk, total)) = stage
                    .strip_prefix("transcribing ")
                    .and_then(|progress| progress.split_once('/'))
                    .and_then(|(chunk, total)| {
                        Some((chunk.parse::<u32>().ok()?, total.parse::<u32>().ok()?))
                    })
                    .filter(|(chunk, total)| *chunk >= 1 && *chunk <= *total)
                {
                    Self::Transcribing { chunk, total }
                } else {
                    Self::Unknown(stage)
                }
            }
        }
    }
}

/// Outcome of running one WAV through the backend.
pub struct Outcome {
    /// Final text: post-processed, raw on cleanup failure, or the STT error.
    pub text: Result<String, String>,
    /// Wall time of the STT stage only.
    pub stt_elapsed: Duration,
    pub postproc: PostprocStatus,
    /// True when STT returned text from earlier chunks after a later failure.
    pub partial: bool,
    /// Keep the WAV on disk for operator recovery (full STT failure).
    pub keep_wav: bool,
    pub archive: ArchiveStatus,
}

/// Cache of the loaded local transcriber, keyed by model name. Keep one
/// mutable instance across dictations to avoid reloading the model.
pub type TranscriberCache = Option<(String, Transcriber)>;

/// Transcribe `wav` with the configured STT backend, then apply the
/// configured post-processing pass. STT is local (Parakeet) unless
/// `stt.endpoint` is set, which selects an OpenAI-compatible cloud.
///
/// A post-processing failure never drops the dictation: the raw text is
/// returned with `PostprocStatus::Failed`. Short transcripts under
/// `postproc.min_chars` skip cleanup and return the raw text.
/// Whether the cleanup pass should run for a transcript of `chars` length.
pub fn should_run_postproc(cfg: &PostprocConfig, chars: usize) -> bool {
    if !cfg.enabled {
        return false;
    }
    // min_chars == 0 means never skip for length.
    if cfg.min_chars > 0 && chars < cfg.min_chars {
        return false;
    }
    true
}

pub fn run(
    cache: &mut TranscriberCache,
    wav: &Path,
    config: &Config,
    source: Source,
    on_stage: impl FnMut(Stage),
) -> Outcome {
    let stt_cfg = &config.stt;
    let vocabulary = &config.vocabulary;
    let postproc_cfg = &config.postproc;
    let pipeline_started = Instant::now();
    let audio_duration_ms = match stt::wav_duration_ms(wav) {
        Ok(duration) => Some(duration),
        Err(error) => {
            tracing::warn!("[STT] WAV duration unavailable error={error:#}");
            None
        }
    };
    let mut on_stage = on_stage;
    let stt_started = Instant::now();
    on_stage(Stage::Transcribing { chunk: 1, total: 1 });
    let transcription = transcribe(cache, wav, stt_cfg, vocabulary, &mut on_stage);
    let stt_elapsed = stt_started.elapsed();

    let LocalOk { text: raw, partial } = match transcription {
        Ok(transcription) => transcription,
        Err(error) => {
            return Outcome {
                text: Err(error),
                stt_elapsed,
                postproc: PostprocStatus::Off,
                partial: false,
                keep_wav: true,
                archive: ArchiveStatus::NotApplicable,
            };
        }
    };

    let (postproc, processed, postproc_usage) = if !raw.trim().is_empty() && postproc_cfg.enabled {
        let chars = raw.chars().count();
        if !should_run_postproc(postproc_cfg, chars) {
            tracing::info!(
                "[Postproc] skipped_short chars={} min_chars={}",
                chars,
                postproc_cfg.min_chars
            );
            (PostprocStatus::SkippedShort { chars }, None, None)
        } else {
            on_stage(Stage::CleaningUp);
            let postproc_started = Instant::now();
            match resolve_api_key(postproc_cfg.api_key_id.as_deref())
                .and_then(|key| postproc::refine(&raw, postproc_cfg, vocabulary, key.as_deref()))
            {
                Ok(refined) => (
                    PostprocStatus::Applied {
                        ms: postproc_started.elapsed().as_millis(),
                    },
                    Some(refined.text),
                    refined.usage,
                ),
                Err(error) => {
                    tracing::warn!("[Postproc] cleanup failed error={error:#}");
                    (
                        PostprocStatus::Failed {
                            ms: postproc_started.elapsed().as_millis(),
                        },
                        None,
                        None,
                    )
                }
            }
        }
    } else {
        (PostprocStatus::Off, None, None)
    };

    let attempted_postproc = matches!(
        &postproc,
        PostprocStatus::Applied { .. } | PostprocStatus::Failed { .. }
    );
    let postproc_elapsed_ms = match &postproc {
        PostprocStatus::Applied { ms } | PostprocStatus::Failed { ms } => Some(duration_ms(*ms)),
        PostprocStatus::Off | PostprocStatus::SkippedShort { .. } => None,
    };
    let archive = match archive::save(archive::Entry {
        source: source.as_str(),
        raw_transcript: &raw,
        postprocessed_transcript: processed.as_deref(),
        audio_duration_ms,
        pipeline_elapsed_ms: duration_ms(pipeline_started.elapsed().as_millis()),
        stt_model: &stt_cfg.model,
        stt_remote: stt_cfg.endpoint.is_some(),
        stt_elapsed_ms: duration_ms(stt_elapsed.as_millis()),
        stt_api_cost_usd: stt_cfg.endpoint.is_none().then_some(0.0),
        partial,
        postproc_status: match &postproc {
            PostprocStatus::Off => "off",
            PostprocStatus::Applied { .. } => "applied",
            PostprocStatus::Failed { .. } => "failed",
            PostprocStatus::SkippedShort { .. } => "skipped_short",
        },
        postproc_model: attempted_postproc.then_some(postproc_cfg.model.as_str()),
        postproc_elapsed_ms,
        postproc_passes: attempted_postproc.then_some(postproc_cfg.passes.max(1)),
        postproc_prompt_version: attempted_postproc.then_some(postproc::PROMPT_VERSION),
        postproc_instructions: attempted_postproc
            .then_some(postproc_cfg.instructions.as_str())
            .filter(|instructions| !instructions.is_empty()),
        postproc_prompt_tokens: postproc_usage.as_ref().map(|usage| usage.prompt_tokens),
        postproc_completion_tokens: postproc_usage.as_ref().map(|usage| usage.completion_tokens),
        postproc_total_tokens: postproc_usage.as_ref().map(|usage| usage.total_tokens),
        postproc_reasoning_tokens: postproc_usage.as_ref().map(|usage| usage.reasoning_tokens),
        postproc_cached_tokens: postproc_usage.as_ref().map(|usage| usage.cached_tokens),
        postproc_reported_cost_usd: postproc_usage
            .as_ref()
            .and_then(|usage| usage.reported_cost_usd),
        postproc_usage_requests: postproc_usage.as_ref().map(|usage| usage.requests),
        postproc_usage_responses: postproc_usage
            .as_ref()
            .map(|usage| usage.responses_with_usage),
    }) {
        Ok(path) => ArchiveStatus::Saved(path),
        Err(error) => ArchiveStatus::Failed(format!("{error:#}")),
    };

    Outcome {
        text: Ok(processed.unwrap_or(raw)),
        stt_elapsed,
        postproc,
        partial,
        keep_wav: false,
        archive,
    }
}

fn duration_ms(ms: u128) -> u64 {
    u64::try_from(ms).unwrap_or(u64::MAX)
}

struct LocalOk {
    text: String,
    partial: bool,
}

fn transcribe(
    cache: &mut TranscriberCache,
    wav: &Path,
    stt_cfg: &SttConfig,
    vocabulary: &[String],
    on_stage: &mut impl FnMut(Stage),
) -> Result<LocalOk, String> {
    if let Some(endpoint) = &stt_cfg.endpoint {
        let key = resolve_api_key(stt_cfg.api_key_id.as_deref()).map_err(|e| format!("{e:#}"))?;
        let text =
            stt::transcribe_remote(wav, endpoint, &stt_cfg.model, vocabulary, key.as_deref())
                .map_err(|e| format!("{e:#}"))?;
        return Ok(LocalOk {
            text,
            partial: false,
        });
    }

    let reload = cache
        .as_ref()
        .is_none_or(|(name, _)| name != &stt_cfg.model);
    if reload {
        *cache = Some(load_transcriber(&stt_cfg.model).map_err(|e| format!("{e:#}"))?);
    }
    let transcriber = cache
        .as_mut()
        .map(|(_, transcriber)| transcriber)
        .ok_or_else(|| "transcription backend has no model".to_owned())?;
    let outcome = transcriber
        .transcribe_wav(wav, |progress| {
            on_stage(Stage::Transcribing {
                chunk: progress.index,
                total: progress.total,
            });
        })
        .map_err(|e| format!("{e:#}"))?;
    match outcome {
        stt::LocalTranscript::Complete(text) => Ok(LocalOk {
            text,
            partial: false,
        }),
        stt::LocalTranscript::Partial { text, .. } => Ok(LocalOk {
            text,
            partial: true,
        }),
    }
}

/// Load a local model by registry name. The transcriber is cached at the
/// call site; model files must already be installed.
pub fn load_transcriber(model: &str) -> Result<(String, Transcriber)> {
    let spec = models::require(model)?;
    let model_dir =
        models::installed(spec)?.context("model not installed — run: cantrip models pull")?;
    let transcriber = Transcriber::load(&model_dir)
        .with_context(|| format!("loading transcription model '{model}'"))?;
    Ok((model.to_owned(), transcriber))
}

fn resolve_api_key(id: Option<&str>) -> Result<Option<String>> {
    id.map(|id| crate::keys::get(id).with_context(|| format!("api key '{id}' unavailable")))
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measured_progress_requires_valid_multi_chunk_bounds() {
        assert_eq!(
            Stage::Transcribing { chunk: 1, total: 4 }.measured_progress(),
            Some((1, 4))
        );
        assert_eq!(
            Stage::Transcribing { chunk: 1, total: 1 }.measured_progress(),
            None
        );
        assert_eq!(
            Stage::Transcribing { chunk: 0, total: 3 }.measured_progress(),
            None
        );
        assert_eq!(
            Stage::Transcribing { chunk: 4, total: 3 }.measured_progress(),
            None
        );
        assert_eq!(Stage::CleaningUp.measured_progress(), None);
        assert_eq!(
            Stage::Unknown("future".to_owned()).measured_progress(),
            None
        );
    }

    #[test]
    fn should_run_postproc_respects_enabled_and_min_chars() {
        let off = PostprocConfig {
            enabled: false,
            min_chars: 40,
            ..Default::default()
        };
        assert!(!should_run_postproc(&off, 100));

        let on = PostprocConfig {
            enabled: true,
            min_chars: 40,
            ..Default::default()
        };
        assert!(!should_run_postproc(&on, 12));
        assert!(should_run_postproc(&on, 40));
        assert!(should_run_postproc(&on, 41));

        let no_floor = PostprocConfig {
            enabled: true,
            min_chars: 0,
            ..Default::default()
        };
        assert!(should_run_postproc(&no_floor, 1));
    }
}
