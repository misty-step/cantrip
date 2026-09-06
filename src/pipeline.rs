//! One WAV file through the full dictation backend: STT then optional
//! post-processing. Shared by the daemon worker and `cantrip transcribe`.

use crate::archive;
use crate::config::{PostprocConfig, SttConfig};
use crate::models;
use crate::postproc::{self, RefinementUsage};
use crate::stt::{self, Transcriber};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
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
    Replay,
}

impl Source {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Dictation => "dictation",
            Self::Recover => "recover",
            Self::Transcribe => "transcribe",
            Self::Replay => "replay",
        }
    }
}

/// Stable take identity and cooperative cancellation, snapshotted by callers.
pub struct RunContext<'a> {
    pub source: Source,
    pub take_id: Option<&'a str>,
    pub cancel: Option<&'a AtomicBool>,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    /// Capture is stopping or its WAV is being finalized and retained.
    FinalizingAudio,
    /// Completed backend calls, not the chunk currently being attempted.
    Transcribing {
        completed: u32,
        total: u32,
    },
    CleaningUp,
    Delivering,
    Cancelling,
    RemovingRecording,
    /// A stage added by a newer daemon or a malformed stage from the wire.
    Unknown(String),
}

impl Stage {
    /// Return validated measured chunk progress. Single-chunk transcription is
    /// intentionally indeterminate on the HUD.
    pub fn measured_progress(&self) -> Option<(u32, u32)> {
        match self {
            Self::Transcribing { completed, total } if *total > 1 && *completed <= *total => {
                Some((*completed, *total))
            }
            _ => None,
        }
    }
}

