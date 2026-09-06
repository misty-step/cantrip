//! Local Parakeet inference and bounded OpenAI-compatible WAV transcription.
//! Native capture uses low-energy splits; remote chunks retain source audio
//! frames and format rather than resampling or quantizing the recording.
use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use transcribe_rs::onnx::parakeet::{ParakeetModel, ParakeetParams};
use transcribe_rs::onnx::Quantization;

const REMOTE_TIMEOUT: Duration = Duration::from_secs(60);
const MULTIPART_BOUNDARY: &str = "cantrip-audio-boundary";
/// Includes multipart framing and vocabulary, not just the WAV payload.
const MAX_REMOTE_REQUEST_BYTES: usize = 24_000_000;
const SAMPLE_RATE: f32 = 16_000.0;
/// Target chunk length for local Parakeet. Longer single-pass audio has
/// crashed the ONNX encoder (~400s failed; ~180s previously worked). Stay
/// well under that cliff with energy-based splits near this target.
const LOCAL_CHUNK_SECS: f32 = 30.0;
/// Search window around the target for a low-energy split point.
const LOCAL_CHUNK_SEARCH_SECS: f32 = 3.0;
/// Minimum residual kept as its own chunk.
const LOCAL_MIN_CHUNK_SECS: f32 = 0.5;

/// Progress of a multi-chunk transcription (1-based index).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkProgress {
    pub index: u32,
    pub total: u32,
}

/// STT outcome: full text, or partial text when a later chunk failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transcript {
    Complete(String),
    /// Chunks before the failure produced text; remaining audio was skipped.
    Partial {
        text: String,
        failed_at: u32,
        total: u32,
    },
}

impl Transcript {
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
    ) -> Result<Transcript> {
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
    ) -> Result<Transcript> {
        collect_chunks(
            &plan_chunks(samples),
            SAMPLE_RATE,
            on_progress,
            |start, end| self.transcribe_chunk(&samples[start..end]),
        )
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

/// Share ordering, measured progress, and partial failure semantics between
/// local inference and remote requests. Empty audio never invokes the backend.
fn collect_chunks(
    ranges: &[(usize, usize)],
    sample_rate: f32,
    on_progress: &mut impl FnMut(ChunkProgress),
    mut transcribe_chunk: impl FnMut(usize, usize) -> Result<String>,
) -> Result<Transcript> {
    let total = u32::try_from(ranges.len()).context("too many transcription chunks")?;
    let mut parts = Vec::with_capacity(ranges.len());
    for (index, &(start, end)) in ranges.iter().enumerate() {
        let progress = ChunkProgress {
            index: index as u32 + 1,
            total,
        };
        on_progress(progress);
        tracing::info!(
            "[STT] chunk={}/{} start_s={:.2} duration_s={:.2}",
            progress.index,
            progress.total,
            start as f32 / sample_rate,
            (end - start) as f32 / sample_rate
        );
        match transcribe_chunk(start, end) {
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
                    parts.iter().map(|part| part.chars().count()).sum::<usize>()
                );
                return Ok(Transcript::Partial {
                    text: parts.join(" "),
                    failed_at: progress.index,
                    total,
                });
            }
            Err(error) => return Err(error),
        }
    }
    Ok(Transcript::Complete(parts.join(" ")))
}

/// Plan inclusive-exclusive sample ranges for energy-adaptive chunks.
fn plan_chunks(samples: &[f32]) -> Vec<(usize, usize)> {
    plan_chunks_with_limit(samples, usize::MAX)
}

