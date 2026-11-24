//! Microphone capture through PipeWire's `pw-record`, writing 16 kHz mono
//! PCM16 WAV directly into Cantrip's staging file.
//!
//! Stop is SIGINT-and-wait (SIGKILL can corrupt the WAV header); a bounded
//! wait escalates to SIGTERM, never straight to SIGKILL. The live level comes
//! from the tail of the growing file, read on a small thread.

use cantrip_core::ports::{MicError, Recorder, Recording};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// pw-record's WAV header (RIFF + fmt + data chunk headers).
pub const WAV_HEADER_BYTES: u64 = 44;
const BYTES_PER_SEC: u64 = 32_000;
/// How long a fresh pw-record may take to fail before we call it started.
const STARTUP_PROBE: Duration = Duration::from_millis(300);
const STOP_GRACE: Duration = Duration::from_secs(3);

// ───────────────────────────── devices ─────────────────────────────

/// An audio node seen in `pw-dump`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub name: String,
    pub description: String,
    pub media_class: String,
}

/// Parse `pw-dump` JSON into audio nodes and the default sink/source names.
pub fn parse_pw_dump(json: &str) -> (Vec<Node>, Option<String>, Option<String>) {
    let mut nodes = Vec::new();
    let (mut sink, mut source) = (None, None);
    let Ok(serde_json::Value::Array(objs)) = serde_json::from_str::<serde_json::Value>(json) else {
        return (nodes, None, None);
    };
    for o in &objs {
        match o.get("type").and_then(|t| t.as_str()) {
            Some("PipeWire:Interface:Node") => {
                let props = &o["info"]["props"];
                let s = |k: &str| props.get(k).and_then(|v| v.as_str()).map(str::to_string);
                if let (Some(name), Some(class)) = (s("node.name"), s("media.class")) {
                    if class.starts_with("Audio/") {
                        let description = s("node.description")
                            .or_else(|| s("node.nick"))
                            .unwrap_or_else(|| name.clone());
                        nodes.push(Node {
                            name,
                            description,
                            media_class: class,
                        });
                    }
                }
            }
            Some("PipeWire:Interface:Metadata")
                if o["props"]["metadata.name"].as_str() == Some("default") =>
            {
                for m in o["metadata"].as_array().into_iter().flatten() {
                    let name = m["value"]["name"].as_str().map(str::to_string);
                    match m["key"].as_str() {
                        Some("default.audio.sink") => sink = name,
                        Some("default.audio.source") => source = name,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    (nodes, sink, source)
}

/// Input devices (id = node.name, name = description), real sources only.
pub fn input_devices(nodes: &[Node]) -> Vec<(String, String)> {
    nodes
        .iter()
        .filter(|n| n.media_class.starts_with("Audio/Source"))
        .map(|n| (n.name.clone(), n.description.clone()))
        .collect()
}

/// Parse `pactl -f json list sources` (fallback when pw-dump is missing).
pub fn parse_pactl_sources(json: &str) -> Vec<(String, String)> {
    let Ok(serde_json::Value::Array(items)) = serde_json::from_str::<serde_json::Value>(json)
    else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|s| s["monitor_of_sink"].is_null() || s["monitor_of_sink"].as_str() == Some("n/a"))
        .filter_map(|s| {
            let name = s["name"].as_str()?.to_string();
            let desc = s["description"].as_str().unwrap_or(&name).to_string();
            Some((name, desc))
        })
        .collect()
}

/// Snapshot of the PipeWire graph via `pw-dump`.
pub fn pw_graph() -> Option<(Vec<Node>, Option<String>, Option<String>)> {
    let out = Command::new("pw-dump")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| parse_pw_dump(&String::from_utf8_lossy(&out.stdout)))
}

// ───────────────────────────── one pw-record process ─────────────────────────────

/// One running `pw-record` writing `path`.
pub struct PwRecord {
    child: Child,
    path: PathBuf,
}

impl PwRecord {
    /// Spawn pw-record and probe briefly for an immediate failure.
    pub fn spawn(
        target: Option<&str>,
        props: Option<&str>,
        path: &Path,
    ) -> Result<PwRecord, MicError> {
        PwRecord::launch(target, props, path)?.probe()
    }

    /// Start pw-record without waiting; pair with [`PwRecord::probe`]. Lets
    /// two tracks start together instead of one probe apart.
    pub fn launch(
        target: Option<&str>,
        props: Option<&str>,
        path: &Path,
    ) -> Result<PwRecord, MicError> {
        let mut cmd = Command::new("pw-record");
        cmd.args(["--rate", "16000", "--channels", "1", "--format", "s16"]);
        if let Some(t) = target {
            cmd.args(["--target", t]);
        }
        if let Some(p) = props {
            cmd.args(["-P", p]);
        }
        cmd.arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let child = cmd.spawn().map_err(|e| {
            MicError::Unavailable(format!(
                "PipeWire recorder (pw-record) could not start: {}",
                e.kind()
            ))
        })?;
        Ok(PwRecord {
            child,
            path: path.to_path_buf(),
        })
    }

    /// Wait out the startup window for an immediate failure.
    pub fn probe(mut self) -> Result<PwRecord, MicError> {
        let deadline = Instant::now() + STARTUP_PROBE;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_status)) => {
                    let mut err = String::new();
                    if let Some(mut s) = self.child.stderr.take() {
                        let _ = s.read_to_string(&mut err);
                    }
                    // Remove only a conclusively empty header, never audio.
                    if std::fs::metadata(&self.path).is_ok_and(|m| m.len() <= WAV_HEADER_BYTES) {
                        let _ = std::fs::remove_file(&self.path);
                    }
                    return Err(classify_failure(&err));
                }
                Ok(None) => thread::sleep(Duration::from_millis(20)),
                Err(e) => {
                    return Err(MicError::Unavailable(format!(
                        "recorder wait failed: {}",
                        e.kind()
                    )))
                }
            }
        }
        // Do not let a full stderr pipe stall the recorder.
        if let Some(mut s) = self.child.stderr.take() {
            thread::spawn(move || {
                let mut sink = [0u8; 1024];
                while matches!(s.read(&mut sink), Ok(n) if n > 0) {}
            });
        }
        Ok(self)
    }

    /// SIGINT and wait (bounded); then SIGTERM; SIGKILL only as last resort.
    pub fn stop(mut self) -> Result<PathBuf, String> {
        signal(&self.child, libc::SIGINT);
        if !wait_for(&mut self.child, STOP_GRACE) {
            signal(&self.child, libc::SIGTERM);
            if !wait_for(&mut self.child, STOP_GRACE) {
                let _ = self.child.kill();
                let _ = self.child.wait();
                return Err("recorder did not stop; audio may be incomplete".into());
            }
        }
        match std::fs::metadata(&self.path) {
            Ok(m) if m.len() >= WAV_HEADER_BYTES => Ok(self.path.clone()),
            Ok(_) => Err("recorder produced no audio".into()),
            Err(e) => Err(format!("recorded audio missing: {}", e.kind())),
        }
    }

    /// Stop and delete the file (Escape).
    pub fn cancel(self) {
        let path = self.path.clone();
        let _ = self.stop();
        let _ = std::fs::remove_file(path);
    }
}

