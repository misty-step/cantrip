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
use std::time::Instant;

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
    /// Return validated measured chunk progress. `0/1` is an empty meter;
    /// `1/1` fills it. Invalid totals stay indeterminate.
    pub fn measured_progress(&self) -> Option<(u32, u32)> {
        match self {
            Self::Transcribing { completed, total } if *total >= 1 && *completed <= *total => {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SttBackend {
    Cloud,
    Local,
    Mixed,
}

impl SttBackend {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Cloud => "cloud",
            Self::Local => "local",
            Self::Mixed => "mixed",
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
    /// Useful local text completed a cloud failure, either as a native suffix
    /// or as a whole-take retry when no compatible prefix was available.
    pub local_fallback: bool,
    pub stt_backend: SttBackend,
    pub cloud_failure: Option<stt::CloudFailure>,
    pub archive: ArchiveStatus,
}

impl Outcome {
    /// Cause and selected recognition result only; callers supply delivery/retention/actions.
    pub fn cloud_notice(&self) -> Option<String> {
        if self.cancelled {
            return None;
        }
        let cause = self.cloud_failure?;
        let result = if self.partial {
            "Local recovery did not complete the transcript."
        } else if self.local_fallback {
            "Complete transcript recovered locally."
        } else {
            "Local recovery did not produce a complete transcript."
        };
        Some(format!("{cause}. {result}"))
    }
}

/// Cache of the loaded local transcriber, keyed by model name. Keep one
/// mutable instance across dictations to avoid reloading the model.
pub type TranscriberCache = Option<(String, Transcriber)>;

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

/// Transcribe `wav` with the configured STT backend, then apply the
/// configured post-processing pass. A stalled, failed, or empty cloud chunk
/// gets local completion with the installed default Parakeet. Native capture
/// keeps completed cloud text and retries only its exact remaining frames;
/// other formats or failures before any useful prefix retry the whole take.
///
/// A post-processing failure never drops the dictation: the raw text is
/// returned with `PostprocStatus::Failed`. Short transcripts under
/// `postproc.min_chars` skip cleanup and return the raw text.
pub fn run(
    cache: &mut TranscriberCache,
    wav: &Path,
    stt_cfg: &SttConfig,
    vocabulary: &[String],
    postproc_cfg: &PostprocConfig,
    context: RunContext<'_>,
    on_stage: impl FnMut(Stage),
) -> Outcome {
    Pipeline {
        wav,
        stt_cfg,
        vocabulary,
        postproc_cfg,
        context,
    }
    .run(
        on_stage,
        |cfg, suffix, cancel, on_stage| {
            transcribe(cache, wav, cfg, vocabulary, suffix, cancel, on_stage)
        },
        archive::save,
    )
}

struct Pipeline<'a> {
    wav: &'a Path,
    stt_cfg: &'a SttConfig,
    vocabulary: &'a [String],
    postproc_cfg: &'a PostprocConfig,
    context: RunContext<'a>,
}

impl Pipeline<'_> {
    fn run(
        self,
        mut on_stage: impl FnMut(Stage),
        mut transcribe: impl FnMut(
            &SttConfig,
            Option<&stt::NativeSuffix>,
            Option<&AtomicBool>,
            &mut dyn FnMut(Stage),
        ) -> Result<stt::RemoteAttempt>,
        save: impl FnOnce(archive::Entry<'_>) -> Result<PathBuf>,
    ) -> Outcome {
        let Self {
            wav,
            stt_cfg,
            vocabulary,
            postproc_cfg,
            context,
        } = self;
        let pipeline_started = Instant::now();
        let audio = std::fs::File::open(wav)
            .with_context(|| format!("opening WAV {}", wav.display()))
            .and_then(|mut file| stt::wav_metadata_from(&mut file));
        if let Err(error) = &audio {
            tracing::warn!("[STT] WAV metadata unavailable error={error:#}");
        }
        let audio_duration_ms = audio.as_ref().ok().map(|audio| audio.duration_ms);
        let stt_started = Instant::now();
        let attempt = if stt::is_cancelled(context.cancel) {
            Ok(stt::RemoteAttempt::whole(stt::Transcript::Cancelled {
                text: String::new(),
                completed: 0,
                total: 0,
            }))
        } else {
            transcribe(stt_cfg, None, context.cancel, &mut on_stage)
        };
        let (mut transcription, suffix, mut cloud_failure) = match attempt {
            Ok(attempt) => (
                Ok(attempt.transcript),
                attempt.suffix,
                attempt.cloud_failure,
            ),
            Err(error) => {
                let cause = stt_cfg
                    .endpoint
                    .is_some()
                    .then(|| stt::CloudFailure::from_error(&error));
                (Err(format!("{error:#}")), None, cause)
            }
        };
        let mut stt_backend = if stt_cfg.endpoint.is_some() {
            SttBackend::Cloud
        } else {
            SttBackend::Local
        };
        let mut local_fallback = false;
        let retry_local = stt_cfg.endpoint.is_some()
            && !stt::is_cancelled(context.cancel)
            && match &transcription {
                Err(_) | Ok(stt::Transcript::Partial { .. }) => true,
                Ok(stt::Transcript::Complete(text)) => {
                    text.trim().is_empty() && !audio.as_ref().is_ok_and(|audio| audio.frames == 0)
                }
                Ok(stt::Transcript::Cancelled { .. }) => false,
            };
        if retry_local {
            if cloud_failure.is_none() {
                cloud_failure = Some(match &transcription {
                    Ok(stt::Transcript::Complete(_)) => stt::CloudFailure::EmptyResponse,
                    _ => stt::CloudFailure::Unknown,
                });
            }
            // A suffix keeps the cloud meter and frame plan. An alternate
            // whole-take pass must not inherit that measured progress.
            on_stage(Stage::Transcribing {
                completed: suffix.as_ref().map_or(0, stt::NativeSuffix::completed),
                total: suffix.as_ref().map_or(1, stt::NativeSuffix::total),
            });
            if !stt::is_cancelled(context.cancel) {
                tracing::info!(
                    "[STT] completing cloud failure with installed local Parakeet suffix={}",
                    suffix.is_some()
                );
                let cfg = SttConfig {
                    model: models::PARAKEET_V3_INT8.dir_name.to_owned(),
                    endpoint: None,
                    api_key_id: None,
                };
                let local = transcribe(&cfg, suffix.as_ref(), context.cancel, &mut on_stage)
                    .map(|attempt| attempt.transcript)
                    .map_err(|error| format!("{error:#}"));
                if let Some(suffix) = &suffix {
                    match local {
                        Ok(local) => {
                            let useful_local = !local.text().trim().is_empty();
                            transcription =
                                transcription.map(|cloud| join_suffix(cloud, local, suffix));
                            if useful_local {
                                local_fallback = true;
                                stt_backend = SttBackend::Mixed;
                            }
                        }
                        Err(error) => {
                            tracing::warn!(
                                "[STT] local suffix unavailable class={}",
                                stt::classify_failure(&error)
                            );
                        }
                    }
                } else if prefer_local(&transcription, &local) {
                    transcription = local;
                    local_fallback = true;
                    stt_backend = SttBackend::Local;
                } else {
                    if let Err(error) = &local {
                        tracing::warn!(
                            "[STT] local retry unavailable class={}",
                            stt::classify_failure(error)
                        );
                    }
                    if let Err(cloud_error) = &mut transcription {
                        match local {
                            Err(local_error) => {
                                *cloud_error = format!(
                                    "{cloud_error}; local Parakeet retry failed: {local_error}"
                                );
                            }
                            Ok(_) => {
                                cloud_error.push_str("; local Parakeet retry returned no text")
                            }
                        }
                    }
                }
            }
        }
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
                    partial: false,
                    cancelled: stt::is_cancelled(context.cancel),
                    local_fallback,
                    stt_backend,
                    cloud_failure,
                    archive: ArchiveStatus::NotApplicable,
                };
            }
        };

        let (postproc, processed, postproc_usage) = if cancelled {
            (PostprocStatus::Off, None, None)
        } else {
            cleanup(
                &raw,
                postproc_cfg,
                vocabulary,
                context.cancel,
                &mut on_stage,
            )
        };
        cancelled |= stt::is_cancelled(context.cancel);
        if cancelled {
            on_stage(Stage::Cancelling);
        }

        let attempted_postproc = matches!(
            &postproc,
            PostprocStatus::Applied { .. } | PostprocStatus::Failed { .. }
        );
        let postproc_elapsed_ms = match &postproc {
            PostprocStatus::Applied { ms } | PostprocStatus::Failed { ms } => {
                Some(duration_ms(*ms))
            }
            PostprocStatus::Off | PostprocStatus::SkippedShort { .. } => None,
        };
        let archive = match save(archive::Entry {
            take_id: context.take_id,
            source: context.source.as_str(),
            raw_transcript: &raw,
            postprocessed_transcript: processed.as_deref(),
            audio_duration_ms,
            pipeline_elapsed_ms: duration_ms(pipeline_started.elapsed().as_millis()),
            stt_model: if local_fallback {
                models::PARAKEET_V3_INT8.dir_name
            } else {
                &stt_cfg.model
            },
            stt_backend: stt_backend.as_str(),
            stt_fallback_from_model: local_fallback.then_some(stt_cfg.model.as_str()),
            stt_elapsed_ms: duration_ms(stt_elapsed.as_millis()),
            // Local completion does not erase any unknown cloud charges.
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
            postproc_completion_tokens: postproc_usage
                .as_ref()
                .map(|usage| usage.completion_tokens),
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

        Outcome {
            text: Ok(text),
            stt_elapsed,
            postproc,
            postproc_usage,
            partial,
            cancelled,
            local_fallback,
            stt_backend,
            cloud_failure,
            archive,
        }
    }
}

fn join_suffix(
    cloud: stt::Transcript,
    local: stt::Transcript,
    suffix: &stt::NativeSuffix,
) -> stt::Transcript {
    if matches!(&local, stt::Transcript::Complete(text) if text.trim().is_empty()) {
        return cloud;
    }
    let stt::Transcript::Partial { mut text, .. } = cloud else {
        return cloud;
    };
    let mut append = |local: &str| {
        if !local.trim().is_empty() {
            text.push(' ');
            text.push_str(local);
        }
    };
    match local {
        stt::Transcript::Complete(local) => {
            append(&local);
            stt::Transcript::Complete(text)
        }
        stt::Transcript::Partial {
            text: local,
            failed_at,
            ..
        } => {
            append(&local);
            stt::Transcript::Partial {
                text,
                failed_at: suffix.completed() + failed_at,
                total: suffix.total(),
            }
        }
        stt::Transcript::Cancelled {
            text: local,
            completed,
            ..
        } => {
            append(&local);
            stt::Transcript::Cancelled {
                text,
                completed: suffix.completed() + completed,
                total: suffix.total(),
            }
        }
    }
}

fn prefer_local(
    cloud: &Result<stt::Transcript, String>,
    local: &Result<stt::Transcript, String>,
) -> bool {
    let Ok(local) = local else {
        return false;
    };
    if local.text().trim().is_empty() {
        return false;
    }
    // Complete coverage beats any cloud failure. Two partial passes cannot be
    // compared by character count or chunk fractions: their boundaries may
    // differ. Conservatively keep usable cloud text unless local covered the
    // whole take; use local partial text only when cloud has none. Never join
    // alternate whole-take passes, which would duplicate overlapping speech.
    matches!(local, stt::Transcript::Complete(_))
        || !cloud
            .as_ref()
            .is_ok_and(|cloud| !cloud.text().trim().is_empty())
}

fn duration_ms(ms: u128) -> u64 {
    u64::try_from(ms).unwrap_or(u64::MAX)
}

fn cleanup(
    raw: &str,
    cfg: &PostprocConfig,
    vocabulary: &[String],
    cancel: Option<&AtomicBool>,
    on_stage: &mut dyn FnMut(Stage),
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
    let deadline = postproc::deadline(cfg);
    let key = resolve_api_key(cfg.api_key_id.as_deref(), deadline, cancel);
    if stt::is_cancelled(cancel) {
        return (PostprocStatus::Off, None, None);
    }
    match key.and_then(|key| {
        postproc::refine_until(raw, cfg, vocabulary, key.as_deref(), cancel, deadline)
    }) {
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
    suffix: Option<&stt::NativeSuffix>,
    cancel: Option<&AtomicBool>,
    on_stage: &mut dyn FnMut(Stage),
) -> Result<stt::RemoteAttempt> {
    if stt::is_cancelled(cancel) {
        return Ok(stt::RemoteAttempt::whole(stt::Transcript::Cancelled {
            text: String::new(),
            completed: 0,
            total: 0,
        }));
    }
    let on_progress = |progress: stt::ChunkProgress| {
        on_stage(Stage::Transcribing {
            completed: suffix.map_or(0, stt::NativeSuffix::completed) + progress.completed,
            total: suffix.map_or(progress.total, stt::NativeSuffix::total),
        });
    };
    if let Some(endpoint) = &stt_cfg.endpoint {
        let key = resolve_api_key(
            stt_cfg.api_key_id.as_deref(),
            Instant::now() + crate::keys::LOOKUP_TIMEOUT,
            cancel,
        )
        .map_err(|_| anyhow::anyhow!(stt::CloudFailure::CredentialUnavailable))?;
        return stt::transcribe_cloud(
            wav,
            endpoint,
            &stt_cfg.model,
            vocabulary,
            key.as_deref(),
            cancel,
            on_progress,
        );
    }

    let reload = cache
        .as_ref()
        .is_none_or(|(name, _)| name != &stt_cfg.model);
    if reload {
        *cache = Some(load_transcriber(&stt_cfg.model)?);
    }
    let transcriber = cache
        .as_mut()
        .map(|(_, transcriber)| transcriber)
        .ok_or_else(|| anyhow::anyhow!("transcription backend has no model"))?;
    let transcript = if let Some(suffix) = suffix {
        transcriber.transcribe_suffix(suffix, cancel, on_progress)
    } else {
        transcriber.transcribe_wav(wav, cancel, on_progress)
    };
    transcript.map(stt::RemoteAttempt::whole)
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

fn resolve_api_key(
    id: Option<&str>,
    deadline: Instant,
    cancel: Option<&AtomicBool>,
) -> Result<Option<String>> {
    id.map(|id| crate::keys::get_until(id, deadline, cancel))
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
    use std::net::TcpListener;
    use std::sync::atomic::Ordering;
    use std::thread;
    use std::time::Duration;

    struct Recording {
        root: PathBuf,
        wav: PathBuf,
    }

    impl Recording {
        fn new(frames: usize) -> Self {
            let root = std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("cantrip-pipeline-{}", archive::new_id()));
            std::fs::create_dir(&root).unwrap();
            let wav = root.join("take.wav");
            let mut writer = hound::WavWriter::create(
                &wav,
                hound::WavSpec {
                    channels: 1,
                    sample_rate: 16_000,
                    bits_per_sample: 16,
                    sample_format: hound::SampleFormat::Int,
                },
            )
            .unwrap();
            for index in 0..frames {
                writer.write_sample(recorded_sample(index)).unwrap();
            }
            writer.finalize().unwrap();
            Self { root, wav }
        }

        fn with_oversized_metadata(self) -> Self {
            const METADATA_BYTES: u32 = 25_000_000;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .open(&self.wav)
                .unwrap();
            let original_len = file.seek(SeekFrom::End(0)).unwrap();
            file.write_all(b"JUNK").unwrap();
            file.write_all(&METADATA_BYTES.to_le_bytes()).unwrap();
            let file_len = original_len + 8 + u64::from(METADATA_BYTES);
            file.set_len(file_len).unwrap();
            file.seek(SeekFrom::Start(4)).unwrap();
            file.write_all(&((file_len - 8) as u32).to_le_bytes())
                .unwrap();
            self
        }

        fn pipeline<'a>(
            &'a self,
            stt_cfg: &'a SttConfig,
            postproc_cfg: &'a PostprocConfig,
            cancel: Option<&'a AtomicBool>,
        ) -> Pipeline<'a> {
            Pipeline {
                wav: &self.wav,
                stt_cfg,
                vocabulary: &[],
                postproc_cfg,
                context: RunContext {
                    source: Source::Dictation,
                    take_id: None,
                    cancel,
                },
            }
        }

        fn save(&self, entry: archive::Entry<'_>) -> Result<PathBuf> {
            archive::save_to(&self.root.join("history"), entry)
        }

        fn recognize(
            &self,
            cfg: &SttConfig,
            attempts: Vec<Result<stt::Transcript, String>>,
        ) -> Outcome {
            let mut attempts = attempts.into_iter();
            self.pipeline(cfg, &PostprocConfig::default(), None).run(
                |_| {},
                |_, _, _, _| {
                    attempts
                        .next()
                        .expect("unexpected additional inference attempt")
                        .map(stt::RemoteAttempt::whole)
                        .map_err(anyhow::Error::msg)
                },
                |entry| self.save(entry),
            )
        }
    }

    impl Drop for Recording {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn recorded_sample(index: usize) -> i16 {
        let value = (index as u32).wrapping_mul(0x9e37_79b9).rotate_left(11);
        (value as i32 >> 16) as i16
    }

    fn cloud_config() -> SttConfig {
        SttConfig {
            model: "configured-cloud-model".to_owned(),
            endpoint: Some("http://127.0.0.1:1".to_owned()),
            api_key_id: None,
        }
    }

    fn record(outcome: &Outcome) -> serde_json::Value {
        let ArchiveStatus::Saved(path) = &outcome.archive else {
            panic!("transcript was not durably archived: {:?}", outcome.archive);
        };
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    fn partial(text: &str) -> stt::Transcript {
        stt::Transcript::Partial {
            text: text.to_owned(),
            failed_at: 2,
            total: 3,
        }
    }

    fn json_response(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    struct Request {
        line: String,
        body: Vec<u8>,
    }

    impl Request {
        fn audio(&self) -> Vec<i16> {
            let header = b"Content-Type: audio/wav\r\n\r\n";
            let start = self
                .body
                .windows(header.len())
                .position(|bytes| bytes == header)
                .unwrap()
                + header.len();
            let wav = &self.body[start..];
            let len = u32::from_le_bytes(wav[4..8].try_into().unwrap()) as usize + 8;
            hound::WavReader::new(std::io::Cursor::new(&wav[..len]))
                .unwrap()
                .samples::<i16>()
                .map(|sample| sample.unwrap())
                .collect()
        }
    }

    fn http_server(responses: Vec<String>) -> (String, thread::JoinHandle<Vec<Request>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            responses
                .into_iter()
                .map(|response| {
                    let (mut stream, _) = listener.accept().unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut reader = BufReader::new(&stream);
                    let mut request_line = String::new();
                    reader.read_line(&mut request_line).unwrap();
                    let mut length = 0;
                    loop {
                        let mut line = String::new();
                        assert!(
                            reader.read_line(&mut line).unwrap() > 0,
                            "incomplete HTTP headers"
                        );
                        if line == "\r\n" {
                            break;
                        }
                        if let Some(value) =
                            line.to_ascii_lowercase().strip_prefix("content-length:")
                        {
                            length = value.trim().parse().unwrap();
                        }
                    }
                    let mut body = vec![0; length];
                    reader.read_exact(&mut body).unwrap();
                    drop(reader);
                    stream.write_all(response.as_bytes()).unwrap();
                    Request {
                        line: request_line.trim().to_owned(),
                        body,
                    }
                })
                .collect()
        });
        (endpoint, server)
    }

    #[test]
    fn cloud_429_on_chunk_two_finishes_locally_before_one_cleanup() {
        let recording = Recording::new(70 * 16_000).with_oversized_metadata();
        let (endpoint, server) = http_server(vec![
            json_response(r#"{"text":"cloud prefix that must not be duplicated"}"#),
            "HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_owned(),
            json_response(
                r#"{"choices":[{"message":{"content":"Cloud prefix and local suffix."}}]}"#,
            ),
        ]);
        let cfg = SttConfig {
            endpoint: Some(endpoint.clone()),
            ..cloud_config()
        };
        let postproc_cfg = PostprocConfig {
            enabled: true,
            endpoint,
            model: "configured-cleanup".to_owned(),
            api_key_id: None,
            min_chars: 0,
            passes: 1,
            ..Default::default()
        };
        let mut stages = Vec::new();
        let mut cache = None;
        let mut local_audio = Vec::new();
        let mut local_chunks = Vec::new();
        let outcome = recording.pipeline(&cfg, &postproc_cfg, None).run(
            |stage| stages.push(stage),
            |cfg, suffix, cancel, on_stage| {
                if cfg.endpoint.is_some() {
                    transcribe(&mut cache, &recording.wav, cfg, &[], None, cancel, on_stage)
                } else {
                    let suffix = suffix.expect("native retry must supply an exact suffix");
                    suffix
                        .transcribe(
                            cancel,
                            &mut |progress| {
                                on_stage(Stage::Transcribing {
                                    completed: suffix.completed() + progress.completed,
                                    total: suffix.total(),
                                })
                            },
                            |samples| {
                                local_audio.extend_from_slice(samples);
                                local_chunks.push(samples.len());
                                Ok(format!("local-{}", local_chunks.len()))
                            },
                        )
                        .map(stt::RemoteAttempt::whole)
                }
            },
            |entry| recording.save(entry),
        );
        assert_eq!(
            outcome.text.as_deref(),
            Ok("Cloud prefix and local suffix.")
        );
        assert_eq!(
            outcome.cloud_failure,
            Some(stt::CloudFailure::Http { status: 429 })
        );
        assert!(outcome.local_fallback);
        assert!(!outcome.partial);
        assert!(!outcome.cancelled);
        let saved = record(&outcome);
        assert_eq!(
            saved["raw_transcript"],
            "cloud prefix that must not be duplicated local-1 local-2"
        );
        assert_eq!(saved["stt"]["backend"], "mixed");
        assert_eq!(saved["stt"]["model"], models::PARAKEET_V3_INT8.dir_name);
        assert_eq!(saved["stt"]["fallback_from_model"], cfg.model);
        assert!(saved["stt"].get("api_cost_usd").is_none());
        assert_eq!(saved["recovery"]["retry_incomplete"], false);
        let requests = server.join().unwrap();
        let prefix = requests[0].audio();
        assert_eq!(
            local_chunks.len(),
            2,
            "only failed and unstarted chunks run locally"
        );
        assert_eq!(local_chunks[0], requests[1].audio().len());
        assert!(
            local_chunks.iter().all(|&frames| frames <= 33 * 16_000),
            "native suffix inference must never inherit an oversized cloud range"
        );
        assert_eq!(prefix.len() + local_audio.len(), 70 * 16_000);
        for (index, sample) in prefix.into_iter().enumerate() {
            assert_eq!(sample, recorded_sample(index));
        }
        let suffix_start = 70 * 16_000 - local_audio.len();
        for (offset, sample) in local_audio.into_iter().enumerate() {
            assert_eq!(
                sample,
                f32::from(recorded_sample(suffix_start + offset)) / f32::from(i16::MAX),
                "each remaining native frame must be recognized exactly once"
            );
        }
        assert_eq!(
            requests
                .iter()
                .map(|request| request.line.as_str())
                .collect::<Vec<_>>(),
            [
                "POST /audio/transcriptions HTTP/1.1",
                "POST /audio/transcriptions HTTP/1.1",
                "POST /chat/completions HTTP/1.1",
            ]
        );
        let cleanup: serde_json::Value = serde_json::from_slice(&requests[2].body).unwrap();
        assert_eq!(
            cleanup["messages"][1]["content"],
            "Source:\ncloud prefix that must not be duplicated local-1 local-2"
        );
        assert_eq!(
            stages
                .iter()
                .filter(|stage| **stage == Stage::CleaningUp)
                .count(),
            1
        );
        assert_eq!(
            stages[stages.len() - 2],
            Stage::Transcribing {
                completed: 3,
                total: 3
            },
            "the suffix completes the original cloud meter without a whole-take reset"
        );
        assert!(recording.wav.is_file());
    }

    #[test]
    fn key_lookup_failure_can_finish_with_a_complete_local_transcript() {
        let recording = Recording::new(1);
        let outcome = recording.recognize(
            &cloud_config(),
            vec![
                Err("api key 'configured-cloud' unavailable".to_owned()),
                Ok(stt::Transcript::Complete("offline dictation".to_owned())),
            ],
        );
        assert_eq!(outcome.text.as_deref(), Ok("offline dictation"));
        assert!(outcome.local_fallback);
        assert!(!outcome.partial);
        assert_eq!(record(&outcome)["stt"]["backend"], "local");
    }

    #[test]
    fn incomplete_local_retry_cannot_replace_usable_cloud_partial_text() {
        let recording = Recording::new(1);
        for local in [
            Err("local model unavailable".to_owned()),
            Ok(stt::Transcript::Complete(String::new())),
            Ok(partial(
                "a much longer local alternative is not evidence of greater coverage",
            )),
        ] {
            let outcome =
                recording.recognize(&cloud_config(), vec![Ok(partial("cloud prefix")), local]);
            assert_eq!(outcome.text.as_deref(), Ok("cloud prefix"));
            assert!(outcome.partial);
            assert!(!outcome.local_fallback);
            let saved = record(&outcome);
            assert_eq!(saved["stt"]["backend"], "cloud");
            assert_eq!(saved["raw_transcript"], "cloud prefix");
            assert_eq!(saved["recovery"]["retry_incomplete"], true);
            assert!(recording.wav.is_file());
        }
    }

    #[test]
    fn useful_local_partial_survives_when_cloud_produced_no_text() {
        let recording = Recording::new(1);
        let outcome = recording.recognize(
            &cloud_config(),
            vec![
                Err("remote transcription endpoint returned HTTP 503".to_owned()),
                Ok(partial("locally recovered prefix")),
            ],
        );
        assert_eq!(outcome.text.as_deref(), Ok("locally recovered prefix"));
        assert!(outcome.partial);
        assert!(outcome.local_fallback);
        let saved = record(&outcome);
        assert_eq!(saved["stt"]["backend"], "local");
        assert_eq!(saved["recovery"]["retry_incomplete"], true);
    }

    #[test]
    fn both_failures_leave_audio_and_report_both_structural_causes() {
        let recording = Recording::new(1);
        let outcome = recording.recognize(
            &cloud_config(),
            vec![
                Err("remote transcription endpoint returned HTTP 429".to_owned()),
                Err("model not installed — run: cantrip models pull".to_owned()),
            ],
        );
        let error = outcome.text.unwrap_err();
        assert!(error.contains("HTTP 429"));
        assert!(error.contains("model not installed"));
        assert!(!outcome.local_fallback);
        assert!(matches!(outcome.archive, ArchiveStatus::NotApplicable));
        assert!(recording.wav.is_file());
    }

    #[test]
    fn cancellation_at_suffix_boundary_keeps_cloud_text_without_inference_or_cleanup() {
        let recording = Recording::new(70 * 16_000).with_oversized_metadata();
        let (endpoint, server) = http_server(vec![
            json_response(r#"{"text":"preserve cloud words"}"#),
            "HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_owned(),
        ]);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let cfg = SttConfig {
            endpoint: Some(endpoint),
            ..cloud_config()
        };
        let postproc_cfg = PostprocConfig {
            enabled: true,
            endpoint: format!("http://{}", listener.local_addr().unwrap()),
            api_key_id: None,
            min_chars: 0,
            ..Default::default()
        };
        let cancel = AtomicBool::new(false);
        let mut prefix_events = 0;
        let mut cache = None;
        let outcome = recording.pipeline(&cfg, &postproc_cfg, Some(&cancel)).run(
            |stage| {
                if matches!(stage, Stage::Transcribing { completed: 1, .. }) {
                    prefix_events += 1;
                    if prefix_events == 2 {
                        cancel.store(true, Ordering::Release);
                    }
                }
            },
            |cfg, suffix, cancel, on_stage| {
                assert!(
                    cfg.endpoint.is_some() && suffix.is_none(),
                    "cancelled fallback must not start local inference"
                );
                transcribe(&mut cache, &recording.wav, cfg, &[], None, cancel, on_stage)
            },
            |entry| recording.save(entry),
        );
        assert_eq!(outcome.text.as_deref(), Ok("preserve cloud words"));
        assert!(outcome.cancelled);
        assert!(outcome.partial);
        assert!(!outcome.local_fallback);
        assert_eq!(outcome.postproc, PostprocStatus::Off);
        assert_eq!(record(&outcome)["stt"]["cancelled"], true);
        assert_eq!(server.join().unwrap().len(), 2);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn cancellation_during_local_retry_preserves_useful_text_without_cleanup() {
        let recording = Recording::new(1);
        let cfg = cloud_config();
        let postproc_cfg = PostprocConfig {
            enabled: true,
            min_chars: 0,
            ..Default::default()
        };
        let cancel = AtomicBool::new(false);
        let outcome = recording.pipeline(&cfg, &postproc_cfg, Some(&cancel)).run(
            |_| {},
            |cfg, _, _, _| {
                if cfg.endpoint.is_some() {
                    Err(anyhow::anyhow!(stt::CloudFailure::Http { status: 429 }))
                } else {
                    cancel.store(true, Ordering::Release);
                    Ok(stt::RemoteAttempt::whole(stt::Transcript::Cancelled {
                        text: "locally recovered before cancellation".to_owned(),
                        completed: 1,
                        total: 3,
                    }))
                }
            },
            |entry| recording.save(entry),
        );
        assert_eq!(
            outcome.text.as_deref(),
            Ok("locally recovered before cancellation")
        );
        assert!(outcome.cancelled);
        assert!(outcome.partial);
        assert!(outcome.local_fallback);
        assert_eq!(outcome.postproc, PostprocStatus::Off);
        assert_eq!(record(&outcome)["recovery"]["retry_incomplete"], true);
    }

    #[test]
    fn native_suffix_failure_empty_recognition_and_cancellation_preserve_usable_text() {
        let recording = Recording::new(70 * 16_000).with_oversized_metadata();
        for case in [
            "unavailable",
            "silent-first",
            "silent-last",
            "empty-all",
            "cancel-first",
            "cloud-empty",
        ] {
            let (endpoint, server) = http_server(vec![
                json_response(r#"{"text":"cloud prefix"}"#),
                if case == "cloud-empty" {
                    json_response(r#"{"text":" "}"#)
                } else {
                    "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_owned()
                },
            ]);
            let cfg = SttConfig {
                endpoint: Some(endpoint),
                ..cloud_config()
            };
            let cancel = AtomicBool::new(false);
            let postproc_cfg = PostprocConfig {
                enabled: case == "cancel-first",
                min_chars: 0,
                ..Default::default()
            };
            let mut cache = None;
            let mut local_calls = 0;
            let outcome = recording.pipeline(&cfg, &postproc_cfg, Some(&cancel)).run(
                |_| {},
                |cfg, suffix, cancel, on_stage| {
                    if cfg.endpoint.is_some() {
                        return transcribe(
                            &mut cache,
                            &recording.wav,
                            cfg,
                            &[],
                            None,
                            cancel,
                            on_stage,
                        );
                    }
                    let suffix = suffix.expect("a native prefix needs a suffix, not a whole retry");
                    if case == "unavailable" {
                        return Err(anyhow::anyhow!("model not installed"));
                    }
                    suffix
                        .transcribe(cancel, &mut |_| {}, |_| {
                            local_calls += 1;
                            if case == "empty-all"
                                || (case == "silent-first" && local_calls == 1)
                                || (case == "silent-last" && local_calls == 2)
                            {
                                return Ok(String::new());
                            }
                            if case == "cancel-first" {
                                cancel.unwrap().store(true, Ordering::Release);
                            }
                            Ok(format!("local-{local_calls}"))
                        })
                        .map(stt::RemoteAttempt::whole)
                },
                |entry| recording.save(entry),
            );
            let expected_cause = if case == "cloud-empty" {
                stt::CloudFailure::EmptyResponse
            } else {
                stt::CloudFailure::Http { status: 503 }
            };
            assert_eq!(outcome.cloud_failure, Some(expected_cause));
            let expected = match case {
                "unavailable" | "empty-all" => "cloud prefix",
                "silent-first" => "cloud prefix local-2",
                "cloud-empty" => "cloud prefix local-1 local-2",
                _ => "cloud prefix local-1",
            };
            assert_eq!(outcome.text.as_deref(), Ok(expected), "{case}");
            let incomplete = matches!(case, "unavailable" | "empty-all" | "cancel-first");
            assert_eq!(outcome.partial, incomplete, "{case}");
            assert_eq!(outcome.cancelled, case == "cancel-first", "{case}");
            assert_eq!(outcome.postproc, PostprocStatus::Off);
            let saved = record(&outcome);
            assert_eq!(saved["raw_transcript"], expected);
            assert_eq!(
                saved["stt"]["backend"],
                if expected == "cloud prefix" {
                    "cloud"
                } else {
                    "mixed"
                }
            );
            assert_eq!(saved["recovery"]["retry_incomplete"], incomplete);
            assert_eq!(
                server.join().unwrap().len(),
                2,
                "local switching is one-way"
            );
            assert!(recording.wav.is_file());
        }
    }

    #[test]
    fn cloud_success_and_explicit_local_never_start_an_automatic_retry() {
        let recording = Recording::new(1);
        let cloud = recording.recognize(
            &cloud_config(),
            vec![Ok(stt::Transcript::Complete("cloud dictation".to_owned()))],
        );
        assert_eq!(cloud.text.as_deref(), Ok("cloud dictation"));
        assert!(!cloud.local_fallback);
        assert_eq!(record(&cloud)["stt"]["backend"], "cloud");
        let local = recording.recognize(
            &SttConfig::default(),
            vec![Ok(partial("explicit local partial"))],
        );
        assert_eq!(local.text.as_deref(), Ok("explicit local partial"));
        assert!(local.partial);
        assert!(!local.local_fallback);
        let saved = record(&local);
        assert_eq!(saved["stt"]["backend"], "local");
        assert_eq!(saved["stt"]["api_cost_usd"], 0.0);
    }

    #[test]
    fn empty_recognition_retries_even_submillisecond_audio_and_keeps_empty_takes() {
        let recording = Recording::new(1);
        let recognized = recording.recognize(
            &cloud_config(),
            vec![
                Ok(stt::Transcript::Complete(" \n".to_owned())),
                Ok(stt::Transcript::Complete("brief dictation".to_owned())),
            ],
        );
        assert_eq!(recognized.text.as_deref(), Ok("brief dictation"));
        assert!(recognized.local_fallback);
        let empty = recording.recognize(
            &cloud_config(),
            vec![
                Ok(stt::Transcript::Complete(String::new())),
                Ok(stt::Transcript::Complete(String::new())),
            ],
        );
        assert_eq!(empty.text.as_deref(), Ok(""));
        assert!(!empty.local_fallback);
        assert_eq!(record(&empty)["recovery"]["retry_incomplete"], true);
        assert!(recording.wav.is_file());
        let no_frames = Recording::new(0);
        let empty_audio = no_frames.recognize(
            &cloud_config(),
            vec![Ok(stt::Transcript::Complete(String::new()))],
        );
        assert_eq!(empty_audio.text.as_deref(), Ok(""));
        assert!(no_frames.wav.is_file());
    }

    #[test]
    fn empty_local_retry_never_claims_complete_after_a_cloud_error() {
        let recording = Recording::new(8_000);
        let outcome = recording.recognize(
            &cloud_config(),
            vec![
                Err("remote transcription endpoint returned HTTP 429".to_owned()),
                Ok(stt::Transcript::Complete(String::new())),
            ],
        );
        assert!(outcome.text.unwrap_err().contains("HTTP 429"));
        assert!(!outcome.local_fallback);
        assert!(matches!(outcome.archive, ArchiveStatus::NotApplicable));
        assert!(recording.wav.is_file());
    }

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
            Some((0, 1))
        );
        assert_eq!(
            Stage::Transcribing {
                completed: 1,
                total: 1
            }
            .measured_progress(),
            Some((1, 1))
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
    fn cancellation_at_cleanup_boundary_never_contacts_the_provider() {
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
