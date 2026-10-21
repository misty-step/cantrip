//! PipeWire capture; SIGINT and reaping finalize the original recording.

use anyhow::{anyhow, bail, Context, Result};
use cantrip_engine::audio::{verify_wav, InputSignal, SignalMonitor};
use cantrip_engine::ports::MissingMicrophone;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

// One budget from the first SIGINT, including time spent queued for the worker.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
const KILL_TIMEOUT: Duration = Duration::from_secs(2);
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
    stop_requested: Option<Instant>,
}

impl Recorder {
    /// Start a 16 kHz, mono, signed 16-bit WAV recording.
    pub fn start(wav_path: &Path, source: Option<&str>) -> Result<Self> {
        let source = source.map(check_source).transpose()?;
        let mut command = Command::new("pw-record");
        if let Some(source) = &source {
            // libpipewire applies PIPEWIRE_PROPS after CLI properties. Let
            // pw-record parse the inherited SPA syntax first, then enforce our
            // routing safeguards, so unrelated properties survive unchanged.
            if let Some(inherited) = std::env::var_os("PIPEWIRE_PROPS") {
                command.arg("--properties").arg(inherited);
            }
            command
                .args([
                    "--properties",
                    "{ node.dont-fallback = true node.dont-reconnect = true }",
                ])
                .env_remove("PIPEWIRE_PROPS")
                .env("PIPEWIRE_NODE", source);
        }
        let child = command
            .args(pw_record_args(wav_path, source.as_deref()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
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
            stop_requested: None,
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
        if self.stop_requested.is_some() {
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
        self.stop_requested = Some(Instant::now());
        Ok(())
    }

    fn finish_stop(&mut self) -> Result<()> {
        self.signal_stop()?;
        let deadline = self.stop_requested.expect("stop was requested") + STOP_TIMEOUT;
        let exit = stop_child(&mut self.child, deadline)?;
        // The child has been reaped. Never signal it again, even if repair fails.
        self.disarmed = true;
        if matches!(exit, RecorderExit::Forced) {
            finalize_wav_prefix(&self.wav_path)
                .context("finalizing interrupted pw-record audio")?;
            bail!("pw-record exceeded its stop budget; saved the accepted WAV prefix");
        }
        Ok(())
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

/// Resolve the same node name/object serial accepted by pw-record. The serial
/// pins this take to this device instance even if a node name is later reused.
pub fn check_source(source: &str) -> Result<String> {
    let mut child = Command::new("pw-dump")
        .arg("--no-colors")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("cannot inspect PipeWire microphones; check pw-dump and the PipeWire session")?;
    let mut stdout = child.stdout.take().expect("piped pw-dump stdout");
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                break match result {
                    Err(error) => Err(anyhow!(error)),
                    _ => Err(anyhow!("PipeWire microphone query timed out")),
                };
            }
        }
    };
    let bytes = reader
        .join()
        .map_err(|_| anyhow!("PipeWire microphone query reader failed"))??;
    if !status?.success() {
        bail!("cannot inspect PipeWire microphones; check the PipeWire session");
    }
    source_serial(
        source,
        &serde_json::from_slice::<Vec<serde_json::Value>>(&bytes)?,
    )
}

fn source_serial(source: &str, nodes: &[serde_json::Value]) -> Result<String> {
    for node in nodes {
        if node["type"] != "PipeWire:Interface:Node" {
            continue;
        }
        let props = &node["info"]["props"];
        if !props["media.class"]
            .as_str()
            .is_some_and(|class| class.starts_with("Audio/Source"))
        {
            continue;
        }
        if let Some(serial) = props["object.serial"].as_u64() {
            let serial = serial.to_string();
            if props["node.name"].as_str() == Some(source) || serial == source {
                return Ok(serial);
            }
        }
    }
    Err(MissingMicrophone(source.to_owned()).into())
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }
        tracing::warn!("[Capture] recorder dropped while running; stopping pw-record");
        if let Err(error) = self.finish_stop() {
            tracing::warn!("[Capture] abandoned recording stop failed: {error:#}");
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

enum RecorderExit {
    Graceful,
    Forced,
}

fn stop_child(child: &mut Child, deadline: Instant) -> Result<RecorderExit> {
    if let Some(status) = wait_for_exit(child, deadline)? {
        // pw-record exits with status 1 on SIGINT even after finalizing its WAV.
        tracing::debug!("[Capture] pw-record exited after SIGINT: {status}");
        return Ok(RecorderExit::Graceful);
    }
    tracing::warn!(
        "[Capture] pw-record exceeded {} second stop budget; terminating and retaining accepted audio",
        STOP_TIMEOUT.as_secs()
    );
    child.kill().context("terminating stalled pw-record")?;
    wait_for_exit(child, Instant::now() + KILL_TIMEOUT)?
        .context("pw-record still running after SIGKILL; runtime audio left untouched")?;
    Ok(RecorderExit::Forced)
}

/// Repair only the known pw-record PCM format, after the producer is reaped.
/// Chunk offsets come from the file, never a presumed 44-byte header or RIFF
/// size (libsndfile leaves that size stale until close). Keep complete frames.
fn finalize_wav_prefix(path: &Path) -> Result<()> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let len = file.metadata()?.len();
    let mut header = [0_u8; 12];
    file.read_exact(&mut header)?;
    if &header[..4] != b"RIFF" || &header[8..] != b"WAVE" {
        bail!("interrupted recording is not RIFF/WAVE");
    }
    let mut position = 12_u64;
    let mut pcm = false;
    let data_offset = loop {
        if position + 8 > len || position > 65_536 {
            bail!("interrupted recording has no supported PCM data header");
        }
        file.seek(SeekFrom::Start(position))?;
        let mut chunk = [0_u8; 8];
        file.read_exact(&mut chunk)?;
        let size = u64::from(u32::from_le_bytes(chunk[4..].try_into()?));
        if &chunk[..4] == b"data" {
            if !pcm {
                bail!("interrupted recording is not 16 kHz mono PCM16");
            }
            break position + 8;
        }
        if &chunk[..4] == b"fmt " {
            if size < 16 || position + 8 + size > len {
                bail!("interrupted recording has an incomplete format header");
            }
            let mut format = [0_u8; 16];
            file.read_exact(&mut format)?;
            pcm = u16::from_le_bytes(format[0..2].try_into()?) == 1
                && u16::from_le_bytes(format[2..4].try_into()?) == 1
                && u32::from_le_bytes(format[4..8].try_into()?) == 16_000
                && u32::from_le_bytes(format[8..12].try_into()?) == 32_000
                && u16::from_le_bytes(format[12..14].try_into()?) == 2
                && u16::from_le_bytes(format[14..16].try_into()?) == 16;
        }
        position += 8 + size + (size & 1);
    };
    let data_len = (len - data_offset) & !1;
    let end = data_offset + data_len;
    let riff_len =
        u32::try_from(end - 8).context("interrupted recording exceeds RIFF size limit")?;
    let data_len = u32::try_from(data_len)?;
    // All validation precedes mutation. Never truncate an unfamiliar header.
    file.set_len(end)?;
    file.seek(SeekFrom::Start(data_offset - 4))?;
    file.write_all(&data_len.to_le_bytes())?;
    file.seek(SeekFrom::Start(4))?;
    file.write_all(&riff_len.to_le_bytes())?;
    file.sync_all()?;
    tracing::warn!("[Capture] finalized interrupted recording PCM bytes={data_len}");
    Ok(())
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
fn wait_for_exit(child: &mut Child, deadline: Instant) -> Result<Option<ExitStatus>> {
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

#[cfg(test)]
mod tests {
    use cantrip_engine::audio::SignalMonitor;
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    fn slow_recorder(name: &str, delay: &str) -> super::Recorder {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};
        let path = wav_path(name);
        let mut audio = wav(&[1000, -1000, 2000, -2000]);
        // libsndfile's live RIFF header excludes the not-yet-finalized chunks.
        audio[4..8].copy_from_slice(&8_u32.to_le_bytes());
        audio[40..44].copy_from_slice(&0_u32.to_le_bytes());
        fs::write(&path, audio).unwrap();
        let mut child = Command::new("python3")
            .args([
                "-c",
                r#"
import os, signal, struct, sys, time
path, delay = sys.argv[1], float(sys.argv[2])
def finish(signum, frame):
    time.sleep(delay)
    with open(path, 'r+b') as wav:
        size = os.fstat(wav.fileno()).st_size
        wav.seek(4); wav.write(struct.pack('<I', size - 8))
        wav.seek(40); wav.write(struct.pack('<I', size - 44))
    sys.exit(0)
signal.signal(signal.SIGINT, finish)
print('ready', flush=True)
signal.pause()
"#,
            ])
            .arg(&path)
            .arg(delay)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready, "ready\n");
        let mut recorder = recorder_for_test(path);
        recorder.child.wait().unwrap();
        recorder.child = child;
        recorder
    }

    fn assert_retained_pcm(path: &std::path::Path) {
        let mut reader = hound::WavReader::open(path).expect("retained WAV is recoverable");
        assert_eq!(reader.spec().sample_rate, 16000);
        assert_eq!(reader.spec().channels, 1);
        assert_eq!(
            reader
                .samples::<i16>()
                .map(Result::unwrap)
                .collect::<Vec<_>>(),
            [1000, -1000, 2000, -2000]
        );
    }

    #[test]
    fn slow_recorder_finishes_after_old_stop_timeout() {
        let recorder = slow_recorder("slow-stop", "3.3");
        let path = recorder.wav_path.clone();
        assert_eq!(
            recorder.stop().expect("wait for slow SIGINT finalization"),
            path
        );
        assert_retained_pcm(&path);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn timed_out_recorder_retains_recoverable_pcm() {
        let recorder = slow_recorder("forced-stop", "60");
        let path = recorder.wav_path.clone();
        assert!(recorder.stop().is_err(), "forced capture remains a failure");
        assert_retained_pcm(&path);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn interrupted_wav_repair_scans_chunks_and_keeps_complete_frames() {
        let path = wav_path("repair-chunks");
        let mut audio = wav(&[1000, -1000, 2000, -2000]);
        // An ancillary chunk puts PCM beyond byte 44.
        audio.splice(36..36, *b"JUNK\x02\x00\x00\x00x\x00");
        audio[4..8].copy_from_slice(&8_u32.to_le_bytes());
        audio[50..54].copy_from_slice(&0_u32.to_le_bytes());
        audio.push(123); // A torn frame is not accepted PCM.
        fs::write(&path, audio).unwrap();
        super::finalize_wav_prefix(&path).unwrap();
        assert_retained_pcm(&path);
        let finalized = fs::read(&path).unwrap();
        super::finalize_wav_prefix(&path).unwrap();
        assert_eq!(fs::read(&path).unwrap(), finalized, "repair is idempotent");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn interrupted_wav_repair_leaves_unknown_or_incomplete_headers_untouched() {
        let path = wav_path("repair-invalid");
        let original = wav(&[1000, -1000]);
        let mut unknown_format = original.clone();
        unknown_format[20] = 3; // Float, not pw-record's signed PCM16.
        let mut missing_format = original.clone();
        missing_format[12..16].copy_from_slice(b"JUNK");
        let mut oversized_chunk = original.clone();
        oversized_chunk[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        for audio in [
            original[..30].to_vec(),
            unknown_format,
            missing_format,
            oversized_chunk,
        ] {
            fs::write(&path, &audio).unwrap();
            assert!(super::finalize_wav_prefix(&path).is_err());
            assert_eq!(fs::read(&path).unwrap(), audio);
        }
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn abandoned_slow_recorder_repairs_prefix_without_restarting_stop_budget() {
        let mut recorder = slow_recorder("abandoned-slow-stop", "60");
        let path = recorder.wav_path.clone();
        recorder.request_stop().unwrap();
        // Model a worker queue that consumed the entire grace period.
        recorder.stop_requested = Some(Instant::now() - super::STOP_TIMEOUT);
        let requested = recorder.stop_requested;
        recorder.request_stop().unwrap();
        assert_eq!(recorder.stop_requested, requested);
        drop(recorder);
        assert_retained_pcm(&path);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn selected_microphone_matches_name_or_serial_but_not_reusable_node_id() {
        let nodes = serde_json::json!([
            {"id": 11, "type": "PipeWire:Interface:Node", "info": {"props": {
                "media.class": "Audio/Source/Virtual", "node.name": "private_mic", "object.serial": 90
            }}},
            {"id": 12, "type": "PipeWire:Interface:Node", "info": {"props": {
                "media.class": "Audio/Sink", "node.name": "speaker", "object.serial": 91
            }}}
        ]);
        let nodes = nodes.as_array().unwrap();
        assert_eq!(super::source_serial("private_mic", nodes).unwrap(), "90");
        assert_eq!(super::source_serial("90", nodes).unwrap(), "90");
        for missing in ["11", "speaker", "91", "unplugged"] {
            let error = super::source_serial(missing, nodes).unwrap_err();
            assert_eq!(
                error
                    .downcast_ref::<cantrip_engine::ports::MissingMicrophone>()
                    .unwrap()
                    .0,
                missing
            );
        }
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
            stop_requested: None,
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
            stop_requested: None,
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
            stop_requested: None,
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
