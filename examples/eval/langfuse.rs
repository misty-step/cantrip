//! Optional Langfuse publish path for the evaluation gauntlet.
//!
//! The regular `run` and `behavior` commands write local, versioned result
//! JSON only. `eval langfuse` is the separate explicit data path that uploads
//! the public/synthetic corpus as a Langfuse dataset, then posts metadata-only
//! experiment traces and scores for the results already on disk. It changes
//! nothing about how the local run was scored or whether a run is
//! reproducible; it only mirrors the existing outputs into Langfuse.
//!
//! Privacy boundary: dataset inputs/expected outputs are the public clip
//! references and synthetic behavior cases. Experimental OTEL spans carry
//! counts, ids, latency/cost, and error flags only — never transcript text,
//! never audio. Daily operator dictations do not reach this path.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::{load_config, parse_flag, resolve_out_dir, validate_config, wer};

/// Public Langfuse ingest base used by both REST and OTLP paths. The REST
/// endpoints are sibling routes under `/api/public`, so the base is derived
/// from the configured OTLP trace endpoint rather than configured twice.
const PUBLIC_API_MARKER: &str = "/api/public";

/// One HTTP client for the Langfuse public API. REST calls share the same
/// Basic auth credential as the daemon's metadata-only OTLP exporter.
struct Client {
    agent: ureq::Agent,
    base_url: String,
    otlp_endpoint: String,
    auth: String,
    dataset_name: String,
}

pub fn publish(args: &[String]) -> Result<()> {
    let config = load_config(args)?;
    validate_config(&config)?;

    // Langfuse publish is opt-in and reuses the daemon's [telemetry]
    // configuration: the public key lives in config, the secret half lives in
    // the OS keyring under api_key_id. No separate eval credentials exist.
    let telemetry = cantrip::config::Config::load()?.telemetry;
    anyhow::ensure!(
        telemetry.enabled,
        "Langfuse eval publishing is disabled; set [telemetry] enabled = true first"
    );
    anyhow::ensure!(
        !telemetry.public_key.trim().is_empty(),
        "telemetry.public_key is empty; set it before publishing eval runs"
    );

    let out_dir = resolve_out_dir(&config, args);
    let transcripts = out_dir.join("transcripts.json");
    let postproc = out_dir.join("postproc.json");
    let behavior = out_dir.join("behavior.json");
    anyhow::ensure!(
        transcripts.exists() || postproc.exists() || behavior.exists(),
        "no eval result files in {}; run the eval harness first",
        out_dir.display()
    );

    let manifest_path = &config.manifest;
    let manifest: crate::Manifest = serde_json::from_str(
        &fs::read_to_string(manifest_path).with_context(|| format!("reading {manifest_path}"))?,
    )
    .with_context(|| format!("parsing {manifest_path}"))?;
    let refs: BTreeMap<String, String> = manifest
        .clips
        .iter()
        .map(|clip| (clip.id.clone(), clip.reference.clone()))
        .collect();

    let behavior_manifest: Option<crate::BehaviorManifest> =
        match config.postproc_manifest.as_deref() {
            Some(path) if behavior.exists() => Some(
                serde_json::from_str(
                    &fs::read_to_string(path).with_context(|| format!("reading {path}"))?,
                )
                .with_context(|| format!("parsing {path}"))?,
            ),
            Some(_) => None,
            None if behavior.exists() => {
                bail!("behavior.json exists but config has no postproc_manifest");
            }
            None => None,
        };

    let dataset_name = dataset_name(args);
    let client = Client::new(&telemetry, dataset_name.clone())?;
    let _dataset_id = client.create_dataset()?;

    let mut item_count = 0_usize;
    let mut items = BTreeMap::new();
    for clip in &manifest.clips {
        let item_id = client.upload_item(
            json!({ "clip": clip.id }),
            json!({ "reference": clip.reference }),
            json!({ "kind": "stt", "file": clip.file }),
        )?;
        items.insert(stt_item_key(&clip.id), item_id);
        item_count += 1;
    }
    if let Some(behavior_manifest) = &behavior_manifest {
        for case in &behavior_manifest.cases {
            let item_id = client.upload_item(
                json!({ "input": case.input }),
                json!({ "accepted": case.accepted }),
                json!({ "kind": "behavior", "category": case.category }),
            )?;
            items.insert(behavior_item_key(&case.id), item_id);
            item_count += 1;
        }
    }

    let mut stt_runs = 0_u64;
    let mut ppr_runs = 0_u64;
    let mut behavior_runs = 0_u64;

    if transcripts.exists() {
        let results: Vec<crate::SttResult> = serde_json::from_str(
            &fs::read_to_string(&transcripts)
                .with_context(|| format!("reading {}", transcripts.display()))?,
        )
        .with_context(|| format!("parsing {}", transcripts.display()))?;
        for result in &results {
            publish_stt_result(&client, &refs, &items, result)?;
            stt_runs += 1;
        }
    }

    if postproc.exists() {
        let results: Vec<crate::PprResult> = serde_json::from_str(
            &fs::read_to_string(&postproc)
                .with_context(|| format!("reading {}", postproc.display()))?,
        )
        .with_context(|| format!("parsing {}", postproc.display()))?;
        for result in &results {
            publish_ppr_result(&client, &refs, &items, result)?;
            ppr_runs += 1;
        }
    }

    if behavior.exists() {
        anyhow::ensure!(
            behavior_manifest.is_some(),
            "behavior.json exists but no behavior manifest was loaded"
        );
        let results: Vec<crate::BehaviorResult> = serde_json::from_str(
            &fs::read_to_string(&behavior)
                .with_context(|| format!("reading {}", behavior.display()))?,
        )
        .with_context(|| format!("parsing {}", behavior.display()))?;
        for result in &results {
            publish_behavior_result(&client, &items, result)?;
            behavior_runs += 1;
        }
        if results.is_empty() {
            eprintln!(
                "[eval] warn: {} has no behavior results; no behavior runs published",
                behavior.display()
            );
        }
    }

    eprintln!(
        "[eval] langfuse publish done: dataset={dataset_name} items={item_count} stt_runs={stt_runs} ppr_runs={ppr_runs} behavior_runs={behavior_runs}",
    );
    Ok(())
}