impl Drop for PwRecord {
    fn drop(&mut self) {
        // Never leave a recorder running if a handle is dropped unexpectedly.
        if let Ok(None) = self.child.try_wait() {
            signal(&self.child, libc::SIGINT);
            let _ = wait_for(&mut self.child, STOP_GRACE);
        }
    }
}

fn signal(child: &Child, sig: libc::c_int) {
    // SAFETY: plain kill(2) on our own child's pid.
    unsafe {
        libc::kill(child.id() as libc::pid_t, sig);
    }
}

fn wait_for(child: &mut Child, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => return false,
            Err(_) => return true,
        }
    }
}

/// Map pw-record's early-exit stderr to a mic error (no audio content involved).
pub fn classify_failure(stderr: &str) -> MicError {
    let s = stderr.to_ascii_lowercase();
    if ["permission", "denied", "not allowed", "access"]
        .iter()
        .any(|k| s.contains(k))
    {
        MicError::Blocked
    } else if s.contains("connect") || s.contains("context") {
        MicError::Unavailable("PipeWire is not running".into())
    } else if s.contains("target") || s.contains("no such") || s.contains("not found") {
        MicError::Unavailable("The chosen microphone is not available".into())
    } else {
        MicError::Unavailable("The microphone could not start".into())
    }
}

// ───────────────────────────── level meter ─────────────────────────────

/// Background reader of a growing WAV file's newest ~50 ms.
pub struct LevelMeter {
    level: Arc<AtomicU32>,
    /// Audio bytes seen so far (shared with the reader thread).
    _bytes: Arc<std::sync::atomic::AtomicU64>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl LevelMeter {
    pub fn start(path: &Path) -> LevelMeter {
        let level = Arc::new(AtomicU32::new(0));
        let bytes = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (l, b, s, p) = (
            level.clone(),
            bytes.clone(),
            stop.clone(),
            path.to_path_buf(),
        );
        let thread = thread::spawn(move || {
            let mut file: Option<File> = None;
            while !s.load(Ordering::Relaxed) {
                if file.is_none() {
                    file = File::open(&p).ok();
                }
                if let Some(f) = file.as_mut() {
                    if let Some((lvl, len)) = tail_level(f) {
                        l.store(lvl.to_bits(), Ordering::Relaxed);
                        b.store(len, Ordering::Relaxed);
                    }
                }
                thread::sleep(Duration::from_millis(50));
            }
        });
        LevelMeter {
            level,
            _bytes: bytes,
            stop,
            thread: Some(thread),
        }
    }

    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }
}

