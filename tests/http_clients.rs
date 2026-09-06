//! HTTP round-trip contracts for post-processing and remote transcription
//! against a local mock OpenAI-compatible server.

use cantrip::config::PostprocConfig;
use cantrip::postproc;
use cantrip::stt;
use std::io::{BufRead, BufReader, Cursor, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;

/// One-shot HTTP server: accepts a single request, captures it, sends `response`.
fn mock_server(response: String) -> (String, thread::JoinHandle<CapturedRequest>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binding mock server");
    let addr = listener.local_addr().expect("mock server address");
    let handle = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accepting mock connection");
        let captured = read_request(&stream);
        let mut stream = stream;
        stream
            .write_all(response.as_bytes())
            .expect("writing mock response");
        captured
    });
    (format!("http://{addr}"), handle)
}

struct CapturedRequest {
    request_line: String,
    headers: Vec<String>,
    body: Vec<u8>,
}

impl CapturedRequest {
    fn header(&self, name: &str) -> Option<&str> {
        let prefix = format!("{}:", name.to_ascii_lowercase());
        self.headers
            .iter()
            .find(|line| line.to_ascii_lowercase().starts_with(&prefix))
            .map(|line| line[prefix.len()..].trim())
    }

    fn part(&self, name: &str) -> &[u8] {
        let content_type = self.header("content-type").expect("multipart content type");
        let boundary = content_type
            .strip_prefix("multipart/form-data; boundary=")
            .expect("multipart boundary");
        let disposition = format!("name=\"{name}\"");
        let start = self
            .body
            .windows(disposition.len())
            .position(|bytes| bytes == disposition.as_bytes())
            .expect("multipart field");
        let header_end = self.body[start..]
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .expect("multipart field header");
        let value = &self.body[start + header_end + 4..];
        let delimiter = format!("\r\n--{boundary}");
        let end = value
            .windows(delimiter.len())
            .position(|bytes| bytes == delimiter.as_bytes())
            .expect("multipart field terminator");
        &value[..end]
    }
}

fn read_request(stream: &TcpStream) -> CapturedRequest {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .expect("reading request line");
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).expect("reading header line");
        let line = line.trim_end().to_owned();
        if line.is_empty() {
            break;
        }
        headers.push(line);
    }
    let length: usize = headers
        .iter()
        .find(|line| line.to_ascii_lowercase().starts_with("content-length:"))
        .and_then(|line| line.split(':').nth(1))
        .and_then(|value| value.trim().parse().ok())
        .expect("content-length header");
    let mut body = vec![0_u8; length];
    reader.read_exact(&mut body).expect("reading request body");
    CapturedRequest {
        request_line: request_line.trim_end().to_owned(),
        headers,
        body,
    }
}