impl fmt::Display for Stage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FinalizingAudio => formatter.write_str("finalizing audio"),
            Self::Transcribing { completed, total } if *total > 1 => {
                write!(formatter, "transcribing {completed}/{total}")
            }
            Self::Transcribing { .. } => formatter.write_str("transcribing"),
            Self::CleaningUp => formatter.write_str("cleaning"),
            Self::Delivering => formatter.write_str("delivering"),
            Self::Cancelling => formatter.write_str("cancelling"),
            Self::RemovingRecording => formatter.write_str("removing recording"),
            Self::Unknown(stage) => formatter.write_str(stage),
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
    /// Token usage reported by the cleanup provider, when it ran and
    /// reported usage.
    pub postproc_usage: Option<RefinementUsage>,
    /// True when STT returned text from earlier chunks after a later failure.
    pub partial: bool,
    /// Cancellation was requested before this job settled; never auto-deliver.
    pub cancelled: bool,
    /// Preserve failed, partial, cancelled, or meaningful empty audio.
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
    stt_cfg: &SttConfig,
    vocabulary: &[String],
    postproc_cfg: &PostprocConfig,
    context: RunContext<'_>,
    on_stage: impl FnMut(Stage),
) -> Outcome {
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
    let transcription = transcribe(
        cache,
        wav,
        stt_cfg,
        vocabulary,
        context.cancel,
        &mut on_stage,
    );
    let stt_elapsed = stt_started.elapsed();

    let (raw, partial, mut cancelled) = match transcription {
        Ok(stt::Transcript::Complete(text)) => (text, false, stt::is_cancelled(context.cancel)),
        Ok(stt::Transcript::Partial { text, .. }) => {
            (text, true, stt::is_cancelled(context.cancel))
        }
        Ok(stt::Transcript::Cancelled {
            text,
            completed,
            total,
        }) => (text, completed < total, true),
        Err(error) => {
            return Outcome {
                text: Err(error),
                stt_elapsed,
                postproc: PostprocStatus::Off,
                postproc_usage: None,
                keep_wav: true,
                partial: false,
                cancelled: stt::is_cancelled(context.cancel),
                archive: ArchiveStatus::NotApplicable,
            };
        }
    };

    let (postproc, processed, postproc_usage) = cleanup(
        &raw,
        postproc_cfg,
        vocabulary,
        context.cancel,
        &mut on_stage,
    );
    cancelled |= stt::is_cancelled(context.cancel);
    if cancelled {
        on_stage(Stage::Cancelling);
    }

    let attempted_postproc = matches!(
        &postproc,
        PostprocStatus::Applied { .. } | PostprocStatus::Failed { .. }
    );
    let postproc_elapsed_ms = match &postproc {
        PostprocStatus::Applied { ms } | PostprocStatus::Failed { ms } => Some(duration_ms(*ms)),
        PostprocStatus::Off | PostprocStatus::SkippedShort { .. } => None,
    };
    let archive = match archive::save(archive::Entry {
        take_id: context.take_id,
        source: context.source.as_str(),
        raw_transcript: &raw,
        postprocessed_transcript: processed.as_deref(),
        audio_duration_ms,
        pipeline_elapsed_ms: duration_ms(pipeline_started.elapsed().as_millis()),
        stt_model: &stt_cfg.model,
        stt_remote: stt_cfg.endpoint.is_some(),
        stt_elapsed_ms: duration_ms(stt_elapsed.as_millis()),
        stt_api_cost_usd: stt_cfg.endpoint.is_none().then_some(0.0),
        partial,
        cancelled,
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
    cancelled |= stt::is_cancelled(context.cancel);
    let text = processed.unwrap_or(raw);
    let keep_wav = retain_audio(
        partial,
        cancelled,
        text.trim().is_empty(),
        audio_duration_ms,
    );

    Outcome {
        text: Ok(text),
        stt_elapsed,
        postproc,
        postproc_usage,
        partial,
        cancelled,
        keep_wav,
        archive,
    }
}

fn duration_ms(ms: u128) -> u64 {
    u64::try_from(ms).unwrap_or(u64::MAX)
}

fn retain_audio(partial: bool, cancelled: bool, empty: bool, duration_ms: Option<u64>) -> bool {
    partial || cancelled || (empty && duration_ms.is_none_or(|duration| duration >= 3_000))
}

fn cleanup(
    raw: &str,
    cfg: &PostprocConfig,
    vocabulary: &[String],
    cancel: Option<&AtomicBool>,
    on_stage: &mut impl FnMut(Stage),
) -> (PostprocStatus, Option<String>, Option<RefinementUsage>) {
    if stt::is_cancelled(cancel) || raw.trim().is_empty() || !cfg.enabled {
        return (PostprocStatus::Off, None, None);
    }
    let chars = raw.chars().count();
    if !should_run_postproc(cfg, chars) {
        tracing::info!(
            "[Postproc] skipped_short chars={} min_chars={}",
            chars,
            cfg.min_chars
        );
        return (PostprocStatus::SkippedShort { chars }, None, None);
    }
    on_stage(Stage::CleaningUp);
    if stt::is_cancelled(cancel) {
        return (PostprocStatus::Off, None, None);
    }
    let started = Instant::now();
    let key = resolve_api_key(cfg.api_key_id.as_deref());
    if stt::is_cancelled(cancel) {
        return (PostprocStatus::Off, None, None);
    }
    match key.and_then(|key| postproc::refine(raw, cfg, vocabulary, key.as_deref())) {
        Ok(refined) => (
            PostprocStatus::Applied {
                ms: started.elapsed().as_millis(),
            },
            Some(refined.text),
            refined.usage,
        ),
        Err(error) => {
            tracing::warn!("[Postproc] cleanup failed error={error:#}");
            (
                PostprocStatus::Failed {
                    ms: started.elapsed().as_millis(),
                },
                None,
                None,
            )
        }
    }
}

fn transcribe(
    cache: &mut TranscriberCache,
    wav: &Path,
    stt_cfg: &SttConfig,
    vocabulary: &[String],
    cancel: Option<&AtomicBool>,
    on_stage: &mut impl FnMut(Stage),
) -> Result<stt::Transcript, String> {
    if stt::is_cancelled(cancel) {
        return Ok(stt::Transcript::Cancelled {
            text: String::new(),
            completed: 0,
            total: 0,
        });
    }
    let on_progress = |progress: stt::ChunkProgress| {
        on_stage(Stage::Transcribing {
            completed: progress.completed,
            total: progress.total,
        });
    };
    if let Some(endpoint) = &stt_cfg.endpoint {
        let key = resolve_api_key(stt_cfg.api_key_id.as_deref()).map_err(|e| format!("{e:#}"))?;
        return stt::transcribe_remote(
            wav,
            endpoint,
            &stt_cfg.model,
            vocabulary,
            key.as_deref(),
            cancel,
            on_progress,
        )
        .map_err(|e| format!("{e:#}"));
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
    transcriber
        .transcribe_wav(wav, cancel, on_progress)
        .map_err(|e| format!("{e:#}"))
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
            Stage::Transcribing {
                completed: 0,
                total: 4
            }
            .measured_progress(),
            Some((0, 4))
        );
        assert_eq!(
            Stage::Transcribing {
                completed: 4,
                total: 4
            }
            .measured_progress(),
            Some((4, 4))
        );
        assert_eq!(
            Stage::Transcribing {
                completed: 5,
                total: 4
            }
            .measured_progress(),
            None
        );
        assert_eq!(
            Stage::Transcribing {
                completed: 0,
                total: 1
            }
            .measured_progress(),
            None
        );
        assert_eq!(
            Stage::Transcribing {
                completed: 0,
                total: 0
            }
            .measured_progress(),
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

    #[test]
    fn meaningful_empty_and_interrupted_audio_remain_recoverable() {
        assert!(!retain_audio(false, false, true, Some(2_999)));
        assert!(retain_audio(false, false, true, Some(3_000)));
        assert!(retain_audio(false, false, true, None));
        assert!(!retain_audio(false, false, false, Some(60_000)));
        assert!(retain_audio(true, false, false, Some(500)));
        assert!(retain_audio(false, true, false, Some(500)));
    }

    #[test]
    fn cancellation_at_cleanup_boundary_never_contacts_the_provider() {
        use std::net::TcpListener;
        use std::sync::atomic::Ordering;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let cfg = PostprocConfig {
            enabled: true,
            min_chars: 0,
            endpoint: format!("http://{}", listener.local_addr().unwrap()),
            api_key_id: None,
            ..Default::default()
        };
        let cancel = AtomicBool::new(false);
        let (status, processed, _) = cleanup(
            "preserve these words",
            &cfg,
            &[],
            Some(&cancel),
            &mut |_| {
                cancel.store(true, Ordering::Release);
            },
        );
        assert_eq!(status, PostprocStatus::Off);
        assert_eq!(processed, None);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}
