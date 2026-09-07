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
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{load_config, parse_flag, resolve_out_dir, validate_config, wer};

/// Public Langfuse ingest base used by both REST and OTLP paths. The REST
/// endpoints are sibling routes under `/api/public`, so the base is derived
/// from the configured OTLP trace endpoint rather than configured twice.
const PUBLIC_API_MARKER: &str = "/api/public";

const MAX_ATTEMPTS: u32 = 3;
const MAX_RETRY_DELAY: Duration = Duration::from_secs(5);
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
const MAX_DATASET_PAGES: u64 = 100;

/// One HTTP client for the Langfuse public API. REST calls share the same
/// Basic auth credential as the daemon's metadata-only OTLP exporter.
struct Client {
    agent: ureq::Agent,
    base_url: String,
    otlp_endpoint: String,
    auth: String,
    dataset_name: String,
    run_id: String,
    run_start_nanos: u128,
}

struct Dataset {
    id: String,
    // Exact public content -> existing IDs, including pre-idempotency items.
    items: BTreeMap<String, Vec<String>>,
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

    // Load and identify the immutable inputs before making any remote writes.
    let manifest_json = read_json(Path::new(&config.manifest))?;
    let behavior_manifest_json = match config.postproc_manifest.as_deref() {
        Some(path) if behavior.exists() => read_json(Path::new(path))?,
        None if behavior.exists() => {
            bail!("behavior.json exists but config has no postproc_manifest");
        }
        _ => Value::Null,
    };
    let stt_json = read_results(&transcripts)?;
    let ppr_json = read_results(&postproc)?;
    let behavior_json = read_results(&behavior)?;
    let metadata_path = out_dir.join("run.json");
    let metadata = read_json(&metadata_path).context(
        "publishing requires immutable run.json metadata; rerun eval run or behavior for legacy output",
    )?;
    let run_start_nanos = run_start(&metadata)?;
    let run_label = parse_flag(args, "--run-id").and_then(|values| values.into_iter().next());
    let run_id = run_identity(
        run_label.as_deref(),
        &metadata,
        [&manifest_json, &behavior_manifest_json],
        [&stt_json, &ppr_json, &behavior_json],
    )?;
    let manifest: crate::Manifest = serde_json::from_value(manifest_json)?;
    let behavior_manifest: Option<crate::BehaviorManifest> =
        serde_json::from_value(behavior_manifest_json)?;
    let stt_results: Vec<crate::SttResult> = serde_json::from_value(stt_json)?;
    let ppr_results: Vec<crate::PprResult> = serde_json::from_value(ppr_json)?;
    let behavior_results: Vec<crate::BehaviorResult> = serde_json::from_value(behavior_json)?;
    let refs: BTreeMap<String, String> = manifest
        .clips
        .iter()
        .map(|clip| (clip.id.clone(), clip.reference.clone()))
        .collect();

    let dataset_name = dataset_name(args);
    let client = Client::new(&telemetry, dataset_name.clone(), run_id, run_start_nanos)?;
    let mut dataset = client.ensure_dataset()?;

    let mut item_count = 0_usize;
    let mut items = BTreeMap::new();
    for clip in &manifest.clips {
        let item_id = client.upload_item(
            &mut dataset,
            &stt_item_key(&clip.id),
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
                &mut dataset,
                &behavior_item_key(&case.id),
                json!({ "input": case.input }),
                json!({ "accepted": case.accepted }),
                json!({ "kind": "behavior", "category": case.category }),
            )?;
            items.insert(behavior_item_key(&case.id), item_id);
            item_count += 1;
        }
    }

    for (index, result) in stt_results.iter().enumerate() {
        publish_stt_result(&client, &dataset, &refs, &items, index, result)?;
    }
    for (index, result) in ppr_results.iter().enumerate() {
        publish_ppr_result(&client, &dataset, &refs, &items, index, result)?;
    }
    for (index, result) in behavior_results.iter().enumerate() {
        publish_behavior_result(&client, &dataset, &items, index, result)?;
    }
    let (stt_runs, ppr_runs, behavior_runs) =
        (stt_results.len(), ppr_results.len(), behavior_results.len());

    eprintln!(
        "[eval] langfuse publish done: dataset={dataset_name} items={item_count} stt_runs={stt_runs} ppr_runs={ppr_runs} behavior_runs={behavior_runs}",
    );
    Ok(())
}