fn ok_json(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn postproc_config(endpoint: String) -> PostprocConfig {
    PostprocConfig {
        enabled: true,
        endpoint,
        model: "test-model".to_owned(),
        api_key_id: None,
        reasoning_effort: None,
        timeout_ms: 5_000,
        passes: 2,
        min_chars: 0,
        instructions: "Keep numerals as digits.".to_owned(),
    }
}

#[test]
fn refine_round_trip_sends_contract_request_and_strips_model_wrappers() {
    let response = ok_json(
        r#"{"choices":[{"message":{"content":"<think>internal chain</think>Clean transcript:\nHello, Cantrip world."}}],"usage":{"prompt_tokens":7,"completion_tokens":2,"total_tokens":9,"cost":0.0012,"completion_tokens_details":{"reasoning_tokens":1},"prompt_tokens_details":{"cached_tokens":3}}}"#,
    );
    let (endpoint, server) = mock_server(response);
    let mut cfg = postproc_config(endpoint);
    // This test pins the single-round wire contract; the multi-round chain is
    // covered by `refine_two_passes_chains_output_and_sends_verify_prompt`.
    cfg.passes = 1;
    let vocabulary = vec!["Cantrip".to_owned(), "PipeWire".to_owned()];

    let refined = postproc::refine("hello cantrip world", &cfg, &vocabulary, Some("sk-test"))
        .expect("refine should succeed");
    assert_eq!(refined.text, "Hello, Cantrip world.");
    let usage = refined.usage.expect("provider usage should survive");
    assert_eq!(usage.prompt_tokens, 7);
    assert_eq!(usage.completion_tokens, 2);
    assert_eq!(usage.total_tokens, 9);
    assert_eq!(usage.reasoning_tokens, 1);
    assert_eq!(usage.cached_tokens, 3);
    assert_eq!(usage.requests, 1);
    assert_eq!(usage.responses_with_usage, 1);
    assert_eq!(usage.reported_cost_usd, Some(0.0012));

    let request = server.join().expect("mock server thread");
    assert_eq!(request.request_line, "POST /chat/completions HTTP/1.1");
    assert_eq!(request.header("authorization"), Some("Bearer sk-test"));
    assert_eq!(request.header("content-type"), Some("application/json"));

    let body: serde_json::Value =
        serde_json::from_slice(&request.body).expect("request body is JSON");
    assert_eq!(body["model"], "test-model");
    assert!(body.get("temperature").is_none());
    assert_eq!(body["messages"][0]["role"], "system");
    let system = body["messages"][0]["content"]
        .as_str()
        .expect("system prompt");
    assert!(system.contains("Cantrip, PipeWire"));
    assert!(system.contains("Keep numerals as digits."));
    assert_eq!(body["messages"][1]["role"], "user");
    assert_eq!(
        body["messages"][1]["content"],
        "Source:\nhello cantrip world"
    );
}

/// Mock server accepting `n` sequential requests, answering each in order with
/// its matching response. Returns the captured requests.
fn mock_server_multi(responses: Vec<String>) -> (String, thread::JoinHandle<Vec<CapturedRequest>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binding mock server");
    let addr = listener.local_addr().expect("mock server address");
    let handle = thread::spawn(move || {
        let mut captured = Vec::new();
        for response in responses {
            let (stream, _) = listener.accept().expect("accepting mock connection");
            captured.push(read_request(&stream));
            let mut stream = stream;
            stream
                .write_all(response.as_bytes())
                .expect("writing mock response");
        }
        captured
    });
    (format!("http://{addr}"), handle)
}

#[test]
fn refine_two_passes_chains_output_and_sends_verify_prompt() {
    let (endpoint, server) = mock_server_multi(vec![
        ok_json(
            r#"{"choices":[{"message":{"content":"First-pass text. The Exa AP and the CL expose methods."}}]}"#,
        ),
        ok_json(
            r#"{"choices":[{"message":{"content":"First-pass text. The Exa API and the CLI expose methods."}}]}"#,
        ),
    ]);
    let cfg = postproc_config(endpoint); // passes = 2
    let first = "Initial text. The Exa AP and the CL expose methods.";

    let refined = postproc::refine(first, &cfg, &[], None).expect("two-pass refine should succeed");
    assert_eq!(
        refined.text,
        "First-pass text. The Exa API and the CLI expose methods."
    );

    let requests = server.join().expect("mock server thread");
    assert_eq!(requests.len(), 2, "two passes must make two requests");

    let pass1: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
        pass1["messages"][1]["content"],
        postproc::build_user_prompt(first)
    );
    let system1 = pass1["messages"][0]["content"].as_str().unwrap();
    assert!(
        system1.contains("You clean speech-to-text transcripts"),
        "pass 1 uses the cleanup prompt"
    );

    let pass2: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    // The source of pass 2 is chained from pass 1's output.
    assert_eq!(
        pass2["messages"][1]["content"],
        postproc::build_user_prompt("First-pass text. The Exa AP and the CL expose methods.")
    );
    let system2 = pass2["messages"][0]["content"].as_str().unwrap();
    assert!(
        system2.contains("final check of a speech-to-text transcript"),
        "pass 2 must use the verify prompt, got: {system2}"
    );
    assert!(
        !system2.contains("Examples:"),
        "pass 2 must use only the focused verify prompt"
    );
}

#[test]
fn refine_http_error_reports_status_without_response_body() {
    static RESPONSE: &str = "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: 43\r\nConnection: close\r\n\r\n{\"error\":\"SECRET-MARKER transcript echoed\"}";
    let (endpoint, server) = mock_server(RESPONSE.to_owned());
    let cfg = postproc_config(endpoint);

    let error =
        postproc::refine("some dictated words", &cfg, &[], None).expect_err("HTTP 500 must fail");
    let message = format!("{error:#}");
    assert!(message.contains("HTTP 500"), "got: {message}");
    assert!(
        !message.contains("SECRET-MARKER"),
        "error must not embed the response body: {message}"
    );
    server.join().expect("mock server thread");
}

