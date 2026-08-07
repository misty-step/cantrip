//! Parakeet speech-to-text through the documented `transcribe-rs` API.
//!
//! Long dictations are split with energy-adaptive chunking before inference.
//! A single Parakeet encoder pass crashes past a few minutes of audio
//! (ONNX self-attention broadcast failure); chunking keeps each pass inside
//! a known-safe window.
use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use transcribe_rs::onnx::parakeet::{ParakeetModel, ParakeetParams};
use transcribe_rs::onnx::Quantization;

const REMOTE_TIMEOUT: Duration = Duration::from_secs(60);
const MULTIPART_BOUNDARY: &str = "cantrip-audio-boundary";
const SAMPLE_RATE: f32 = 16_000.0;
/// Target chunk length for local Parakeet. Longer single-pass audio has
/// crashed the ONNX encoder (~400s failed; ~180s previously worked). Stay
/// well under that cliff with energy-based splits near this target.
const LOCAL_CHUNK_SECS: f32 = 30.0;
/// Search window around the target for a low-energy split point.
const LOCAL_CHUNK_SEARCH_SECS: f32 = 3.0;
/// Minimum residual kept as its own chunk (shorter tails are dropped by the
/// energy-adaptive strategy's min_chunk_secs=0 default — keep a small floor).
const LOCAL_MIN_CHUNK_SECS: f32 = 0.5;

pub struct Transcriber {
    model: ParakeetModel,
    model_dir: PathBuf,
}

impl Transcriber {
    pub fn load(model_dir: &Path) -> Result<Self> {
        let started = Instant::now();
        let model_path = model_dir.to_path_buf();
        let model = ParakeetModel::load(&model_path, &Quantization::Int8)
            .with_context(|| format!("loading Parakeet model from {}", model_dir.display()))?;
        tracing::info!("[STT] model loaded in {} ms", started.elapsed().as_millis());
        Ok(Self {
            model,
            model_dir: model_path,
        })
    }

    pub fn transcribe_wav(&mut self, wav: &Path) -> Result<String> {
        let wav_path = wav.to_path_buf();
        let samples = transcribe_rs::audio::read_wav_samples(&wav_path).with_context(|| {
            format!(
                "reading WAV {} with Parakeet model {}",
                wav.display(),
                self.model_dir.display()
            )
        })?;
        let audio_seconds = samples.len() as f64 / f64::from(SAMPLE_RATE);
        let started = Instant::now();
        let text = self
            .transcribe_samples(&samples)
            .with_context(|| {
                format!(
                    "transcribing WAV {} with Parakeet model {}",
                    wav.display(),
                    self.model_dir.display()
                )
            })?
            .trim()
            .to_owned();
        tracing::info!(
            "[STT] audio_seconds={audio_seconds:.3} inference_ms={} output_char_count={}",
            started.elapsed().as_millis(),
            text.chars().count()
        );
        Ok(text)
    }

    /// Split long audio into energy-adaptive chunks so each Parakeet pass
    /// stays under the encoder cliff. Short audio still runs as one pass.
    /// Implemented here (not via transcribe-rs chunkers) so dependency
    /// `log::info` of chunk text cannot leak transcript content.
    fn transcribe_samples(&mut self, samples: &[f32]) -> Result<String> {
        let chunk_len = (LOCAL_CHUNK_SECS * SAMPLE_RATE) as usize;
        let min_len = (LOCAL_MIN_CHUNK_SECS * SAMPLE_RATE) as usize;
        if samples.len() <= chunk_len {
            return self.transcribe_chunk(samples);
        }

        let mut parts = Vec::new();
        let mut start = 0;
        let mut chunk_index = 0_u32;
        while start < samples.len() {
            let remaining = samples.len() - start;
            let end = if remaining <= chunk_len {
                samples.len()
            } else {
                let target = start + chunk_len;
                let split = low_energy_split(samples, target, LOCAL_CHUNK_SEARCH_SECS);
                split.max(start + min_len).min(samples.len())
            };
            let chunk = &samples[start..end];
            tracing::info!(
                "[STT] chunk={} start_s={:.2} duration_s={:.2}",
                chunk_index,
                start as f32 / SAMPLE_RATE,
                chunk.len() as f32 / SAMPLE_RATE
            );
            let text = self.transcribe_chunk(chunk)?;
            if !text.is_empty() {
                parts.push(text);
            }
            chunk_index += 1;
            start = end;
        }
        Ok(parts.join(" "))
    }

