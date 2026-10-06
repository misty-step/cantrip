//! Shared PCM monitoring and retained recording operations.

use anyhow::{bail, Context, Result};
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use std::fs::{self, File};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{Duration, Instant};

const NO_SIGNAL_GRACE: Duration = Duration::from_secs(3);
/// `20 * log10(32 / i16::MAX)` is approximately -60 dBFS.
const SIGNAL_FLOOR: u16 = 32;
const WAV_HEADER_SCAN_LIMIT: usize = 4_096;
/// 16 kHz mono PCM samples in the newest at-most-100 ms monitoring window.
const SIGNAL_WINDOW_SAMPLES: usize = 1_600;
const SIGNAL_WINDOW_BYTES: u64 = (SIGNAL_WINDOW_SAMPLES * std::mem::size_of::<i16>()) as u64;
/// Chronological raw PCM min/max buckets in each fresh monitoring window.
pub const AUDIO_WAVEFORM_BINS: usize = 60;
pub type InputWaveform = [[i16; 2]; AUDIO_WAVEFORM_BINS];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputSignal {
    /// Peak level for the newest samples, mapped logarithmically to 0..=100.
    pub level: u8,
    /// Chronological `[minimum, maximum]` raw signed 16-bit PCM samples from
    /// the same fresh window, capped at 100 ms. Empty bins contain `[0, 0]`.
    pub waveform: InputWaveform,
    /// True after PCM has remained at or below approximately -60 dBFS for
    /// `NO_SIGNAL_GRACE`.
    pub silent: bool,
}

pub struct SignalMonitor {
    file: Option<File>,
    data_offset: Option<u64>,
    cursor: u64,
    trailing_byte: Option<u8>,
    last_signal_at: Instant,
}

impl SignalMonitor {
    pub fn new(started_at: Instant) -> Self {
        Self {
            file: None,
            data_offset: None,
            cursor: 0,
            trailing_byte: None,
            last_signal_at: started_at,
        }
    }

    pub fn sample(&mut self, path: &Path, now: Instant) -> Result<Option<InputSignal>> {
        if self.file.is_none() {
            match File::open(path) {
                Ok(file) => self.file = Some(file),
                Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("opening live WAV {}", path.display()));
                }
            }
        }

        if self.data_offset.is_none() {
            let file = self
                .file
                .as_mut()
                .context("signal monitor file was not initialized")?;
            let Some(offset) = find_live_wav_data(file)? else {
                return Ok(None);
            };
            self.data_offset = Some(offset);
            self.cursor = offset;
        }

        let data_offset = self
            .data_offset
            .context("signal monitor data offset was not initialized")?;
        let file = self
            .file
            .as_mut()
            .context("signal monitor file was not initialized")?;
        let file_len = file
            .metadata()
            .context("checking live WAV sample length")?
            .len();
        let pcm_bytes = file_len.saturating_sub(data_offset);
        let complete_pcm_end = data_offset + pcm_bytes - pcm_bytes % 2;
        let window_start = complete_pcm_end
            .saturating_sub(SIGNAL_WINDOW_BYTES)
            .max(data_offset);
        if self.cursor < window_start {
            self.cursor = window_start;
            self.trailing_byte = None;
        }
        let available = file_len.saturating_sub(self.cursor);
        file.seek(SeekFrom::Start(self.cursor))
            .context("seeking live WAV samples")?;

        let available = usize::try_from(available)
            .context("live WAV sample window does not fit memory size")?;
        let total_samples = (available + usize::from(self.trailing_byte.is_some())) / 2;
        let mut waveform = [[i16::MAX, i16::MIN]; AUDIO_WAVEFORM_BINS];
        let mut peak = 0_u16;
        let mut sample_index = 0_usize;
        let mut observe = |sample: i16| {
            peak = peak.max(sample.unsigned_abs());
            if total_samples == 0 {
                return;
            }
            let bucket =
                (sample_index * AUDIO_WAVEFORM_BINS / total_samples).min(AUDIO_WAVEFORM_BINS - 1);
            let extremes = &mut waveform[bucket];
            extremes[0] = extremes[0].min(sample);
            extremes[1] = extremes[1].max(sample);
            sample_index += 1;
        };

        let mut remaining = available;
        let mut buffer = [0_u8; SIGNAL_WINDOW_BYTES as usize + 1];
        while remaining > 0 {
            let request = remaining.min(buffer.len());
            let read = file
                .read(&mut buffer[..request])
                .context("reading live WAV samples")?;
            if read == 0 {
                break;
            }
            remaining -= read;
            self.cursor += read as u64;

            let mut index = 0;
            if let Some(low) = self.trailing_byte.take() {
                observe(i16::from_le_bytes([low, buffer[0]]));
                index = 1;
            }
            while index + 1 < read {
                observe(i16::from_le_bytes([buffer[index], buffer[index + 1]]));
                index += 2;
            }
            if index < read {
                self.trailing_byte = Some(buffer[index]);
            }
        }

        if peak > SIGNAL_FLOOR {
            self.last_signal_at = now;
        }
        let level = if sample_index > 0 {
            peak_level(peak)
        } else {
            0
        };
        for extremes in &mut waveform {
            // An inverted range is empty; either PCM extreme is valid data.
            if extremes[0] > extremes[1] {
                *extremes = [0, 0];
            }
        }
        Ok(Some(InputSignal {
            level,
            waveform,
            silent: now.duration_since(self.last_signal_at) >= NO_SIGNAL_GRACE,
        }))
    }
}