struct WavFixture {
    path: PathBuf,
}

impl WavFixture {
    fn new(spec: hound::WavSpec, frames: usize) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "cantrip-http-{}-{}.wav",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut writer = hound::WavWriter::create(&path, spec).expect("creating fixture WAV");
        for index in 0..frames * spec.channels as usize {
            match spec.sample_format {
                hound::SampleFormat::Int => writer
                    .write_sample(pcm_sample(index, spec.bits_per_sample))
                    .expect("writing PCM sample"),
                hound::SampleFormat::Float => writer
                    .write_sample(float_sample(index))
                    .expect("writing float sample"),
            }
        }
        writer.finalize().expect("finalizing fixture WAV");
        // Hound 3.5 omits the RIFF pad for an odd-sized data chunk.
        let file_len = std::fs::metadata(&path).unwrap().len();
        if file_len % 2 == 1 {
            let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            file.seek(SeekFrom::End(0)).unwrap();
            file.write_all(&[0]).unwrap();
            file.seek(SeekFrom::Start(4)).unwrap();
            file.write_all(&((file_len + 1 - 8) as u32).to_le_bytes())
                .unwrap();
        }
        Self { path }
    }
}

impl Drop for WavFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn native_spec() -> hound::WavSpec {
    hound::WavSpec {
        channels: 1,
        sample_rate: 16_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    }
}

fn pcm_sample(index: usize, bits: u16) -> i32 {
    // Exercise both signs and all bit patterns without repeating at each
    // likely split point, so boundary gaps, overlaps, and reordering fail.
    let value = (index as u32).wrapping_mul(0x9e37_79b9).rotate_left(11);
    (value as i32) >> (32 - bits)
}

fn float_sample(index: usize) -> f32 {
    pcm_sample(index, 32) as f32 / 2_147_483_648.0
}

