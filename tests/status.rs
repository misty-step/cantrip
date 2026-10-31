//! Status next-step recommendations cross the actual CLI/typed IPC boundary.
#![cfg(unix)]

use cantrip_engine::config::HudConfig;
use cantrip_engine::ipc::{
    Artifacts, Capabilities, Cleanup, Completeness, Delivery, StateKind, StatusSnapshot,
    TerminalOutcome,
};
use cantrip_engine::stt::CloudFailure;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[test]
fn status_recommends_recovery_only_for_confirmed_undelivered_or_incomplete_takes() {
    // Keep the Unix socket short on macOS; all fixture directories are owner-only.
    let root = PathBuf::from("/tmp").join(format!(
        "ct-status-{}-{:x}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket_dir = root.join("cantrip");
    std::fs::create_dir(&socket_dir).unwrap();
    std::fs::set_permissions(&socket_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = socket_dir.join("cantrip.sock");
    for (completeness, delivery, recovery_expected) in [
        (Completeness::Complete, Delivery::Pasted, false),
        (Completeness::Complete, Delivery::Copied, false),
        (Completeness::Complete, Delivery::Uncertain, false),
        (Completeness::Partial, Delivery::Uncertain, false),
        (Completeness::Partial, Delivery::Deferred, true),
    ] {
        let status = StatusSnapshot {
            epoch: "synthetic-status".to_owned(),
            operation_id: None,
            operation_kind: None,
            state: StateKind::Idle,
            elapsed: 0,
            signal: None,
            stage: None,
            outcome: Some(TerminalOutcome {
                event_id: 1,
                operation_id: Some("synthetic-operation".to_owned()),
                message: "Synthetic settled recognition result.".to_owned(),
                completeness,
                delivery,
                cleanup: Cleanup::Off,
                error: None,
                cloud_failure: Some(CloudFailure::Timeout),
                artifacts: Artifacts {
                    take_id: Some("synthetic-status-take".to_owned()),
                    audio: true,
                    text: true,
                },
                dismissed: false,
                handoff: None,
            }),
            notice: None,
            pending_recordings: 1,
            attention: false,
            capabilities: Capabilities::default(),
            hud: HudConfig::default(),
            handoff: None,
            microphone: None,
        };
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "status consumer did not connect");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("status fixture accept: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = String::new();
            BufReader::new(&stream).read_line(&mut request).unwrap();
            serde_json::to_writer(&mut stream, &status).unwrap();
            stream.write_all(b"\n").unwrap();
        });
        let output = Command::new(env!("CARGO_BIN_EXE_cantrip"))
            .arg("status")
            .env("XDG_RUNTIME_DIR", &root)
            .output()
            .unwrap();
        server.join().unwrap();
        std::fs::remove_file(&socket).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("recording: synthetic-status-take"));
        assert!(text.contains("cantrip recordings"));
        assert_eq!(
            text.contains("cantrip copy synthetic-status-take"),
            recovery_expected,
            "{completeness:?}/{delivery:?}: {text}"
        );
        assert_eq!(
            text.contains("cantrip recover --id synthetic-status-take"),
            recovery_expected,
            "{completeness:?}/{delivery:?}: {text}"
        );
        if delivery == Delivery::Uncertain {
            assert!(text.contains("destination or clipboard"));
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}