fn plan_chunks_with_limit(samples: &[f32], max_frames: usize) -> Vec<(usize, usize)> {
    let chunk_len = ((LOCAL_CHUNK_SECS * SAMPLE_RATE) as usize).min(max_frames);
    let min_len = ((LOCAL_MIN_CHUNK_SECS * SAMPLE_RATE) as usize).min(chunk_len);
    if samples.is_empty() {
        return Vec::new();
    }
    if samples.len() <= chunk_len {
        return vec![(0, samples.len())];
    }

    let mut ranges = Vec::with_capacity(samples.len().div_ceil(chunk_len));
    let mut start = 0;
    while start < samples.len() {
        let remaining = samples.len() - start;
        let end = if remaining <= chunk_len {
            samples.len()
        } else {
            let target = start + chunk_len;
            let limit = start.saturating_add(max_frames).min(samples.len());
            let split = low_energy_split(&samples[..limit], target, LOCAL_CHUNK_SEARCH_SECS);
            split.max(start + min_len).min(limit)
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

/// Transcribe a WAV through ordered, bounded OpenAI-compatible requests.
///
/// PCM and IEEE-float WAVs retain their source frames and format. Native
/// 16 kHz mono PCM16 uses the local low-energy planner; other formats split on
/// frame boundaries without decoding. A short bounded file is sent unchanged.
pub fn transcribe_remote(
    wav: &Path,
    endpoint: &str,
    model: &str,
    vocabulary: &[String],
    api_key: Option<&str>,
    mut on_progress: impl FnMut(ChunkProgress),
) -> Result<Transcript> {
    let mut source =
        RemoteWav::open(wav).with_context(|| format!("reading WAV {}", wav.display()))?;
    if source.frames == 0 {
        return Ok(Transcript::Complete(String::new()));
    }
    let mut body = build_multipart_prefix(model, vocabulary);
    let prefix_len = body.len();
    let suffix_len = MULTIPART_BOUNDARY.len() + 8;
    let wav_budget = MAX_REMOTE_REQUEST_BYTES
        .checked_sub(prefix_len + suffix_len)
        .context("remote transcription fields exceed the upload size limit")?;
    let max_frames = wav_budget
        .checked_sub(source.header_len() + 1)
        .context("WAV format header exceeds the remote upload size limit")?
        / source.frame_bytes;
    anyhow::ensure!(
        max_frames > 0,
        "WAV frame exceeds the remote upload size limit"
    );

    let short_frames = source.sample_rate as usize * LOCAL_CHUNK_SECS as usize;
    let unchanged = source.frames <= short_frames && source.file_len <= wav_budget as u64;
    let ranges = if unchanged {
        vec![(0, source.frames)]
    } else if source.native_capture {
        // The installed helper only accepts this exact native format. It is
        // used for planning, never to re-encode the source samples.
        let samples = transcribe_rs::audio::read_wav_samples(wav)
            .with_context(|| format!("reading WAV {} for chunk planning", wav.display()))?;
        plan_chunks_with_limit(&samples, max_frames)
    } else {
        let chunk_frames = short_frames.min(max_frames);
        (0..source.frames)
            .step_by(chunk_frames)
            .map(|start| (start, (start + chunk_frames).min(source.frames)))
            .collect()
    };
    let largest_wav = if unchanged {
        source.file_len as usize
    } else {
        let frames = ranges
            .iter()
            .map(|(start, end)| end - start)
            .max()
            .unwrap_or(0);
        source.header_len() + frames * source.frame_bytes + 1
    };
    body.reserve(largest_wav + suffix_len);

    let endpoint = format!("{}/audio/transcriptions", endpoint.trim_end_matches('/'));
    let content_type = format!("multipart/form-data; boundary={MULTIPART_BOUNDARY}");
    let agent = ureq::AgentBuilder::new().timeout(REMOTE_TIMEOUT).build();
    let mut request = agent.post(&endpoint).set("Content-Type", &content_type);
    if let Some(api_key) = api_key {
        request = request.set("Authorization", &format!("Bearer {api_key}"));
    }
    collect_chunks(
        &ranges,
        source.sample_rate as f32,
        &mut on_progress,
        |start, end| {
            body.truncate(prefix_len);
            source
                .append_chunk(&mut body, start, end, unchanged)
                .with_context(|| format!("reading WAV {} chunk", wav.display()))?;
            let audio_bytes = body.len() - prefix_len;
            body.extend_from_slice(b"\r\n--");
            body.extend_from_slice(MULTIPART_BOUNDARY.as_bytes());
            body.extend_from_slice(b"--\r\n");
            anyhow::ensure!(
                body.len() <= MAX_REMOTE_REQUEST_BYTES,
                "remote transcription upload exceeds its size limit"
            );

            let started = Instant::now();
            let response = match request.clone().send_bytes(&body) {
                Ok(response) => response,
                Err(ureq::Error::Status(413, _)) => {
                    anyhow::bail!(
                        "remote transcription endpoint rejected upload size {} bytes (HTTP 413)",
                        body.len()
                    );
                }
                Err(ureq::Error::Status(code, _)) => {
                    anyhow::bail!("remote transcription endpoint returned HTTP {code}");
                }
                Err(ureq::Error::Transport(transport)) => {
                    // Transport display strings may embed server-controlled
                    // headers or URLs. Keep only the kind and timeout cause.
                    let mut cause = std::error::Error::source(&transport);
                    while let Some(error) = cause {
                        if error
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|error| error.kind() == std::io::ErrorKind::TimedOut)
                        {
                            anyhow::bail!("remote transcription request timed out");
                        }
                        cause = error.source();
                    }
                    anyhow::bail!(
                        "remote transcription request failed ({:?})",
                        transport.kind()
                    );
                }
            };
            let response: RemoteTranscriptionResponse =
                serde_json::from_reader(response.into_reader()).map_err(|_| {
                    anyhow::anyhow!("remote transcription returned unexpected response shape")
                })?;
            let text = response.text.trim().to_owned();
            tracing::info!(
                "[STT] remote transcription audio_bytes={} ms={} output_char_count={}",
                audio_bytes,
                started.elapsed().as_millis(),
                text.chars().count()
            );
            Ok(text)
        },
    )
}

/// Metadata is small; audio stays in the source file until a bounded request
/// buffer is filled. Native low-energy planning still allocates one f32 per
/// source frame through transcribe-rs.
struct RemoteWav {
    file: File,
    file_len: u64,
    format: Vec<u8>,
    data_start: u64,
    frames: usize,
    frame_bytes: usize,
    sample_rate: u32,
    float: bool,
    native_capture: bool,
}

impl RemoteWav {
    fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path).context("opening source WAV")?;
        let file_len = file.metadata().context("reading WAV size")?.len();
        let mut header = [0_u8; 12];
        file.read_exact(&mut header)
            .context("reading RIFF header")?;
        anyhow::ensure!(
            &header[..4] == b"RIFF" && &header[8..] == b"WAVE",
            "expected a RIFF/WAVE file"
        );
        let riff_end = u64::from(u32::from_le_bytes(header[4..8].try_into()?)) + 8;
        anyhow::ensure!(
            (12..=file_len).contains(&riff_end),
            "WAV RIFF length exceeds the file or omits the WAVE header"
        );

        let mut format = None;
        let mut data = None;
        let mut position = 12;
        while position < riff_end {
            anyhow::ensure!(riff_end - position >= 8, "truncated WAV chunk header");
            let mut chunk = [0_u8; 8];
            file.read_exact(&mut chunk)
                .context("reading WAV chunk header")?;
            let size = u32::from_le_bytes(chunk[4..].try_into()?) as u64;
            let start = position + 8;
            let end = start + size + size % 2;
            anyhow::ensure!(end <= riff_end, "WAV chunk or padding exceeds RIFF length");
            match &chunk[..4] {
                b"fmt " => {
                    anyhow::ensure!(format.is_none(), "multiple WAV format chunks");
                    anyhow::ensure!(
                        (16..MAX_REMOTE_REQUEST_BYTES as u64).contains(&size),
                        "WAV format chunk is too short or exceeds the upload size limit"
                    );
                    let mut bytes = vec![0; size as usize];
                    file.read_exact(&mut bytes).context("reading WAV format")?;
                    format = Some(bytes);
                }
                b"data" => {
                    anyhow::ensure!(data.is_none(), "multiple WAV data chunks are unsupported");
                    data = Some((start, size as usize));
                }
                _ => {}
            }
            file.seek(SeekFrom::Start(end))
                .context("seeking WAV chunk")?;
            position = end;
        }
        let format = format.context("WAV has no format chunk")?;
        let (data_start, data_len) = data.context("WAV has no data chunk")?;
        let mut encoding = u16::from_le_bytes(format[..2].try_into()?);
        let channels = u16::from_le_bytes(format[2..4].try_into()?);
        let sample_rate = u32::from_le_bytes(format[4..8].try_into()?);
        let byte_rate = u32::from_le_bytes(format[8..12].try_into()?);
        let frame_bytes = u16::from_le_bytes(format[12..14].try_into()?) as usize;
        let bits = u16::from_le_bytes(format[14..16].try_into()?);
        let mut valid_bits = bits;
        if format.len() != 16 {
            anyhow::ensure!(format.len() >= 18, "truncated WAV format extension");
            let extension_len = u16::from_le_bytes(format[16..18].try_into()?) as usize;
            anyhow::ensure!(
                18 + extension_len <= format.len(),
                "truncated WAV format extension"
            );
            if encoding == 0xfffe {
                anyhow::ensure!(extension_len >= 22, "truncated extensible WAV format");
                let declared_bits = u16::from_le_bytes(format[18..20].try_into()?);
                anyhow::ensure!(declared_bits <= bits, "WAV valid bits exceed the container");
                if declared_bits > 0 {
                    valid_bits = declared_bits;
                }
                anyhow::ensure!(
                    format[26..40] == [0, 0, 0, 0, 16, 0, 128, 0, 0, 170, 0, 56, 155, 113],
                    "unsupported extensible WAV encoding"
                );
                encoding = u16::from_le_bytes(format[24..26].try_into()?);
            }
        }
        anyhow::ensure!(
            encoding == 1 || encoding == 3,
            "unsupported WAV encoding 0x{encoding:04x}; bounded uploads require PCM or IEEE float"
        );
        anyhow::ensure!(
            channels > 0
                && sample_rate > 0
                && frame_bytes > 0
                && frame_bytes.is_multiple_of(channels as usize)
                && bits > 0
                && usize::from(bits) <= frame_bytes / channels as usize * 8,
            "invalid WAV sample rate, channels, or block alignment"
        );
        anyhow::ensure!(
            encoding != 3
                || (matches!(bits, 32 | 64)
                    && valid_bits == bits
                    && usize::from(bits) == frame_bytes / channels as usize * 8),
            "IEEE-float WAV requires packed 32-bit or 64-bit samples",
        );
        anyhow::ensure!(
            u64::from(byte_rate) == u64::from(sample_rate) * frame_bytes as u64,
            "WAV byte rate does not match its sample frames"
        );
        anyhow::ensure!(
            data_len.is_multiple_of(frame_bytes),
            "WAV data ends inside a sample frame"
        );
        Ok(Self {
            file,
            file_len,
            format,
            data_start,
            frames: data_len / frame_bytes,
            frame_bytes,
            sample_rate,
            float: encoding == 3,
            native_capture: encoding == 1
                && channels == 1
                && frame_bytes == 2
                && sample_rate == SAMPLE_RATE as u32
                && bits == 16
                && valid_bits == 16,
        })
    }

    fn header_len(&self) -> usize {
        12 + 8 + self.format.len() + self.format.len() % 2 + if self.float { 12 } else { 0 } + 8
    }

    fn append_chunk(
        &mut self,
        body: &mut Vec<u8>,
        start: usize,
        end: usize,
        unchanged: bool,
    ) -> Result<()> {
        let (offset, bytes) = if unchanged {
            (0, self.file_len as usize)
        } else {
            let bytes = (end - start) * self.frame_bytes;
            let file_len = self.header_len() + bytes + bytes % 2;
            body.extend_from_slice(b"RIFF");
            body.extend_from_slice(&((file_len - 8) as u32).to_le_bytes());
            body.extend_from_slice(b"WAVEfmt ");
            body.extend_from_slice(&(self.format.len() as u32).to_le_bytes());
            body.extend_from_slice(&self.format);
            if self.format.len() % 2 == 1 {
                body.push(0);
            }
            if self.float {
                body.extend_from_slice(b"fact");
                body.extend_from_slice(&4_u32.to_le_bytes());
                body.extend_from_slice(&((end - start) as u32).to_le_bytes());
            }
            body.extend_from_slice(b"data");
            body.extend_from_slice(&(bytes as u32).to_le_bytes());
            (self.data_start + (start * self.frame_bytes) as u64, bytes)
        };
        self.file
            .seek(SeekFrom::Start(offset))
            .context("seeking WAV audio")?;
        let begin = body.len();
        body.resize(begin + bytes, 0);
        self.file
            .read_exact(&mut body[begin..])
            .context("reading WAV audio")?;
        if !unchanged && bytes % 2 == 1 {
            body.push(0);
        }
        Ok(())
    }
}

/// Classify an STT failure into a short operator-facing notice.
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
    if lower.contains("http 413") {
        return "Transcription upload too large";
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

fn build_multipart_prefix(model: &str, vocabulary: &[String]) -> Vec<u8> {
    let fields_len = model.len() + vocabulary.iter().map(|word| word.len() + 2).sum::<usize>();
    let mut body = Vec::with_capacity(fields_len + 512);
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