/// Locate the PCM payload without assuming a 44-byte WAV header. `pw-record`
/// writes a normal RIFF/WAVE stream and updates its chunk sizes while capture
/// is live; only the stable chunk layout matters here.
fn find_live_wav_data(file: &mut File) -> Result<Option<u64>> {
    file.seek(SeekFrom::Start(0))
        .context("seeking live WAV header")?;
    let mut header = [0_u8; WAV_HEADER_SCAN_LIMIT];
    let read = file.read(&mut header).context("reading live WAV header")?;
    if read < 12 {
        return Ok(None);
    }
    if &header[..4] != b"RIFF" || &header[8..12] != b"WAVE" {
        bail!("live recording is not RIFF/WAVE");
    }

    let mut cursor = 12_usize;
    while cursor + 8 <= read {
        let size = u32::from_le_bytes([
            header[cursor + 4],
            header[cursor + 5],
            header[cursor + 6],
            header[cursor + 7],
        ]) as usize;
        let payload = cursor + 8;
        if &header[cursor..cursor + 4] == b"data" {
            return Ok(Some(payload as u64));
        }
        let padded = size
            .checked_add(size & 1)
            .and_then(|value| payload.checked_add(value))
            .context("live WAV chunk size overflow")?;
        if padded > read {
            return Ok(None);
        }
        cursor = padded;
    }
    Ok(None)
}
fn peak_level(peak: u16) -> u8 {
    if peak <= SIGNAL_FLOOR {
        return 0;
    }
    let dbfs = 20.0 * (f32::from(peak) / f32::from(i16::MAX)).log10();
    (((dbfs + 60.0) / 60.0).clamp(0.0, 1.0) * 100.0)
        .ceil()
        .clamp(1.0, 100.0) as u8
}

pub fn verify_wav(path: &Path) -> Result<()> {
    let metadata =
        fs::metadata(path).with_context(|| format!("checking recorded WAV {}", path.display()))?;
    if metadata.len() <= 44 {
        bail!(
            "recorded WAV {} is {} bytes; expected more than 44 bytes",
            path.display(),
            metadata.len()
        );
    }
    Ok(())
}

pub fn remove_recording(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing recording {}", path.display())),
    }
}

/// The on-disk and inference format, independent of a device's hardware rate.
pub const PCM_SAMPLE_RATE: u32 = 16_000;
const CONVERSION_FRAMES: usize = 1_024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PcmEncoding {
    Float32,
    Float64,
    Signed16,
    Signed32,
}

impl PcmEncoding {
    pub fn bytes_per_sample(self) -> usize {
        match self {
            Self::Float32 | Self::Signed32 => 4,
            Self::Float64 => 8,
            Self::Signed16 => 2,
        }
    }

    fn decode(self, bytes: &[u8]) -> f64 {
        match self {
            Self::Float32 => f32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f64,
            Self::Float64 => f64::from_ne_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ]),
            Self::Signed16 => i16::from_ne_bytes([bytes[0], bytes[1]]) as f64 / 32_768.0,
            Self::Signed32 => {
                i32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f64
                    / 2_147_483_648.0
            }
        }
    }
}

/// A packed transport block has either one interleaved plane or one plane per
/// channel. Planar planes are adjacent and each contains exactly `frames`
/// samples; hardware buffer capacity/padding never enters the transport.
#[derive(Clone, Copy, Debug)]
pub struct PcmFormat {
    pub sample_rate: f64,
    pub channels: usize,
    pub encoding: PcmEncoding,
    pub interleaved: bool,
}

