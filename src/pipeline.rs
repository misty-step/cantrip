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
}

/// Which sub-stage of a job is running right now, observable live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Transcribing,
    CleaningUp,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Transcribing => "transcribing",
            Self::CleaningUp => "cleaning",
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
}

/// Cache of the loaded local transcriber, keyed by model name. Keep one
/// mutable instance across dictations to avoid reloading the model.
pub type TranscriberCache = Option<(String, Transcriber)>;

/// Transcribe `wav` with the configured STT backend, then apply the
/// configured post-processing pass. STT is local (Parakeet) unless
/// `stt.endpoint` is set, which selects an OpenAI-compatible cloud.
///
/// A post-processing failure never drops the dictation: the raw text is
/// returned with `PostprocStatus::Failed`.
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
    on_stage(Stage::Transcribing);
    let transcription =
        transcribe(cache, wav, stt_cfg, vocabulary).map_err(|error| format!("{error:#}"));
    let stt_elapsed = stt_started.elapsed();

    let (text, postproc) = match transcription {
        Ok(text) if !text.trim().is_empty() && postproc_cfg.enabled => {
            on_stage(Stage::CleaningUp);
            let postproc_started = Instant::now();
            match resolve_api_key(postproc_cfg.api_key_id.as_deref())
                .and_then(|key| postproc::refine(&text, postproc_cfg, vocabulary, key.as_deref()))
            {
                Ok(refined) => (
                    Ok(refined),
                    PostprocStatus::Applied {
                        ms: postproc_started.elapsed().as_millis(),
                    },
                ),
                Err(error) => {
                    tracing::warn!("[Postproc] cleanup failed error={error:#}");
                    (Ok(text), PostprocStatus::Failed)
                }
            }
        }
        Ok(text) => (Ok(text), PostprocStatus::Off),
        Err(error) => (Err(error), PostprocStatus::Off),
    };

    Outcome {
        text,
        stt_elapsed,
        postproc,
    }
}

fn transcribe(
    cache: &mut TranscriberCache,
    wav: &Path,
    stt_cfg: &SttConfig,
    vocabulary: &[String],
) -> Result<String> {
    if let Some(endpoint) = &stt_cfg.endpoint {
        let key = resolve_api_key(stt_cfg.api_key_id.as_deref())?;
        return stt::transcribe_remote(wav, endpoint, &stt_cfg.model, vocabulary, key.as_deref());
    }

    let reload = cache
        .as_ref()
        .is_none_or(|(name, _)| name != &stt_cfg.model);
    if reload {
        *cache = Some(load_transcriber(&stt_cfg.model)?);
    }
    cache
        .as_mut()
        .map(|(_, transcriber)| transcriber)
        .context("transcription backend has no model")?
        .transcribe_wav(wav)
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
