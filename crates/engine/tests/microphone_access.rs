//! Microphone authorization belongs to the capture owner: status reports the
//! owner's own answer, and only the explicit command asks for it.

use cantrip_engine::config::Config;
use cantrip_engine::engine;
use cantrip_engine::ipc::{self, Command, MicrophoneAuthorization, MicrophoneGrantee};
use cantrip_engine::ports::{DeliveryPermit, Platform, Recorder};
use cantrip_engine::recovery;
use parking_lot::Mutex;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command as Process;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// A capture owner that refuses capture until authorized and keeps an explicit
/// request pending until the operator answers, as macOS does.
struct PromptingOwner {
    authorization: Mutex<MicrophoneAuthorization>,
    requests: AtomicUsize,
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
}

impl Platform for PromptingOwner {
    fn start_recording(&self, _: &Path, _: Option<&str>) -> anyhow::Result<Box<dyn Recorder>> {
        anyhow::bail!("microphone access not authorized")
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
        Some(MicrophoneGrantee::Cantrip)
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

fn exercise_owner_authorization() {
    let owner = Arc::new(PromptingOwner {
        authorization: Mutex::new(MicrophoneAuthorization::NotDetermined),
        requests: AtomicUsize::new(0),
    });
    let mut config = Config::default();
    // Reach native capture without a local model or a provider call.
    config.stt.endpoint = Some("http://127.0.0.1:1/v1".to_owned());
    let platform: Arc<dyn Platform> = owner.clone();
    let (ready, started) = mpsc::channel();
    let daemon = std::thread::spawn(move || {
        engine::run(config, false, platform, || ready.send(()).unwrap())
    });
    started.recv_timeout(Duration::from_secs(10)).unwrap();

    let authorization = || {
        ipc::status()
            .unwrap()
            .microphone
            .map(|access| access.authorization)
    };
    assert_eq!(
        authorization(),
        Some(MicrophoneAuthorization::NotDetermined)
    );
    // Neither status nor a refused start asks for access.
    let start = ipc::command(Command::Start {
        postproc: None,
        handoff: None,
    })
    .unwrap();
    assert!(!start.ok);
    assert_eq!(owner.requests(), 0);

    let allow = ipc::command(Command::AllowMicrophone).unwrap();
    assert!(allow.ok, "{allow:?}");
    assert_eq!(owner.requests(), 1);
    assert_eq!(authorization(), Some(MicrophoneAuthorization::Requesting));

    owner.answer(MicrophoneAuthorization::Authorized);
    let deadline = Instant::now() + Duration::from_secs(10);
    while authorization() != Some(MicrophoneAuthorization::Authorized) {
        assert!(
            Instant::now() < deadline,
            "status never reported the owner's answer"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(owner.requests(), 1);

    engine::request_shutdown();
    daemon.join().unwrap().unwrap();
}

#[test]
fn status_follows_the_owners_answer_to_its_explicit_request() {
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
            "status_follows_the_owners_answer_to_its_explicit_request",
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
