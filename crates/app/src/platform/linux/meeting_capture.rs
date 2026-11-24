//! Dual-track meeting capture on PipeWire: `You` from the microphone, `Others`
//! from the default sink's monitor (`stream.capture.sink = true`), both as
//! 16 kHz mono PCM16 WAV.
//!
//! One shared monotonic origin (an `Instant` taken before either recorder
//! starts). Each track's offset is the moment its first audio byte appeared,
//! minus the audio already buffered in that first observation, measured from
//! that origin — so both tracks place the same sound at the same timestamp up
//! to the polling resolution (5 ms) plus PipeWire's per-stream latency.

use super::audio::{pw_graph, PwRecord, WAV_HEADER_BYTES};
use cantrip_core::ports::{MeetingCapture, MeetingRecording, MeetingTracks, MicError};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const BYTES_PER_MS: u64 = 32;
const FIRST_DATA_WAIT: Duration = Duration::from_secs(5);

#[derive(Default)]
pub struct PipewireMeetingCapture;

impl PipewireMeetingCapture {
    pub fn new() -> Self {
        PipewireMeetingCapture
    }
}

impl MeetingCapture for PipewireMeetingCapture {
    fn start(
        &self,
        device: Option<&str>,
        you_path: &Path,
        others_path: &Path,
    ) -> Result<Box<dyn MeetingRecording>, String> {
        let (_, default_sink, _) = pw_graph()
            .ok_or_else(|| "Meeting capture needs PipeWire, which is not running".to_string())?;
        let sink = default_sink
            .ok_or_else(|| "System audio capture is unavailable on this session".to_string())?;
        let mic_error = |e| match e {
            MicError::Blocked => "Microphone blocked".to_string(),
            MicError::Unavailable(m) => m,
        };
        let origin = Instant::now();
        // Launch both before probing either, so they start together.
        let you = PwRecord::launch(device, None, you_path).map_err(mic_error)?;
        let others = PwRecord::launch(
            Some(&sink),
            Some("{ stream.capture.sink = true }"),
            others_path,
        );
        let you = you.probe().map_err(mic_error);
        let others = others.and_then(PwRecord::probe);
        let (you, others) = match (you, others) {
            (Ok(y), Ok(o)) => (y, o),
            (Err(e), others) => {
                if let Ok(o) = others {
                    let _ = o.stop();
                }
                return Err(e);
            }
            (Ok(y), Err(_)) => {
                let _ = y.stop();
                return Err("System audio capture is unavailable on this session".into());
            }
        };
        let offsets = Arc::new(Mutex::new((None, None)));
        let watcher = {
            let offsets = offsets.clone();
            let (yp, op) = (you_path.to_path_buf(), others_path.to_path_buf());
            thread::spawn(move || watch_first_data(origin, &yp, &op, &offsets))
        };
        Ok(Box::new(PipewireMeeting {
            origin,
            you: Some(you),
            others: Some(others),
            offsets,
            watcher: Some(watcher),
        }))
    }
}

/// Offset of a track's first sample given when it was first seen with
/// `audio_bytes` already written.
pub fn first_sample_offset_ms(seen_at_ms: u64, audio_bytes: u64) -> u64 {
    seen_at_ms.saturating_sub(audio_bytes / BYTES_PER_MS)
}

type Offsets = Arc<Mutex<(Option<u64>, Option<u64>)>>;

fn watch_first_data(origin: Instant, you: &Path, others: &Path, offsets: &Offsets) {
    let deadline = origin + FIRST_DATA_WAIT;
    loop {
        let now = Instant::now();
        let ms = now.duration_since(origin).as_millis() as u64;
        let mut o = offsets.lock().unwrap();
        let (a, b) = &mut *o;
        for (path, slot) in [(you, a), (others, b)] {
            if slot.is_none() {
                if let Ok(m) = std::fs::metadata(path) {
                    if m.len() > WAV_HEADER_BYTES {
                        *slot = Some(first_sample_offset_ms(ms, m.len() - WAV_HEADER_BYTES));
                    }
                }
            }
        }
        if (o.0.is_some() && o.1.is_some()) || now >= deadline {
            return;
        }
        drop(o);
        thread::sleep(Duration::from_millis(5));
    }
}

struct PipewireMeeting {
    origin: Instant,
    you: Option<PwRecord>,
    others: Option<PwRecord>,
    offsets: Offsets,
    watcher: Option<thread::JoinHandle<()>>,
}

impl MeetingRecording for PipewireMeeting {
    fn elapsed(&self) -> Duration {
        self.origin.elapsed()
    }

