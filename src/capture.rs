//! Audio capture through PipeWire's `pw-record` command.

use anyhow::{anyhow, bail, Context, Result};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const STOP_TIMEOUT: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const NO_SIGNAL_GRACE: Duration = Duration::from_secs(3);
/// `20 * log10(32 / i16::MAX)` is approximately -60 dBFS.
const SIGNAL_FLOOR: u16 = 32;
const WAV_HEADER_SCAN_LIMIT: usize = 4_096;
/// 16 kHz mono PCM samples in the newest fixed 200 ms monitoring window.
const SIGNAL_WINDOW_SAMPLES: usize = 3_200;
const SIGNAL_WINDOW_BYTES: u64 = (SIGNAL_WINDOW_SAMPLES * std::mem::size_of::<i16>()) as u64;
/// Chronological min/max buckets in each daemon-owned 200 ms sample window.
pub const AUDIO_WAVEFORM_BINS: usize = 11;
pub(crate) type InputWaveform = [[i8; 2]; AUDIO_WAVEFORM_BINS];

/// A running `pw-record` process and its output path.
///
/// `stop` and `cancel` consume the recorder. If the recorder is dropped any
/// other way (daemon shutdown, panic), `Drop` stops the child and removes the
/// partial recording.
pub struct Recorder {
    child: Child,
    wav_path: PathBuf,
    started_at: Instant,
    signal_monitor: SignalMonitor,
    signal_monitor_warned: bool,
    disarmed: bool,
}

impl Recorder {
    /// Start a 16 kHz, mono, signed 16-bit WAV recording.
    pub fn start(wav_path: &Path, source: Option<&str>) -> Result<Self> {
        let args = pw_record_args(wav_path, source);
        let child = Command::new("pw-record")
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("starting pw-record for recording {}", wav_path.display()))?;
        let started_at = Instant::now();

