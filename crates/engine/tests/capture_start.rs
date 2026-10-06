use cantrip_engine::audio::Pcm16WavWriter;
use cantrip_engine::config::Config;
use cantrip_engine::engine;
use cantrip_engine::ipc::{self, Command, Delivery};
use cantrip_engine::ports::{DeliveryPermit, Platform, Recorder};
use cantrip_engine::recovery;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command as Process;
use std::sync::{mpsc, Arc};
use std::time::Duration;

struct FailedNativeStart;

impl Platform for FailedNativeStart {
    fn start_recording(&self, path: &Path, _: Option<&str>) -> anyhow::Result<Box<dyn Recorder>> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        let mut writer = Pcm16WavWriter::new(file)?;
        writer.write_samples(&[22; 1_600])?;
        writer.finish()?;
        writer.get_ref().sync_all()?;
        anyhow::bail!("injected native startup failure after finalized PCM")
    }

    fn prepare_delivery(&self) {}

    fn delivery_permit(&self) -> Arc<dyn DeliveryPermit> {
        panic!("failed startup must not schedule transcription or delivery")
    }

    fn handoff_color(&self, _: usize) -> [u8; 3] {
        [0; 3]
    }

    fn sender_identity(&self, _: &UnixStream) -> String {
        "fault-injected-native-start".to_owned()
    }
}

fn exercise_failed_start() {
    let mut config = Config::default();
    // Bypass local-model admission without contacting a provider: capture fails
    // before any work can be submitted to the transcription worker.
    config.stt.endpoint = Some("http://127.0.0.1:1/v1".to_owned());
    let (ready, receiver) = mpsc::channel();
    let daemon = std::thread::spawn(move || {
        engine::run(config, false, Arc::new(FailedNativeStart), || {
            ready.send(()).unwrap()
        })
    });
    receiver.recv_timeout(Duration::from_secs(10)).unwrap();
    let reply = ipc::command(Command::Start {
        postproc: None,
        handoff: None,
    })
    .unwrap();
    let snapshot = ipc::status().unwrap();
    let takes = ipc::recordings().unwrap();
    engine::request_shutdown();
    daemon.join().unwrap().unwrap();

    assert!(!reply.ok);
    assert_eq!(reply.error.as_deref(), Some("capture-failed"));
    let outcome = snapshot.outcome.unwrap();
    assert_eq!(outcome.delivery, Delivery::None);
    assert!(outcome.artifacts.audio);
    assert!(!outcome.artifacts.text);
    assert_eq!(takes.len(), 1);
    let take = &takes[0];
    assert_eq!(outcome.artifacts.take_id.as_deref(), Some(take.id.as_str()));
    assert!(take.unresolved && take.audio_available && !take.text_available);
    assert_eq!(take.duration_ms, Some(100));
    let retained = recovery::audio_path(&take.id).unwrap();
    assert_eq!(
        fs::metadata(&retained).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let mut wav = hound::WavReader::open(retained).unwrap();
    assert_eq!(wav.spec().sample_rate, 16_000);
    assert!(wav
        .samples::<i16>()
        .map(Result::unwrap)
        .eq(std::iter::repeat_n(22, 1_600)));
    assert!(!cantrip_engine::paths::runtime_dir()
        .unwrap()
        .join(format!("rec-{}.wav", take.id))
        .exists());
}

#[test]
fn failed_capture_start_retains_actual_pcm() {
    const CHILD: &str = "CANTRIP_CAPTURE_START_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        exercise_failed_start();
        return;
    }
    // Native Darwin sockets need a short pathname; no app-owned or operator
    // runtime directory is touched. A subprocess isolates environment/signals.
    #[cfg(target_os = "macos")]
    let base = std::path::PathBuf::from("/private/tmp");
    #[cfg(target_os = "linux")]
    let base = std::env::temp_dir().canonicalize().unwrap();
    let root = base.join(format!("ct-start-{}", recovery::new_id()));
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let child = Process::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "failed_capture_start_retains_actual_pcm",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env("XDG_CONFIG_HOME", root.join("c"))
        .env("XDG_DATA_HOME", root.join("d"))
        .env("XDG_STATE_HOME", root.join("s"))
        .env("XDG_RUNTIME_DIR", root.join("r"))
        .output();
    fs::remove_dir_all(&root).unwrap();
    let child = child.unwrap();
    assert!(
        child.status.success(),
        "{}{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
}
