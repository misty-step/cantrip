use cantrip_engine::audio::{self, InputSignal, Pcm16WavWriter};
use cantrip_engine::config::Config;
use cantrip_engine::engine;
use cantrip_engine::ipc::{self, Command, Completeness, Delivery};
use cantrip_engine::ports::{DeliveryPermit, Platform, Recorder};
use cantrip_engine::{paths, recovery};
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

fn write_empty(path: &Path) {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    let mut writer = Pcm16WavWriter::new(file).unwrap();
    writer.finish().unwrap();
    writer.get_ref().sync_all().unwrap();
    assert_eq!(fs::metadata(path).unwrap().len(), 44);
}

struct EmptyCapture;
struct EmptyRecorder(PathBuf);

impl Recorder for EmptyRecorder {
    fn input_signal(&mut self) -> Option<InputSignal> {
        None
    }

    fn request_stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn stop(self: Box<Self>) -> anyhow::Result<PathBuf> {
        // Exercise the shared verification used by both native recorders.
        audio::verify_wav(&self.0)?;
        Ok(self.0)
    }
}

impl Platform for EmptyCapture {
    fn start_recording(&self, path: &Path, _: Option<&str>) -> anyhow::Result<Box<dyn Recorder>> {
        write_empty(path);
        Ok(Box::new(EmptyRecorder(path.to_owned())))
    }

    fn prepare_delivery(&self) {}

    fn delivery_permit(&self) -> Arc<dyn DeliveryPermit> {
        // The engine prepares a permit when capture starts, but must never use it.
        Arc::new(NoDelivery)
    }

    fn handoff_color(&self, _: usize) -> [u8; 3] {
        [0; 3]
    }

    fn sender_identity(&self, _: &UnixStream) -> String {
        "empty-capture-regression".to_owned()
    }
}

struct NoDelivery;
impl DeliveryPermit for NoDelivery {
    fn clipboard_fallback(&self) -> cantrip_engine::delivery::ClipboardFallback {
        panic!("empty capture must not attempt delivery")
    }

    fn deliver(
        &self,
        _: &str,
        _: cantrip_engine::delivery::InjectionMode,
        _: &std::sync::atomic::AtomicBool,
    ) -> Result<
        cantrip_engine::delivery::InjectionOutcome,
        cantrip_engine::delivery::InjectionFailure,
    > {
        panic!("empty capture must not attempt delivery")
    }
}

fn start_daemon() -> std::thread::JoinHandle<anyhow::Result<()>> {
    let mut config = Config::default();
    // Admit capture without loading a model or contacting any provider.
    config.stt.endpoint = Some("http://127.0.0.1:1/v1".to_owned());
    let (ready, receiver) = mpsc::channel();
    let daemon = std::thread::spawn(move || {
        engine::run(config, false, Arc::new(EmptyCapture), || {
            ready.send(()).unwrap()
        })
    });
    receiver.recv_timeout(Duration::from_secs(10)).unwrap();
    daemon
}

fn assert_retained_empty(id: &str, expected: &[u8]) {
    let takes = recovery::list().unwrap();
    assert_eq!(takes.len(), 1);
    assert_eq!(takes[0].id, id);
    assert!(!takes[0].audio_available);
    assert!(!takes[0].text_available);
    assert!(takes[0].unresolved);
    assert!(recovery::audio_path(id).is_err());
    assert_eq!(
        fs::read(
            paths::transcript_history_dir()
                .unwrap()
                .join(format!("{id}.wav"))
        )
        .unwrap(),
        expected
    );
}

#[test]
fn finalized_empty_take_persists_and_import_consumes_confirmed_original() {
    isolated(
        "finalized_empty_take_persists_and_import_consumes_confirmed_original",
        || {
            let runtime = paths::ensure_dir(paths::runtime_dir().unwrap()).unwrap();
            let id = recovery::new_id();
            let source = runtime.join(format!("rec-{id}.wav"));
            write_empty(&source);
            let expected = fs::read(&source).unwrap();
            let take = recovery::persist(&id, 59, None, Some(&source), false, true).unwrap();
            assert!(!take.audio_available);
            assert_retained_empty(&id, &expected);
            recovery::import_runtime(&runtime).unwrap();
            assert!(!source.exists());
            assert_retained_empty(&id, &expected);
        },
    );
}