fn publish_stt_result(
    client: &Client,
    refs: &BTreeMap<String, String>,
    items: &BTreeMap<String, String>,
    result: &crate::SttResult,
) -> Result<()> {
    let item_id = items
        .get(&stt_item_key(&result.clip))
        .with_context(|| format!("dataset item for clip '{}' not uploaded", result.clip))?;
    let reference = refs.get(&result.clip).map(String::as_str).unwrap_or("");
    let stt_wer = wer::wer(reference, &result.text);
    let stt_cer = wer::cer(reference, &result.text);

    let input = json!({
        "clip": result.clip,
        "lane": result.lane,
        "audio_secs": result.audio_secs,
        "cold": result.cold,
        "load_ms": result.load_ms,
    });
    let output = json!({
        "stt_chars": result.text.chars().count(),
        "latency_ms": result.latency_ms,
        "cost_usd": result.cost_usd,
    });

    let trace_id = client.publish_run(
        item_id,
        "cantrip-eval-stt",
        result.latency_ms,
        &input,
        &output,
        false,
    )?;
    let comment = format!("lane={} clip={}", result.lane, result.clip);
    client.post_score(&trace_id, "stt.wer", json!(stt_wer), &comment)?;
    client.post_score(&trace_id, "stt.cer", json!(stt_cer), &comment)?;
    Ok(())
}

fn publish_ppr_result(
    client: &Client,
    refs: &BTreeMap<String, String>,
    items: &BTreeMap<String, String>,
    result: &crate::PprResult,
) -> Result<()> {
    let item_id = items
        .get(&stt_item_key(&result.clip))
        .with_context(|| format!("dataset item for clip '{}' not uploaded", result.clip))?;
    let reference = refs.get(&result.clip).map(String::as_str).unwrap_or("");
    let input_wer = wer::wer(reference, &result.raw_text);
    let final_wer = wer::wer(reference, &result.text);

    let input = json!({
        "clip": result.clip,
        "stt_lane": result.stt_lane,
        "lane": result.lane,
        "degenerate": result.degenerate,
    });
    let output = json!({
        "ppr_chars": result.text.chars().count(),
        "input_tokens": result.input_tokens,
        "output_tokens": result.output_tokens,
        "latency_ms": result.latency_ms,
        "cost_usd": result.cost_usd,
    });

    let trace_id = client.publish_run(
        item_id,
        "cantrip-eval-ppr",
        result.latency_ms,
        &input,
        &output,
        result.degenerate,
    )?;
    let comment = format!(
        "lane={} stt={} clip={}",
        result.lane, result.stt_lane, result.clip
    );
    client.post_score(&trace_id, "ppr.input_wer", json!(input_wer), &comment)?;
    client.post_score(&trace_id, "ppr.final_wer", json!(final_wer), &comment)?;
    Ok(())
}