    fn transcribe_chunk(&mut self, samples: &[f32]) -> Result<String> {
        let result = self
            .model
            .transcribe_with(
                samples,
                &ParakeetParams {
                    ..Default::default()
                },
            )
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        Ok(result.text.trim().to_owned())
    }
}

/// Find a low-energy frame near `target` to avoid splitting mid-word.
fn low_energy_split(samples: &[f32], target: usize, search_secs: f32) -> usize {
    const FRAME: usize = 480;
    let search = (search_secs * SAMPLE_RATE) as usize;
    let start = target.saturating_sub(search);
    let end = (target + search).min(samples.len());
    let start = (start / FRAME) * FRAME;

    let mut best = target.min(samples.len());
    let mut best_rms = f32::MAX;
    let mut offset = start;
    while offset + FRAME <= end {
        let frame = &samples[offset..offset + FRAME];
        let rms = (frame.iter().map(|s| s * s).sum::<f32>() / FRAME as f32).sqrt();
        if rms < best_rms {
            best_rms = rms;
            best = offset + FRAME;
        }
        offset += FRAME;
    }
    best
}

#[derive(Debug, Deserialize)]
struct RemoteTranscriptionResponse {
    text: String,
}

/// Transcribe a WAV file through an OpenAI-compatible audio endpoint.
pub fn transcribe_remote(
    wav: &Path,
    endpoint: &str,
    model: &str,
    vocabulary: &[String],
    api_key: Option<&str>,
) -> Result<String> {
    let wav_bytes = fs::read(wav).with_context(|| format!("reading WAV {}", wav.display()))?;
    let body = build_multipart_body(&wav_bytes, model, vocabulary);
    let endpoint = format!("{}/audio/transcriptions", endpoint.trim_end_matches('/'));
    let content_type = format!("multipart/form-data; boundary={MULTIPART_BOUNDARY}");
    let agent = ureq::AgentBuilder::new().timeout(REMOTE_TIMEOUT).build();
    let mut request = agent.post(&endpoint).set("Content-Type", &content_type);
    if let Some(api_key) = api_key {
        request = request.set("Authorization", &format!("Bearer {api_key}"));
    }

    let started = Instant::now();
    let response = match request.send_bytes(&body) {
        Ok(response) => response,
        Err(ureq::Error::Status(code, _)) => {
            anyhow::bail!("remote transcription endpoint returned HTTP {code}");
        }
        Err(ureq::Error::Transport(transport)) => {
            anyhow::bail!("remote transcription request failed: {transport}");
        }
    };
    let response: RemoteTranscriptionResponse = serde_json::from_reader(response.into_reader())
        .map_err(|_| anyhow::anyhow!("remote transcription returned unexpected response shape"))?;
    let text = response.text.trim().to_owned();
    tracing::info!(
        "[STT] remote transcription audio_bytes={} ms={} output_char_count={}",
        wav_bytes.len(),
        started.elapsed().as_millis(),
        text.chars().count()
    );
    Ok(text)
}

/// Classify a local STT failure into a short operator-facing notice.
/// Never includes transcript content — structural causes only.
pub fn classify_failure(error: &str) -> &'static str {
    let lower = error.to_ascii_lowercase();
    // Parakeet ONNX encoder cliff (observed live as axis broadcast 77 by 5077).
    if lower.contains("broadcast") || lower.contains("axis ==") {
        return "Audio too long for the model";
    }
    if lower.contains("timed out") || lower.contains("timeout") {
        return "Transcription timed out";
    }
    // Match the ureq status form we emit: "returned HTTP {code}".
    if lower.contains("returned http ") || lower.contains("http 4") || lower.contains("http 5") {
        return "Transcription service error";
    }
    if lower.contains("reading wav") || lower.contains("failed to open") {
        return "Recording unreadable";
    }
    "Transcription failed"
}

