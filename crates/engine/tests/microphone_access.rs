//! The public engine boundary: status never asks, explicit Allow never records,
//! and a first press records only after authorization while its intent is live.

use cantrip_engine::audio::{InputSignal, Pcm16WavWriter};
use cantrip_engine::config::Config;
use cantrip_engine::engine;
use cantrip_engine::ipc::{
    self, Command, Completeness, Delivery, MicrophoneAuthorization, MicrophoneGrantee, StateKind,
    StatusSnapshot,
};
use cantrip_engine::pipeline::Stage;
use cantrip_engine::ports::{DeliveryPermit, Platform, Recorder};
use cantrip_engine::recovery;
use parking_lot::Mutex;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// A capture owner that refuses capture until authorized and keeps an explicit
/// request pending until the operator answers, as macOS does.
struct PromptingOwner {
    authorization: Mutex<MicrophoneAuthorization>,
    requests: AtomicUsize,
    starts: AtomicUsize,
    fail_start: AtomicBool,
}

impl PromptingOwner {
    /// The operator's answer, delivered off the engine thread like the system
    /// prompt's completion handler.
    fn answer(&self, authorization: MicrophoneAuthorization) {
        *self.authorization.lock() = authorization;
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    fn starts(&self) -> usize {
        self.starts.load(Ordering::SeqCst)
    }
}

impl Platform for PromptingOwner {
    fn start_recording(&self, path: &Path, _: Option<&str>) -> anyhow::Result<Box<dyn Recorder>> {
        assert_eq!(
            *self.authorization.lock(),
            MicrophoneAuthorization::Authorized
        );
        if self.fail_start.load(Ordering::SeqCst) {
            anyhow::bail!("injected capture startup refusal");
        }
        self.starts.fetch_add(1, Ordering::SeqCst);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        let mut writer = Pcm16WavWriter::new(file)?;
        writer.write_samples(&[22; 1_600])?;
        writer.finish()?;
        writer.get_ref().sync_all()?;
        Ok(Box::new(SyntheticRecording(path.to_owned())))
    }

    fn prepare_delivery(&self) {}

    fn delivery_permit(&self) -> Arc<dyn DeliveryPermit> {
        panic!("refused capture must not schedule delivery")
    }

    fn handoff_color(&self, _: usize) -> [u8; 3] {
        [0; 3]
    }

    fn sender_identity(&self, _: &UnixStream) -> String {
        "microphone-access-test".to_owned()
    }

    fn microphone_authorization(&self) -> Option<MicrophoneAuthorization> {
        Some(*self.authorization.lock())
    }

    fn microphone_grantee(&self) -> Option<MicrophoneGrantee> {
        Some(MicrophoneGrantee::Launcher(Some("Terminal".to_owned())))
    }

    fn request_microphone_access(&self) -> anyhow::Result<MicrophoneAuthorization> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        let mut authorization = self.authorization.lock();
        if *authorization == MicrophoneAuthorization::NotDetermined {
            *authorization = MicrophoneAuthorization::Requesting;
        }
        Ok(*authorization)
    }
}

struct SyntheticRecording(PathBuf);

impl Recorder for SyntheticRecording {
    fn input_signal(&mut self) -> anyhow::Result<Option<InputSignal>> {
        Ok(None)
    }

    fn request_stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn stop(self: Box<Self>) -> anyhow::Result<PathBuf> {
        Ok(self.0)
    }
}