fn publish_behavior_result(
    client: &Client,
    items: &BTreeMap<String, String>,
    result: &crate::BehaviorResult,
) -> Result<()> {
    let item_id = items
        .get(&behavior_item_key(&result.case))
        .with_context(|| format!("dataset item for case '{}' not uploaded", result.case))?;

    let input = json!({
        "case": result.case,
        "category": result.category,
        "lane": result.lane,
        "iteration": result.iteration,
    });
    let output = json!({
        "passed": result.passed,
        "behavior_chars": result.text.chars().count(),
        "input_tokens": result.input_tokens,
        "output_tokens": result.output_tokens,
        "latency_ms": result.latency_ms,
        "cost_usd": result.cost_usd,
    });

    let trace_id = client.publish_run(
        item_id,
        "cantrip-eval-behavior",
        result.latency_ms,
        &input,
        &output,
        !result.passed,
    )?;
    let comment = format!(
        "lane={} case={} iteration={}",
        result.lane, result.case, result.iteration
    );
    client.post_score(
        &trace_id,
        "behavior.pass",
        json!(if result.passed { 1.0 } else { 0.0 }),
        &comment,
    )?;
    Ok(())
}

fn dataset_name(args: &[String]) -> String {
    parse_flag(args, "--dataset")
        .and_then(|values| values.into_iter().next())
        .unwrap_or_else(|| {
            let millis = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            format!("cantrip-eval-{millis}")
        })
}

fn stt_item_key(clip: &str) -> String {
    format!("stt:{clip}")
}

fn dataset_item_body(
    dataset_name: &str,
    input: Value,
    expected_output: Value,
    metadata: Value,
) -> Value {
    json!({
        "datasetName": dataset_name,
        "input": input,
        "expectedOutput": expected_output,
        "metadata": metadata,
    })
}

fn behavior_item_key(case: &str) -> String {
    format!("behavior:{case}")
}

impl Client {
    fn new(telemetry: &cantrip::config::TelemetryConfig, dataset_name: String) -> Result<Self> {
        let base_url = langfuse_base(&telemetry.endpoint)?;
        let secret = match &telemetry.api_key_id {
            Some(id) => cantrip::keys::get(id)
                .with_context(|| format!("reading Langfuse key '{id}' from OS keyring"))?,
            None => String::new(),
        };
        let auth = format!(
            "Basic {}",
            base64(format!("{}:{}", telemetry.public_key, secret).as_bytes())
        );
        Ok(Self {
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(60))
                .build(),
            base_url,
            otlp_endpoint: telemetry.endpoint.clone(),
            auth,
            dataset_name,
        })
    }

    fn rest_url(&self, path: &str) -> String {
        format!("{}{}", self.base_url.trim_end_matches('/'), path)
    }

    fn create_dataset(&self) -> Result<String> {
        let url = self.rest_url("/api/public/v2/datasets");
        let body = json!({
            "name": self.dataset_name,
            "description": "Cantrip public/synthetic evaluation corpus and runs",
            "metadata": { "source": "cantrip-eval" },
        });
        let response = self
            .agent
            .post(&url)
            .set("Authorization", &self.auth)
            .set("Content-Type", "application/json")
            .send_string(&body.to_string())
            .with_context(|| format!("creating Langfuse dataset '{}'", self.dataset_name))?;
        ensure_ok(response.status(), "dataset create")?;
        let raw = response
            .into_string()
            .with_context(|| "reading Langfuse dataset create response")?;
        let parsed: Value = serde_json::from_str(&raw)
            .with_context(|| "parsing Langfuse dataset create response")?;
        parsed
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .with_context(|| "Langfuse dataset create response missing id")
    }

    fn upload_item(&self, input: Value, expected_output: Value, metadata: Value) -> Result<String> {
        let url = self.rest_url("/api/public/dataset-items");
        let body = dataset_item_body(&self.dataset_name, input, expected_output, metadata);
        let response = self
            .agent
            .post(&url)
            .set("Authorization", &self.auth)
            .set("Content-Type", "application/json")
            .send_string(&body.to_string())
            .with_context(|| "creating Langfuse dataset item")?;
        ensure_ok(response.status(), "dataset item create")?;
        let raw = response
            .into_string()
            .with_context(|| "reading Langfuse dataset item response")?;
        let parsed: Value =
            serde_json::from_str(&raw).with_context(|| "parsing Langfuse dataset item response")?;
        parsed
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .with_context(|| "Langfuse dataset item response missing id")
    }

    fn post_trace(&self, payload: &Value) -> Result<()> {
        let response = self
            .agent
            .post(&self.otlp_endpoint)
            .set("Authorization", &self.auth)
            .set("Content-Type", "application/json")
            .set("x-langfuse-ingestion-version", "4")
            .send_string(&payload.to_string())
            .with_context(|| "exporting Langfuse experiment trace")?;
        ensure_ok(response.status(), "OTLP trace export")
    }

    fn post_score(&self, trace_id: &str, name: &str, value: Value, comment: &str) -> Result<()> {
        let url = self.rest_url("/api/public/scores");
        let body = json!({
            "traceId": trace_id,
            "name": name,
            "value": value,
            "dataType": "NUMERIC",
            "comment": comment,
        });
        let response = self
            .agent
            .post(&url)
            .set("Authorization", &self.auth)
            .set("Content-Type", "application/json")
            .send_string(&body.to_string())
            .with_context(|| format!("creating Langfuse score '{name}'"))?;
        ensure_ok(response.status(), "score create")
    }

    fn publish_run(
        &self,
        item_id: &str,
        run_name: &str,
        duration_ms: u128,
        input: &Value,
        output: &Value,
        error: bool,
    ) -> Result<String> {
        let (trace_id, payload) = experiment_trace(
            item_id,
            run_name,
            &self.dataset_name,
            duration_ms,
            input,
            output,
            error,
        );
        self.post_trace(&payload)?;
        Ok(trace_id)
    }
}