fn build_multipart_body(wav_bytes: &[u8], model: &str, vocabulary: &[String]) -> Vec<u8> {
    let mut body = Vec::new();
    append_multipart_field(&mut body, "model", model);
    if !vocabulary.is_empty() {
        append_multipart_field(&mut body, "prompt", &vocabulary.join(", "));
    }
    append_multipart_field(&mut body, "response_format", "json");

    body.extend_from_slice(b"--");
    body.extend_from_slice(MULTIPART_BOUNDARY.as_bytes());
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\n",
    );
    body.extend_from_slice(b"Content-Type: audio/wav\r\n\r\n");
    body.extend_from_slice(wav_bytes);
    body.extend_from_slice(b"\r\n--");
    body.extend_from_slice(MULTIPART_BOUNDARY.as_bytes());
    body.extend_from_slice(b"--\r\n");
    body
}

fn append_multipart_field(body: &mut Vec<u8>, name: &str, value: &str) {
    body.extend_from_slice(b"--");
    body.extend_from_slice(MULTIPART_BOUNDARY.as_bytes());
    body.extend_from_slice(b"\r\nContent-Disposition: form-data; name=\"");
    body.extend_from_slice(name.as_bytes());
    body.extend_from_slice(b"\"\r\n\r\n");
    body.extend_from_slice(value.as_bytes());
    body.extend_from_slice(b"\r\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multipart_body_has_expected_framing_and_fields() {
        let wav = [0_u8, 1, 2, 255];
        let vocabulary = vec!["Cantrip".to_owned(), "Parakeet".to_owned()];
        let body = build_multipart_body(&wav, "whisper-large-v3", &vocabulary);
        let body_text = String::from_utf8_lossy(&body);

        assert!(body_text.starts_with("--cantrip-audio-boundary\r\n"));
        assert!(body_text.contains(
            "Content-Disposition: form-data; name=\"model\"\r\n\r\nwhisper-large-v3\r\n"
        ));
        assert!(body_text.contains(
            "Content-Disposition: form-data; name=\"prompt\"\r\n\r\nCantrip, Parakeet\r\n"
        ));
        assert!(body_text
            .contains("Content-Disposition: form-data; name=\"response_format\"\r\n\r\njson\r\n"));
        assert!(body_text.contains(
            "Content-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\nContent-Type: audio/wav\r\n\r\n"
        ));
        assert!(body.windows(wav.len()).any(|window| window == wav));
        assert!(body.ends_with(b"\r\n--cantrip-audio-boundary--\r\n"));
    }

    #[test]
    fn classify_failure_maps_known_causes() {
        assert_eq!(
            classify_failure(
                "inference error: Attempting to broadcast an axis by a dimension other than 1. 77 by 5077"
            ),
            "Audio too long for the model"
        );
        assert_eq!(
            classify_failure("remote transcription endpoint returned HTTP 403"),
            "Transcription service error"
        );
        assert_eq!(
            classify_failure("client request timed out"),
            "Transcription timed out"
        );
        assert_eq!(
            classify_failure("reading WAV /tmp/x.wav failed"),
            "Recording unreadable"
        );
        assert_eq!(classify_failure("something else"), "Transcription failed");
    }

    #[test]
    fn low_energy_split_prefers_quiet_frame_near_target() {
        // 2s of noise, 0.25s of silence around 1.0s, more noise.
        let mut samples = vec![0.2_f32; 32_000];
        for sample in &mut samples[15_000..19_000] {
            *sample = 0.0;
        }
        let split = low_energy_split(&samples, 16_000, 0.5);
        assert!(
            (14_500..=19_500).contains(&split),
            "split {split} should land in the quiet window"
        );
    }
}