fn wait_for(predicate: impl Fn(&StatusSnapshot) -> bool) -> StatusSnapshot {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = ipc::status().unwrap();
        if predicate(&status) {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "expected owner transition never arrived: {status:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn exercise_owner_authorization() {
    let owner = Arc::new(PromptingOwner {
        authorization: Mutex::new(MicrophoneAuthorization::NotDetermined),
        requests: AtomicUsize::new(0),
        starts: AtomicUsize::new(0),
        fail_start: AtomicBool::new(false),
    });
    let platform: Arc<dyn Platform> = owner.clone();
    let (ready, started) = mpsc::channel();
    let daemon = std::thread::spawn(move || {
        engine::run(Config::default(), false, platform, || {
            ready.send(()).unwrap()
        })
    });
    started.recv_timeout(Duration::from_secs(10)).unwrap();

    let authorization = || ipc::status().unwrap().microphone.unwrap().authorization;
    let press = || {
        ipc::command(Command::Start {
            postproc: None,
            handoff: None,
        })
        .unwrap()
    };
    assert_eq!(authorization(), MicrophoneAuthorization::NotDetermined);
    assert_eq!(owner.requests(), 0, "status requested access");
    let missing_model = press();
    assert!(!missing_model.ok);
    assert_eq!(
        missing_model.error.as_deref(),
        Some("local-model-unavailable")
    );
    assert_eq!(
        owner.requests(),
        0,
        "missing model requested microphone access"
    );

    // Every captured take is cancelled, so the configured endpoint is never called.
    let mut config = Config::default();
    config.stt.endpoint = Some("http://127.0.0.1:1/v1".to_owned());
    let config_file = cantrip_engine::paths::config_file().unwrap();
    fs::create_dir_all(config_file.parent().unwrap()).unwrap();
    fs::write(config_file, toml::to_string(&config).unwrap()).unwrap();
    assert!(ipc::command(Command::Reload).unwrap().ok);

    let allow = ipc::command(Command::AllowMicrophone).unwrap();
    assert!(allow.ok);
    assert_eq!(owner.requests(), 1);
    assert_eq!(authorization(), MicrophoneAuthorization::Requesting);
    assert_eq!(owner.starts(), 0, "explicit Allow started a recording");
    let adopted = press();
    assert!(adopted.ok, "{adopted:?}");
    assert!(adopted.message.as_ref().unwrap().contains("Terminal"));
    assert!(adopted
        .message
        .as_ref()
        .unwrap()
        .contains("everything it runs"));
    assert_eq!(
        owner.requests(),
        1,
        "Start opened a second explicit-Allow prompt"
    );
    let adopted_id = ipc::status().unwrap().operation_id;
    owner.answer(MicrophoneAuthorization::Authorized);
    let recording = wait_for(|s| s.state == StateKind::Recording);
    assert_eq!(recording.operation_id, adopted_id);
    assert_eq!(owner.starts(), 1);
    assert!(ipc::command(Command::Cancel).unwrap().ok);
    wait_for(|s| s.state == StateKind::Idle);

    // Already-authorized Start records without requesting again.
    assert!(press().ok);
    let authorized = ipc::status().unwrap();
    assert_eq!(authorized.state, StateKind::Recording);
    assert_ne!(authorized.operation_id, adopted_id);
    assert_eq!(owner.requests(), 1);
    assert_eq!(owner.starts(), 2);
    assert!(ipc::command(Command::Cancel).unwrap().ok);
    wait_for(|s| s.state == StateKind::Idle);

    owner.answer(MicrophoneAuthorization::NotDetermined);
    let start = press();
    assert!(start.ok, "{start:?}");
    assert_eq!(owner.requests(), 2);
    assert_eq!(start.stage, Some(Stage::RequestingMicrophone));
    let pending = ipc::status().unwrap();
    assert_eq!(pending.state, StateKind::Processing);
    assert!(pending.capabilities.cancel && pending.capabilities.stop);
    assert_eq!(owner.starts(), 2);
    assert!(ipc::command(Command::Ping).unwrap().ok);
    assert!(!press().ok, "another Start replaced the pending press");
    assert_eq!(owner.requests(), 2);
    owner.answer(MicrophoneAuthorization::Authorized);
    let recording = wait_for(|s| s.state == StateKind::Recording);
    assert_eq!(recording.operation_id, pending.operation_id);
    assert_eq!(
        owner.starts(),
        3,
        "same press did not start exactly one recording"
    );
    assert!(ipc::command(Command::Cancel).unwrap().ok);
    wait_for(|s| s.state == StateKind::Idle);
    let takes = ipc::recordings().unwrap();
    assert_eq!(takes.len(), 3);
    assert!(takes.iter().all(|take| take.audio_available));

    for cancel in [
        Command::Cancel,
        Command::Stop,
        Command::Toggle {
            postproc: None,
            handoff: None,
        },
    ] {
        owner.answer(MicrophoneAuthorization::NotDetermined);
        assert!(press().ok);
        let pending_id = ipc::status().unwrap().operation_id;
        let requests = owner.requests();
        let cancelled = ipc::command(cancel).unwrap();
        assert!(cancelled.ok);
        assert_eq!(cancelled.state, StateKind::Idle);
        let outcome = cancelled.outcome.unwrap();
        assert_eq!(outcome.operation_id, pending_id);
        assert_eq!(outcome.completeness, Completeness::Cancelled);
        assert_eq!(outcome.delivery, Delivery::Cancelled);
        assert_eq!(outcome.error, None);
        assert_eq!(outcome.artifacts.take_id, None);
        assert!(!outcome.artifacts.audio && !outcome.artifacts.text);
        owner.answer(MicrophoneAuthorization::Authorized);
        wait_for(|s| {
            s.microphone.as_ref().unwrap().authorization == MicrophoneAuthorization::Authorized
        });
        assert_eq!(owner.starts(), 3, "late Allow revived cancelled intent");
        assert_eq!(owner.requests(), requests, "polling requested access again");
        assert_eq!(ipc::recordings().unwrap().len(), 3);
    }

    for (answer, class) in [
        (MicrophoneAuthorization::Denied, "microphone-denied"),
        (MicrophoneAuthorization::Restricted, "microphone-restricted"),
    ] {
        owner.answer(answer);
        let requests = owner.requests();
        let refused = press();
        assert!(!refused.ok);
        assert_eq!(refused.error.as_deref(), Some(class));
        assert!(refused
            .message
            .unwrap()
            .contains("System Settings › Privacy & Security › Microphone"));
        assert_eq!(
            owner.requests(),
            requests,
            "answered access was requested again"
        );

        owner.answer(MicrophoneAuthorization::NotDetermined);
        assert!(press().ok);
        owner.answer(answer);
        let failed = wait_for(|s| s.state == StateKind::Idle);
        let outcome = failed.outcome.unwrap();
        assert_eq!(outcome.error.as_deref(), Some(class));
        assert!(outcome
            .message
            .contains("System Settings › Privacy & Security › Microphone"));
        assert_eq!(owner.starts(), 3);
    }

    owner.answer(MicrophoneAuthorization::NotDetermined);
    assert!(press().ok);
    let requests = owner.requests();
    owner.answer(MicrophoneAuthorization::NotDetermined);
    let unanswered = wait_for(|s| s.state == StateKind::Idle);
    assert_eq!(
        unanswered.outcome.unwrap().error.as_deref(),
        Some("microphone-request-failed")
    );
    assert_eq!(
        owner.requests(),
        requests,
        "an unanswered request was retried by polling"
    );

    owner.answer(MicrophoneAuthorization::Authorized);
    owner.fail_start.store(true, Ordering::SeqCst);
    assert!(!ipc::command(Command::Cancel).unwrap().ok);
    let prior_notice = ipc::status().unwrap().notice;
    let failed = press();
    assert_eq!(failed.error.as_deref(), Some("capture-failed"));
    assert!(failed.outcome.unwrap().operation_id.is_some());
    assert_eq!(ipc::status().unwrap().notice, prior_notice);
    assert_eq!(
        owner.requests(),
        requests,
        "authorized startup failure requested access"
    );

    owner.answer(MicrophoneAuthorization::NotDetermined);
    assert!(press().ok);
    let pending_id = ipc::status().unwrap().operation_id;
    owner.answer(MicrophoneAuthorization::Authorized);
    let failed = wait_for(|s| s.state == StateKind::Idle).outcome.unwrap();
    assert_eq!(failed.error.as_deref(), Some("capture-failed"));
    assert_eq!(failed.operation_id, pending_id);
    assert_eq!(failed.artifacts.take_id, None);
    assert_eq!(owner.starts(), 3);
    assert_eq!(ipc::recordings().unwrap().len(), 3);
    engine::request_shutdown();
    daemon.join().unwrap().unwrap();
}

#[test]
fn first_press_follows_the_owner_answer_and_cancellation() {
    const CHILD: &str = "CANTRIP_MICROPHONE_ACCESS_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        exercise_owner_authorization();
        return;
    }
    // Native Darwin sockets need a short pathname; no app-owned or operator
    // runtime directory is touched. A subprocess isolates environment/signals.
    #[cfg(target_os = "macos")]
    let base = std::path::PathBuf::from("/private/tmp");
    #[cfg(target_os = "linux")]
    let base = std::env::temp_dir().canonicalize().unwrap();
    let root = base.join(format!("ct-mic-{}", recovery::new_id()));
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let child = Process::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "first_press_follows_the_owner_answer_and_cancellation",
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