fn langfuse_base(endpoint: &str) -> Result<String> {
    let Some(index) = endpoint.find(PUBLIC_API_MARKER) else {
        bail!(
            "cannot derive Langfuse REST base from telemetry endpoint '{endpoint}': expected an endpoint under /api/public"
        );
    };
    let base = endpoint[..index].trim_end_matches('/');
    anyhow::ensure!(
        !base.is_empty(),
        "cannot derive Langfuse REST base from telemetry endpoint '{endpoint}'"
    );
    Ok(base.to_owned())
}

fn experiment_trace(
    item_id: &str,
    run_name: &str,
    experiment: &str,
    duration_ms: u128,
    input: &Value,
    output: &Value,
    error: bool,
) -> (String, Value) {
    let trace_id = random_hex(16);
    let span_id = random_hex(8);
    let end_nanos = system_nanos();
    let start_nanos = end_nanos.saturating_sub(duration_ms.saturating_mul(1_000_000));
    let start_nanos = if start_nanos < end_nanos {
        start_nanos
    } else {
        end_nanos.saturating_sub(1)
    };

    let input_raw = input.to_string();
    let output_raw = output.to_string();
    let span = json!({
        "traceId": trace_id,
        "spanId": span_id,
        "parentSpanId": "",
        "name": "cantrip-eval",
        "kind": 1,
        "startTimeUnixNano": start_nanos.to_string(),
        "endTimeUnixNano": end_nanos.to_string(),
        "attributes": [
            attr("langfuse.experiment.name", string_value(experiment)),
            attr("langfuse.experiment.item_id", string_value(item_id)),
            attr("langfuse.trace.name", string_value(run_name)),
            attr("langfuse.observation.input", string_value(&input_raw)),
            attr("langfuse.observation.output", string_value(&output_raw)),
        ],
        "status": { "code": if error { 2 } else { 1 } },
    });
    let payload = json!({
        "resourceSpans": [{
            "resource": { "attributes": [
                attr("service.name", string_value("cantrip-eval")),
                attr("service.version", string_value(env!("CARGO_PKG_VERSION"))),
            ]},
            "scopeSpans": [{
                "scope": { "name": "cantrip-eval", "version": env!("CARGO_PKG_VERSION") },
                "spans": [span],
            }],
        }]
    });
    (trace_id, payload)
}

fn attr(key: &str, value: Value) -> Value {
    json!({ "key": key, "value": value })
}

fn string_value(text: &str) -> Value {
    json!({ "stringValue": text })
}

fn ensure_ok(status: u16, action: &str) -> Result<()> {
    if !(200..300).contains(&status) {
        bail!("Langfuse {action} returned status {status}");
    }
    Ok(())
}

fn system_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0_u8; bytes];
    let mut random = match fs::File::open("/dev/urandom") {
        Ok(random) => random,
        Err(_) => {
            let seed = system_nanos() as u64;
            for (index, byte) in buf.iter_mut().enumerate() {
                *byte = (seed >> ((index % 8) * 8)) as u8 ^ index as u8;
            }
            return hex(&buf);
        }
    };
    if random.read_exact(&mut buf).is_err() {
        let seed = system_nanos() as u64;
        for (index, byte) in buf.iter_mut().enumerate() {
            *byte = (seed >> ((index % 8) * 8)) as u8 ^ index as u8;
        }
    }
    hex(&buf)
}

