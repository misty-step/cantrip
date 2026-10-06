//! PipeWire capture; SIGINT and reaping finalize the original recording.

use anyhow::{anyhow, bail, Context, Result};
use cantrip_engine::audio::{verify_wav, InputSignal, SignalMonitor};
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const STOP_TIMEOUT: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// A running `pw-record` process and its output path.
///
/// `stop` consumes the recorder and transfers its completed WAV path. Dropping
/// an abandoned capture also stops the child, retaining any available audio
/// for startup recovery even when finalization fails.
pub struct Recorder {
    child: Child,
    wav_path: PathBuf,
    started_at: Instant,
    signal_monitor: SignalMonitor,
    signal_monitor_warned: bool,
    disarmed: bool,
    stop_requested: bool,
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
            stop_requested: false,
        })
    }
    /// Measure the newest PCM appended by `pw-record`.
    ///
    /// Monitoring is deliberately best-effort: a missing or unfamiliar WAV
    /// header removes the visual meter but never interrupts capture.
    pub fn input_signal(&mut self) -> Option<InputSignal> {
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

    /// Request capture termination immediately; the worker still owns reaping
    /// and WAV finalization. Repeated requests never interrupt finalization.
    /// The WAV is always retained, including when a queued worker disappears.
    pub fn request_stop(&mut self) -> Result<()> {
        self.signal_stop()
    }

    fn signal_stop(&mut self) -> Result<()> {
        if self.stop_requested {
            return Ok(());
        }
        if self
            .child
            .try_wait()
            .context("checking pw-record state")?
            .is_none()
        {
            send_sigint(&self.child).or_else(|signal_error| {
                if self
                    .child
                    .try_wait()
                    .context("checking pw-record after SIGINT failure")?
                    .is_some()
                {
                    Ok(())
                } else {
                    Err(signal_error)
                }
            })?;
        }
        self.stop_requested = true;
        Ok(())
    }

    fn finish_stop(&mut self) -> Result<()> {
        self.signal_stop()?;
        stop_child(&mut self.child)
    }

    /// Stop the process cleanly and return the completed WAV path.
    /// On error the caller still owns the WAV at the original path; cleanup must
    /// not erase the only recording before recovery can preserve it.
    pub fn stop(mut self) -> Result<PathBuf> {
        self.finish_stop().with_context(|| "stopping pw-record")?;
        verify_wav(&self.wav_path)?;
        self.disarmed = true;
        tracing::info!(
            "[Capture] recording stopped after {} ms",
            self.started_at.elapsed().as_millis()
        );
        Ok(std::mem::take(&mut self.wav_path))
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }
        tracing::warn!("[Capture] recorder dropped while running; stopping pw-record");
        if self.finish_stop().is_err() {
            let _ = unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGKILL) };
            let _ = self.child.wait();
        }
    }
}

impl cantrip_engine::ports::Recorder for Recorder {
    fn input_signal(&mut self) -> Option<InputSignal> {
        Recorder::input_signal(self)
    }

    fn request_stop(&mut self) -> Result<()> {
        Recorder::request_stop(self)
    }

    fn stop(self: Box<Self>) -> Result<PathBuf> {
        Recorder::stop(*self)
    }
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

#[cfg(test)]
mod tests {
    use cantrip_engine::audio::SignalMonitor;
    use std::fs;
    use std::io::Write;
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
    fn recorder_for_test(path: PathBuf) -> super::Recorder {
        let started = Instant::now();
        super::Recorder {
            child: std::process::Command::new("/usr/bin/true")
                .spawn()
                .expect("start bounded recorder fixture"),
            wav_path: path,
            started_at: started,
            signal_monitor: SignalMonitor::new(started),
            signal_monitor_warned: false,
            disarmed: false,
            stop_requested: false,
        }
    }

