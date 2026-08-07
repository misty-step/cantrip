//! One WAV file through the full dictation backend: STT then optional
//! post-processing. Shared by the daemon worker and `cantrip transcribe`.

use crate::config::{PostprocConfig, SttConfig};
use crate::models;
use crate::postproc;
use crate::stt::{self, Transcriber};
use anyhow::{Context, Result};
use std::path::Path;
use std::time::{Duration, Instant};

/// Result of the optional post-processing pass on a transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostprocStatus {
    /// Not enabled, or no transcript to clean.
    Off,
    /// LLM cleanup succeeded, taking `ms`.
    Applied { ms: u128 },
    /// Cleanup failed; the raw transcript is preserved.
    Failed,
    /// Enabled, but the transcript was under `min_chars`.
    SkippedShort { chars: usize },
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
}

impl Stage {
    pub fn as_str(&self) -> String {
        match self {
            Self::Transcribing { chunk, total } if *total > 1 => {
                format!("transcribing {chunk}/{total}")
            }
            Self::Transcribing { .. } => "transcribing".to_owned(),
            Self::CleaningUp => "cleaning".to_owned(),
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
    stt_cfg: &SttConfig,
    vocabulary: &[String],
    postproc_cfg: &PostprocConfig,
    on_stage: impl FnMut(Stage),
) -> Outcome {
    let mut on_stage = on_stage;
    let stt_started = Instant::now();
    on_stage(Stage::Transcribing { chunk: 1, total: 1 });
    let transcription = transcribe(cache, wav, stt_cfg, vocabulary, &mut on_stage);
    let stt_elapsed = stt_started.elapsed();

    let (text, postproc, partial, keep_wav) = match transcription {
        Ok(LocalOk { text, partial }) if !text.trim().is_empty() && postproc_cfg.enabled => {
            let chars = text.chars().count();
            if !should_run_postproc(postproc_cfg, chars) {
                tracing::info!(
                    "[Postproc] skipped_short chars={} min_chars={}",
                    chars,
                    postproc_cfg.min_chars
                );
                (
                    Ok(text),
                    PostprocStatus::SkippedShort { chars },
                    partial,
                    false,
                )
            } else {
                on_stage(Stage::CleaningUp);
                let postproc_started = Instant::now();
                match resolve_api_key(postproc_cfg.api_key_id.as_deref()).and_then(|key| {
                    postproc::refine(&text, postproc_cfg, vocabulary, key.as_deref())
                }) {
                    Ok(refined) => (
                        Ok(refined),
                        PostprocStatus::Applied {
                            ms: postproc_started.elapsed().as_millis(),
                        },
                        partial,
                        false,
                    ),
                    Err(error) => {
                        tracing::warn!("[Postproc] cleanup failed error={error:#}");
                        (Ok(text), PostprocStatus::Failed, partial, false)
                    }
                }
            }
        }
        Ok(LocalOk { text, partial }) => (Ok(text), PostprocStatus::Off, partial, false),
        Err(error) => (Err(error), PostprocStatus::Off, false, true),
    };

    Outcome {
        text,
        stt_elapsed,
        postproc,
        partial,
        keep_wav,
    }
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
    fn stage_as_str_matches_daemon_hud_contract() {
        assert_eq!(
            Stage::Transcribing { chunk: 1, total: 1 }.as_str(),
            "transcribing"
        );
        assert_eq!(
            Stage::Transcribing { chunk: 2, total: 5 }.as_str(),
            "transcribing 2/5"
        );
        assert_eq!(Stage::CleaningUp.as_str(), "cleaning");
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