impl PcmFormat {
    pub fn validate(self) -> Result<()> {
        if !self.sample_rate.is_finite() || !(8_000.0..=384_000.0).contains(&self.sample_rate) {
            bail!("microphone sample rate is unsupported");
        }
        if !(1..=32).contains(&self.channels) {
            bail!("microphone channel count is unsupported");
        }
        Ok(())
    }

    pub fn bytes_per_frame(self) -> usize {
        self.channels * self.encoding.bytes_per_sample()
    }
}

/// Streaming, anti-aliased device PCM -> mono PCM16 conversion. Buffers and the
/// sinc filter are allocated once, outside the realtime callback. Input
/// boundaries do not reset phase/filter history, and final padding flushes only
/// the real take's duration (never synthetic trailing audio).
pub struct Pcm16Converter {
    format: PcmFormat,
    resampler: Option<SincFixedIn<f32>>,
    input: [Vec<f32>; 1],
    input_used: usize,
    output: [Vec<f32>; 1],
    pcm: Vec<i16>,
    delay_remaining: usize,
    input_frames: u64,
    output_frames: u64,
    finished: bool,
}

impl Pcm16Converter {
    pub fn new(format: PcmFormat) -> Result<Self> {
        format.validate()?;
        let resampler = if format.sample_rate == f64::from(PCM_SAMPLE_RATE) {
            None
        } else {
            Some(
                SincFixedIn::<f32>::new(
                    f64::from(PCM_SAMPLE_RATE) / format.sample_rate,
                    1.0,
                    SincInterpolationParameters {
                        sinc_len: 256,
                        f_cutoff: 0.95,
                        interpolation: SincInterpolationType::Cubic,
                        oversampling_factor: 256,
                        window: WindowFunction::BlackmanHarris2,
                    },
                    CONVERSION_FRAMES,
                    1,
                )
                .context("preparing microphone sample rate conversion")?,
            )
        };
        let output_capacity = resampler
            .as_ref()
            .map_or(CONVERSION_FRAMES, |resampler| resampler.output_frames_max());
        let delay_remaining = resampler
            .as_ref()
            .map_or(0, |resampler| resampler.output_delay());
        Ok(Self {
            format,
            resampler,
            input: [vec![0.0; CONVERSION_FRAMES]],
            input_used: 0,
            output: [vec![0.0; output_capacity]],
            pcm: vec![0; output_capacity],
            delay_remaining,
            input_frames: 0,
            output_frames: 0,
            finished: false,
        })
    }

    pub fn write_pcm<W: Write + Seek>(
        &mut self,
        bytes: &[u8],
        frames: usize,
        writer: &mut Pcm16WavWriter<W>,
    ) -> Result<()> {
        if self.finished {
            bail!("microphone conversion has already finished");
        }
        let expected = frames
            .checked_mul(self.format.bytes_per_frame())
            .context("microphone PCM block size overflow")?;
        if bytes.len() != expected {
            bail!("microphone PCM block has an incomplete frame");
        }
        let width = self.format.encoding.bytes_per_sample();
        for frame in 0..frames {
            let mut sum = 0.0_f64;
            for channel in 0..self.format.channels {
                let sample_index = if self.format.interleaved {
                    frame * self.format.channels + channel
                } else {
                    channel * frames + frame
                };
                let offset = sample_index * width;
                let sample = self.format.encoding.decode(&bytes[offset..offset + width]);
                if !sample.is_finite() {
                    bail!("microphone supplied non-finite PCM");
                }
                sum += sample;
            }
            let mono = (sum / self.format.channels as f64) as f32;
            if !mono.is_finite() {
                bail!("microphone PCM is outside the supported range");
            }
            self.input[0][self.input_used] = mono;
            self.input_used += 1;
            self.input_frames += 1;
            if self.input_used == CONVERSION_FRAMES {
                self.process_chunk(writer)?;
            }
        }
        Ok(())
    }

    fn desired_output_frames(&self) -> u64 {
        (self.input_frames as f64 * f64::from(PCM_SAMPLE_RATE) / self.format.sample_rate).floor()
            as u64
    }