    #[test]
    fn failed_finalization_preserves_original_recording() {
        let path = wav_path("failed-finalization");
        let audio = b"unfinished WAV header";
        fs::write(&path, audio).expect("write incomplete recording");
        assert!(recorder_for_test(path.clone()).stop().is_err());
        assert_eq!(
            fs::read(&path).expect("recording survives failed stop"),
            audio
        );
        fs::remove_file(path).expect("remove retained fixture");
    }

    #[test]
    fn abandoning_capture_finalizes_and_preserves_audio() {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};
        use std::sync::mpsc;

        let path = wav_path("abandoned-capture");
        let finalized = wav_path("abandoned-capture-finalized");
        let audio = wav(&[1000, -1000]);
        fs::write(&path, b"unfinished WAV header").expect("write live recording");
        fs::write(&finalized, &audio).expect("write finalized recording fixture");
        let mut child = Command::new("/bin/sh")
            .args([
                "-c",
                "trap 'cat \"$1\" > \"$2\"; exit 0' INT; printf 'ready\\n'; while :; do read -r pending; done",
                "recorder-fixture",
            ])
            .arg(&finalized)
            .arg(&path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("start recorder finalization fixture");
        let stdout = child.stdout.take().expect("fixture output");
        let (sender, receiver) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut ready = String::new();
            BufReader::new(stdout).read_line(&mut ready).unwrap();
            let _ = sender.send(ready);
        });
        let started_at = Instant::now();
        let recorder = super::Recorder {
            child,
            wav_path: path.clone(),
            started_at,
            signal_monitor: SignalMonitor::new(started_at),
            signal_monitor_warned: false,
            disarmed: false,
            stop_requested: false,
        };
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
            "ready\n"
        );
        drop(recorder);
        reader.join().unwrap();
        assert_eq!(
            fs::read(&path).expect("finalized recording survives recorder drop"),
            audio
        );
        fs::remove_file(path).expect("remove retained fixture");
        fs::remove_file(finalized).expect("remove finalized fixture");
    }

    #[test]
    fn requested_stop_survives_abandoned_worker_queue() {
        let path = wav_path("abandoned-stopped-capture");
        let audio = wav(&[1000, -1000]);
        fs::write(&path, &audio).expect("write stopped recording");
        let mut recorder = recorder_for_test(path.clone());
        recorder.request_stop().expect("request capture stop");
        let (sender, receiver) = std::sync::mpsc::channel();
        assert!(sender.send(recorder).is_ok());
        drop(receiver);
        assert_eq!(
            fs::read(&path).expect("stopped recording survives worker disappearance"),
            audio
        );
        fs::remove_file(path).expect("remove retained fixture");
    }

    #[test]
    fn stop_request_signals_before_worker_finalizes() {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};
        use std::sync::mpsc;

        let path = wav_path("requested-stop");
        fs::write(&path, wav(&[1000, -1000])).expect("write recording");
        let mut child = Command::new("/bin/sh")
            .args(["-c", "trap 'printf \"stopped\\n\"; read -r release; exit 0' INT; printf 'ready\\n'; while :; do read -r pending; done"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("start recorder finalization fixture");
        let stdout = child.stdout.take().expect("fixture output");
        let (sender, receiver) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let started_at = Instant::now();
        let mut recorder = super::Recorder {
            child,
            wav_path: path.clone(),
            started_at,
            signal_monitor: SignalMonitor::new(started_at),
            signal_monitor_warned: false,
            disarmed: false,
            stop_requested: false,
        };
        assert_eq!(
            receiver
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap(),
            "ready"
        );
        recorder
            .request_stop()
            .expect("request without waiting for finalization");
        assert_eq!(
            receiver
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap(),
            "stopped"
        );
        assert!(
            recorder.child.try_wait().unwrap().is_none(),
            "worker has not finalized yet"
        );
        recorder.request_stop().expect("repeated stop request");
        recorder
            .child
            .stdin
            .take()
            .unwrap()
            .write_all(b"finish\n")
            .unwrap();
        assert_eq!(recorder.stop().expect("worker finishes recording"), path);
        reader.join().unwrap();
        fs::remove_file(path).unwrap();
    }
}
