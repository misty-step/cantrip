//! Mode selection crosses real IPC, capture retention, Reload, STT and recovery.
//! Only native capture/delivery and the provider are synthetic; no desktop is touched.

use cantrip_engine::audio::{InputSignal, Pcm16WavWriter};
use cantrip_engine::config::{
    Config, Mode, ModeModelConfig, PostprocConfig, SttConfig, VocabularyEntry,
};
use cantrip_engine::delivery::{
    ClipboardFallback, InjectionFailure, InjectionMode, InjectionOutcome,
};
use cantrip_engine::ipc::{self, Command, Completeness, StateKind, StatusSnapshot};
use cantrip_engine::ports::{DeliveryPermit, Platform, Recorder};
use cantrip_engine::{engine, paths, recovery};
use parking_lot::Mutex;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

struct SyntheticPlatform(Arc<Mutex<Vec<String>>>);
struct SyntheticRecording(PathBuf);
struct SyntheticClipboard(Arc<Mutex<Vec<String>>>);

impl Recorder for SyntheticRecording {
    fn input_signal(&mut self) -> Option<InputSignal> {
        None
    }
    fn request_stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn stop(self: Box<Self>) -> anyhow::Result<PathBuf> {
        Ok(self.0)
    }
}

impl DeliveryPermit for SyntheticClipboard {
    fn clipboard_fallback(&self) -> ClipboardFallback {
        ClipboardFallback::ExplicitOnly
    }
    fn deliver(
        &self,
        text: &str,
        mode: InjectionMode,
        _: &AtomicBool,
    ) -> Result<InjectionOutcome, InjectionFailure> {
        assert_eq!(mode, InjectionMode::Clipboard);
        self.0.lock().push(text.to_owned());
        Ok(InjectionOutcome::Clipboard)
    }
}

impl Platform for SyntheticPlatform {
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
        Ok(Box::new(SyntheticRecording(path.to_owned())))
    }
    fn prepare_delivery(&self) {}
    fn delivery_permit(&self) -> Arc<dyn DeliveryPermit> {
        Arc::new(SyntheticClipboard(self.0.clone()))
    }
    fn handoff_color(&self, _: usize) -> [u8; 3] {
        [0; 3]
    }
    fn sender_identity(&self, _: &UnixStream) -> String {
        "synthetic-modes-test".into()
    }
}

fn settled() -> StatusSnapshot {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = ipc::status().unwrap();
        if status.state == StateKind::Idle {
            return status;
        }
        assert!(Instant::now() < deadline, "mode workflow did not settle");
        thread::sleep(Duration::from_millis(20));
    }
}

fn save_and_reload(config: &Config) {
    let path = paths::config_file().unwrap();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, toml::to_string(config).unwrap()).unwrap();
    assert!(ipc::command(Command::Reload).unwrap().ok);
}