fn publish_stt_result(
    client: &Client,
    dataset: &Dataset,
    refs: &BTreeMap<String, String>,
    items: &BTreeMap<String, String>,
    index: usize,
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
        (&dataset.id, item_id),
        ("cantrip-eval-stt", index),
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
    dataset: &Dataset,
    refs: &BTreeMap<String, String>,
    items: &BTreeMap<String, String>,
    index: usize,
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
        (&dataset.id, item_id),
        ("cantrip-eval-ppr", index),
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
    dataset: &Dataset,
    items: &BTreeMap<String, String>,
    index: usize,
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
        (&dataset.id, item_id),
        ("cantrip-eval-behavior", index),
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
        .unwrap_or_else(|| "cantrip-evals".to_owned())
}

fn stt_item_key(clip: &str) -> String {
    format!("stt:{clip}")
}

fn read_json(path: &Path) -> Result<Value> {
    let raw = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&raw).with_context(|| format!("parsing {}", path.display()))
}

fn read_results(path: &Path) -> Result<Value> {
    if path.exists() {
        read_json(path)
    } else {
        Ok(json!([]))
    }
}

fn run_identity(
    label: Option<&str>,
    metadata: &Value,
    corpus: [&Value; 2],
    results: [&Value; 3],
) -> Result<String> {
    let content = serde_json::to_string(&(label, metadata, corpus, results))?;
    Ok(hex(&identity("cantrip-eval-run", &[&content])))
}

fn item_content(input: &Value, expected_output: &Value, metadata: &Value) -> String {
    let content =
        serde_json::to_string(&(input, expected_output, metadata)).expect("JSON values serialize");
    hex(&identity("cantrip-eval-item-content", &[&content]))
}

fn behavior_item_key(case: &str) -> String {
    format!("behavior:{case}")
}

