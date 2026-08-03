//! Parakeet speech-to-text through the documented `transcribe-rs` API.
use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use transcribe_rs::onnx::parakeet::{ParakeetModel, ParakeetParams};
use transcribe_rs::onnx::Quantization;

const REMOTE_TIMEOUT: Duration = Duration::from_secs(60);
const MULTIPART_BOUNDARY: &str = "cantrip-audio-boundary";

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
        let started = Instant::now();
        let result = self
            .model
            .transcribe_with(
                &samples,
                &ParakeetParams {
                    ..Default::default()
                },
            )
            .with_context(|| {
                format!(
                    "transcribing WAV {} with Parakeet model {}",
                    wav.display(),
                    self.model_dir.display()
                )
            })?;
        let text = result.text.trim().to_owned();
        tracing::info!(
            "[STT] audio_seconds={:.3} inference_ms={} output_char_count={}",
            samples.len() as f64 / 16_000.0,
            started.elapsed().as_millis(),
            text.chars().count()
        );
        Ok(text)
    }
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
}