    fn process_chunk<W: Write + Seek>(&mut self, writer: &mut Pcm16WavWriter<W>) -> Result<()> {
        let frames = if let Some(resampler) = &mut self.resampler {
            let (_, frames) = resampler
                .process_into_buffer(&self.input, &mut self.output, None)
                .context("converting microphone sample rate")?;
            frames
        } else {
            self.output[0][..self.input_used].copy_from_slice(&self.input[0][..self.input_used]);
            self.input_used
        };
        self.input_used = 0;
        let skip = self.delay_remaining.min(frames);
        self.delay_remaining -= skip;
        let remaining = self
            .desired_output_frames()
            .saturating_sub(self.output_frames);
        let count = (frames - skip).min(usize::try_from(remaining).unwrap_or(usize::MAX));
        for (sample, value) in self.pcm[..count]
            .iter_mut()
            .zip(&self.output[0][skip..skip + count])
        {
            if !value.is_finite() {
                bail!("microphone conversion produced non-finite PCM");
            }
            *sample = (value * 32_768.0)
                .round()
                .clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        }
        writer.write_samples(&self.pcm[..count])?;
        self.output_frames += count as u64;
        Ok(())
    }

    pub fn finish<W: Write + Seek>(&mut self, writer: &mut Pcm16WavWriter<W>) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        if self.resampler.is_none() {
            if self.input_used != 0 {
                self.process_chunk(writer)?;
            }
        } else {
            let desired = self.desired_output_frames();
            // One partial block plus enough bounded zero blocks for the 256
            // input-frame filter tail, even at the highest supported rate.
            for _ in 0..4 {
                if self.output_frames == desired {
                    break;
                }
                self.input[0][self.input_used..].fill(0.0);
                self.process_chunk(writer)?;
            }
            if self.output_frames != desired {
                bail!("microphone conversion did not flush the complete take");
            }
        }
        self.finished = true;
        Ok(())
    }
}

/// A monitorable 16 kHz mono PCM16 RIFF/WAVE stream. Checkpoints and finalization
/// update sizes in place; neither failure nor Drop ever truncates/removes the
/// original. A caller using a File must sync it after explicit finalization.
pub struct Pcm16WavWriter<W: Write + Seek> {
    writer: W,
    data_bytes: u64,
    finalized: bool,
}

impl<W: Write + Seek> Pcm16WavWriter<W> {
    pub fn new(mut writer: W) -> Result<Self> {
        let mut header = [0_u8; 44];
        header[..4].copy_from_slice(b"RIFF");
        header[4..8].copy_from_slice(&36_u32.to_le_bytes());
        header[8..16].copy_from_slice(b"WAVEfmt ");
        header[16..20].copy_from_slice(&16_u32.to_le_bytes());
        header[20..22].copy_from_slice(&1_u16.to_le_bytes());
        header[22..24].copy_from_slice(&1_u16.to_le_bytes());
        header[24..28].copy_from_slice(&PCM_SAMPLE_RATE.to_le_bytes());
        header[28..32].copy_from_slice(&(PCM_SAMPLE_RATE * 2).to_le_bytes());
        header[32..34].copy_from_slice(&2_u16.to_le_bytes());
        header[34..36].copy_from_slice(&16_u16.to_le_bytes());
        header[36..40].copy_from_slice(b"data");
        writer
            .write_all(&header)
            .context("writing microphone WAV header")?;
        writer.flush().context("flushing microphone WAV header")?;
        Ok(Self {
            writer,
            data_bytes: 0,
            finalized: false,
        })
    }

    pub fn write_samples(&mut self, samples: &[i16]) -> Result<()> {
        if self.finalized {
            bail!("microphone WAV has already been finalized");
        }
        let added = (samples.len() as u64)
            .checked_mul(2)
            .context("microphone WAV length overflow")?;
        if self.data_bytes.saturating_add(added) > u64::from(u32::MAX) - 36 {
            bail!("microphone take exceeds the RIFF/WAVE size limit");
        }
        let mut bytes = [0_u8; CONVERSION_FRAMES * 2];
        for chunk in samples.chunks(CONVERSION_FRAMES) {
            for (destination, sample) in bytes.as_chunks_mut::<2>().0.iter_mut().zip(chunk) {
                *destination = sample.to_le_bytes();
            }
            self.writer
                .write_all(&bytes[..chunk.len() * 2])
                .context("writing microphone PCM")?;
            self.data_bytes += (chunk.len() * 2) as u64;
        }
        Ok(())
    }