        tracing::info!("[Capture] recording started");
        Ok(Self {
            child,
            wav_path: wav_path.to_path_buf(),
            started_at,
            signal_monitor: SignalMonitor::new(started_at),
            signal_monitor_warned: false,
            disarmed: false,
        })
    }
    /// Measure the newest PCM appended by `pw-record`.
    ///
    /// Monitoring is deliberately best-effort: a missing or unfamiliar WAV
    /// header removes the visual meter but never interrupts capture.
    pub(crate) fn input_signal(&mut self) -> Option<InputSignal> {
        match self.signal_monitor.sample(&self.wav_path, Instant::now()) {
            Ok(signal) => signal,
            Err(error) => {
                if !self.signal_monitor_warned {
                    tracing::warn!("[Capture] input signal monitor unavailable: {error:#}");
                    self.signal_monitor_warned = true;
                }
                None
            }
        }
    }

    /// Stop the process cleanly and return the completed WAV path.
    pub fn stop(mut self) -> Result<PathBuf> {
        stop_child(&mut self.child).with_context(|| "stopping pw-record")?;
        verify_wav(&self.wav_path)?;
        self.disarmed = true;
        tracing::info!(
            "[Capture] recording stopped after {} ms",
            self.started_at.elapsed().as_millis()
        );
        Ok(std::mem::take(&mut self.wav_path))
    }

    /// Stop the process and remove its partial recording.
    pub fn cancel(mut self) -> Result<()> {
        self.disarmed = true;
        let elapsed = self.started_at.elapsed();
        let stop_result = stop_child(&mut self.child);
        if let Err(error) = stop_result {
            if let Err(remove_error) = remove_recording(&self.wav_path) {
                tracing::warn!(
                    "[Capture] failed to remove canceled recording {}: {}",
                    self.wav_path.display(),
                    remove_error
                );
            }
            return Err(error).context("canceling pw-record");
        }
        remove_recording(&self.wav_path)?;

        tracing::info!(
            "[Capture] recording canceled after {} ms",
            elapsed.as_millis()
        );
        Ok(())
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }
        tracing::warn!("[Capture] recorder dropped while running; stopping pw-record");
        if stop_child(&mut self.child).is_err() {
            let _ = unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGKILL) };
            let _ = self.child.wait();
        }
        if let Err(error) = remove_recording(&self.wav_path) {
            tracing::warn!(
                "[Capture] failed to remove abandoned recording {}: {}",
                self.wav_path.display(),
                error
            );
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct InputSignal {
    /// Peak level for the newest samples, mapped logarithmically to 0..=100.
    pub(crate) level: u8,
    /// Chronological signed min/max levels downsampled from the same PCM
    /// window. Each bin is `[minimum, maximum]` on a -100..=100 scale.
    pub(crate) waveform: InputWaveform,
    /// True after PCM has remained at or below approximately -60 dBFS for
    /// `NO_SIGNAL_GRACE`.
    pub(crate) silent: bool,
}

struct SignalMonitor {
    file: Option<File>,
    data_offset: Option<u64>,
    cursor: u64,
    trailing_byte: Option<u8>,
    last_signal_at: Instant,
}

impl SignalMonitor {
    fn new(started_at: Instant) -> Self {
        Self {
            file: None,
            data_offset: None,
            cursor: 0,
            trailing_byte: None,
            last_signal_at: started_at,
        }
    }

    fn sample(&mut self, path: &Path, now: Instant) -> Result<Option<InputSignal>> {
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
        let mut minima = [i16::MAX; AUDIO_WAVEFORM_BINS];
        let mut maxima = [i16::MIN; AUDIO_WAVEFORM_BINS];
        let mut peak = 0_u16;
        let mut sample_index = 0_usize;
        let mut observe = |sample: i16| {
            peak = peak.max(sample.unsigned_abs());
            if total_samples == 0 {
                return;
            }
            let bucket =
                (sample_index * AUDIO_WAVEFORM_BINS / total_samples).min(AUDIO_WAVEFORM_BINS - 1);
            minima[bucket] = minima[bucket].min(sample);
            maxima[bucket] = maxima[bucket].max(sample);
            sample_index += 1;
        };

        let mut remaining = available;
        let mut buffer = [0_u8; 8_192];
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
        let waveform = std::array::from_fn(|index| {
            if minima[index] == i16::MAX {
                [0, 0]
            } else {
                [signed_level(minima[index]), signed_level(maxima[index])]
            }
        });
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
fn signed_level(sample: i16) -> i8 {
    let level = peak_level(sample.unsigned_abs()) as i8;
    if sample < 0 {
        -level
    } else {
        level
    }
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

fn pw_record_args(wav_path: &Path, source: Option<&str>) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("--rate"),
        OsString::from("16000"),
        OsString::from("--channels"),
        OsString::from("1"),
        OsString::from("--format"),
        OsString::from("s16"),
    ];
    if let Some(source) = source {
        args.push(OsString::from("--target"));
        args.push(OsString::from(source));
    }
    args.push(wav_path.as_os_str().to_owned());
    args
}

fn stop_child(child: &mut Child) -> Result<()> {
    if let Some(status) = child.try_wait().context("checking pw-record state")? {
        let stderr = read_stderr(child)?;
        tracing::debug!(
            "[Capture] pw-record already exited before SIGINT: {status}; stderr: {}",
            display_stderr(&stderr)
        );
        return Ok(());
    }

    send_sigint(child).or_else(|signal_error| {
        if let Some(status) = child
            .try_wait()
            .context("checking pw-record after SIGINT failure")?
        {
            let stderr = read_stderr(child)?;
            tracing::debug!(
                "[Capture] pw-record already exited before SIGINT: {status}; stderr: {}",
                display_stderr(&stderr)
            );
            return Ok(());
        }
        Err(signal_error)
    })?;

    if let Some(status) = wait_for_exit(child)? {
        // pw-record exits with status 1 on SIGINT by design (verified against
        // PipeWire 1.5.85); the WAV is still finalized. Exit status is not a
        // success signal here — verify_wav() on the produced file is.
        if !status.success() {
            let stderr = read_stderr(child)?;
            tracing::debug!(
                "[Capture] pw-record exit after SIGINT: {status}; stderr: {}",
                display_stderr(&stderr)
            );
        }
        return Ok(());
    }

    let pid = child.id();
    let kill_result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    let kill_error = if kill_result == -1 {
        Some(std::io::Error::last_os_error())
    } else {
        None
    };
    let status = child
        .wait()
        .context("waiting for pw-record after SIGKILL")?;
    let stderr = read_stderr(child)?;
    if let Some(error) = kill_error {
        bail!(
            "pw-record did not stop within {} seconds; SIGKILL failed: {error}; status {status}; stderr: {}",
            STOP_TIMEOUT.as_secs(),
            display_stderr(&stderr)
        );
    }
    bail!(
        "pw-record did not stop within {} seconds; sent SIGKILL; status {status}; stderr: {}",
        STOP_TIMEOUT.as_secs(),
        display_stderr(&stderr)
    );
}

fn send_sigint(child: &Child) -> Result<()> {
    let pid = child.id();
    let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGINT) };
    if result == -1 {
        return Err(anyhow!(std::io::Error::last_os_error()))
            .with_context(|| format!("sending SIGINT to pw-record process {pid}"));
    }
    Ok(())
}