fn upload_error() -> String {
    let body = r#"{"error":"PRIVATE-RESPONSE-MARKER sk-private-http-test"}"#;
    format!(
        "HTTP/1.1 413 Payload Too Large\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Respond until all source frames arrive, or fail a chosen request. The
/// independent decoder makes malformed or unbounded uploads fail at the
/// HTTP boundary, without predicting implementation-specific split points.
fn transcription_server(
    spec: hound::WavSpec,
    frames: usize,
    fail_at: Option<usize>,
) -> (String, thread::JoinHandle<Vec<CapturedRequest>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binding transcription server");
    let addr = listener.local_addr().expect("transcription server address");
    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        let mut received = 0;
        while received < frames {
            let (mut stream, _) = listener.accept().expect("accepting transcription request");
            let request = read_request(&stream);
            assert!(
                request.body.len() < 25_000_000,
                "upload must stay below the provider cap"
            );
            let reader = hound::WavReader::new(Cursor::new(request.part("file")))
                .expect("uploaded WAV must decode");
            assert_eq!(reader.spec(), spec);
            let chunk_frames = reader.duration() as usize;
            assert!(chunk_frames > 0, "empty WAV chunks must not be uploaded");
            assert!(chunk_frames <= spec.sample_rate as usize * 33);
            received += chunk_frames;
            assert!(
                received <= frames,
                "requests must not duplicate source frames"
            );
            requests.push(request);
            let index = requests.len();
            let failed = fail_at == Some(index);
            let response = if failed {
                upload_error()
            } else {
                ok_json(&format!(r#"{{"text":" chunk-{index} "}}"#))
            };
            stream
                .write_all(response.as_bytes())
                .expect("writing transcription response");
            if failed {
                break;
            }
        }
        requests
    });
    (format!("http://{addr}/v1/"), handle)
}

fn assert_audio_coverage(requests: &[CapturedRequest], spec: hound::WavSpec, frames: usize) {
    let mut index = 0;
    for request in requests {
        assert_eq!(
            request.request_line,
            "POST /v1/audio/transcriptions HTTP/1.1"
        );
        assert_eq!(
            request.header("authorization"),
            Some("Bearer sk-private-http-test")
        );
        assert_eq!(request.part("model"), b"test-stt-model");
        assert_eq!(request.part("prompt"), b"Cantrip, Parakeet");
        assert_eq!(request.part("response_format"), b"json");
        let wav = request.part("file");
        let declared_len = u32::from_le_bytes(wav[4..8].try_into().unwrap()) as usize + 8;
        assert_eq!(
            declared_len,
            wav.len(),
            "RIFF size must include chunk padding"
        );
        let mut reader = hound::WavReader::new(Cursor::new(wav)).expect("decoding uploaded WAV");
        match spec.sample_format {
            hound::SampleFormat::Int => {
                for sample in reader.samples::<i32>() {
                    assert_eq!(
                        sample.expect("decoding PCM frame"),
                        pcm_sample(index, spec.bits_per_sample),
                        "source sample {index} must arrive exactly once in order"
                    );
                    index += 1;
                }
            }
            hound::SampleFormat::Float => {
                for sample in reader.samples::<f32>() {
                    assert_eq!(
                        sample.expect("decoding float frame").to_bits(),
                        float_sample(index).to_bits(),
                        "source float sample {index} must remain bit-exact"
                    );
                    index += 1;
                }
            }
        }
    }
    assert_eq!(index, frames * spec.channels as usize);
}

#[test]
fn transcribe_remote_round_trip_preserves_short_wav() {
    let fixture = WavFixture::new(native_spec(), 1_001);
    let original = std::fs::read(&fixture.path).expect("reading short fixture");
    let (endpoint, server) = mock_server(ok_json(r#"{"text":" hello from the cloud "}"#));
    let mut progress = Vec::new();
    let transcript = stt::transcribe_remote(
        &fixture.path,
        &endpoint,
        "whisper-large-v3-turbo",
        &["Cantrip".to_owned()],
        Some("sk-cloud"),
        |chunk| progress.push(chunk),
    )
    .expect("remote transcription should succeed");
    assert_eq!(
        transcript,
        stt::Transcript::Complete("hello from the cloud".to_owned())
    );
    assert_eq!(progress, [stt::ChunkProgress { index: 1, total: 1 }]);

    let request = server.join().expect("mock server thread");
    assert_eq!(request.request_line, "POST /audio/transcriptions HTTP/1.1");
    assert_eq!(request.header("authorization"), Some("Bearer sk-cloud"));
    assert_eq!(request.part("model"), b"whisper-large-v3-turbo");
    assert_eq!(request.part("prompt"), b"Cantrip");
    assert_eq!(request.part("response_format"), b"json");
    assert_eq!(request.part("file"), original);
    let decoded = hound::WavReader::new(Cursor::new(request.part("file")))
        .expect("short uploaded WAV must decode");
    assert_eq!(decoded.spec(), native_spec());
    assert_eq!(decoded.duration(), 1_001);
}

fn assert_bounded_transcription(spec: hound::WavSpec, frames: usize, exceeds_old_cap: bool) {
    let fixture = WavFixture::new(spec, frames);
    if exceeds_old_cap {
        assert!(std::fs::metadata(&fixture.path).unwrap().len() > 25_000_000);
    }
    let (endpoint, server) = transcription_server(spec, frames, None);
    let mut progress = Vec::new();
    let transcript = stt::transcribe_remote(
        &fixture.path,
        &endpoint,
        "test-stt-model",
        &["Cantrip".to_owned(), "Parakeet".to_owned()],
        Some("sk-private-http-test"),
        |chunk| progress.push(chunk),
    )
    .expect("bounded transcription should succeed");
    let requests = server.join().expect("transcription server thread");
    assert_audio_coverage(&requests, spec, frames);
    let total = requests.len() as u32;
    assert!(
        total > 1,
        "long or oversized audio must use multiple requests"
    );
    assert_eq!(
        transcript,
        stt::Transcript::Complete(
            (1..=total)
                .map(|index| format!("chunk-{index}"))
                .collect::<Vec<_>>()
                .join(" ")
        )
    );
    assert_eq!(
        progress,
        (1..=total)
            .map(|index| stt::ChunkProgress { index, total })
            .collect::<Vec<_>>()
    );
}

#[test]
fn transcribe_remote_bounds_888_second_wav_without_losing_boundary_samples() {
    assert_bounded_transcription(native_spec(), 888 * 16_000, true);
}

#[test]
fn transcribe_remote_preserves_non_native_stereo_frames() {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 44_100,
        bits_per_sample: 24,
        ..native_spec()
    };
    assert_bounded_transcription(spec, 61 * 44_100 + 1, false);
}

#[test]
fn transcribe_remote_bounds_bytes_even_for_short_high_bandwidth_wav() {
    let spec = hound::WavSpec {
        channels: 8,
        sample_rate: 192_000,
        bits_per_sample: 32,
        ..native_spec()
    };
    assert_bounded_transcription(spec, 5 * 192_000, true);
}

#[test]
fn transcribe_remote_preserves_float_samples_across_chunks() {
    let spec = hound::WavSpec {
        sample_rate: 8_000,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
        ..native_spec()
    };
    assert_bounded_transcription(spec, 31 * 8_000, false);
}

#[test]
fn transcribe_remote_pads_odd_sized_pcm_chunks_without_adding_samples() {
    let spec = hound::WavSpec {
        sample_rate: 8_001,
        bits_per_sample: 8,
        ..native_spec()
    };
    assert_bounded_transcription(spec, 31 * 8_001, false);
}

#[derive(Clone)]
struct LogCapture(mpsc::Sender<Vec<u8>>);

impl Write for LogCapture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .send(bytes.to_vec())
            .map_err(|_| std::io::Error::other("log capture closed"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn transcribe_remote_keeps_partial_text_and_progress_without_leaking_failure_body() {
    let spec = native_spec();
    let fixture = WavFixture::new(spec, 70 * 16_000);
    let (endpoint, server) = transcription_server(spec, 70 * 16_000, Some(2));
    let (log_tx, log_rx) = mpsc::channel();
    let capture = LogCapture(log_tx);
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || capture.clone())
        .finish();
    let mut progress = Vec::new();
    let transcript = tracing::subscriber::with_default(subscriber, || {
        stt::transcribe_remote(
            &fixture.path,
            &endpoint,
            "test-stt-model",
            &["Cantrip".to_owned(), "Parakeet".to_owned()],
            Some("sk-private-http-test"),
            |chunk| progress.push(chunk),
        )
    })
    .expect("later failure should preserve earlier text");
    let requests = server.join().expect("transcription server thread");
    assert_eq!(
        requests.len(),
        2,
        "stop after the failed request without retrying"
    );
    let total = progress[0].total;
    assert!(total > 2, "failure must leave untranscribed audio");
    assert_eq!(
        transcript,
        stt::Transcript::Partial {
            text: "chunk-1".to_owned(),
            failed_at: 2,
            total
        }
    );
    assert_eq!(
        progress,
        [
            stt::ChunkProgress { index: 1, total },
            stt::ChunkProgress { index: 2, total }
        ]
    );
    let logs = String::from_utf8(log_rx.try_iter().flatten().collect()).unwrap();
    assert!(
        logs.contains("HTTP 413"),
        "failure class should remain observable"
    );
    for private in ["PRIVATE-RESPONSE-MARKER", "sk-private-http-test", "chunk-1"] {
        assert!(
            !logs.contains(private),
            "operational logs must not expose private content"
        );
    }
}

#[test]
fn transcribe_remote_failure_without_earlier_text_remains_an_error() {
    let fixture = WavFixture::new(native_spec(), 70 * 16_000);
    let (endpoint, server) = mock_server_multi(vec![ok_json(r#"{"text":" "}"#), upload_error()]);
    let error = stt::transcribe_remote(
        &fixture.path,
        &endpoint,
        "test-stt-model",
        &[],
        Some("sk-private-http-test"),
        |_| {},
    )
    .expect_err("empty earlier chunks do not make a partial transcript");
    let requests = server.join().expect("transcription server thread");
    assert_eq!(requests.len(), 2);
    let message = format!("{error:#}");
    assert!(message.contains("HTTP 413"));
    assert!(
        message.contains(&requests[1].body.len().to_string()),
        "report rejected upload bytes"
    );
    assert!(!message.contains("PRIVATE-RESPONSE-MARKER"));
    assert!(!message.contains("sk-private-http-test"));
}

#[test]
fn transcribe_remote_empty_wav_sends_no_request_or_progress() {
    let fixture = WavFixture::new(native_spec(), 0);
    let listener = TcpListener::bind("127.0.0.1:0").expect("binding unused endpoint");
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let mut progress = Vec::new();
    let transcript = stt::transcribe_remote(
        &fixture.path,
        &endpoint,
        "test-stt-model",
        &[],
        None,
        |chunk| progress.push(chunk),
    )
    .expect("empty audio should not contact the provider");
    assert_eq!(transcript, stt::Transcript::Complete(String::new()));
    assert!(progress.is_empty());
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn transcribe_remote_rejects_inconsistent_wav_container_before_upload() {
    let fixture = WavFixture::new(native_spec(), 10);
    let original = std::fs::read(&fixture.path).unwrap();
    let mut oversized_riff = original.clone();
    oversized_riff[4..8].copy_from_slice(&(original.len() as u32).to_le_bytes());
    let mut short_riff = original.clone();
    short_riff[4..8].copy_from_slice(&8_u32.to_le_bytes());
    let mut partial_frame = original.clone();
    partial_frame[40..44].copy_from_slice(&19_u32.to_le_bytes());
    let mut bad_alignment = original;
    bad_alignment[32..34].copy_from_slice(&3_u16.to_le_bytes());
    let odd = WavFixture::new(
        hound::WavSpec {
            bits_per_sample: 8,
            ..native_spec()
        },
        1,
    );
    let mut missing_padding = std::fs::read(&odd.path).unwrap();
    missing_padding.pop();
    let riff_len = missing_padding.len() as u32 - 8;
    missing_padding[4..8].copy_from_slice(&riff_len.to_le_bytes());

    let listener = TcpListener::bind("127.0.0.1:0").expect("binding unused endpoint");
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    for invalid in [
        oversized_riff,
        short_riff,
        partial_frame,
        bad_alignment,
        missing_padding,
    ] {
        std::fs::write(&fixture.path, invalid).unwrap();
        let mut progress = Vec::new();
        stt::transcribe_remote(
            &fixture.path,
            &endpoint,
            "test-stt-model",
            &[],
            None,
            |chunk| progress.push(chunk),
        )
        .expect_err("inconsistent WAV metadata must fail before upload");
        assert!(progress.is_empty());
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}

/// Telemetry export speaks Langfuse's OTLP/JSON dialect: Basic auth from the
/// keyring secret, the v4 ingestion header, and a payload that carries only
/// counts and controlled terms — never transcript text.
#[test]
fn telemetry_round_trip_sends_metadata_only_otlp() {
    let (base, server) = mock_server(ok_json("{}"));
    let endpoint = format!("{base}/api/public/otel/v1/traces");
    let config = cantrip::config::TelemetryConfig {
        enabled: true,
        endpoint,
        public_key: "pk-test".to_owned(),
        api_key_id: None,
    };
    let job = cantrip::telemetry::JobTelemetry {
        source: "dictation",
        capture_ms: 1_500,
        stt_ms: 250,
        stt_model: "parakeet-tdt-0.6b-v3-int8".to_owned(),
        stt_remote: false,
        chars: 11,
        partial: false,
        cleanup_state: "applied",
        cleanup_ms: Some(120),
        cleanup_model: Some("test-model".to_owned()),
        tokens_in: Some(30),
        tokens_out: Some(11),
        tokens_total: Some(41),
        inject_ms: Some(40),
        delivered: Some("pasted"),
        error_class: None,
        total_ms: 1_910,
    };

    let reporter = cantrip::telemetry::TelemetryReporter::spawn();
    reporter.report(&config, job);
    // Shutdown drains the queue and joins the worker: the request must have
    // landed by the time this returns.
    reporter.shutdown();

    let request = server.join().expect("mock server thread");
    assert_eq!(
        request.request_line,
        "POST /api/public/otel/v1/traces HTTP/1.1"
    );
    assert_eq!(
        request.header("authorization"),
        Some("Basic cGstdGVzdDo=") // base64("pk-test:")
    );
    assert_eq!(request.header("x-langfuse-ingestion-version"), Some("4"));
    assert_eq!(request.header("content-type"), Some("application/json"));

    let body = String::from_utf8_lossy(&request.body);
    assert!(body.contains("\"resourceSpans\""));
    assert!(body.contains("\"name\":\"dictation\""));
    assert!(body.contains("\"name\":\"cleanup-transcript\""));
    assert!(body.contains("langfuse.observation.type"));
    assert!(body.contains("\"intValue\":120"));
    assert!(body.contains("\\\"total\\\":41"));
    // Content-free guarantee: no transcript text field can exist, and no
    // prose leaks into the wire payload.
    assert!(!body.contains("transcript text"));
    assert!(!body.contains("hello cantrip world"));
}
