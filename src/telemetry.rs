//! Opt-in, metadata-only Langfuse telemetry: one trace per dictation job.
//!
//! Repo rule (AGENTS.md): transcript content never leaves the machine except
//! through `cantrip transcribe` stdout. Telemetry inherits that rule with no
//! exception — spans carry character counts, durations, model names, backend
//! names, and error classifications only. Never transcript text, never audio.
//!
//! Transport is hand-rolled OTLP/JSON over `ureq` (the same house pattern as
//! `stt.rs` and `postproc.rs`): no async runtime. The blocking POST runs on a
//! dedicated std thread fed by a bounded `sync_channel`; producers never
//! block — a full queue drops the trace with one warning line. Shutting the
//! [`TelemetryReporter`] down drains what was queued, so nothing is silently
//! lost on clean exits.
//!
//! Payload shape follows the Langfuse OpenTelemetry mapping: spans become
//! observations, `langfuse.observation.type = "generation"` (plus
//! `gen_ai.request.model` and `langfuse.observation.usage_details`) marks the
//! LLM calls, and the root span's input/output become the trace's.

use crate::config::TelemetryConfig;
use crate::keys;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::fs;
use std::io::Read;
use std::sync::mpsc::{self, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const EXPORT_TIMEOUT: Duration = Duration::from_secs(5);
/// Queue bound: past this, new traces are dropped instead of stalling the
/// daemon. Dictation cadence makes overflow a pathology, not steady state.
const QUEUE_BOUND: usize = 32;

/// Everything the exporter is allowed to know about one settled job.
/// There is deliberately no field here that could carry transcript text.
#[derive(Debug, Clone, Default)]
pub struct JobTelemetry {
    /// `dictation`, `recover`, or `transcribe`.
    pub source: &'static str,
    pub capture_ms: u64,
    pub stt_ms: u64,
    pub stt_model: String,
    pub stt_remote: bool,
    pub chars: usize,
    pub partial: bool,
    /// `applied`, `failed`, `off`, or `skipped_short`.
    pub cleanup_state: &'static str,
    pub cleanup_ms: Option<u64>,
    pub cleanup_model: Option<String>,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub tokens_total: Option<u64>,
    pub inject_ms: Option<u64>,
    /// `typed-wtype`, `typed-ydotool`, `pasted`, `clipboard`, or `None`
    /// when the text was not delivered.
    pub delivered: Option<&'static str>,
    /// Coarse failure class (e.g. `no-mic-signal`); never error bodies that
    /// could quote content.
    pub error_class: Option<String>,
    pub total_ms: u64,
}

/// Background exporter. Clone-free by design: hand the single value through
/// your call graph, call [`TelemetryReporter::report`] after jobs settle, and
/// [`TelemetryReporter::shutdown`] (or drop) on the way out.
pub struct TelemetryReporter {
    tx: Option<SyncSender<JobTelemetry>>,
    handle: Option<JoinHandle<()>>,
}

impl TelemetryReporter {
    /// Start the exporter thread. Does nothing when telemetry is disabled —
    /// `report` becomes a no-op and `shutdown` is free.
    pub fn spawn(config: TelemetryConfig) -> Self {
        if !config.enabled {
            return Self {
                tx: None,
                handle: None,
            };
        }
        let (tx, rx) = mpsc::sync_channel::<JobTelemetry>(QUEUE_BOUND);
        let handle = match thread::Builder::new()
            .name("telemetry".into())
            .spawn(move || {
                // Block for work while the reporter lives; when it is
                // dropped the channel closes, this loop drains whatever was
                // queued, and the thread exits. No trace is silently lost
                while let Ok(job) = rx.recv() {
                    if let Err(error) = export(&config, &job) {
                        tracing::warn!("[Telemetry] export failed status-only error={error:#}");
                    }
                }
            }) {
            Ok(handle) => handle,
            Err(error) => {
                // Fail open: telemetry is optional and must never take
                // dictation down under thread or resource exhaustion.
                tracing::warn!(
                    "[Telemetry] exporter thread unavailable, telemetry disabled error={error}"
                );
                return Self {
                    tx: None,
                    handle: None,
                };
            }
        };
        Self {
            tx: Some(tx),
            handle: Some(handle),
        }
    }

    /// Queue one settled job. Never blocks: when the queue is full the trace
    /// is dropped with a warning rather than stalling the producer.
    pub fn report(&self, job: JobTelemetry) {
        if let Some(tx) = &self.tx {
            match tx.try_send(job) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(job)) => {
                    tracing::warn!("[Telemetry] queue full, dropped trace chars={}", job.chars);
                }
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    tracing::warn!("[Telemetry] exporter closed, dropped trace");
                }
            }
        }
    }

    /// Close the queue and wait for the worker to drain it. Call this on
    /// clean shutdown paths so queued traces are not lost.
    pub fn shutdown(mut self) {
        self.tx.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for TelemetryReporter {
    fn drop(&mut self) {
        // Closing the channel lets the worker drain and exit on its own tick.
        self.tx.take();
    }
}

fn export(config: &TelemetryConfig, job: &JobTelemetry) -> Result<()> {
    let payload = otlp_payload(job);
    // No key id configured means no OS-keyring round-trip: tests and
    // disabled setups must never touch the secret service.
    let secret = match &config.api_key_id {
        Some(id) => keys::get(id)?,
        None => String::new(),
    };
    let credential = format!("{}:{}", config.public_key, secret);
    let agent = ureq::AgentBuilder::new().timeout(EXPORT_TIMEOUT).build();
    let response = agent
        .post(&config.endpoint)
        .set(
            "Authorization",
            &format!("Basic {}", base64(credential.as_bytes())),
        )
        .set("Content-Type", "application/json")
        .set("x-langfuse-ingestion-version", "4")
        .send_string(&payload.to_string())
        .with_context(|| "sending telemetry trace")?;
    let status = response.status();
    if !(200..300).contains(&status) {
        anyhow::bail!("telemetry endpoint returned status {status}");
    }
    Ok(())
}

/// The OTLP/JSON `ExportTraceServiceRequest` for one job: a root
/// `dictation` span plus one child per attempted stage.
fn otlp_payload(job: &JobTelemetry) -> Value {
    let trace_id = random_hex(16);
    let root_span_id = random_hex(8);
    let start_nanos = system_nanos();
    let capture_end = start_nanos + ms_to_nanos(job.capture_ms);
    let stt_end = capture_end + ms_to_nanos(job.stt_ms);
    let cleanup_end = stt_end + ms_to_nanos(job.cleanup_ms.unwrap_or(0));
    let deliver_nanos = ms_to_nanos(job.inject_ms.unwrap_or(1));
    let end_nanos = cleanup_end.max(start_nanos + 1) + deliver_nanos;

    let mut spans = vec![span(
        &trace_id,
        &root_span_id,
        "",
        "dictation",
        start_nanos,
        end_nanos,
        job.error_class.is_some(),
        &[
            attr_input(&json!({
                "source": job.source,
                "capture_ms": job.capture_ms,
            })),
            attr_output(&json!({
                "delivered": job.delivered,
                "chars": job.chars,
                "cleanup": job.cleanup_state,
                "partial": job.partial,
            })),
            attr(
                "cantrip.error_class",
                string_value_opt(job.error_class.clone()),
            ),
        ],
    )];

    spans.push(span(
        &trace_id,
        &random_hex(8),
        &root_span_id,
        "transcribe-speech",
        capture_end,
        stt_end,
        false,
        &[
            attr("stt.ms", int_value(job.stt_ms)),
            attr("stt.chars", int_value(job.chars as u64)),
            attr("stt.partial", bool_value(job.partial)),
            if job.stt_remote {
                // Cloud STT is an AI call: mark it a generation so Langfuse
                // renders model and cost analytics for it.
                attr("langfuse.observation.type", string_value("generation"))
            } else {
                // Local Parakeet carries a model name, which the model-based
                // mapper would misread as a generation; pin the type.
                attr("langfuse.observation.type", string_value("span"))
            },
            attr("gen_ai.request.model", string_value(&job.stt_model)),
        ],
    ));

    if job.cleanup_ms.is_some() || job.cleanup_state != "off" {
        let mut cleanup_attrs = vec![
            attr("langfuse.observation.type", string_value("generation")),
            attr("cleanup.ms", int_value(job.cleanup_ms.unwrap_or(0))),
            attr("cleanup.state", string_value(job.cleanup_state)),
            attr_input(&json!({ "chars": job.chars })),
            attr_output(&json!({ "chars": job.chars })),
        ];
        if let Some(model) = &job.cleanup_model {
            cleanup_attrs.push(attr("gen_ai.request.model", string_value(model)));
        }
        if let (Some(input), Some(output), Some(total)) =
            (job.tokens_in, job.tokens_out, job.tokens_total)
        {
            // Primary mapping (parsed since langfuse#10153) plus the
            // gen_ai.* fallback some ingestion versions prefer.
            cleanup_attrs.push(attr(
                "langfuse.observation.usage_details",
                string_value(
                    &json!({ "input": input, "output": output, "total": total }).to_string(),
                ),
            ));
            cleanup_attrs.push(attr("gen_ai.usage.input_tokens", int_value(input)));
            cleanup_attrs.push(attr("gen_ai.usage.output_tokens", int_value(output)));
            cleanup_attrs.push(attr("gen_ai.usage.total_tokens", int_value(total)));
        }
        spans.push(span(
            &trace_id,
            &random_hex(8),
            &root_span_id,
            "cleanup-transcript",
            stt_end,
            cleanup_end,
            job.cleanup_state == "failed",
            &cleanup_attrs,
        ));
    }

    if let Some(inject_ms) = job.inject_ms {
        spans.push(span(
            &trace_id,
            &random_hex(8),
            &root_span_id,
            "deliver-text",
            cleanup_end,
            end_nanos,
            job.delivered.is_none(),
            &[
                attr("inject.ms", int_value(inject_ms)),
                attr(
                    "inject.backend",
                    string_value(job.delivered.unwrap_or("none")),
                ),
            ],
        ));
    }

    json!({
        "resourceSpans": [{
            "resource": { "attributes": [
                attr("service.name", string_value("cantrip")),
                attr("service.version", string_value(env!("CARGO_PKG_VERSION"))),
            ]},
            "scopeSpans": [{
                "scope": { "name": "cantrip-telemetry", "version": env!("CARGO_PKG_VERSION") },
                "spans": spans,
            }],
        }]
    })
}

/// Paint-primitive-style plumbing: every field of an OTel span is a
/// parameter, matching the house pattern in `hud.rs`.
#[allow(clippy::too_many_arguments)]
fn span(
    trace_id: &str,
    span_id: &str,
    parent_span_id: &str,
    name: &str,
    start_nanos: u128,
    end_nanos: u128,
    error: bool,
    attributes: &[Value],
) -> Value {
    json!({
        "traceId": trace_id,
        "spanId": span_id,
        "parentSpanId": parent_span_id,
        "name": name,
        "kind": 1,
        "startTimeUnixNano": start_nanos.to_string(),
        "endTimeUnixNano": end_nanos.to_string(),
        "attributes": attributes,
        "status": { "code": if error { 2 } else { 1 } },
    })
}

fn attr(key: &str, value: Value) -> Value {
    json!({ "key": key, "value": value })
}

fn attr_input(value: &Value) -> Value {
    attr(
        "langfuse.observation.input",
        string_value(&value.to_string()),
    )
}

fn attr_output(value: &Value) -> Value {
    attr(
        "langfuse.observation.output",
        string_value(&value.to_string()),
    )
}

fn string_value(text: &str) -> Value {
    json!({ "stringValue": text })
}

fn string_value_opt(text: Option<String>) -> Value {
    match text {
        Some(text) => string_value(&text),
        None => json!({}),
    }
}

fn int_value(number: u64) -> Value {
    json!({ "intValue": number })
}

fn bool_value(flag: bool) -> Value {
    json!({ "boolValue": flag })
}

fn ms_to_nanos(ms: u64) -> u128 {
    u128::from(ms) * 1_000_000
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
            // Fallback entropy: address-space jitter is weak but keeps the
            // exporter working on exotic systems without /dev/urandom.
            let seed = system_nanos() as u64;
            for (index, byte) in buf.iter_mut().enumerate() {
                *byte = (seed >> ((index % 8) * 8)) as u8 ^ index as u8;
            }
            return buf.iter().map(|byte| format!("{byte:02x}")).collect();
        }
    };
    if random.read_exact(&mut buf).is_err() {
        let seed = system_nanos() as u64;
        for (index, byte) in buf.iter_mut().enumerate() {
            *byte = (seed >> ((index % 8) * 8)) as u8 ^ index as u8;
        }
    }
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

    fn sample_job() -> JobTelemetry {
        JobTelemetry {
            source: "dictation",
            capture_ms: 4_200,
            stt_ms: 310,
            stt_model: "parakeet-tdt-0.6b-v3-int8".into(),
            stt_remote: false,
            chars: 42,
            partial: false,
            cleanup_state: "applied",
            cleanup_ms: Some(900),
            cleanup_model: Some("qwen3-8b".into()),
            tokens_in: Some(120),
            tokens_out: Some(45),
            tokens_total: Some(165),
            inject_ms: Some(80),
            delivered: Some("pasted"),
            error_class: None,
            total_ms: 5_510,
        }
    }

    #[test]
    fn payload_has_root_and_stage_spans() {
        let payload = otlp_payload(&sample_job());
        let spans = &payload["resourceSpans"][0]["scopeSpans"][0]["spans"];
        assert_eq!(spans.as_array().unwrap().len(), 4);
        assert_eq!(spans[0]["name"], "dictation");
        assert_eq!(spans[0]["parentSpanId"], "");
        assert_eq!(spans[1]["name"], "transcribe-speech");
        assert_eq!(spans[2]["name"], "cleanup-transcript");
        assert_eq!(spans[3]["name"], "deliver-text");
        for span in spans.as_array().unwrap() {
            assert_eq!(span["traceId"].as_str().unwrap().len(), 32);
            assert_eq!(span["spanId"].as_str().unwrap().len(), 16);
        }
    }

    #[test]
    fn local_stt_is_span_and_cleanup_is_generation() {
        let payload = otlp_payload(&sample_job());
        let spans = &payload["resourceSpans"][0]["scopeSpans"][0]["spans"];
        let types: Vec<&str> = spans.as_array().unwrap().iter().map(type_of).collect();
        assert_eq!(types, vec!["", "span", "generation", ""]);
    }

    fn type_of(span: &Value) -> &str {
        span["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["key"] == "langfuse.observation.type")
            .and_then(|a| a["value"]["stringValue"].as_str())
            .unwrap_or("")
    }

    #[test]
    fn root_input_output_carry_counts_only() {
        let payload = otlp_payload(&sample_job());
        let spans = &payload["resourceSpans"][0]["scopeSpans"][0]["spans"];
        for span in spans.as_array().unwrap() {
            for key in ["langfuse.observation.input", "langfuse.observation.output"] {
                if let Some(raw) = find_attr(span, key).and_then(|v| v.as_str()) {
                    let parsed: Value =
                        serde_json::from_str(raw).expect("input/output must be JSON");
                    const CONTROLLED: [&str; 11] = [
                        "dictation",
                        "recover",
                        "transcribe",
                        "applied",
                        "failed",
                        "off",
                        "skipped_short",
                        "typed-wtype",
                        "typed-ydotool",
                        "pasted",
                        "clipboard",
                    ];
                    for (name, value) in parsed.as_object().expect("JSON object") {
                        let allowed = value.is_number()
                            || value.is_boolean()
                            || value.is_null()
                            || value
                                .as_str()
                                .map(|text| CONTROLLED.contains(&text))
                                .unwrap_or(false);
                        assert!(
                            allowed,
                            "{key}.{name} must be a count, flag, or controlled term, got {value}"
                        );
                    }
                }
            }
        }
    }

    fn find_attr<'a>(span: &'a Value, key: &str) -> Option<&'a Value> {
        span["attributes"]
            .as_array()?
            .iter()
            .find(|a| a["key"] == key)
            .map(|a| &a["value"]["stringValue"])
    }

    #[test]
    fn usage_details_land_on_the_generation() {
        let payload = otlp_payload(&sample_job());
        let spans = &payload["resourceSpans"][0]["scopeSpans"][0]["spans"];
        let cleanup = &spans.as_array().unwrap()[2];
        let usage = find_attr(cleanup, "langfuse.observation.usage_details")
            .expect("usage details attribute");
        let parsed: Value = serde_json::from_str(usage.as_str().unwrap()).unwrap();
        assert_eq!(parsed["input"], 120);
        assert_eq!(parsed["output"], 45);
        assert_eq!(parsed["total"], 165);
    }

    #[test]
    fn disabled_reporter_never_spawns_a_thread_or_sends() {
        let reporter = TelemetryReporter::spawn(TelemetryConfig {
            enabled: false,
            ..Default::default()
        });
        reporter.report(sample_job()); // must be a silent no-op
        reporter.shutdown();
    }

    #[test]
    fn failed_stt_marks_the_root_span_errored() {
        let mut job = sample_job();
        job.chars = 0;
        job.delivered = None;
        job.error_class = Some("no-mic-signal".into());
        let payload = otlp_payload(&job);
        let spans = &payload["resourceSpans"][0]["scopeSpans"][0]["spans"];
        assert_eq!(spans[0]["status"]["code"], 2);
    }

    #[test]

    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }
}
