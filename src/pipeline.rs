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
    /// The selected transcript came from an automatic whole-take local retry.
    pub local_fallback: bool,
    pub archive: ArchiveStatus,
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
/// configured post-processing pass. A failed, partial, or unexpectedly empty
/// cloud result gets one whole-take retry with the installed default Parakeet.
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
        |cfg, cancel, on_stage| transcribe(cache, wav, cfg, vocabulary, cancel, on_stage),
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
            Option<&AtomicBool>,
            &mut dyn FnMut(Stage),
        ) -> Result<stt::Transcript, String>,
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
        let mut transcription = if stt::is_cancelled(context.cancel) {
            Ok(stt::Transcript::Cancelled {
                text: String::new(),
                completed: 0,
                total: 0,
            })
        } else {
            transcribe(stt_cfg, context.cancel, &mut on_stage)
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
            // A fresh pass must not inherit determinate progress from cloud chunks.
            on_stage(Stage::Transcribing {
                completed: 0,
                total: 1,
            });
            if !stt::is_cancelled(context.cancel) {
                tracing::info!("[STT] retrying whole recording with installed local Parakeet");
                let cfg = SttConfig {
                    model: models::PARAKEET_V3_INT8.dir_name.to_owned(),
                    endpoint: None,
                    api_key_id: None,
                };
                let local = transcribe(&cfg, context.cancel, &mut on_stage);
                if prefer_local(&transcription, &local) {
                    transcription = local;
                    local_fallback = true;
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
            stt_remote: stt_cfg.endpoint.is_some() && !local_fallback,
            stt_fallback_from_model: local_fallback.then_some(stt_cfg.model.as_str()),
            stt_elapsed_ms: duration_ms(stt_elapsed.as_millis()),
            // A successful local retry does not erase any unknown cloud charges.
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
            archive,
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
        // A completed silent local pass is still stronger evidence than a
        // cloud error. Keep usable cloud text when local recognition is empty.
        return matches!(local, stt::Transcript::Complete(_)) && cloud.is_err();
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
    let key = resolve_api_key(cfg.api_key_id.as_deref());
    if stt::is_cancelled(cancel) {
        return (PostprocStatus::Off, None, None);
    }
    match key.and_then(|key| postproc::refine(raw, cfg, vocabulary, key.as_deref(), cancel)) {
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
    on_stage: &mut dyn FnMut(Stage),
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
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::Ordering;
    use std::thread;

    struct Recording {
        root: PathBuf,
        wav: PathBuf,
    }

    impl Recording {
        fn new(frames: usize) -> Self {
            let root = std::env::temp_dir().join(format!("cantrip-pipeline-{}", archive::new_id()));
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
            for _ in 0..frames {
                writer.write_sample(0_i16).unwrap();
            }
            writer.finalize().unwrap();
            Self { root, wav }
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
                |_, _, _| {
                    attempts
                        .next()
                        .expect("unexpected additional inference attempt")
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
        let recording = Recording::new(70 * 16_000);
        let (endpoint, server) = http_server(vec![
            json_response(r#"{"text":"cloud prefix that must not be duplicated"}"#),
            "HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_owned(),
            json_response(r#"{"choices":[{"message":{"content":"Complete local dictation."}}]}"#),
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
        let outcome = recording.pipeline(&cfg, &postproc_cfg, None).run(
            |stage| stages.push(stage),
            |cfg, cancel, on_stage| {
                if cfg.endpoint.is_some() {
                    transcribe(&mut cache, &recording.wav, cfg, &[], cancel, on_stage)
                } else {
                    Ok(stt::Transcript::Complete(
                        "complete local dictation".to_owned(),
                    ))
                }
            },
            |entry| recording.save(entry),
        );
        assert_eq!(outcome.text.as_deref(), Ok("Complete local dictation."));
        assert!(outcome.local_fallback);
        assert!(!outcome.partial);
        assert!(!outcome.cancelled);
        let saved = record(&outcome);
        assert_eq!(saved["raw_transcript"], "complete local dictation");
        assert_eq!(saved["stt"]["backend"], "local");
        assert_eq!(saved["stt"]["model"], models::PARAKEET_V3_INT8.dir_name);
        assert_eq!(saved["stt"]["fallback_from_model"], cfg.model);
        assert!(saved["stt"].get("api_cost_usd").is_none());
        assert_eq!(saved["recovery"]["retry_incomplete"], false);
        let requests = server.join().unwrap();
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
            "Source:\ncomplete local dictation"
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
                completed: 0,
                total: 1
            },
            "the fresh local pass must reset measured cloud progress"
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
    fn cancellation_at_fallback_boundary_keeps_cloud_text_without_inference_or_cleanup() {
        let recording = Recording::new(1);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let cfg = cloud_config();
        let postproc_cfg = PostprocConfig {
            enabled: true,
            endpoint: format!("http://{}", listener.local_addr().unwrap()),
            api_key_id: None,
            min_chars: 0,
            ..Default::default()
        };
        let cancel = AtomicBool::new(false);
        let outcome = recording.pipeline(&cfg, &postproc_cfg, Some(&cancel)).run(
            |stage| {
                if matches!(
                    stage,
                    Stage::Transcribing {
                        completed: 0,
                        total: 1
                    }
                ) {
                    cancel.store(true, Ordering::Release);
                }
            },
            |cfg, _, _| {
                assert!(
                    cfg.endpoint.is_some(),
                    "cancelled fallback must not start local inference"
                );
                Ok(partial("preserve cloud words"))
            },
            |entry| recording.save(entry),
        );
        assert_eq!(outcome.text.as_deref(), Ok("preserve cloud words"));
        assert!(outcome.cancelled);
        assert!(outcome.partial);
        assert!(!outcome.local_fallback);
        assert_eq!(outcome.postproc, PostprocStatus::Off);
        assert_eq!(record(&outcome)["stt"]["cancelled"], true);
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
            |cfg, _, _| {
                if cfg.endpoint.is_some() {
                    Err("remote transcription endpoint returned HTTP 429".to_owned())
                } else {
                    cancel.store(true, Ordering::Release);
                    Ok(stt::Transcript::Cancelled {
                        text: "locally recovered before cancellation".to_owned(),
                        completed: 1,
                        total: 3,
                    })
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
    fn completed_empty_local_retry_replaces_cloud_error_without_resolving_audio() {
        let recording = Recording::new(8_000);
        let outcome = recording.recognize(
            &cloud_config(),
            vec![
                Err("remote transcription endpoint returned HTTP 429".to_owned()),
                Ok(stt::Transcript::Complete(String::new())),
            ],
        );
        assert_eq!(outcome.text.as_deref(), Ok(""));
        assert!(!outcome.partial);
        assert_eq!(record(&outcome)["stt"]["backend"], "local");
        assert_eq!(record(&outcome)["recovery"]["retry_incomplete"], true);
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
