//! Parakeet speech-to-text through the documented `transcribe-rs` API.
//!
//! Long dictations are split with energy-adaptive chunking before inference.
//! A single Parakeet encoder pass crashes past a few minutes of audio
//! (ONNX self-attention broadcast failure); chunking keeps each pass inside
//! a known-safe window.
use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
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
/// Minimum residual kept as its own chunk.
const LOCAL_MIN_CHUNK_SECS: f32 = 0.5;

/// Progress of a multi-chunk local transcription (1-based index).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkProgress {
    pub index: u32,
    pub total: u32,
}

/// Local STT outcome: full text, or partial text when a later chunk failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalTranscript {
    Complete(String),
    /// Chunks before the failure produced text; remaining audio was skipped.
    Partial {
        text: String,
        failed_at: u32,
        total: u32,
    },
}

impl LocalTranscript {
    pub fn text(&self) -> &str {
        match self {
            Self::Complete(text) | Self::Partial { text, .. } => text,
        }
    }

    pub fn is_partial(&self) -> bool {
        matches!(self, Self::Partial { .. })
    }
}

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

    pub fn transcribe_wav(
        &mut self,
        wav: &Path,
        mut on_progress: impl FnMut(ChunkProgress),
    ) -> Result<LocalTranscript> {
        let samples = transcribe_rs::audio::read_wav_samples(wav).with_context(|| {
            format!(
                "reading WAV {} with Parakeet model {}",
                wav.display(),
                self.model_dir.display()
            )
        })?;
        let audio_seconds = samples.len() as f64 / f64::from(SAMPLE_RATE);
        let started = Instant::now();
        let outcome = self
            .transcribe_samples(&samples, &mut on_progress)
            .with_context(|| {
                format!(
                    "transcribing WAV {} with Parakeet model {}",
                    wav.display(),
                    self.model_dir.display()
                )
            })?;
        tracing::info!(
            "[STT] audio_seconds={audio_seconds:.3} inference_ms={} output_char_count={} partial={}",
            started.elapsed().as_millis(),
            outcome.text().chars().count(),
            outcome.is_partial()
        );
        Ok(outcome)
    }

    /// Split long audio into energy-adaptive chunks so each Parakeet pass
    /// stays under the encoder cliff. Short audio still runs as one pass.
    /// On a mid-stream chunk failure, return any text already produced.
    fn transcribe_samples(
        &mut self,
        samples: &[f32],
        on_progress: &mut impl FnMut(ChunkProgress),
    ) -> Result<LocalTranscript> {
        let ranges = plan_chunks(samples);
        let total = ranges.len() as u32;
        if total == 0 {
            return Ok(LocalTranscript::Complete(String::new()));
        }

        let mut parts = Vec::new();
        for (index, &(start, end)) in ranges.iter().enumerate() {
            let chunk = &samples[start..end];
            let progress = ChunkProgress {
                index: (index as u32) + 1,
                total,
            };
            on_progress(progress);
            tracing::info!(
                "[STT] chunk={}/{} start_s={:.2} duration_s={:.2}",
                progress.index,
                progress.total,
                start as f32 / SAMPLE_RATE,
                chunk.len() as f32 / SAMPLE_RATE
            );
            match self.transcribe_chunk(chunk) {
                Ok(text) => {
                    if !text.is_empty() {
                        parts.push(text);
                    }
                }
                Err(error) if !parts.is_empty() => {
                    tracing::warn!(
                        "[STT] chunk {}/{} failed after partial text chars={} error={error:#}",
                        progress.index,
                        progress.total,
                        parts.iter().map(|p| p.chars().count()).sum::<usize>()
                    );
                    return Ok(LocalTranscript::Partial {
                        text: parts.join(" "),
                        failed_at: progress.index,
                        total: progress.total,
                    });
                }
                Err(error) => return Err(error),
            }
        }
        Ok(LocalTranscript::Complete(parts.join(" ")))
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

/// Plan inclusive-exclusive sample ranges for energy-adaptive chunks.
fn plan_chunks(samples: &[f32]) -> Vec<(usize, usize)> {
    let chunk_len = (LOCAL_CHUNK_SECS * SAMPLE_RATE) as usize;
    let min_len = (LOCAL_MIN_CHUNK_SECS * SAMPLE_RATE) as usize;
    if samples.is_empty() {
        return Vec::new();
    }
    if samples.len() <= chunk_len {
        return vec![(0, samples.len())];
    }

    let mut ranges = Vec::new();
    let mut start = 0;
    while start < samples.len() {
        let remaining = samples.len() - start;
        let end = if remaining <= chunk_len {
            samples.len()
        } else {
            let target = start + chunk_len;
            let split = low_energy_split(samples, target, LOCAL_CHUNK_SEARCH_SECS);
            split.max(start + min_len).min(samples.len())
        };
        ranges.push((start, end));
        start = end;
    }
    ranges
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

/// Read the duration from a RIFF/WAVE file without decoding or allocating its
/// audio payload. Unknown/non-RIFF inputs return an error while transcription
/// remains free to report its own format diagnostics.
pub fn wav_duration_ms(path: &Path) -> Result<u64> {
    let mut file = File::open(path).with_context(|| format!("opening WAV {}", path.display()))?;
    wav_duration_ms_from(&mut file)
        .with_context(|| format!("reading WAV duration {}", path.display()))
}

fn wav_duration_ms_from(reader: &mut (impl Read + Seek)) -> Result<u64> {
    let mut header = [0_u8; 12];
    reader
        .read_exact(&mut header)
        .context("reading RIFF header")?;
    if &header[0..4] != b"RIFF" || &header[8..12] != b"WAVE" {
        anyhow::bail!("not a RIFF/WAVE file");
    }

    let mut byte_rate = None;
    let mut data_bytes = None;
    loop {
        let mut chunk = [0_u8; 8];
        match reader.read_exact(&mut chunk) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error).context("reading WAV chunk header"),
        }
        let size = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]) as u64;
        match &chunk[0..4] {
            b"fmt " => {
                if size < 12 {
                    anyhow::bail!("WAV fmt chunk is too short");
                }
                let mut format = [0_u8; 12];
                reader
                    .read_exact(&mut format)
                    .context("reading WAV fmt chunk")?;
                byte_rate =
                    Some(u32::from_le_bytes([format[8], format[9], format[10], format[11]]) as u64);
                reader
                    .seek(SeekFrom::Current(
                        i64::try_from(size - 12).context("WAV chunk too large")?,
                    ))
                    .context("skipping WAV fmt extension")?;
            }
            b"data" => {
                data_bytes = Some(size);
                reader
                    .seek(SeekFrom::Current(
                        i64::try_from(size).context("WAV data too large")?,
                    ))
                    .context("skipping WAV data")?;
            }
            _ => {
                reader
                    .seek(SeekFrom::Current(
                        i64::try_from(size).context("WAV chunk too large")?,
                    ))
                    .context("skipping WAV chunk")?;
            }
        }
        if size % 2 == 1 {
            reader
                .seek(SeekFrom::Current(1))
                .context("skipping WAV chunk padding")?;
        }
        if byte_rate.is_some() && data_bytes.is_some() {
            break;
        }
    }

    let byte_rate = byte_rate
        .filter(|rate| *rate > 0)
        .context("WAV has no byte rate")?;
    let data_bytes = data_bytes.context("WAV has no data chunk")?;
    Ok(data_bytes.saturating_mul(1_000) / byte_rate)
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
    fn wav_duration_uses_data_size_and_byte_rate() {
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&32_036_u32.to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&16_000_u32.to_le_bytes());
        wav.extend_from_slice(&32_000_u32.to_le_bytes());
        wav.extend_from_slice(&2_u16.to_le_bytes());
        wav.extend_from_slice(&16_u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&32_000_u32.to_le_bytes());

        assert_eq!(
            wav_duration_ms_from(&mut std::io::Cursor::new(wav)).unwrap(),
            1_000
        );
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

    #[test]
    fn plan_chunks_splits_long_audio_and_keeps_short_as_one() {
        let short = vec![0.1_f32; 8_000]; // 0.5s
        assert_eq!(plan_chunks(&short), vec![(0, 8_000)]);

        let long = vec![0.1_f32; 160_000]; // 10s < 30s target still one chunk
        assert_eq!(plan_chunks(&long), vec![(0, 160_000)]);

        let very_long = vec![0.1_f32; 960_000]; // 60s -> two ~30s chunks
        let ranges = plan_chunks(&very_long);
        assert!(
            ranges.len() >= 2,
            "expected multiple chunks, got {ranges:?}"
        );
        assert_eq!(ranges.first().map(|r| r.0), Some(0));
        assert_eq!(ranges.last().map(|r| r.1), Some(very_long.len()));
        // Contiguous coverage with no gaps.
        for window in ranges.windows(2) {
            assert_eq!(window[0].1, window[1].0);
        }
    }
}