fn exercise_modes() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (request_tx, request_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let provider = thread::spawn(move || {
        for attempt in 0..6 {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "missing recovery STT request");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("provider fixture accept: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(
                line.trim(),
                "POST /audio/transcriptions HTTP/1.1",
                "recorded alias-only models must not turn into live-default LLM calls"
            );
            let mut length = None;
            loop {
                line.clear();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':') {
                    if name.eq_ignore_ascii_case("content-length") {
                        length = Some(value.trim().parse::<usize>().unwrap());
                    }
                }
            }
            let mut body = vec![0; length.unwrap()];
            reader.read_exact(&mut body).unwrap();
            drop(reader);
            request_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            if attempt % 2 == 0 {
                let response = r#"{"text":"Gantrip"}"#;
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
            } else {
                stream.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            }
        }
    });
    let alias_only = ModeModelConfig {
        model: String::new(),
        reasoning_effort: None,
    };
    let config = Config {
        injection: InjectionMode::Clipboard,
        stt: SttConfig {
            model: "fixture-stt".into(),
            endpoint: Some(endpoint.clone()),
            api_key_id: None,
        },
        postproc: PostprocConfig {
            mode: Mode::Raw,
            endpoint,
            api_key_id: None,
            clean: alias_only.clone(),
            elevate: alias_only,
            ..Default::default()
        },
        vocabulary: vec![VocabularyEntry::Aliased {
            term: "Cantrip".into(),
            aliases: vec!["Gantrip".into()],
        }],
        ..Default::default()
    };
    let delivered = Arc::new(Mutex::new(Vec::new()));
    let platform = Arc::new(SyntheticPlatform(delivered.clone()));
    let (ready_tx, ready_rx) = mpsc::channel();
    let initial = config.clone();
    let daemon =
        thread::spawn(move || engine::run(initial, false, platform, || ready_tx.send(()).unwrap()));
    ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();

    for mode in Mode::ALL {
        save_and_reload(&config);
        assert!(
            ipc::command(Command::Start {
                postproc: Some(mode),
                handoff: None
            })
            .unwrap()
            .ok
        );
        let recording = ipc::status().unwrap();
        assert_eq!(recording.state, StateKind::Recording);
        assert_eq!(recording.pipeline.as_ref().unwrap().mode, mode);
        let mut live = config.clone();
        live.postproc.mode = if mode == Mode::Raw {
            Mode::Elevate
        } else {
            Mode::Raw
        };
        live.postproc.clean.model = "live-clean-must-not-run".into();
        live.postproc.elevate.model = "live-elevate-must-not-run".into();
        save_and_reload(&live);
        assert_eq!(ipc::status().unwrap().pipeline, recording.pipeline);
        assert!(ipc::command(Command::Cancel).unwrap().ok);
        let cancelled = settled().outcome.unwrap();
        assert_eq!(cancelled.completeness, Completeness::Cancelled);
        let id = cancelled.artifacts.take_id.unwrap();
        assert!(cancelled.artifacts.audio);
        let record_path = paths::transcript_history_dir()
            .unwrap()
            .join(format!("{id}.json"));
        let before: serde_json::Value =
            serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
        assert_eq!(before["mode"], mode.as_str());
        assert_eq!(before["mode_inferred"], false);

        for failed in [false, true] {
            assert!(
                ipc::command(Command::Recover {
                    id: Some(id.clone()),
                    local: false,
                    clipboard: true
                })
                .unwrap()
                .ok
            );
            request_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            let active = ipc::status().unwrap();
            assert_eq!(active.pipeline.as_ref().unwrap().mode, mode);
            assert_eq!(active.pipeline.as_ref().unwrap().llm_model, None);
            release_tx.send(()).unwrap();
            let outcome = settled().outcome.unwrap();
            assert_eq!(
                outcome.completeness,
                if failed {
                    Completeness::Failed
                } else {
                    Completeness::Complete
                }
            );
            let after: serde_json::Value =
                serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
            assert_eq!(after["mode"], before["mode"]);
            assert_eq!(after["mode_model"], before["mode_model"]);
            assert_eq!(after["mode_inferred"], false);
        }
        assert_eq!(
            recovery::read_text(&id).unwrap(),
            if mode == Mode::Raw {
                "Gantrip"
            } else {
                "Cantrip"
            }
        );
    }
    assert_eq!(*delivered.lock(), ["Gantrip", "Cantrip", "Cantrip"]);
    engine::request_shutdown();
    daemon.join().unwrap().unwrap();
    provider.join().unwrap();
}

#[test]
fn recorded_modes_drive_recovery_across_reload() {
    const CHILD: &str = "CANTRIP_MODES_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        exercise_modes();
        return;
    }
    // Isolate native socket paths, signals, models, config and retained fixtures.
    #[cfg(target_os = "macos")]
    let base = PathBuf::from("/private/tmp");
    #[cfg(target_os = "linux")]
    let base = std::env::temp_dir().canonicalize().unwrap();
    let root = base.join(format!("ct-mode-{}", recovery::new_id()));
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let child = Process::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "recorded_modes_drive_recovery_across_reload",
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