    pub fn checkpoint(&mut self) -> Result<()> {
        let end = self
            .writer
            .seek(SeekFrom::End(0))
            .context("locating microphone WAV end")?;
        let data_bytes = end
            .checked_sub(44)
            .context("microphone WAV header is incomplete")?;
        if data_bytes % 2 != 0 || data_bytes > u64::from(u32::MAX) - 36 {
            bail!("microphone WAV contains an incomplete or oversized PCM payload");
        }
        self.writer
            .seek(SeekFrom::Start(4))
            .context("seeking microphone WAV size")?;
        self.writer
            .write_all(&(36 + data_bytes as u32).to_le_bytes())
            .context("updating microphone WAV size")?;
        self.writer
            .seek(SeekFrom::Start(40))
            .context("seeking microphone PCM size")?;
        self.writer
            .write_all(&(data_bytes as u32).to_le_bytes())
            .context("updating microphone PCM size")?;
        self.writer
            .seek(SeekFrom::Start(end))
            .context("restoring microphone WAV cursor")?;
        self.writer.flush().context("flushing microphone WAV")?;
        self.data_bytes = data_bytes;
        Ok(())
    }

    pub fn finish(&mut self) -> Result<()> {
        if !self.finalized {
            self.checkpoint()?;
            self.finalized = true;
        }
        Ok(())
    }

    pub fn get_ref(&self) -> &W {
        &self.writer
    }
}