/// Return `Some(status)` when the process exits before the timeout.
fn wait_for_exit(child: &mut Child) -> Result<Option<ExitStatus>> {
    let deadline = Instant::now() + STOP_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().context("waiting for pw-record")? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn read_stderr(child: &mut Child) -> Result<String> {
    let Some(mut stderr) = child.stderr.take() else {
        return Ok(String::new());
    };
    let mut output = String::new();
    stderr
        .read_to_string(&mut output)
        .context("reading pw-record stderr")?;
    Ok(output.trim().to_owned())
}

fn display_stderr(stderr: &str) -> &str {
    if stderr.is_empty() {
        "(no stderr output)"
    } else {
        stderr
    }
}

fn verify_wav(path: &Path) -> Result<()> {
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

pub(crate) fn remove_recording(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing recording {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        peak_level, pw_record_args, SignalMonitor, AUDIO_WAVEFORM_BINS, SIGNAL_WINDOW_SAMPLES,
    };
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    fn strings(args: Vec<std::ffi::OsString>) -> Vec<String> {
        args.into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }
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
        assert!(active.waveform.iter().flatten().any(|level| *level > 0));
        assert!(active.waveform.iter().flatten().any(|level| *level < 0));
        assert!(!active.silent);

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
        assert!(restored.waveform.iter().flatten().any(|level| *level < 0));
        assert!(
            !restored.silent,
            "signal must clear the warning immediately"
        );
        drop(file);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn live_signal_downsamples_chronological_min_max_bins() {
        let samples: Vec<i16> = (1..=AUDIO_WAVEFORM_BINS)
            .flat_map(|index| {
                let amplitude = i16::try_from(index * 2_000).expect("test amplitude fits i16");
                [-amplitude, amplitude]
            })
            .collect();
        let path = wav_path("envelope");
        fs::write(&path, wav(&samples)).expect("write envelope WAV");
        let started = Instant::now();
        let signal = SignalMonitor::new(started)
            .sample(&path, started + Duration::from_secs(1))
            .expect("sample envelope WAV")
            .expect("WAV header is ready");

        assert!(
            signal
                .waveform
                .iter()
                .all(|[minimum, maximum]| *minimum == -*maximum),
            "each bin must preserve both measured PCM edges"
        );
        assert!(
            signal
                .waveform
                .windows(2)
                .all(|pair| pair[0][1] < pair[1][1]),
            "bins must remain chronological"
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn live_signal_discards_backlog_before_newest_fixed_window() {
        let path = wav_path("backlog");
        fs::write(&path, wav(&[1_000, -1_000])).expect("write initial WAV");
        let started = Instant::now();
        let mut monitor = SignalMonitor::new(started);
        monitor
            .sample(&path, started + Duration::from_secs(1))
            .expect("sample initial WAV")
            .expect("WAV header is ready");

        let mut backlog = vec![i16::MAX; SIGNAL_WINDOW_SAMPLES];
        backlog.extend(std::iter::repeat_n(0, SIGNAL_WINDOW_SAMPLES));
        let bytes: Vec<u8> = backlog.into_iter().flat_map(i16::to_le_bytes).collect();
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open WAV append");
        file.write_all(&bytes)
            .expect("append stalled-reader backlog");

        let newest = monitor
            .sample(&path, started + Duration::from_secs(4))
            .expect("sample newest fixed window")
            .expect("WAV header remains ready");
        assert_eq!(newest.level, 0, "older loud PCM must not affect the peak");
        assert_eq!(
            newest.waveform,
            [[0, 0]; AUDIO_WAVEFORM_BINS],
            "older loud PCM must not affect the envelope"
        );
        assert!(
            newest.silent,
            "silence timing must follow the newest fixed window"
        );
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

    #[test]
    fn pw_record_args_without_source() {
        assert_eq!(
            strings(pw_record_args(Path::new("/tmp/recording.wav"), None)),
            vec![
                "--rate",
                "16000",
                "--channels",
                "1",
                "--format",
                "s16",
                "/tmp/recording.wav"
            ]
        );
    }

    #[test]
    fn pw_record_args_with_source() {
        assert_eq!(
            strings(pw_record_args(
                Path::new("/tmp/recording.wav"),
                Some("alsa_input.pci-1"),
            )),
            vec![
                "--rate",
                "16000",
                "--channels",
                "1",
                "--format",
                "s16",
                "--target",
                "alsa_input.pci-1",
                "/tmp/recording.wav"
            ]
        );
    }
}