    fn stop(mut self: Box<Self>) -> Result<MeetingTracks, String> {
        let you = self.you.take().expect("you track").stop();
        let others = self.others.take().expect("others track").stop();
        if let Some(w) = self.watcher.take() {
            let _ = w.join();
        }
        let (yo, oo) = *self.offsets.lock().unwrap();
        let you: PathBuf = you.map_err(|e| format!("Microphone track: {e}"))?;
        let others: PathBuf = others.map_err(|e| format!("System audio track: {e}"))?;
        Ok(MeetingTracks {
            you,
            others,
            you_offset_ms: yo.unwrap_or(0),
            others_offset_ms: oo.unwrap_or(0),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offset_subtracts_audio_already_buffered() {
        assert_eq!(first_sample_offset_ms(120, 32 * 40), 80);
        assert_eq!(first_sample_offset_ms(10, 32 * 400), 0);
    }
}

#[cfg(test)]
mod live {
    use super::*;
    use crate::platform::linux::audio::live::{play, tone_file};

    /// `CANTRIP_TEST_MIC=<source> cargo test -p cantrip live_meeting -- --ignored`:
    /// a tone played to the default sink must land on `Others`, not `You`.
    #[test]
    #[ignore]
    fn live_meeting_others_hears_system_output() {
        let mic = std::env::var("CANTRIP_TEST_MIC").ok();
        let dir = tempfile::tempdir().unwrap();
        let tone = tone_file(dir.path(), 2);
        let (you, others) = (dir.path().join("you.wav"), dir.path().join("others.wav"));
        let m = PipewireMeetingCapture::new()
            .start(mic.as_deref(), &you, &others)
            .unwrap();
        thread::sleep(Duration::from_millis(300));
        let mut p = play(None, &tone);
        thread::sleep(Duration::from_millis(1800));
        assert!(m.elapsed() >= Duration::from_millis(2000));
        let tracks = m.stop().unwrap();
        let _ = p.kill();
        let _ = p.wait();
        let (y, _) = cantrip_core::wav::read_pcm16(&tracks.you).unwrap();
        let (o, _) = cantrip_core::wav::read_pcm16(&tracks.others).unwrap();
        let (ry, ro) = (cantrip_core::wav::rms(&y), cantrip_core::wav::rms(&o));
        eprintln!(
            "you: {:.2}s rms={ry:.3} offset={}ms | others: {:.2}s rms={ro:.3} offset={}ms",
            y.len() as f32 / 16000.0,
            tracks.you_offset_ms,
            o.len() as f32 / 16000.0,
            tracks.others_offset_ms
        );
        assert!(cantrip_core::wav::has_speech(&o, 16_000));
        assert!(ro > 0.05 && ry < 0.01);
        assert!(tracks.you_offset_ms < 500 && tracks.others_offset_ms < 500);
    }

    /// A burst played to the mic feed and the default sink at the same moment
    /// must land at the same shared-clock time on both tracks (≤ 100 ms).
    #[test]
    #[ignore]
    fn live_meeting_tracks_share_one_clock() {
        let mic = std::env::var("CANTRIP_TEST_MIC").expect("CANTRIP_TEST_MIC");
        let feed = std::env::var("CANTRIP_TEST_MIC_FEED").expect("CANTRIP_TEST_MIC_FEED");
        let dir = tempfile::tempdir().unwrap();
        let tone = tone_file(dir.path(), 1);
        let (you, others) = (dir.path().join("you.wav"), dir.path().join("others.wav"));
        let m = PipewireMeetingCapture::new()
            .start(Some(&mic), &you, &others)
            .unwrap();
        thread::sleep(Duration::from_millis(1200));
        let mut a = play(Some(&feed), &tone);
        let mut b = play(None, &tone);
        thread::sleep(Duration::from_millis(1500));
        let tracks = m.stop().unwrap();
        let _ = (a.kill(), b.kill());
        let _ = (a.wait(), b.wait());
        let onset = |p: &Path, offset: u64| {
            let (s, _) = cantrip_core::wav::read_pcm16(p).unwrap();
            let i = s
                .iter()
                .position(|x| x.unsigned_abs() > 3000)
                .expect("burst");
            offset as f64 + i as f64 / 16.0
        };
        let ty = onset(&tracks.you, tracks.you_offset_ms);
        let to = onset(&tracks.others, tracks.others_offset_ms);
        eprintln!(
            "burst onset: you={ty:.1}ms others={to:.1}ms diff={:.1}ms",
            (ty - to).abs()
        );
        assert!((ty - to).abs() <= 100.0);
    }
}
