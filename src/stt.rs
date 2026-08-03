//! Parakeet speech-to-text through the documented `transcribe-rs` API.
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::Instant;
use transcribe_rs::onnx::parakeet::{ParakeetModel, ParakeetParams};
use transcribe_rs::onnx::Quantization;
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