fn hex(buf: &[u8]) -> String {
    buf.iter().map(|byte| format!("{byte:02x}")).collect()
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(BASE64[(n >> 18) as usize & 63] as char);
        out.push(BASE64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            BASE64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            BASE64[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

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

        fn body_json(&self) -> Value {
            serde_json::from_slice(&self.body).expect("request body is JSON")
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

    fn telemetry_config(base: &str) -> cantrip::config::TelemetryConfig {
        cantrip::config::TelemetryConfig {
            enabled: true,
            endpoint: format!("{base}/api/public/otel/v1/traces"),
            public_key: "pk-test".to_owned(),
            api_key_id: None,
        }
    }

    #[test]
    fn langfuse_base_derives_from_otlp_endpoint() {
        assert_eq!(
            langfuse_base("https://us.cloud.langfuse.com/api/public/otel/v1/traces").unwrap(),
            "https://us.cloud.langfuse.com"
        );
        assert_eq!(
            langfuse_base("https://cloud.langfuse.com/api/public/otel/v1/traces").unwrap(),
            "https://cloud.langfuse.com"
        );
        assert!(langfuse_base("https://example.com/not-langfuse").is_err());
    }

    #[test]
    fn dataset_item_body_shapes_public_corpus_fields() {
        let body = dataset_item_body(
            "cantrip-eval",
            json!({ "clip": "jfk" }),
            json!({ "reference": "ask not what" }),
            json!({ "kind": "stt", "file": "samples/jfk.wav" }),
        );
        assert_eq!(body["datasetName"], "cantrip-eval");
        assert_eq!(body["input"]["clip"], "jfk");
        assert_eq!(body["expectedOutput"]["reference"], "ask not what");
    }

    #[test]
    fn experiment_trace_links_item_and_carries_no_transcript_text() {
        let (trace_id, payload) = experiment_trace(
            "item-123",
            "cantrip-eval-stt",
            "cantrip-eval",
            12,
            &json!({ "clip": "jfk" }),
            &json!({ "stt_chars": 11 }),
            false,
        );
        assert_eq!(trace_id.len(), 32);
        let spans = payload["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap();
        assert_eq!(spans.len(), 1);
        let span = &spans[0];
        assert_eq!(span["name"], "cantrip-eval");
        assert_eq!(span["traceId"], trace_id.as_str());
        let attributes = span["attributes"].as_array().unwrap();
        let attr_value = |key: &str| -> Value {
            attributes
                .iter()
                .find(|a| a["key"] == key)
                .map(|a| a["value"].clone())
                .unwrap()
        };
        assert_eq!(
            attr_value("langfuse.experiment.name")["stringValue"],
            "cantrip-eval"
        );
        assert_eq!(
            attr_value("langfuse.experiment.item_id")["stringValue"],
            "item-123"
        );
        let raw = payload.to_string();
        assert!(!raw.contains("ask not what"));
    }

    #[test]
    fn create_dataset_round_trip_uses_langfuse_rest_contract() {
        let (base, server) = mock_server(ok_json(r#"{"id":"dataset-1","name":"cantrip-eval"}"#));
        let client = Client::new(&telemetry_config(&base), "cantrip-eval".to_owned()).unwrap();
        let id = client.create_dataset().unwrap();
        assert_eq!(id, "dataset-1");

        let request = server.join().expect("mock server thread");
        assert_eq!(
            request.request_line,
            "POST /api/public/v2/datasets HTTP/1.1"
        );
        assert_eq!(
            request.header("authorization"),
            Some("Basic cGstdGVzdDo=") // base64("pk-test:")
        );
        assert_eq!(request.header("content-type"), Some("application/json"));
        assert_eq!(request.body_json()["name"], "cantrip-eval");
    }

    #[test]
    fn post_trace_round_trip_sends_otlp_experiment_attributes() {
        let (base, server) = mock_server(ok_json("{}"));
        let client = Client::new(&telemetry_config(&base), "cantrip-eval".to_owned()).unwrap();
        let (_, payload) = experiment_trace(
            "item-1",
            "cantrip-eval-stt",
            "cantrip-eval",
            30,
            &json!({ "clip": "jfk" }),
            &json!({ "stt_chars": 9 }),
            false,
        );
        client.post_trace(&payload).unwrap();

        let request = server.join().expect("mock server thread");
        assert_eq!(
            request.request_line,
            "POST /api/public/otel/v1/traces HTTP/1.1"
        );
        assert_eq!(request.header("x-langfuse-ingestion-version"), Some("4"));
        let raw = String::from_utf8_lossy(&request.body);
        assert!(raw.contains("langfuse.experiment.name"));
        assert!(raw.contains("langfuse.experiment.item_id"));
    }
}