#[test]
fn startup_imports_empty_leftover_with_existing_identical_durable_copy() {
    isolated(
        "startup_imports_empty_leftover_with_existing_identical_durable_copy",
        || {
            let runtime = paths::ensure_dir(paths::runtime_dir().unwrap()).unwrap();
            let id = recovery::new_id();
            let source = runtime.join(format!("rec-{id}.wav"));
            write_empty(&source);
            let expected = fs::read(&source).unwrap();
            // Recreate 0.1.11's live state independently of empty persistence:
            // trusted, identical durable bytes plus a positive elapsed duration.
            recovery::persist(&id, 2062, None, None, false, true).unwrap();
            let history = paths::transcript_history_dir().unwrap();
            let retained = history.join(format!("{id}.wav"));
            fs::copy(&source, &retained).unwrap();
            std::fs::File::open(&retained).unwrap().sync_all().unwrap();
            recovery::import_runtime(&runtime).unwrap();
            assert!(!source.exists());
            assert_retained_empty(&id, &expected);
            assert_eq!(recovery::get(&id).unwrap().duration_ms, Some(2062));
            // Startup must also consume the exact duplicate without a discovery notice.
            fs::write(&source, &expected).unwrap();
            let daemon = start_daemon();
            let snapshot = ipc::status().unwrap();
            engine::request_shutdown();
            daemon.join().unwrap().unwrap();
            assert!(snapshot.notice.is_none());
            assert!(!source.exists());
            assert_retained_empty(&id, &expected);
        },
    );
}

#[test]
fn startup_import_retains_a_fresh_finalized_empty_leftover() {
    isolated(
        "startup_import_retains_a_fresh_finalized_empty_leftover",
        || {
            let runtime = paths::ensure_dir(paths::runtime_dir().unwrap()).unwrap();
            let id = recovery::new_id();
            let source = runtime.join(format!("rec-{id}.wav"));
            write_empty(&source);
            let expected = fs::read(&source).unwrap();
            recovery::import_runtime(&runtime).unwrap();
            assert!(!source.exists());
            assert_retained_empty(&id, &expected);
        },
    );
}

fn exercise_capture(cancel: bool) {
    let daemon = start_daemon();
    assert!(
        ipc::command(Command::Start {
            postproc: None,
            handoff: None
        })
        .unwrap()
        .ok
    );
    assert!(
        ipc::command(if cancel {
            Command::Cancel
        } else {
            Command::Stop
        })
        .unwrap()
        .ok
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let snapshot = loop {
        let snapshot = ipc::status().unwrap();
        if snapshot.outcome.is_some() || Instant::now() >= deadline {
            break snapshot;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    engine::request_shutdown();
    daemon.join().unwrap().unwrap();
    let outcome = snapshot.outcome.expect("capture settles");
    assert!(snapshot.notice.is_none());
    assert_eq!(
        outcome.completeness,
        if cancel {
            Completeness::Cancelled
        } else {
            Completeness::Failed
        }
    );
    assert_eq!(
        outcome.delivery,
        if cancel {
            Delivery::Cancelled
        } else {
            Delivery::None
        }
    );
    assert_eq!(
        outcome.error.as_deref(),
        if cancel { None } else { Some("capture-failed") }
    );
    if !cancel {
        assert!(outcome.message.contains("microphone") && outcome.message.contains("no audio"));
    }
    assert!(!outcome.artifacts.audio && !outcome.artifacts.text);
    let id = outcome.artifacts.take_id.unwrap();
    let retained = paths::transcript_history_dir()
        .unwrap()
        .join(format!("{id}.wav"));
    assert_retained_empty(&id, &fs::read(retained).unwrap());
    assert!(!paths::runtime_dir()
        .unwrap()
        .join(format!("rec-{id}.wav"))
        .exists());
}

#[test]
fn cancelled_empty_capture_is_retained_without_failure_or_storage_warning() {
    isolated(
        "cancelled_empty_capture_is_retained_without_failure_or_storage_warning",
        || exercise_capture(true),
    );
}

#[test]
fn stopped_empty_capture_reports_microphone_failure_not_storage_failure() {
    isolated(
        "stopped_empty_capture_reports_microphone_failure_not_storage_failure",
        || exercise_capture(false),
    );
}

fn isolated(test_name: &str, exercise: impl FnOnce()) {
    const CHILD: &str = "CANTRIP_EMPTY_TAKE_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        exercise();
        return;
    }
    #[cfg(target_os = "macos")]
    let base = PathBuf::from("/private/tmp");
    #[cfg(target_os = "linux")]
    let base = std::env::temp_dir().canonicalize().unwrap();
    let root = base.join(format!("ct-empty-{}", recovery::new_id()));
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let child = Process::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env(CHILD, "1")
        .env("XDG_CONFIG_HOME", root.join("c"))
        .env("XDG_DATA_HOME", root.join("d"))
        .env("XDG_STATE_HOME", root.join("s"))
        .env("XDG_RUNTIME_DIR", root.join("r"))
        .output();
    fs::remove_dir_all(root).unwrap();
    let child = child.unwrap();
    assert!(
        child.status.success(),
        "{}{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
}
