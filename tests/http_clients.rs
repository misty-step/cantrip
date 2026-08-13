//! HTTP round-trip contracts for post-processing and remote transcription
//! against a local mock OpenAI-compatible server.

use cantrip::config::PostprocConfig;
use cantrip::postproc;
use cantrip::stt;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
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
        timeout_ms: 5_000,
        passes: 2,
        min_chars: 0,
        instructions: "Keep numerals as digits.".to_owned(),
    }
}

#[test]
fn refine_round_trip_sends_contract_request_and_strips_think() {
    let response = ok_json(
        r#"{"choices":[{"message":{"content":"<think>internal chain</think>Hello, Cantrip world."}}],"usage":{"prompt_tokens":7,"completion_tokens":2,"total_tokens":9,"cost":0.0012,"completion_tokens_details":{"reasoning_tokens":1},"prompt_tokens_details":{"cached_tokens":3}}}"#,
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
        "Source:\nhello cantrip world\nClean transcript:"
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

#[test]
fn transcribe_remote_round_trip_sends_multipart_wav() {
    let (endpoint, server) = mock_server(ok_json(r#"{"text":" hello from the cloud "}"#));

    let dir = std::env::temp_dir().join(format!("cantrip-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("creating temp dir");
    let wav = dir.join("fake.wav");
    std::fs::write(&wav, b"RIFF-fake-wav-bytes").expect("writing fake wav");

    let text = stt::transcribe_remote(
        &wav,
        &endpoint,
        "whisper-large-v3-turbo",
        &["Cantrip".to_owned()],
        Some("sk-cloud"),
    )
    .expect("remote transcription should succeed");
    assert_eq!(text, "hello from the cloud");

    let request = server.join().expect("mock server thread");
    assert_eq!(request.request_line, "POST /audio/transcriptions HTTP/1.1");
    assert_eq!(request.header("authorization"), Some("Bearer sk-cloud"));
    let content_type = request.header("content-type").expect("content type");
    assert!(content_type.starts_with("multipart/form-data; boundary="));

    let body = String::from_utf8_lossy(&request.body);
    assert!(body.contains("name=\"model\"\r\n\r\nwhisper-large-v3-turbo"));
    assert!(body.contains("name=\"prompt\"\r\n\r\nCantrip"));
    assert!(body.contains("name=\"response_format\"\r\n\r\njson"));
    assert!(body.contains("filename=\"audio.wav\""));
    assert!(body.contains("RIFF-fake-wav-bytes"));

    std::fs::remove_dir_all(&dir).expect("removing temp dir");
}
