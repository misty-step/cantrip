//! Optional cleanup honors its configured shared budget and preserves usable text.

use cantrip_engine::config::{Config, PostprocConfig, SttConfig};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const RAW: &str = "preserve these original dictated words";
const CLEAN: &str = "A cleanup candidate, not the original words.";
const CHAT: &str =
    r#"{"choices":[{"message":{"content":"A cleanup candidate, not the original words."}}]}"#;
const TRIAGE: &str =
    r#"{"model":"typesafe/jev-1.13","answers":{"needs_cleanup":{"type":"noul","noul":0.9}}}"#;
const REJECT: &str = r#"{"model":"typesafe/jev-1.13","answers":{"answered_question":{"type":"noul","noul":0.95},"content_fidelity":{"type":"score","score":0.2}}}"#;

enum Reply {
    Json(Duration, &'static str),
    StalledHeaders,
    StalledBody,
}

struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn assert_budget_outcome(
    timeout_ms: u64,
    passes: u8,
    decision: bool,
    replies: Vec<Reply>,
    expected: &str,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let server = thread::spawn(move || {
        // Bound fixture lifetime if a regression never makes a required
        // request or never closes a stalled response.
        let deadline = Instant::now() + Duration::from_secs(30);
        for reply in replies {
            let mut stream = loop {
                assert!(Instant::now() < deadline, "missing cleanup fixture request");
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accepting cleanup fixture request: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            let remaining = deadline.saturating_duration_since(Instant::now());
            stream.set_read_timeout(Some(remaining)).unwrap();
            stream.set_write_timeout(Some(remaining)).unwrap();
            let mut reader = BufReader::new(&stream);
            let mut content_length = None;
            loop {
                let mut line = String::new();
                assert!(
                    reader.read_line(&mut line).unwrap() > 0,
                    "request ended before its headers"
                );
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':') {
                    if name.eq_ignore_ascii_case("content-length") {
                        content_length = Some(value.trim().parse::<usize>().unwrap());
                    }
                }
            }
            let mut body = vec![0; content_length.unwrap()];
            reader.read_exact(&mut body).unwrap();
            drop(reader);
            match reply {
                Reply::Json(delay, body) => {
                    thread::sleep(delay);
                    // A shared-budget test may intentionally finish this
                    // valid reply after the client has already timed out.
                    let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                }
                Reply::StalledHeaders | Reply::StalledBody => {
                    if matches!(reply, Reply::StalledBody) {
                        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n{\"choices\":").unwrap();
                    }
                    // Hold the HTTP boundary open until the client enforces its deadline.
                    assert_eq!(stream.read(&mut [0]).unwrap(), 0);
                    break;
                }
            }
        }
        let observe_until = Instant::now() + Duration::from_millis(200);
        while Instant::now() < observe_until {
            assert!(
                matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
                "cleanup must not start unplanned requests after its outcome"
            );
            thread::sleep(Duration::from_millis(5));
        }
    });
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let root = Fixture(std::env::temp_dir().join(format!(
        "cantrip-cleanup-budget-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )));
    let config_dir = root.0.join("config/cantrip");
    std::fs::create_dir_all(&config_dir).unwrap();
    let cfg = Config {
        stt: SttConfig {
            endpoint: Some(endpoint.clone()),
            model: "test-stt".to_owned(),
            api_key_id: None,
        },
        postproc: PostprocConfig {
            enabled: true,
            endpoint: endpoint.clone(),
            model: "test-cleanup".to_owned(),
            timeout_ms,
            passes,
            min_chars: 0,
            decision_model: decision.then(|| "typesafe/jev-1.13".to_owned()),
            decision_endpoint: decision.then_some(endpoint),
            ..Default::default()
        },
        ..Default::default()
    };
    std::fs::write(
        config_dir.join("config.toml"),
        toml::to_string(&cfg).unwrap(),
    )
    .unwrap();
    let wav = root.0.join("recording.wav");
    let mut writer = hound::WavWriter::create(
        &wav,
        hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        },
    )
    .unwrap();
    for _ in 0..160 {
        writer.write_sample(1_i16).unwrap();
    }
    writer.finalize().unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_cantrip"))
        .arg("transcribe")
        .arg(&wav)
        .env("XDG_CONFIG_HOME", root.0.join("config"))
        .env("XDG_STATE_HOME", root.0.join("state"))
        .env("XDG_DATA_HOME", root.0.join("data"))
        .env("RUST_LOG", "warn")
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), expected);
    assert!(!String::from_utf8(output.stderr).unwrap().contains(RAW));
    let records: Vec<_> = std::fs::read_dir(root.0.join("state/cantrip/transcripts"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    assert_eq!(records.len(), 1);
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&records[0]).unwrap()).unwrap();
    assert_eq!(saved["raw_transcript"], RAW);
    assert_eq!(
        saved["postprocessed_transcript"],
        if expected == RAW {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(expected.to_owned())
        }
    );
    assert!(wav.is_file());
}

fn stt_reply() -> Reply {
    Reply::Json(
        Duration::ZERO,
        r#"{"text":"preserve these original dictated words"}"#,
    )
}

#[test]
fn cleanup_honors_an_explicit_budget_above_five_seconds() {
    assert_budget_outcome(
        8_000,
        1,
        false,
        vec![stt_reply(), Reply::Json(Duration::from_secs(6), CHAT)],
        CLEAN,
    );
}

#[test]
fn cleanup_body_honors_a_smaller_explicit_budget() {
    assert_budget_outcome(1_000, 1, false, vec![stt_reply(), Reply::StalledBody], RAW);
}

#[test]
fn cleanup_passes_share_one_budget_and_discard_an_unfinished_chain() {
    assert_budget_outcome(
        2_000,
        2,
        false,
        vec![
            stt_reply(),
            Reply::Json(Duration::from_millis(1_200), CHAT),
            Reply::Json(Duration::from_millis(1_200), CHAT),
        ],
        RAW,
    );
}

#[test]
fn cleanup_triage_timeout_returns_raw_without_starting_a_pass() {
    assert_budget_outcome(
        1_000,
        1,
        true,
        vec![stt_reply(), Reply::StalledHeaders],
        RAW,
    );
}

#[test]
fn cleanup_watchdog_receives_only_the_remaining_budget() {
    assert_budget_outcome(
        2_000,
        1,
        true,
        vec![
            stt_reply(),
            Reply::Json(Duration::from_millis(300), TRIAGE),
            Reply::Json(Duration::from_millis(300), CHAT),
            Reply::Json(Duration::from_millis(1_600), REJECT),
        ],
        CLEAN,
    );
}