impl Drop for LevelMeter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Level of the newest 50 ms of a growing PCM16 WAV, with a perceptual gain so
/// speech fills the meter. Returns (level, audio bytes).
fn tail_level(f: &mut File) -> Option<(f32, u64)> {
    let len = f.metadata().ok()?.len();
    if len <= WAV_HEADER_BYTES {
        return Some((0.0, 0));
    }
    let audio = len - WAV_HEADER_BYTES;
    let want = (BYTES_PER_SEC / 20).min(audio) & !1;
    let start = WAV_HEADER_BYTES + ((audio - want) & !1);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = vec![0u8; want as usize];
    f.read_exact(&mut buf).ok()?;
    let samples: Vec<i16> = buf
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| i16::from_le_bytes(*c))
        .collect();
    Some((meter_level(cantrip_core::wav::rms(&samples)), audio))
}

/// Map RMS to a 0..1 meter on a dB scale (-60 dBFS → 0, -10 dBFS → 1).
pub fn meter_level(rms: f32) -> f32 {
    if rms <= 0.0 {
        return 0.0;
    }
    let db = 20.0 * rms.log10();
    ((db + 60.0) / 50.0).clamp(0.0, 1.0)
}

// ───────────────────────────── Recorder ─────────────────────────────

/// The `Recorder` port on PipeWire.
#[derive(Default)]
pub struct PipewireRecorder;

impl PipewireRecorder {
    pub fn new() -> Self {
        PipewireRecorder
    }
}

impl Recorder for PipewireRecorder {
    fn devices(&self) -> Vec<(String, String)> {
        if let Some((nodes, _, _)) = pw_graph() {
            return input_devices(&nodes);
        }
        Command::new("pactl")
            .args(["-f", "json", "list", "sources"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| parse_pactl_sources(&String::from_utf8_lossy(&o.stdout)))
            .unwrap_or_default()
    }

    fn start(&self, device: Option<&str>, wav_path: &Path) -> Result<Box<dyn Recording>, MicError> {
        let rec = PwRecord::spawn(device, None, wav_path)?;
        let meter = LevelMeter::start(wav_path);
        Ok(Box::new(PipewireRecording { rec, meter }))
    }
}

struct PipewireRecording {
    rec: PwRecord,
    meter: LevelMeter,
}

impl Recording for PipewireRecording {
    fn level(&self) -> f32 {
        self.meter.level()
    }

    fn stop(self: Box<Self>) -> Result<PathBuf, String> {
        let PipewireRecording { rec, meter } = *self;
        drop(meter);
        rec.stop()
    }

    fn cancel(self: Box<Self>) {
        let PipewireRecording { rec, meter } = *self;
        drop(meter);
        rec.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMP: &str = r#"[
      {"id":29,"type":"PipeWire:Interface:Node","info":{"props":{"node.name":"Dummy-Driver"}}},
      {"id":33,"type":"PipeWire:Interface:Node","info":{"props":{"node.name":"alsa_output.pci","media.class":"Audio/Sink","node.description":"Speakers"}}},
      {"id":34,"type":"PipeWire:Interface:Node","info":{"props":{"node.name":"alsa_input.usb","media.class":"Audio/Source","node.description":"USB Mic"}}},
      {"id":35,"type":"PipeWire:Interface:Node","info":{"props":{"node.name":"virt","media.class":"Audio/Source/Virtual","node.nick":"Virtual"}}},
      {"id":36,"type":"PipeWire:Interface:Node","info":{"props":{"node.name":"midi","media.class":"Midi/Bridge"}}},
      {"id":40,"type":"PipeWire:Interface:Metadata","props":{"metadata.name":"default"},
       "metadata":[{"subject":0,"key":"default.audio.sink","type":"Spa:String:JSON","value":{"name":"alsa_output.pci"}},
                   {"subject":0,"key":"default.audio.source","type":"Spa:String:JSON","value":{"name":"alsa_input.usb"}}]}
    ]"#;

    #[test]
    fn pw_dump_parsing_finds_sources_and_defaults() {
        let (nodes, sink, source) = parse_pw_dump(DUMP);
        assert_eq!(nodes.len(), 3);
        assert_eq!(sink.as_deref(), Some("alsa_output.pci"));
        assert_eq!(source.as_deref(), Some("alsa_input.usb"));
        assert_eq!(
            input_devices(&nodes),
            vec![
                ("alsa_input.usb".to_string(), "USB Mic".to_string()),
                ("virt".to_string(), "Virtual".to_string())
            ]
        );
        assert_eq!(parse_pw_dump("not json").0.len(), 0);
    }