impl Client {
    fn new(
        telemetry: &cantrip::config::TelemetryConfig,
        dataset_name: String,
        run_id: String,
        run_start_nanos: u128,
    ) -> Result<Self> {
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
            run_id,
            run_start_nanos,
        })
    }

    fn rest_url(&self, path: &str) -> String {
        format!("{}{}", self.base_url.trim_end_matches('/'), path)
    }

    // One bounded policy for REST and OTLP; bodies and credentials never enter errors.
    fn send(
        &self,
        request: ureq::Request,
        body: Option<&str>,
        action: &str,
    ) -> Result<(u16, String)> {
        use std::io::Read;

        let request = request.set("Authorization", &self.auth);
        for attempt in 1..=MAX_ATTEMPTS {
            let response = match body {
                Some(body) => request.clone().send_string(body),
                None => request.clone().call(),
            };
            let mut retry_after = None;
            let failure = match response {
                Ok(response) | Err(ureq::Error::Status(_, response)) => {
                    let status = response.status();
                    if matches!(status, 408 | 429 | 500 | 502 | 503 | 504) {
                        retry_after = response
                            .header("Retry-After")
                            .and_then(|value| parse_retry_after(value, SystemTime::now()));
                        format!("HTTP {status}")
                    } else if !(200..300).contains(&status) {
                        // Do not consume error bodies, which may be truncated or sensitive.
                        return Ok((status, String::new()));
                    } else {
                        let mut raw = String::new();
                        match response
                            .into_reader()
                            .take(MAX_RESPONSE_BYTES + 1)
                            .read_to_string(&mut raw)
                        {
                            Ok(_) => {
                                anyhow::ensure!(
                                    raw.len() as u64 <= MAX_RESPONSE_BYTES,
                                    "Langfuse {action} response exceeded {MAX_RESPONSE_BYTES} bytes"
                                );
                                return Ok((status, raw));
                            }
                            Err(error) if is_transient_io(&error) => {
                                format!("response read {:?}", error.kind())
                            }
                            Err(error) => {
                                bail!(
                                    "Langfuse {action} response read failed ({:?})",
                                    error.kind()
                                );
                            }
                        }
                    }
                }
                Err(ureq::Error::Transport(error)) => {
                    let kind = error.kind();
                    let retryable = match kind {
                        ureq::ErrorKind::Dns
                        | ureq::ErrorKind::ConnectionFailed
                        | ureq::ErrorKind::ProxyConnect => true,
                        ureq::ErrorKind::Io => std::error::Error::source(&error)
                            .and_then(|source| source.downcast_ref::<std::io::Error>())
                            .is_some_and(is_transient_io),
                        _ => false,
                    };
                    if !retryable {
                        bail!("Langfuse {action} failed ({kind:?}, attempt {attempt})");
                    }
                    format!("transport {kind:?}")
                }
            };
            anyhow::ensure!(
                attempt < MAX_ATTEMPTS,
                "Langfuse {action} failed after {attempt} attempts: {failure}"
            );
            let delay = retry_delay(attempt, retry_after);
            eprintln!(
                "[eval] Langfuse {action}: {failure}; retrying in {}ms (attempt {attempt}/{MAX_ATTEMPTS})",
                delay.as_millis()
            );
            std::thread::sleep(delay);
        }
        unreachable!("the final attempt returns an error")
    }

    fn get_json(&self, request: ureq::Request, action: &str) -> Result<Option<Value>> {
        let (status, raw) = self.send(request, None, action)?;
        if status == 404 {
            return Ok(None);
        }
        ensure_ok(status, action)?;
        Ok(Some(
            serde_json::from_str(&raw).context("parsing Langfuse response")?,
        ))
    }

    fn post_json(&self, url: &str, body: &Value, action: &str) -> Result<Value> {
        let request = self.agent.post(url).set("Content-Type", "application/json");
        let (status, raw) = self.send(request, Some(&body.to_string()), action)?;
        ensure_ok(status, action)?;
        serde_json::from_str(&raw).context("parsing Langfuse response")
    }

    fn ensure_dataset(&self) -> Result<Dataset> {
        let url = format!(
            "{}/api/public/v2/datasets/{}",
            self.base_url,
            percent_encode_component(&self.dataset_name)
        );
        let existing = self.get_json(self.agent.get(&url), "dataset lookup")?;
        let dataset = match existing {
            Some(dataset) => dataset,
            None => self.post_json(
                &self.rest_url("/api/public/v2/datasets"),
                &json!({
                    "name": self.dataset_name,
                    "description": "Cantrip public/synthetic evaluation corpus and runs",
                    "metadata": { "source": "cantrip-eval" },
                }),
                "dataset create",
            )?,
        };
        let id = dataset["id"]
            .as_str()
            .context("Langfuse dataset response missing id")?
            .to_owned();
        let mut items: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for page in 1..=MAX_DATASET_PAGES {
            let request = self
                .agent
                .get(&self.rest_url("/api/public/dataset-items"))
                .query("datasetName", &self.dataset_name)
                .query("limit", "100")
                .query("page", &page.to_string());
            let response = self
                .get_json(request, "dataset items lookup")?
                .context("Langfuse dataset items lookup returned 404")?;
            for item in response["data"]
                .as_array()
                .context("dataset items missing data")?
            {
                let key = item_content(&item["input"], &item["expectedOutput"], &item["metadata"]);
                let id = item["id"].as_str().context("dataset item missing id")?;
                items.entry(key).or_default().push(id.to_owned());
            }
            let pages = response["meta"]["totalPages"]
                .as_u64()
                .context("dataset items missing totalPages")?;
            anyhow::ensure!(
                pages <= MAX_DATASET_PAGES,
                "Langfuse dataset exceeds {MAX_DATASET_PAGES} pages; use a smaller dataset"
            );
            if page >= pages {
                break;
            }
        }
        for ids in items.values_mut() {
            ids.sort();
        }
        Ok(Dataset { id, items })
    }

    fn upload_item(
        &self,
        dataset: &mut Dataset,
        key: &str,
        input: Value,
        expected_output: Value,
        mut metadata: Value,
    ) -> Result<String> {
        let legacy_content = item_content(&input, &expected_output, &metadata);
        metadata["cantrip_key"] = json!(key);
        let content = item_content(&input, &expected_output, &metadata);
        if let Some(id) = dataset.items.get_mut(&content).and_then(Vec::pop) {
            return Ok(id);
        }
        // Adopt only exact legacy content, not a clip/case name whose reference changed.
        let id = dataset
            .items
            .get_mut(&legacy_content)
            .and_then(Vec::pop)
            .unwrap_or_else(|| {
                hex(&identity(
                    "cantrip-eval-item",
                    &[&dataset.id, key, &content],
                ))
            });
        let response = self.post_json(
            &self.rest_url("/api/public/dataset-items"),
            &json!({
                "id": id,
                "datasetName": self.dataset_name,
                "input": input,
                "expectedOutput": expected_output,
                "metadata": metadata,
            }),
            "dataset item upsert",
        )?;
        anyhow::ensure!(
            response["id"].as_str() == Some(id.as_str()),
            "Langfuse dataset item upsert returned an unexpected id"
        );
        Ok(id)
    }

    fn post_trace(&self, payload: &Value) -> Result<()> {
        let request = self
            .agent
            .post(&self.otlp_endpoint)
            .set("Content-Type", "application/json")
            .set("x-langfuse-ingestion-version", "4");
        let (status, raw) = self.send(request, Some(&payload.to_string()), "OTLP trace export")?;
        ensure_ok(status, "OTLP trace export")?;
        let response: Value = serde_json::from_str(&raw).context("parsing OTLP response")?;
        let rejected = &response["partialSuccess"]["rejectedSpans"];
        anyhow::ensure!(
            rejected.is_null() || rejected == 0 || rejected == "0",
            "Langfuse OTLP trace export rejected spans"
        );
        Ok(())
    }

    fn post_score(&self, trace_id: &str, name: &str, value: Value, comment: &str) -> Result<()> {
        let url = self.rest_url("/api/public/scores");
        let body = json!({
            "id": hex(&identity("cantrip-eval-score", &[trace_id, name])),
            "traceId": trace_id,
            "name": name,
            "value": value,
            "dataType": "NUMERIC",
            "comment": comment,
        });
        self.post_json(&url, &body, "score upsert")?;
        Ok(())
    }

    fn publish_run(
        &self,
        (dataset_id, item_id): (&str, &str),
        record: (&str, usize),
        duration_ms: u128,
        input: &Value,
        output: &Value,
        error: bool,
    ) -> Result<String> {
        let experiment = format!("{}-{}-{}", self.dataset_name, record.0, self.run_id);
        let run_name = format!("{}-{}", record.0, record.1);
        let (trace_id, payload) = experiment_trace(
            (dataset_id, item_id),
            &run_name,
            &experiment,
            (self.run_start_nanos, duration_ms),
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
    (dataset_id, item_id): (&str, &str),
    run_name: &str,
    experiment: &str,
    (start_nanos, duration_ms): (u128, u128),
    input: &Value,
    output: &Value,
    error: bool,
) -> (String, Value) {
    let experiment_id = hex(&identity("cantrip-eval-experiment", &[dataset_id, experiment])[..8]);
    let digest = identity("cantrip-eval-trace", &[&experiment_id, item_id, run_name]);
    let trace_id = hex(&digest[..16]);
    let span_id = hex(&identity("cantrip-eval-span", &[&trace_id])[..8]);
    // Langfuse's OTLP storage key includes start_time as well as span_id.
    // Never replace this immutable run anchor with the publication clock.
    let end_nanos = start_nanos.saturating_add(duration_ms.saturating_mul(1_000_000).max(1));

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
            attr("langfuse.experiment.id", string_value(&experiment_id)),
            attr("langfuse.experiment.name", string_value(experiment)),
            attr("langfuse.experiment.dataset.id", string_value(dataset_id)),
            attr("langfuse.experiment.item.id", string_value(item_id)),
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

fn run_start(metadata: &Value) -> Result<u128> {
    if let Some(value) = metadata.get("started_at_unix_ms") {
        let millis = value
            .as_u64()
            .context("run.json started_at_unix_ms must be an unsigned integer")?;
        anyhow::ensure!(
            millis > 0,
            "run.json started_at_unix_ms must be after the Unix epoch"
        );
        return Ok(u128::from(millis) * 1_000_000);
    }
    let date = if let Some(started_at) = metadata["started_at"].as_str() {
        parse_utc(started_at, c"%Y-%m-%dT%H:%M:%SZ")
    } else {
        metadata["date"]
            .as_str()
            .and_then(|date| parse_utc(date, c"%Y-%m-%d"))
    }
    .context(
        "run.json has no valid immutable timestamp; rerun eval run or behavior for legacy output",
    )?;
    let nanos = date.duration_since(UNIX_EPOCH)?.as_nanos();
    anyhow::ensure!(nanos > 0, "run.json timestamp must be after the Unix epoch");
    Ok(nanos)
}

fn identity(namespace: &str, parts: &[&str]) -> [u8; 32] {
    let mut hash = Sha256::new();
    for part in std::iter::once(namespace).chain(parts.iter().copied()) {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    hash.finalize().into()
}

fn is_transient_io(error: &std::io::Error) -> bool {
    use std::io::ErrorKind;

    matches!(
        error.kind(),
        ErrorKind::TimedOut
            | ErrorKind::WouldBlock
            | ErrorKind::Interrupted
            | ErrorKind::UnexpectedEof
            | ErrorKind::ConnectionRefused
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::NotConnected
            | ErrorKind::BrokenPipe
    )
}

fn retry_delay(attempt: u32, retry_after: Option<Duration>) -> Duration {
    let backoff = Duration::from_millis(250 * (1 << (attempt - 1)));
    retry_after
        .unwrap_or(backoff)
        .max(backoff)
        .min(MAX_RETRY_DELAY)
}

fn parse_retry_after(header: &str, now: SystemTime) -> Option<Duration> {
    let header = header.trim();
    if let Ok(seconds) = header.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let date = parse_utc(header, c"%a, %d %b %Y %H:%M:%S GMT")?;
    Some(date.duration_since(now).unwrap_or_default())
}

fn parse_utc(text: &str, format: &std::ffi::CStr) -> Option<SystemTime> {
    let text = std::ffi::CString::new(text).ok()?;
    // libc's initial C locale parses HTTP's English month names. timegm uses
    // UTC, not the operator's timezone; both strings remain alive through strptime.
    let seconds = unsafe {
        let mut time: libc::tm = std::mem::zeroed();
        let end = libc::strptime(text.as_ptr(), format.as_ptr(), &mut time);
        if end.is_null() || *end != 0 {
            return None;
        }
        libc::timegm(&mut time)
    };
    UNIX_EPOCH.checked_add(Duration::from_secs(seconds.try_into().ok()?))
}

fn percent_encode_component(input: &str) -> String {
    use std::fmt::Write;

    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => write!(out, "%{byte:02X}").expect("writing to String"),
        }
    }
    out
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
    use std::collections::BTreeSet;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;
    use std::time::Instant;

    struct Request {
        target: String,
        body: Value,
    }

    fn read_request(stream: &TcpStream) -> Request {
        let mut reader = BufReader::new(stream.take(64 * 1024));
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let target = line.trim_end().to_owned();
        let mut length = 0;
        loop {
            line.clear();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse::<usize>().unwrap();
            }
        }
        assert!(length <= 32 * 1024);
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        Request {
            target,
            body: if body.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&body).unwrap()
            },
        }
    }

    fn ok_json(body: &Value) -> String {
        let body = body.to_string();
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn server(
        count: usize,
        mut respond: impl FnMut(usize, &Request) -> Option<String> + Send + 'static,
    ) -> (String, thread::JoinHandle<Vec<Request>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let mut requests = Vec::new();
            for index in 0..count {
                let deadline = Instant::now() + Duration::from_secs(4);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "missing HTTP request {index}");
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => panic!("accepting fixture request: {error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let request = read_request(&stream);
                if let Some(response) = respond(index, &request) {
                    stream.write_all(response.as_bytes()).unwrap();
                }
                requests.push(request);
            }
            requests
        });
        (format!("http://{addr}"), handle)
    }

    fn client(base: &str) -> Client {
        let mut client = Client::new(
            &cantrip::config::TelemetryConfig {
                enabled: true,
                endpoint: format!("{base}/api/public/otel/v1/traces"),
                public_key: "pk-synthetic".to_owned(),
                api_key_id: None,
            },
            "synthetic run/1".to_owned(),
            "first-run".to_owned(),
            run_start(&json!({ "date": "2026-09-07" })).unwrap(),
        )
        .unwrap();
        client.agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(2))
            .build();
        client
    }

    #[test]
    fn retries_rate_limit_and_disconnect_with_the_same_score_id() {
        let (base, server) = server(3, |attempt, _| {
            match attempt {
            0 => Some("HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nContent-Length: 99\r\nConnection: close\r\n\r\n".to_owned()),
            1 => None, // The remote write may have succeeded before losing its response.
            _ => Some(ok_json(&json!({}))),
        }
        });
        client(&base)
            .post_score("trace", "stt.wer", json!(0.25), "")
            .unwrap();
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].body, requests[1].body);
        assert_eq!(requests[1].body, requests[2].body);
        let ids: BTreeSet<_> = requests
            .iter()
            .map(|r| r.body["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), 1);
    }

    #[test]
    fn retries_an_incomplete_success_response() {
        let (base, server) = server(2, |attempt, _| {
            Some(if attempt == 0 {
                "HTTP/1.1 200 OK\r\nContent-Length: 99\r\nConnection: close\r\n\r\n".to_owned()
            } else {
                ok_json(&json!({}))
            })
        });
        client(&base)
            .post_score("trace", "stt.wer", json!(0.25), "")
            .unwrap();
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].body["id"], requests[1].body["id"]);
    }

    #[test]
    fn stops_on_exhaustion_or_auth_failure_without_logging_response_content() {
        let (base, exhausted) = server(3, |_, _| {
            Some(
            "HTTP/1.1 503 Unavailable\r\nContent-Length: 17\r\nConnection: close\r\n\r\nsensitive-fixture".to_owned()
        )
        });
        let error = client(&base)
            .post_score("trace", "score", json!(1), "")
            .unwrap_err();
        assert!(error.to_string().contains("3 attempts"));
        assert!(error.to_string().contains("503"));
        assert!(!format!("{error:#}").contains("sensitive-fixture"));
        assert_eq!(exhausted.join().unwrap().len(), 3);

        let (base, unauthorized) = server(1, |_, _| {
            Some(
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 17\r\nConnection: close\r\n\r\nsensitive-fixture".to_owned()
        )
        });
        let error = client(&base)
            .post_score("trace", "score", json!(1), "")
            .unwrap_err();
        assert!(error.to_string().contains("401"));
        assert!(!format!("{error:#}").contains("sensitive-fixture"));
        assert_eq!(unauthorized.join().unwrap().len(), 1);
    }

    #[test]
    fn retry_after_accepts_dates_and_caps_server_delays() {
        let now = UNIX_EPOCH + Duration::from_secs(784_111_775);
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", now),
            Some(Duration::from_secs(2))
        );
        assert_eq!(parse_retry_after("2", now), Some(Duration::from_secs(2)));
        assert_eq!(parse_retry_after("not a date", now), None);
        assert_eq!(
            retry_delay(1, Some(Duration::from_secs(u64::MAX))),
            MAX_RETRY_DELAY
        );
        assert_eq!(
            retry_delay(2, Some(Duration::ZERO)),
            Duration::from_millis(500)
        );
    }

    #[test]
    fn reuses_legacy_items_without_overwriting_changed_corpus_content() {
        let mut remote_items = vec![json!({
            "id": "legacy-id",
            "input": { "clip": "public-clip" },
            "expectedOutput": { "reference": "public reference" },
            "metadata": { "kind": "stt", "file": "public.wav" },
        })];
        let (base, server) = server(6, move |_, request| {
            if request
                .target
                .starts_with("GET /api/public/v2/datasets/synthetic%20run%2F1 ")
            {
                return Some(ok_json(&json!({ "id": "dataset" })));
            }
            if request.target.starts_with("GET /api/public/dataset-items?") {
                return Some(ok_json(
                    &json!({ "data": remote_items, "meta": { "totalPages": 1 } }),
                ));
            }
            assert!(request
                .target
                .starts_with("POST /api/public/dataset-items "));
            remote_items.retain(|item| item["id"] != request.body["id"]);
            remote_items.push(request.body.clone());
            Some(ok_json(&json!({ "id": request.body["id"] })))
        });
        let client = client(&base);
        let upload = |dataset: &mut Dataset, reference: &str| {
            client
                .upload_item(
                    dataset,
                    "stt:public-clip",
                    json!({ "clip": "public-clip" }),
                    json!({ "reference": reference }),
                    json!({ "kind": "stt", "file": "public.wav" }),
                )
                .unwrap()
        };
        let mut first = client.ensure_dataset().unwrap();
        let original = upload(&mut first, "public reference");
        let mut repeated = client.ensure_dataset().unwrap();
        assert_eq!(upload(&mut repeated, "public reference"), original);
        let changed = upload(&mut repeated, "revised public reference");
        assert_eq!(original, "legacy-id");
        assert_ne!(changed, original);
        let requests = server.join().unwrap();
        let uploads: Vec<_> = requests
            .iter()
            .filter(|r| r.target.starts_with("POST "))
            .collect();
        assert_eq!(uploads.len(), 2);
        assert_ne!(uploads[0].body["id"], uploads[1].body["id"]);
    }

    #[test]
    fn repeat_publication_preserves_ids_but_separates_runs_and_repeated_rows() {
        let (base, server) = server(12, |_, _| Some(ok_json(&json!({}))));
        let mut client = client(&base);
        let dataset = Dataset {
            id: "dataset".to_owned(),
            items: BTreeMap::new(),
        };
        let items = BTreeMap::from([("stt:public".to_owned(), "item".to_owned())]);
        let refs = BTreeMap::from([("public".to_owned(), "public reference".to_owned())]);
        let result = crate::SttResult {
            lane: "synthetic".to_owned(),
            clip: "public".to_owned(),
            audio_secs: 1.0,
            load_ms: None,
            latency_ms: 12,
            cost_usd: 0.0,
            cold: false,
            text: "synthetic transcript never sent in a trace".to_owned(),
        };
        publish_stt_result(&client, &dataset, &refs, &items, 0, &result).unwrap();
        publish_stt_result(&client, &dataset, &refs, &items, 0, &result).unwrap();
        client.run_id = "second-run".to_owned();
        publish_stt_result(&client, &dataset, &refs, &items, 0, &result).unwrap();
        publish_stt_result(&client, &dataset, &refs, &items, 1, &result).unwrap();
        let requests = server.join().unwrap();
        let mut traces = BTreeSet::new();
        let mut scores = BTreeSet::new();
        let mut experiments = BTreeSet::new();
        for request in &requests {
            if request.target.contains("/otel/") {
                assert!(!request.body.to_string().contains(&result.text));
                let span = &request.body["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
                traces.insert((
                    span["traceId"].as_str().unwrap(),
                    span["spanId"].as_str().unwrap(),
                    span["startTimeUnixNano"].as_str().unwrap(),
                ));
                let attributes = span["attributes"].as_array().unwrap();
                experiments.insert(
                    attributes
                        .iter()
                        .find(|a| a["key"] == "langfuse.experiment.id")
                        .unwrap()["value"]["stringValue"]
                        .as_str()
                        .unwrap(),
                );
            } else {
                scores.insert(request.body["id"].as_str().unwrap());
            }
        }
        assert_eq!(traces.len(), 3);
        assert_eq!(scores.len(), 6);
        assert_eq!(experiments.len(), 2);
    }

    #[test]
    fn run_identity_includes_results_corpus_and_optional_run_metadata() {
        let empty = Value::Null;
        let corpus = json!({ "ref": "public reference" });
        let results = json!([{ "text": "synthetic result" }]);
        let id = run_identity(None, &empty, [&corpus, &empty], [&results, &empty, &empty]).unwrap();
        assert_eq!(
            id,
            run_identity(None, &empty, [&corpus, &empty], [&results, &empty, &empty]).unwrap()
        );
        assert_ne!(
            id,
            run_identity(
                None,
                &empty,
                [&json!({ "ref": "corrected reference" }), &empty],
                [&results, &empty, &empty]
            )
            .unwrap()
        );
        assert_ne!(
            id,
            run_identity(
                None,
                &empty,
                [&corpus, &empty],
                [&json!([{ "text": "changed result" }]), &empty, &empty]
            )
            .unwrap()
        );
        assert_ne!(
            id,
            run_identity(
                None,
                &json!({ "run_id": "other" }),
                [&corpus, &empty],
                [&results, &empty, &empty]
            )
            .unwrap()
        );
        assert_ne!(
            id,
            run_identity(
                Some("independent-run"),
                &empty,
                [&corpus, &empty],
                [&results, &empty, &empty]
            )
            .unwrap()
        );
        assert_ne!(
            identity("item", &["a", "b:c"]),
            identity("item", &["a:b", "c"])
        );
    }

    #[test]
    fn immutable_timestamp_prefers_producer_metadata_and_rejects_missing_provenance() {
        assert_eq!(
            run_start(&json!({ "started_at_unix_ms": 1_234, "date": "2026-09-07" })).unwrap(),
            1_234_000_000
        );
        assert!(run_start(&json!({})).is_err());
        assert!(run_start(&json!({ "started_at_unix_ms": "1234", "date": "2026-09-07" })).is_err());
        assert_eq!(
            run_start(&json!({ "date": "2026-09-07" })).unwrap(),
            run_start(&json!({ "started_at": "2026-09-07T00:00:00Z" })).unwrap()
        );
    }

    #[test]
    fn reports_partial_otlp_rejection_without_echoing_server_messages() {
        let (base, server) = server(1, |_, _| {
            Some(ok_json(&json!({
                "partialSuccess": { "rejectedSpans": "1", "errorMessage": "sensitive-fixture" }
            })))
        });
        let error = client(&base).post_trace(&json!({})).unwrap_err();
        assert!(error.to_string().contains("rejected spans"));
        assert!(!format!("{error:#}").contains("sensitive-fixture"));
        assert_eq!(server.join().unwrap().len(), 1);
    }
}