impl<W: Write + Seek> Drop for Pcm16WavWriter<W> {
    fn drop(&mut self) {
        if self.finish().is_err() {
            tracing::warn!("[Capture] retained WAV finalization failed class=storage-failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        peak_level, Pcm16Converter, Pcm16WavWriter, PcmEncoding, PcmFormat, SignalMonitor,
        AUDIO_WAVEFORM_BINS,
    };
    use std::fs::{self, File, OpenOptions};
    use std::io::{Cursor, Seek, SeekFrom, Write};
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    fn wav_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "cantrip-signal-test-{}-{name}.wav",
            std::process::id()
        ))
    }

    fn wav(samples: &[i16]) -> Vec<u8> {
        let data_len = (samples.len() * 2) as u32;
        let mut bytes = Vec::with_capacity(44 + data_len as usize);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&16_000_u32.to_le_bytes());
        bytes.extend_from_slice(&32_000_u32.to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }
    #[test]
    fn live_signal_tracks_peak_and_sustained_digital_silence() {
        let path = wav_path("silence");
        fs::write(&path, wav(&[0, 16_384, -8_192])).expect("write initial WAV");
        let started = Instant::now();
        let mut monitor = SignalMonitor::new(started);

        let active = monitor
            .sample(&path, started + Duration::from_secs(1))
            .expect("sample active WAV")
            .expect("WAV header is ready");
        assert!(active.level >= 85, "half-scale PCM should render high");
        let mut expected = [[0, 0]; AUDIO_WAVEFORM_BINS];
        expected[AUDIO_WAVEFORM_BINS / 3] = [16_384, 16_384];
        expected[2 * AUDIO_WAVEFORM_BINS / 3] = [-8_192, -8_192];
        assert_eq!(active.waveform, expected);
        assert!(!active.silent);

        let no_fresh_data = monitor
            .sample(&path, started + Duration::from_secs(2))
            .expect("sample unchanged WAV")
            .expect("WAV header remains ready");
        assert_eq!(no_fresh_data.level, 0);
        assert_eq!(no_fresh_data.waveform, [[0, 0]; AUDIO_WAVEFORM_BINS]);
        assert!(!no_fresh_data.silent);

        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open WAV append");
        file.write_all(&[0; 64]).expect("append digital silence");
        let silent = monitor
            .sample(&path, started + Duration::from_secs(4))
            .expect("sample silent WAV")
            .expect("WAV header remains ready");
        assert_eq!(silent.level, 0);
        assert_eq!(silent.waveform, [[0, 0]; AUDIO_WAVEFORM_BINS]);
        assert!(silent.silent, "three seconds without signal must warn");

        file.write_all(&(-1_000_i16).to_le_bytes())
            .expect("append restored signal");
        let restored = monitor
            .sample(&path, started + Duration::from_secs(5))
            .expect("sample restored WAV")
            .expect("WAV header remains ready");
        assert!(restored.level > 0);
        assert_eq!(restored.waveform[0], [-1_000, -1_000]);
        assert!(
            !restored.silent,
            "signal must clear the warning immediately"
        );
        drop(file);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn live_signal_downsamples_independent_raw_min_max_bins() {
        let expected = std::array::from_fn(|index| match index {
            0 => [i16::MAX, i16::MAX],
            1 => [i16::MIN, i16::MIN],
            2 => [i16::MIN, i16::MAX],
            _ => {
                let index = i16::try_from(index).expect("test bucket fits i16");
                [-index * 300, index * 500]
            }
        });
        let samples: Vec<i16> = expected
            .iter()
            .flat_map(|[minimum, maximum]| [*maximum, *minimum])
            .collect();
        let path = wav_path("raw-extrema");
        fs::write(&path, wav(&samples)).expect("write raw extrema WAV");
        let started = Instant::now();
        let signal = SignalMonitor::new(started)
            .sample(&path, started + Duration::from_secs(1))
            .expect("sample raw extrema WAV")
            .expect("WAV header is ready");

        assert_eq!(
            signal.waveform, expected,
            "bins must retain their own measured extrema without mixing neighbors"
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn live_signal_caps_backlog_at_100_ms_and_preserves_split_samples() {
        let path = wav_path("backlog");
        let mut initial = wav(&[1_000, -1_000]);
        initial.push(i16::MAX.to_le_bytes()[0]);
        fs::write(&path, initial).expect("write initial WAV with split sample");
        let started = Instant::now();
        let mut monitor = SignalMonitor::new(started);
        monitor
            .sample(&path, started + Duration::from_secs(1))
            .expect("sample initial WAV")
            .expect("WAV header is ready");

        // Complete the old split sample, then append 100 ms each of loud PCM
        // and silence. Only the newest 1,600 complete samples may be observed.
        let mut bytes = vec![i16::MAX.to_le_bytes()[1]];
        bytes.extend(std::iter::repeat_n(i16::MAX, 1_600).flat_map(i16::to_le_bytes));
        bytes.extend_from_slice(&[0; 3_200]);
        let split_sample = (-12_345_i16).to_le_bytes();
        bytes.push(split_sample[0]);
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open WAV append");
        file.write_all(&bytes)
            .expect("append stalled-reader backlog with split sample");

        let newest = monitor
            .sample(&path, started + Duration::from_secs(4))
            .expect("sample newest 100 ms window")
            .expect("WAV header remains ready");
        assert_eq!(newest.level, 0, "older loud PCM must not affect the peak");
        assert_eq!(
            newest.waveform,
            [[0, 0]; AUDIO_WAVEFORM_BINS],
            "older loud PCM and its trailing byte must not affect the waveform"
        );
        assert!(
            newest.silent,
            "silence timing must follow the newest fixed window"
        );

        let no_fresh_data = monitor
            .sample(&path, started + Duration::from_millis(4_500))
            .expect("sample unchanged WAV with pending byte")
            .expect("WAV header remains ready");
        assert_eq!(no_fresh_data.level, 0);
        assert_eq!(no_fresh_data.waveform, [[0, 0]; AUDIO_WAVEFORM_BINS]);
        assert!(no_fresh_data.silent);

        file.write_all(&split_sample[1..])
            .expect("complete newest split sample");
        let completed = monitor
            .sample(&path, started + Duration::from_secs(5))
            .expect("sample completed split sample")
            .expect("WAV header remains ready");
        let mut expected = [[0, 0]; AUDIO_WAVEFORM_BINS];
        expected[0] = [-12_345, -12_345];
        assert_eq!(completed.waveform, expected);
        assert!(completed.level > 0);
        assert!(!completed.silent);
        drop(file);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn peak_level_reserves_zero_for_near_digital_silence() {
        assert_eq!(peak_level(0), 0);
        assert_eq!(peak_level(32), 0);
        assert!(peak_level(33) > 0);
        assert_eq!(peak_level(i16::MAX as u16), 100);
    }

    fn read_converted(writer: &Pcm16WavWriter<Cursor<Vec<u8>>>) -> Vec<i16> {
        let mut wav = hound::WavReader::new(Cursor::new(writer.get_ref().get_ref()))
            .expect("real WAV consumer accepts finalized output");
        assert_eq!(wav.spec().sample_rate, 16_000);
        assert_eq!(wav.spec().channels, 1);
        assert_eq!(wav.spec().bits_per_sample, 16);
        assert_eq!(wav.spec().sample_format, hound::SampleFormat::Int);
        wav.samples().collect::<Result<Vec<i16>, _>>().unwrap()
    }

    fn convert_float(samples: &[f32], rate: f64, block_frames: usize) -> Vec<i16> {
        let format = PcmFormat {
            sample_rate: rate,
            channels: 1,
            encoding: PcmEncoding::Float32,
            interleaved: true,
        };
        let mut converter = Pcm16Converter::new(format).unwrap();
        let mut writer = Pcm16WavWriter::new(Cursor::new(Vec::new())).unwrap();
        for block in samples.chunks(block_frames) {
            let bytes: Vec<u8> = block.iter().flat_map(|value| value.to_ne_bytes()).collect();
            converter
                .write_pcm(&bytes, block.len(), &mut writer)
                .unwrap();
        }
        converter.finish(&mut writer).unwrap();
        writer.finish().unwrap();
        read_converted(&writer)
    }

    fn rms(samples: &[i16]) -> f64 {
        (samples
            .iter()
            .map(|sample| f64::from(*sample).powi(2))
            .sum::<f64>()
            / samples.len() as f64)
            .sqrt()
    }

    #[test]
    fn resampling_preserves_duration_signal_and_phase_across_transport_boundaries() {
        for rate in [8_000.0, 16_000.0, 44_100.0, 48_000.0, 96_000.0, 192_000.0] {
            let frames = rate as usize / 5 + 17;
            let samples: Vec<f32> = (0..frames)
                .map(|index| {
                    (0.25 * (std::f64::consts::TAU * 1_000.0 * index as f64 / rate).sin()) as f32
                })
                .collect();
            let complete = convert_float(&samples, rate, frames);
            let fragmented = convert_float(&samples, rate, 137);
            assert_eq!(
                complete, fragmented,
                "transport boundaries must not reset the filter at {rate}"
            );
            assert_eq!(
                complete.len(),
                (frames as f64 * 16_000.0 / rate).floor() as usize
            );
            let measured = rms(&complete[128..complete.len() - 128]);
            assert!(
                (5_200.0..6_200.0).contains(&measured),
                "speech-band signal must survive conversion at {rate}, rms={measured}"
            );
        }
    }

    #[test]
    fn downsampling_rejects_hardware_frequencies_above_the_inference_nyquist_limit() {
        let samples: Vec<f32> = (0..48_000)
            .map(|index| {
                (0.8 * (std::f64::consts::TAU * 12_000.0 * index as f64 / 48_000.0).sin()) as f32
            })
            .collect();
        let converted = convert_float(&samples, 48_000.0, 769);
        assert_eq!(converted.len(), 16_000);
        assert!(
            rms(&converted[128..converted.len() - 128]) < 32.0,
            "out-of-band energy must not alias into speech"
        );
    }

    #[test]
    fn supported_hardware_encodings_and_layouts_downmix_signed_pcm_correctly() {
        let left = [-1.0_f64, 0.5, 1.0, 0.25];
        let right = [-1.0_f64, -0.25, 1.0, -0.25];
        for encoding in [
            PcmEncoding::Float32,
            PcmEncoding::Float64,
            PcmEncoding::Signed16,
            PcmEncoding::Signed32,
        ] {
            for interleaved in [false, true] {
                let values: Vec<f64> = if interleaved {
                    left.iter()
                        .zip(right)
                        .flat_map(|(left, right)| [*left, right])
                        .collect()
                } else {
                    left.into_iter().chain(right).collect()
                };
                let mut bytes = Vec::new();
                for value in values {
                    match encoding {
                        PcmEncoding::Float32 => {
                            bytes.extend_from_slice(&(value as f32).to_ne_bytes())
                        }
                        PcmEncoding::Float64 => bytes.extend_from_slice(&value.to_ne_bytes()),
                        PcmEncoding::Signed16 => bytes.extend_from_slice(
                            &((value * 32_768.0).clamp(i16::MIN as f64, i16::MAX as f64) as i16)
                                .to_ne_bytes(),
                        ),
                        PcmEncoding::Signed32 => bytes.extend_from_slice(
                            &((value * 2_147_483_648.0).clamp(i32::MIN as f64, i32::MAX as f64)
                                as i32)
                                .to_ne_bytes(),
                        ),
                    }
                }
                let format = PcmFormat {
                    sample_rate: 16_000.0,
                    channels: 2,
                    encoding,
                    interleaved,
                };
                let mut converter = Pcm16Converter::new(format).unwrap();
                let mut writer = Pcm16WavWriter::new(Cursor::new(Vec::new())).unwrap();
                converter
                    .write_pcm(&bytes, left.len(), &mut writer)
                    .unwrap();
                converter.finish(&mut writer).unwrap();
                writer.finish().unwrap();
                assert_eq!(
                    read_converted(&writer),
                    [i16::MIN, 4_096, i16::MAX, 0],
                    "{encoding:?}, interleaved={interleaved}"
                );
            }
        }
    }

    #[test]
    fn malformed_or_nonfinite_pcm_fails_without_replacing_the_accepted_prefix() {
        let format = PcmFormat {
            sample_rate: 16_000.0,
            channels: 1,
            encoding: PcmEncoding::Float32,
            interleaved: true,
        };
        let mut converter = Pcm16Converter::new(format).unwrap();
        let mut writer = Pcm16WavWriter::new(Cursor::new(Vec::new())).unwrap();
        assert!(converter.write_pcm(&[0; 3], 1, &mut writer).is_err());
        let bytes: Vec<u8> = [0.25_f32, f32::NAN]
            .into_iter()
            .flat_map(f32::to_ne_bytes)
            .collect();
        assert!(converter.write_pcm(&bytes, 2, &mut writer).is_err());
        converter.finish(&mut writer).unwrap();
        writer.finish().unwrap();
        assert_eq!(
            read_converted(&writer),
            [8_192],
            "accepted audio survives; invalid samples are not converted to synthetic silence"
        );
    }

    #[test]
    fn tiny_and_exact_boundary_takes_flush_once_without_padding_the_duration() {
        for frames in [0, 1, 2, 3, 1_023, 1_024, 1_025, 2_048] {
            let output = convert_float(&vec![0.25; frames], 48_000.0, 137);
            assert_eq!(
                output.len(),
                frames / 3,
                "tail and filter delay must not add time to {frames} real frames"
            );
        }
        let format = PcmFormat {
            sample_rate: 16_000.0,
            channels: 1,
            encoding: PcmEncoding::Signed16,
            interleaved: true,
        };
        let mut converter = Pcm16Converter::new(format).unwrap();
        let mut writer = Pcm16WavWriter::new(Cursor::new(Vec::new())).unwrap();
        converter
            .write_pcm(&123_i16.to_ne_bytes(), 1, &mut writer)
            .unwrap();
        converter.finish(&mut writer).unwrap();
        converter.finish(&mut writer).unwrap();
        assert!(converter
            .write_pcm(&456_i16.to_ne_bytes(), 1, &mut writer)
            .is_err());
        writer.finish().unwrap();
        writer.finish().unwrap();
        assert_eq!(read_converted(&writer), [123]);
    }

    #[test]
    fn live_wav_remains_monitorable_and_drop_finalizes_the_retained_file() {
        let path = wav_path("native-writer-drop");
        let file = File::create(&path).unwrap();
        let started = Instant::now();
        let mut monitor = SignalMonitor::new(started);
        {
            let mut writer = Pcm16WavWriter::new(file).unwrap();
            writer.write_samples(&[16_384, -8_192]).unwrap();
            let signal = monitor
                .sample(&path, started + Duration::from_secs(1))
                .unwrap()
                .unwrap();
            assert!(
                signal.level >= 85,
                "live metering must not depend on final header sizes"
            );
            writer.checkpoint().unwrap();
            writer.write_samples(&[i16::MIN, i16::MAX]).unwrap();
        }
        let mut wav = hound::WavReader::open(&path).expect("drop finalized the real retained WAV");
        assert_eq!(
            wav.samples::<i16>().collect::<Result<Vec<_>, _>>().unwrap(),
            [16_384, -8_192, i16::MIN, i16::MAX]
        );
        drop(wav);
        drop(monitor);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn finalization_failure_leaves_the_original_audio_bytes_for_recovery() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        struct InterruptedStorage {
            file: File,
            interrupted: Arc<AtomicBool>,
        }
        impl Write for InterruptedStorage {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.file.write(bytes)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.file.flush()
            }
        }
        impl Seek for InterruptedStorage {
            fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
                if self.interrupted.load(Ordering::Acquire) {
                    return Err(std::io::Error::other(
                        "storage interrupted during WAV finalization",
                    ));
                }
                self.file.seek(position)
            }
        }

        let path = wav_path("native-writer-failure");
        let interrupted = Arc::new(AtomicBool::new(false));
        {
            let storage = InterruptedStorage {
                file: File::create(&path).unwrap(),
                interrupted: Arc::clone(&interrupted),
            };
            let mut writer = Pcm16WavWriter::new(storage).unwrap();
            writer.write_samples(&[1_234, -5_678]).unwrap();
            interrupted.store(true, Ordering::Release);
            assert!(writer.finish().is_err());
        }
        let retained =
            fs::read(&path).expect("failed finalization never removes the only recording");
        assert_eq!(
            &retained[44..],
            &[1_234_i16, -5_678]
                .into_iter()
                .flat_map(i16::to_le_bytes)
                .collect::<Vec<_>>()
        );
        fs::remove_file(path).unwrap();
    }
}