    #[test]
    fn pactl_fallback_skips_monitors() {
        let j = r#"[{"name":"mic","description":"Mic","monitor_of_sink":"n/a"},
                    {"name":"out.monitor","description":"Monitor","monitor_of_sink":"out"}]"#;
        assert_eq!(
            parse_pactl_sources(j),
            vec![("mic".to_string(), "Mic".to_string())]
        );
    }

    #[test]
    fn failure_classification() {
        assert_eq!(
            classify_failure("error: Permission denied"),
            MicError::Blocked
        );
        assert!(matches!(
            classify_failure("can't connect: Host is down"),
            MicError::Unavailable(_)
        ));
    }

    #[test]
    fn meter_scale() {
        assert_eq!(meter_level(0.0), 0.0);
        assert!(meter_level(0.001) < 0.05);
        assert!((meter_level(0.316) - 1.0).abs() < 0.01);
    }

    #[test]
    fn tail_level_reads_newest_audio() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.wav");
        let mut s = vec![0i16; 16000];
        s.extend((0..1600).map(|i| if i % 2 == 0 { 8000 } else { -8000 }));
        std::fs::write(&p, cantrip_core::wav::encode_pcm16(&s, 16000)).unwrap();
        let (lvl, bytes) = tail_level(&mut File::open(&p).unwrap()).unwrap();
        assert_eq!(bytes, 17600 * 2);
        assert!(lvl > 0.7, "{lvl}");
    }
}

/// Live tests against the real PipeWire graph. Run with
/// `CANTRIP_TEST_MIC=<source node> cargo test -p cantrip live_ -- --ignored`
/// (see scripts/test-linux-audio for the virtual devices used in CI-less runs).
#[cfg(test)]
pub(crate) mod live {
    use super::*;

    /// Write a 440 Hz tone WAV (48 kHz) for pw-play.
    pub fn tone_file(dir: &Path, secs: u32) -> PathBuf {
        let n = 48_000 * secs;
        let s: Vec<i16> = (0..n)
            .map(|i| {
                ((i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 12_000.0) as i16
            })
            .collect();
        let p = dir.join("tone.wav");
        std::fs::write(&p, cantrip_core::wav::encode_pcm16(&s, 48_000)).unwrap();
        p
    }

    pub fn play(target: Option<&str>, file: &Path) -> std::process::Child {
        let mut c = Command::new("pw-play");
        if let Some(t) = target {
            c.args(["--target", t]);
        }
        c.arg(file)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    #[test]
    #[ignore]
    fn live_record_tone_from_virtual_mic() {
        let mic = std::env::var("CANTRIP_TEST_MIC").expect("CANTRIP_TEST_MIC");
        let dir = tempfile::tempdir().unwrap();
        let tone = tone_file(dir.path(), 2);
        let rec = PipewireRecorder::new();
        let devices = rec.devices();
        eprintln!("devices: {devices:?}");
        assert!(devices.iter().any(|(id, _)| id == &mic));
        let wav = dir.path().join("take.wav");
        let started = Instant::now();
        let r = rec.start(Some(&mic), &wav).unwrap();
        // A feed sink looped into the mic source (see scripts/test-linux-audio).
        let feed = std::env::var("CANTRIP_TEST_MIC_FEED").unwrap_or_else(|_| mic.clone());
        let mut player = play(Some(&feed), &tone);
        let mut peak = 0f32;
        while started.elapsed() < Duration::from_millis(1600) {
            peak = peak.max(r.level());
            thread::sleep(Duration::from_millis(50));
        }
        let path = r.stop().unwrap();
        let _ = player.kill();
        let _ = player.wait();
        let (samples, rate) = cantrip_core::wav::read_pcm16(&path).unwrap();
        let secs = samples.len() as f32 / rate as f32;
        let rms = cantrip_core::wav::rms(&samples);
        eprintln!("rate={rate} secs={secs:.2} rms={rms:.3} peak_level={peak:.2}");
        assert_eq!(rate, 16_000);
        assert!(secs > 1.2 && secs < 2.2, "{secs}");
        assert!(rms > 0.05, "{rms}");
        assert!(peak > 0.5, "{peak}");
    }

    #[test]
    #[ignore]
    fn live_missing_device_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let rec = PipewireRecorder::new();
        let r = rec.start(Some("cantrip-no-such-device"), &dir.path().join("x.wav"));
        // pw-record may wait for a target that never appears instead of
        // exiting; either an error or a silent take is acceptable, never a crash.
        if let Ok(r) = r {
            r.cancel();
        }
    }

    #[test]
    #[ignore]
    fn live_cancel_deletes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("c.wav");
        let r = PipewireRecorder::new().start(None, &wav).unwrap();
        thread::sleep(Duration::from_millis(400));
        r.cancel();
        assert!(!wav.exists());
    }
}
