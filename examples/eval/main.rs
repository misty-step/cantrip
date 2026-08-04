//! Evaluation gauntlet for cantrip transcription + post-processing.
//!
//! Runs every configured STT and postproc model through a clip set and emits
//! per-call metrics (latency, cost) plus WER/CER scores and arrangement
//! scoreboards.
//!
//! Privacy rule: raw transcripts are written only to result JSON files under
//! the output directory. stdout/stderr never carry transcript text — boards
//! and logs carry ids, counts, and metrics only.
//!
//! Usage (cargo example, cwd = repo root):
//!   cargo run --release --example eval -- list [--config PATH]
//!   cargo run --release --example eval -- run [--config PATH] [--stt a,b] [--postproc c,d] [--clips x,y]
//!   cargo run --release --example eval -- run --ppr-only [--out DIR]

use std::collections::BTreeMap;
use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

use transcribe_rs::onnx::canary::{CanaryModel, CanaryParams};
use transcribe_rs::onnx::moonshine::{MoonshineModel, MoonshineParams, MoonshineVariant};
use transcribe_rs::onnx::parakeet::{ParakeetModel, ParakeetParams};
use transcribe_rs::onnx::Quantization;

mod wer;

const BOUNDARY: &str = "cantrip-eval-boundary-3fa91c";

/// Optional proxy prefix for provider endpoints, read from the `CANTRIP_PROXY`
/// environment variable. When set, each HTTPS provider URL is routed through
/// `<prefix>/<host>/<path>`; when unset, providers are called directly. The
/// local credential broker on the tailnet keys on `/proxy/https/<host>/<path>`,
/// so an evaluator supplies e.g. `CANTRIP_PROXY=http://host:4949/proxy/https`.
/// The prefix is never committed so broker endpoints stay out of public source.
fn proxy_prefix() -> Option<String> {
    std::env::var("CANTRIP_PROXY")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_owned())
        .filter(|value| !value.is_empty())
}

fn proxied_with(prefix: &str, url: &str) -> String {
    let prefix = prefix.trim().trim_end_matches('/');
    if prefix.is_empty() || !url.starts_with("https://") {
        return url.to_owned();
    }
    format!("{prefix}/{}", &url["https://".len()..])
}

fn proxied(url: &str) -> String {
    proxied_with(proxy_prefix().as_deref().unwrap_or(""), url)
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("list");
    let rest = args.get(2..).unwrap_or_default();
    if cmd == "run" && rest.iter().any(|a| a == "--ppr-only") {
        return run_ppr_only(rest);
    }
    match cmd {
        "list" => list(rest),
        "run" => run(rest),
        other => bail!("unknown subcommand '{other}' (expected list | run)"),
    }
}

// ---------------------------------------------------------------------------
// Config types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct EvalConfig {
    manifest: String,
    out_dir: String,
    instructions: String,
    #[serde(default)]
    stt: Vec<SttLane>,
    #[serde(default)]
    postproc: Vec<PostprocLane>,
}

#[derive(Debug, Deserialize, Clone)]
struct PriceSpec {
    #[serde(default)]
    unit: String, // per_min | per_token | zero | openrouter
    #[serde(default)]
    rate: f64, // $ per 60s of audio (per_min)
    #[serde(default)]
    input: f64, // $ per 1M input tokens (per_token)
    #[serde(default)]
    output: f64, // $ per 1M output tokens
}

#[derive(Debug, Deserialize)]
struct SttLane {
    id: String,
    kind: String, // transcribe_rs | whisper_cpp | openai | deepgram | elevenlabs
    #[serde(default)]
    family: Option<String>, // parakeet | canary | moonshine
    #[serde(default)]
    dir: Option<String>,
    #[serde(default)]
    variant: Option<String>, // moonshine variant name
    #[serde(default)]
    quant: Option<String>, // int8 | int4 | fp16 | fp32
    #[serde(default)]
    bin: Option<String>, // whisper-cli path
    #[serde(default)]
    model_file: Option<String>, // ggml model path
    #[serde(default)]
    endpoint: Option<String>, // cloud base (already includes mint proxy if needed)
    #[serde(default)]
    path: Option<String>, // cloud path suffix
    #[serde(default)]
    model: Option<String>, // cloud model id
    #[serde(default)]
    marker: Option<String>, // __mint.<alias>.__ value-free marker
    #[serde(default)]
    scheme: Option<String>, // Bearer | Token | <literal header name>
    #[serde(default)]
    extra: Vec<String>, // extra query params "k=v"
    #[serde(default)]
    pricing: Option<PriceSpec>,
}

#[derive(Debug, Deserialize)]
struct PostprocLane {
    id: String,
    endpoint: String,
    model: String,
    #[serde(default)]
    marker: Option<String>,
    #[serde(default)]
    scheme: Option<String>,
    #[serde(default)]
    pricing: Option<PriceSpec>,
    /// Set for local Ollama lanes: the base URL (without /v1) used to evict
    /// the model after the lane so the next large model fits in VRAM.
    #[serde(default)]
    ollama_base: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Manifest {
    clips: Vec<Clip>,
}

#[derive(Debug, Deserialize)]
struct Clip {
    id: String,
    file: String,
    #[serde(rename = "ref", default)]
    reference: String,
}

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct SttResult {
    lane: String,
    clip: String,
    audio_secs: f64,
    load_ms: Option<u128>,
    latency_ms: u128,
    cost_usd: f64,
    cold: bool,
    text: String,
}

#[derive(Debug, Serialize)]
struct PprResult {
    lane: String,
    stt_lane: String,
    clip: String,
    latency_ms: u128,
    cost_usd: f64,
    input_tokens: u64,
    output_tokens: u64,
    /// True when the model hallucinated/degenerated: output far larger than input.
    degenerate: bool,
    raw_text: String,
    text: String,
}

// ---------------------------------------------------------------------------
// Local STT (transcribe-rs)
// ---------------------------------------------------------------------------

enum LocalModel {
    Parakeet(ParakeetModel),
    Canary(CanaryModel),
    Moonshine(MoonshineModel),
}

fn parse_quant(s: Option<&str>) -> Quantization {
    match s.map(str::to_ascii_lowercase).as_deref() {
        Some("int8") => Quantization::Int8,
        Some("int4") => Quantization::Int4,
        Some("fp16") => Quantization::FP16,
        _ => Quantization::FP32,
    }
}

fn parse_variant(s: Option<&str>) -> Result<MoonshineVariant> {
    match s.map(str::to_ascii_lowercase).as_deref() {
        Some("tiny") => Ok(MoonshineVariant::Tiny),
        Some("base") => Ok(MoonshineVariant::Base),
        other => bail!("unsupported moonshine variant {other:?}"),
    }
}

impl LocalModel {
    fn load(lane: &SttLane) -> Result<Self> {
        let dir = lane
            .dir
            .as_deref()
            .with_context(|| format!("lane '{}' needs dir", lane.id))?;
        let dir_path = Path::new(dir);
        let quant_q = parse_quant(lane.quant.as_deref());
        let q = &quant_q;
        match lane.family.as_deref() {
            Some("parakeet") => ParakeetModel::load(dir_path, q)
                .map(LocalModel::Parakeet)
                .with_context(|| format!("loading Parakeet from {dir}")),
            Some("canary") => CanaryModel::load(dir_path, q)
                .map(LocalModel::Canary)
                .with_context(|| format!("loading Canary from {dir}")),
            Some("moonshine") => {
                let variant = parse_variant(lane.variant.as_deref())?;
                MoonshineModel::load(dir_path, variant, q)
                    .map(LocalModel::Moonshine)
                    .with_context(|| format!("loading Moonshine from {dir}"))
            }
            other => bail!(
                "lane '{}' has unknown transcribe_rs family {other:?}",
                lane.id
            ),
        }
    }

    fn transcribe(&mut self, samples: &[f32]) -> Result<String> {
        let text = match self {
            LocalModel::Parakeet(m) => {
                m.transcribe_with(
                    samples,
                    &ParakeetParams {
                        ..Default::default()
                    },
                )
                .map_err(anyhow::Error::from)?
                .text
            }
            LocalModel::Canary(m) => {
                m.transcribe_with(
                    samples,
                    &CanaryParams {
                        ..Default::default()
                    },
                )
                .map_err(anyhow::Error::from)?
                .text
            }
            LocalModel::Moonshine(m) => {
                m.transcribe_with(
                    samples,
                    &MoonshineParams {
                        ..Default::default()
                    },
                )
                .map_err(anyhow::Error::from)?
                .text
            }
        };
        Ok(text.trim().to_owned())
    }
}

// ---------------------------------------------------------------------------
// whisper.cpp local lane
// ---------------------------------------------------------------------------

fn whisper_cpp(lane: &SttLane, wav: &Path) -> Result<String> {
    let bin = lane.bin.as_deref().context("whisper lane needs bin")?;
    let model = lane
        .model_file
        .as_deref()
        .context("whisper lane needs model_file")?;

    // Private scratch dir under the system temp so the `-of` output path is
    // not predictable (no symlink/stale-file race in a shared temp).
    let base = std::env::temp_dir().join(format!("cantrip-eval-whisper-{}", std::process::id()));
    fs::create_dir_all(&base)
        .with_context(|| format!("creating whisper scratch base {}", base.display()))?;
    let mut work_dir: Option<PathBuf> = None;
    for _ in 0..64 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let candidate = base.join(format!("run-{nanos}"));
        if fs::create_dir(&candidate).is_ok() {
            work_dir = Some(candidate);
            break;
        }
    }
    let work_dir = work_dir.context("creating private whisper scratch dir")?;
    let prefix = work_dir.join("out");
    let txt = prefix.with_extension("txt");

    let lib_dir = Path::new(bin)
        .parent()
        .unwrap_or(Path::new("."))
        .join("..")
        .join("lib");
    let lib_dir = if lib_dir.exists() {
        lib_dir
    } else {
        PathBuf::new()
    };
    let mut cmd = Command::new(bin);
    if !lib_dir.as_os_str().is_empty() {
        let existing = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
        cmd.env(
            "LD_LIBRARY_PATH",
            if existing.is_empty() {
                lib_dir.as_os_str().to_os_string()
            } else {
                format!("{}:{}", lib_dir.display(), existing).into()
            },
        );
        // whisper.cpp backend plugin path is a FILE (the named backend .so),
        // not the directory.
        // Prefer the generic x64 backend (the proven one); fall back to any.
        let backend = fs::read_dir(&lib_dir).ok().and_then(|it| {
            let mut candidates: Vec<PathBuf> = it
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    let n = p
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    n.starts_with("libggml-") && n.ends_with(".so")
                })
                .collect();
            candidates.sort_by_key(|p| {
                let n = p
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                !(n == "libggml-cpu-x64.so")
            });
            candidates.into_iter().next()
        });
        if let Some(backend) = backend {
            cmd.env("GGML_BACKEND_PATH", &backend);
        }
    }
    let output = cmd
        .args([
            "-m",
            model,
            "-f",
            wav.to_str().unwrap_or(""),
            "-nt",
            "-otxt",
            "-of",
            prefix.to_str().unwrap_or(""),
            "-t",
            "16",
            "-ng",
            "-np",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .with_context(|| format!("running {} for lane '{}'", bin, lane.id))?;
    if !output.status.success() {
        let _ = fs::remove_file(&txt);
        let _ = fs::remove_dir(&work_dir);
        bail!(
            "whisper-cli exited {:?} for lane '{}'",
            output.status.code(),
            lane.id
        );
    }
    let text = fs::read_to_string(&txt)
        .with_context(|| format!("reading whisper output {}", txt.display()));
    if let Err(error) = fs::remove_file(&txt) {
        eprintln!(
            "[eval] warn: could not remove whisper tmp transcript {}: {error}",
            txt.display()
        );
    }
    let _ = fs::remove_dir(&work_dir);
    Ok(text?.trim().to_owned())
}

// ---------------------------------------------------------------------------
// Cloud STT adapters
// ---------------------------------------------------------------------------

fn auth_headers(scheme: &str, marker: &str) -> Vec<(String, String)> {
    match scheme {
        "Bearer" => vec![("Authorization".to_string(), format!("Bearer {marker}"))],
        "Token" => vec![("Authorization".to_string(), format!("Token {marker}"))],
        other => vec![(other.to_string(), marker.to_string())],
    }
}

fn multipart_wav(wav: &Path, fields: &[(&str, &str)]) -> Result<Vec<u8>> {
    let mut body: Vec<u8> = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    let data = fs::read(wav).with_context(|| format!("reading {}", wav.display()))?;
    let filename: String = wav
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("audio.wav")
        .chars()
        .filter(|c| *c != '"' && !c.is_control())
        .collect();
    let filename = if filename.is_empty() {
        "audio.wav"
    } else {
        &filename
    };
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(&data);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    Ok(body)
}

#[derive(Debug, Deserialize)]
struct OpenAiTranscription {
    text: String,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenAiUsage {
    /// Schema fields retained for full-fidelity parsing; not consumed by cost math.
    #[allow(dead_code)]
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    seconds: f64,
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

/// OpenAI-compatible multipart transcription. Returns (text, input_tokens, output_tokens).
fn openai_transcribe(
    agent: &ureq::Agent,
    lane: &SttLane,
    wav: &Path,
) -> Result<(String, u64, u64)> {
    let endpoint = lane
        .endpoint
        .as_deref()
        .with_context(|| format!("lane '{}' needs endpoint", lane.id))?;
    let path = lane
        .path
        .as_deref()
        .with_context(|| format!("lane '{}' needs path", lane.id))?;
    let model = lane
        .model
        .as_deref()
        .with_context(|| format!("lane '{}' needs model", lane.id))?;
    let marker = lane
        .marker
        .as_deref()
        .with_context(|| format!("lane '{}' needs marker", lane.id))?;
    let scheme = lane.scheme.as_deref().unwrap_or("Bearer");
    let url = proxied(&format!("{}{}", endpoint.trim_end_matches('/'), path));
    let body = multipart_wav(wav, &[("model", model)])?;
    let mut request = agent.post(&url).set(
        "Content-Type",
        &format!("multipart/form-data; boundary={BOUNDARY}"),
    );
    if proxy_prefix().is_some() {
        // Mint markers are only meaningful through the proxy.
        for (name, value) in auth_headers(scheme, marker) {
            request = request.set(&name, &value);
        }
    }
    let response = request
        .send_bytes(&body)
        .with_context(|| format!("POST {url} for lane '{}'", lane.id))?;
    let parsed: OpenAiTranscription = response
        .into_string()
        .with_context(|| format!("reading response for lane '{}'", lane.id))
        .and_then(|raw| {
            serde_json::from_str(&raw).map_err(|e| anyhow!("bad OpenAI transcript response: {e}"))
        })?;
    let usage = parsed.usage.unwrap_or(OpenAiUsage {
        kind: None,
        seconds: 0.0,
        input_tokens: 0,
        output_tokens: 0,
    });
    Ok((
        parsed.text.trim().to_owned(),
        usage.input_tokens,
        usage.output_tokens,
    ))
}

#[derive(Debug, Default, Deserialize)]
struct DeepgramResponse {
    #[serde(default)]
    results: DeepgramResults,
}

#[derive(Debug, Default, Deserialize)]
struct DeepgramResults {
    #[serde(default)]
    channels: Vec<DeepgramChannel>,
}

#[derive(Debug, Default, Deserialize)]
struct DeepgramChannel {
    #[serde(default)]
    alternatives: Vec<DeepgramAlternative>,
}

#[derive(Debug, Default, Deserialize)]
struct DeepgramAlternative {
    #[serde(default)]
    transcript: String,
}

fn deepgram_transcribe(agent: &ureq::Agent, lane: &SttLane, wav: &Path) -> Result<String> {
    let endpoint = lane.endpoint.as_deref().context("deepgram endpoint")?;
    let path = lane.path.as_deref().context("deepgram path")?;
    let model = lane.model.as_deref().context("deepgram model")?;
    let marker = lane.marker.as_deref().context("deepgram marker")?;
    let mut url = proxied(&format!(
        "{}{}?model={}",
        endpoint.trim_end_matches('/'),
        path,
        percent_encode(model)
    ));
    for pair in &lane.extra {
        if let Some((k, v)) = pair.split_once('=') {
            url.push('&');
            url.push_str(&percent_encode(k));
            url.push('=');
            url.push_str(&percent_encode(v));
        }
    }
    let body = fs::read(wav).with_context(|| format!("reading {}", wav.display()))?;
    let mut request = agent.post(&url).set("Content-Type", "audio/wav");
    if proxy_prefix().is_some() {
        // Mint markers are only meaningful through the proxy.
        for (name, value) in auth_headers(lane.scheme.as_deref().unwrap_or("Token"), marker) {
            request = request.set(&name, &value);
        }
    }
    let response = request
        .send_bytes(&body)
        .with_context(|| format!("POST {url} for lane '{}'", lane.id))?;
    let parsed: DeepgramResponse = response
        .into_string()
        .with_context(|| format!("reading response for lane '{}'", lane.id))
        .and_then(|raw| {
            serde_json::from_str(&raw).map_err(|e| anyhow!("bad Deepgram response: {e}"))
        })?;
    let text = parsed
        .results
        .channels
        .first()
        .and_then(|c| c.alternatives.first())
        .map(|a| a.transcript.trim().to_owned())
        .unwrap_or_default();
    if text.is_empty() {
        bail!("Deepgram returned no transcript for lane '{}'", lane.id);
    }
    Ok(text)
}

#[derive(Debug, Deserialize)]
struct ElevenResponse {
    text: String,
    /// Schema field retained for full-fidelity parsing.
    #[allow(dead_code)]
    #[serde(default)]
    words: Option<serde_json::Value>,
}

fn elevenlabs_transcribe(agent: &ureq::Agent, lane: &SttLane, wav: &Path) -> Result<String> {
    let endpoint = lane.endpoint.as_deref().context("elevenlabs endpoint")?;
    let path = lane.path.as_deref().context("elevenlabs path")?;
    let model = lane.model.as_deref().context("elevenlabs model")?;
    let marker = lane.marker.as_deref().context("elevenlabs marker")?;
    let url = proxied(&format!("{}{}", endpoint.trim_end_matches('/'), path));
    let body = multipart_wav(wav, &[("model_id", model)])?;
    let mut request = agent.post(&url).set(
        "Content-Type",
        &format!("multipart/form-data; boundary={BOUNDARY}"),
    );
    if proxy_prefix().is_some() {
        // Mint markers are only meaningful through the proxy.
        for (name, value) in auth_headers(lane.scheme.as_deref().unwrap_or("xi-api-key"), marker) {
            request = request.set(&name, &value);
        }
    }
    let response = request
        .send_bytes(&body)
        .with_context(|| format!("POST {url} for lane '{}'", lane.id))?;
    let parsed: ElevenResponse = response
        .into_string()
        .with_context(|| format!("reading response for lane '{}'", lane.id))
        .and_then(|raw| {
            serde_json::from_str(&raw).map_err(|e| anyhow!("bad ElevenLabs response: {e}"))
        })?;
    let text = parsed.text.trim().to_owned();
    if text.is_empty() {
        bail!("ElevenLabs returned no transcript for lane '{}'", lane.id);
    }
    Ok(text)
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Post-processing (chat completions)
// ---------------------------------------------------------------------------

/// Strip ` thinking…response` blocks from a model reply. Mirrors production
/// `postproc.rs` so eval and the daemon score the same cleaned surface.
fn strip_think_blocks(text: &str) -> String {
    const OPEN: &str = " thinking";
    const CLOSE: &str = " response";
    let mut result = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some(relative_start) = text[cursor..].find(OPEN) {
        let start = cursor + relative_start;
        result.push_str(&text[cursor..start]);
        let content_start = start + OPEN.len();
        let Some(relative_end) = text[content_start..].find(CLOSE) else {
            cursor = text.len();
            break;
        };
        cursor = content_start + relative_end + CLOSE.len();
    }
    if cursor < text.len() {
        result.push_str(&text[cursor..]);
    }
    result.trim().to_owned()
}

/// Clean a reply and fail loudly if nothing usable remains.
fn clean_content(content: &str) -> Result<String> {
    let cleaned = strip_think_blocks(content);
    if cleaned.is_empty() {
        bail!("postproc returned empty text");
    }
    Ok(cleaned)
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<ChatUsage>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

#[derive(Debug, Deserialize)]
struct ChatMessage {
    #[serde(default)]
    content: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ChatUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

fn postproc_call(
    agent: &ureq::Agent,
    lane: &PostprocLane,
    transcript: &str,
    instructions: &str,
) -> Result<(String, ChatUsage, u128)> {
    if let Some(base) = &lane.ollama_base {
        return ollama_postproc(agent, base, lane, transcript, instructions);
    }
    let url = proxied(&format!(
        "{}/chat/completions",
        lane.endpoint.trim_end_matches('/')
    ));
    let body = json!({
        "model": lane.model,
        "temperature": 0,
        // Cap generation: dictation cleanup is small; this also bounds models
        // that decompose into an unbounded reasoning loop (their exorbitant
        // outputs are then flagged as degenerate rather than hanging the run).
        "max_tokens": 2048,
        "messages": [
            {"role": "system", "content": instructions},
            {"role": "user", "content": transcript},
        ],
    });
    let start = Instant::now();
    let mut request = agent.post(&url);
    // Mint markers are only meaningful through the proxy.
    if proxy_prefix().is_some() {
        if let Some(marker) = &lane.marker {
            for (name, value) in auth_headers(lane.scheme.as_deref().unwrap_or("Bearer"), marker) {
                request = request.set(&name, &value);
            }
        }
    }
    let payload = serde_json::to_vec(&body).context("serializing chat request")?;
    let response = request
        .set("Content-Type", "application/json")
        .send_bytes(&payload)
        .with_context(|| format!("POST {url} for lane '{}'", lane.id))?;
    let parsed: ChatResponse = response
        .into_string()
        .with_context(|| format!("reading response for lane '{}'", lane.id))
        .and_then(|raw| {
            serde_json::from_str(&raw).map_err(|e| anyhow!("bad chat response: {e}"))
        })?;
    let latency = start.elapsed().as_millis();
    let raw = parsed
        .choices
        .first()
        .map(|c| c.message.content.as_str())
        .unwrap_or_default();
    let content = clean_content(raw)
        .with_context(|| format!("postproc '{}' returned empty content", lane.id))?;
    let usage = parsed.usage.unwrap_or(ChatUsage {
        prompt_tokens: 0,
        completion_tokens: 0,
    });
    Ok((content, usage, latency))
}

/// Local Ollama lanes use the native `/api/chat` route: the OpenAI-compat
/// `/v1/chat/completions` path on current Ollama returns empty content for
/// qwen-family reasoning models. Thinking is disabled so cleanup is
/// deterministic, and `num_predict` backstops runaway generation.
fn ollama_postproc(
    agent: &ureq::Agent,
    base: &str,
    lane: &PostprocLane,
    transcript: &str,
    instructions: &str,
) -> Result<(String, ChatUsage, u128)> {
    let url = format!("{}/api/chat", base.trim_end_matches('/'));
    let body = json!({
        "model": lane.model,
        "stream": false,
        "think": false,
        "messages": [
            {"role": "system", "content": instructions},
            {"role": "user", "content": transcript},
        ],
        "options": { "num_predict": 1024 },
    });
    let payload = serde_json::to_vec(&body).context("serializing ollama chat request")?;
    let start = Instant::now();

    #[derive(Deserialize)]
    struct OllamaChat {
        #[serde(default)]
        message: OllamaMessage,
        #[serde(default)]
        prompt_eval_count: u64,
        #[serde(default)]
        eval_count: u64,
    }
    #[derive(Default, Deserialize)]
    struct OllamaMessage {
        #[serde(default)]
        content: String,
    }

    let response = agent
        .post(&url)
        .set("Content-Type", "application/json")
        .send_bytes(&payload)
        .with_context(|| format!("POST {url} for lane '{}'", lane.id))?;
    let parsed: OllamaChat = response
        .into_string()
        .with_context(|| format!("reading response for lane '{}'", lane.id))
        .and_then(|raw| {
            serde_json::from_str(&raw).map_err(|e| anyhow!("bad ollama chat response: {e}"))
        })?;
    let latency = start.elapsed().as_millis();
    let content = clean_content(&parsed.message.content)
        .with_context(|| format!("postproc '{}' returned empty content", lane.id))?;
    let usage = ChatUsage {
        prompt_tokens: parsed.prompt_eval_count,
        completion_tokens: parsed.eval_count,
    };
    Ok((content, usage, latency))
}

/// Live OpenRouter pricing: model id -> (prompt $/token, completion $/token).
fn openrouter_pricing(agent: &ureq::Agent, marker: &str) -> Result<BTreeMap<String, (f64, f64)>> {
    let url = proxied("https://openrouter.ai/api/v1/models");
    let mut request = agent.get(&url);
    // Mint markers are only meaningful through the proxy.
    if proxy_prefix().is_some() {
        request = request.set("Authorization", &format!("Bearer {marker}"));
    }
    let response = request.call().with_context(|| format!("GET {url}"))?;
    let parsed: serde_json::Value = serde_json::from_str(
        &response
            .into_string()
            .context("reading OpenRouter model list")?,
    )
    .context("parsing OpenRouter model list")?;
    let mut out = BTreeMap::new();
    if let Some(data) = parsed.get("data").and_then(|d| d.as_array()) {
        for item in data {
            let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let prompt = item
                .get("pricing")
                .and_then(|p| p.get("prompt"))
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            let completion = item
                .get("pricing")
                .and_then(|p| p.get("completion"))
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            if !id.is_empty() {
                out.insert(id.to_string(), (prompt, completion));
            }
        }
    }
    Ok(out)
}

/// True when an anyhow error chain contains a transient `ureq` failure:
/// a network-class transport error (DNS, connect, I/O such as timeouts,
/// proxy connect) or an upstream 5xx status. Deterministic errors (bad URL,
/// bad header, too many redirects) and 4xx are not retried.
fn is_transient(error: &anyhow::Error) -> bool {
    for cause in error.chain() {
        if let Some(ureq_err) = cause.downcast_ref::<ureq::Error>() {
            return match ureq_err {
                ureq::Error::Status(code, _) => *code >= 500,
                ureq::Error::Transport(err) => matches!(
                    err.kind(),
                    ureq::ErrorKind::Dns
                        | ureq::ErrorKind::ConnectionFailed
                        | ureq::ErrorKind::Io
                        | ureq::ErrorKind::ProxyConnect
                ),
            };
        }
    }
    false
}

/// Retry a cloud call up to three times with backoff on transient failures
/// only. Application errors (bad payloads, empty transcripts) fail fast.
fn retry_cloud<T>(label: &str, f: impl Fn() -> Result<T>) -> Result<T> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        match f() {
            Ok(value) => return Ok(value),
            Err(error) if attempt < 3 && is_transient(&error) => {
                eprintln!("[eval] retry {label} attempt {attempt}: {error:#}");
                std::thread::sleep(Duration::from_millis(1500 * attempt as u64));
            }
            Err(error) => return Err(error),
        }
    }
}

// ---------------------------------------------------------------------------
// Cost calculators
// ---------------------------------------------------------------------------

fn stt_cost_usd(lane: &SttLane, audio_secs: f64, input_tokens: u64, output_tokens: u64) -> f64 {
    let Some(p) = &lane.pricing else {
        return 0.0;
    };
    match p.unit.as_str() {
        "per_min" => audio_secs / 60.0 * p.rate,
        "per_token" => (input_tokens as f64 * p.input + output_tokens as f64 * p.output) / 1e6,
        _ => 0.0,
    }
}

fn ppr_cost_usd(
    lane: &PostprocLane,
    usage: &ChatUsage,
    live_or: Option<&BTreeMap<String, (f64, f64)>>,
) -> f64 {
    let Some(p) = &lane.pricing else {
        return 0.0;
    };
    match p.unit.as_str() {
        "openrouter" => {
            let Some(map) = live_or else {
                return 0.0;
            };
            let (pr, co) = map.get(&lane.model).copied().unwrap_or((0.0, 0.0));
            usage.prompt_tokens as f64 * pr + usage.completion_tokens as f64 * co
        }
        "per_token" => {
            usage.prompt_tokens as f64 * p.input / 1e6
                + usage.completion_tokens as f64 * p.output / 1e6
        }
        _ => 0.0,
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn parse_flag(args: &[String], flag: &str) -> Option<Vec<String>> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag {
            return args
                .get(i + 1)
                .map(|v| v.split(',').map(str::to_owned).collect());
        }
        i += 1;
    }
    None
}

fn load_config(args: &[String]) -> Result<EvalConfig> {
    let path = parse_flag(args, "--config")
        .and_then(|v| v.first().cloned())
        .unwrap_or_else(|| "eval/config.json".to_string());
    let raw = fs::read_to_string(&path).with_context(|| format!("reading {path}"))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing {path}"))
}

fn list(args: &[String]) -> Result<()> {
    let config = load_config(args)?;
    match fs::read_to_string(&config.manifest) {
        Ok(raw) => {
            let manifest: Manifest = serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", config.manifest))?;
            println!(
                "clips: {}",
                manifest
                    .clips
                    .iter()
                    .map(|c| c.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        Err(_) => println!("no manifest at {}", config.manifest),
    }
    for lane in &config.stt {
        let ok = lane_available(lane);
        println!(
            "  stt  {:24} [{:14}] {}",
            lane.id,
            lane.kind,
            if ok { "ok" } else { "MISSING" }
        );
    }
    println!("postproc:");
    for lane in &config.postproc {
        println!("  ppr  {:24} {}", lane.id, lane.model);
    }
    Ok(())
}

fn lane_available(lane: &SttLane) -> bool {
    match lane.kind.as_str() {
        "transcribe_rs" => lane
            .dir
            .as_ref()
            .map(|d| Path::new(d).exists())
            .unwrap_or(false),
        "whisper_cpp" => {
            lane.bin
                .as_ref()
                .map(|b| Path::new(b).exists())
                .unwrap_or(false)
                && lane
                    .model_file
                    .as_ref()
                    .map(|m| Path::new(m).exists())
                    .unwrap_or(false)
        }
        _ => lane.endpoint.is_some() && lane.path.is_some() && lane.model.is_some(),
    }
}

/// Classify a postproc pass as degenerate when the output is far larger than
/// the input (models repeating/reasoning instead of cleaning).
fn is_degenerate(in_chars: usize, out_chars: usize) -> bool {
    out_chars > (in_chars.max(1) * 5).max(200)
}

/// Validate pricing units up front so cost math never silently misprices.
fn validate_config(config: &EvalConfig) -> Result<()> {
    for lane in &config.stt {
        let Some(p) = &lane.pricing else { continue };
        match p.unit.as_str() {
            "zero" => {}
            "per_min" => {
                anyhow::ensure!(p.rate > 0.0, "stt '{}': per_min rate must be > 0", lane.id)
            }
            "per_token" => anyhow::ensure!(
                p.input >= 0.0 && p.output >= 0.0,
                "stt '{}': per_token input/output must be >= 0",
                lane.id
            ),
            other => bail!("stt '{}': unknown pricing unit '{other}'", lane.id),
        }
    }
    for lane in &config.postproc {
        let Some(p) = &lane.pricing else { continue };
        match p.unit.as_str() {
            "zero" | "openrouter" => {}
            "per_token" => anyhow::ensure!(
                p.input >= 0.0 && p.output >= 0.0,
                "postproc '{}': per_token input/output must be >= 0",
                lane.id
            ),
            other => bail!("postproc '{}': unknown pricing unit '{other}'", lane.id),
        }
    }
    Ok(())
}

/// Resolve the results directory: the `--out` flag overrides the config path,
/// so partial/experimental runs never clobber the canonical results.
fn resolve_out_dir(config: &EvalConfig, args: &[String]) -> PathBuf {
    parse_flag(args, "--out")
        .and_then(|values| values.first().cloned())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&config.out_dir))
}

fn run(args: &[String]) -> Result<()> {
    let config = load_config(args)?;
    validate_config(&config)?;
    let stt_filter: Vec<String> = parse_flag(args, "--stt").unwrap_or_default();
    let ppr_filter: Vec<String> = parse_flag(args, "--postproc").unwrap_or_default();
    let clip_filter: Vec<String> = parse_flag(args, "--clips").unwrap_or_default();

    let manifest: Manifest = serde_json::from_str(
        &fs::read_to_string(&config.manifest)
            .with_context(|| format!("reading manifest {}", config.manifest))?,
    )
    .with_context(|| format!("parsing {}", config.manifest))?;
    let clips: Vec<&Clip> = manifest
        .clips
        .iter()
        .filter(|c| clip_filter.is_empty() || clip_filter.contains(&c.id.to_string()))
        .collect();
    if clips.is_empty() {
        bail!("no clips selected");
    }

    // Validate + measure every clip once (also enforces 16k/16-bit/mono).
    let mut clip_data: Vec<(String, String, Vec<f32>, f64)> = Vec::new();
    for clip in &clips {
        let samples = transcribe_rs::audio::read_wav_samples(Path::new(&clip.file))
            .with_context(|| format!("reading clip {}", clip.id))?;
        let secs = samples.len() as f64 / 16_000.0;
        clip_data.push((clip.id.clone(), clip.file.clone(), samples, secs));
        eprintln!("[eval] clip {} audio_secs={:.2}", clip.id, secs);
    }

    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(300))
        .build();
    let out_dir = resolve_out_dir(&config, args);
    fs::create_dir_all(&out_dir).with_context(|| format!("creating {}", out_dir.display()))?;

    // ---------------- Transcription ----------------
    let mut stt_results: Vec<SttResult> = Vec::new();
    for lane in &config.stt {
        if !stt_filter.is_empty() && !stt_filter.contains(&lane.id.to_string()) {
            continue;
        }
        if !lane_available(lane) {
            eprintln!("[eval] skip stt lane '{}' (assets missing)", lane.id);
            continue;
        }
        eprintln!("[eval] stt lane '{}' start", lane.id);
        let mut model: Option<LocalModel> = None;
        let mut load_ms: Option<u128> = None;
        if lane.kind == "transcribe_rs" {
            let t0 = Instant::now();
            model = Some(LocalModel::load(lane)?);
            load_ms = Some(t0.elapsed().as_millis());
        }
        for (index, (clip_id, wav_path, samples, secs)) in clip_data.iter().enumerate() {
            let cold = index == 0;
            let t0 = Instant::now();
            let result: Result<(String, u64, u64)> = match lane.kind.as_str() {
                "transcribe_rs" => model
                    .as_mut()
                    .ok_or_else(|| anyhow!("model not loaded for lane '{}'", lane.id))?
                    .transcribe(samples)
                    .map(|t| (t, 0, 0)),
                "whisper_cpp" => whisper_cpp(lane, Path::new(wav_path)).map(|t| (t, 0, 0)),
                "openai" => retry_cloud(&format!("stt {}", lane.id), || {
                    openai_transcribe(&agent, lane, Path::new(wav_path))
                }),
                "deepgram" => retry_cloud(&format!("stt {}", lane.id), || {
                    deepgram_transcribe(&agent, lane, Path::new(wav_path)).map(|t| (t, 0, 0))
                }),
                "elevenlabs" => retry_cloud(&format!("stt {}", lane.id), || {
                    elevenlabs_transcribe(&agent, lane, Path::new(wav_path)).map(|t| (t, 0, 0))
                }),
                other => bail!("unknown stt kind {other:?}"),
            };
            let (text, input_tokens, output_tokens) =
                result.with_context(|| format!("lane '{}' clip '{}'", lane.id, clip_id))?;
            let latency = t0.elapsed().as_millis();
            if lane.pricing.as_ref().map(|p| p.unit.as_str()) == Some("per_token")
                && input_tokens == 0
                && output_tokens == 0
            {
                eprintln!(
                    "[eval] warn: token-priced lane '{}' recorded no usage; cost may underreport",
                    lane.id
                );
            }
            let cost = stt_cost_usd(lane, *secs, input_tokens, output_tokens);
            eprintln!(
                "[eval] stt {} clip {} chars={} ms={} cost_usd={:.6} cold={}",
                lane.id,
                clip_id,
                text.chars().count(),
                latency,
                cost,
                cold
            );
            stt_results.push(SttResult {
                lane: lane.id.clone(),
                clip: clip_id.clone(),
                audio_secs: *secs,
                load_ms,
                latency_ms: latency,
                cost_usd: cost,
                cold,
                text,
            });
        }
        // Unload the model for this lane before the next.
        drop(model);
    }
    let transcripts_path = out_dir.join("transcripts.json");
    fs::write(&transcripts_path, serde_json::to_vec_pretty(&stt_results)?)
        .with_context(|| format!("writing {}", transcripts_path.display()))?;

    postprocess_and_report(
        &config,
        &manifest,
        &agent,
        &stt_results,
        &ppr_filter,
        &out_dir,
    )
}

/// Run the transcription phase only, then delegate to
/// [`postprocess_and_report`] with the cached transcripts.
fn run_ppr_only(args: &[String]) -> Result<()> {
    let config = load_config(args)?;
    validate_config(&config)?;
    let ppr_filter: Vec<String> = parse_flag(args, "--postproc").unwrap_or_default();
    let out_dir = resolve_out_dir(&config, args);
    let transcripts_path = out_dir.join("transcripts.json");
    let raw = fs::read_to_string(&transcripts_path)
        .with_context(|| format!("reading {}", transcripts_path.display()))?;
    let stt_results: Vec<SttResult> = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", transcripts_path.display()))?;
    if stt_results.is_empty() {
        bail!("no transcripts found in {}", transcripts_path.display());
    }
    let manifest: Manifest = serde_json::from_str(
        &fs::read_to_string(&config.manifest)
            .with_context(|| format!("reading manifest {}", config.manifest))?,
    )
    .with_context(|| format!("parsing {}", config.manifest))?;
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(300))
        .build();
    postprocess_and_report(
        &config,
        &manifest,
        &agent,
        &stt_results,
        &ppr_filter,
        &out_dir,
    )
}

/// Post-process every cached transcript through every selected postproc
/// lane, then render and save the scoreboards.
fn postprocess_and_report(
    config: &EvalConfig,
    manifest: &Manifest,
    agent: &ureq::Agent,
    stt_results: &[SttResult],
    ppr_filter: &[String],
    out_dir: &Path,
) -> Result<()> {
    // Fetch live OpenRouter pricing only when a selected lane needs it, and
    // derive the marker from that same (first matching) lane's config.
    let or_lane = config.postproc.iter().find(|lane| {
        (ppr_filter.is_empty() || ppr_filter.contains(&lane.id))
            && lane.pricing.as_ref().map(|p| p.unit.as_str()) == Some("openrouter")
    });
    let or_pricing: BTreeMap<String, (f64, f64)> = match or_lane {
        Some(lane) => {
            let marker = lane
                .marker
                .as_deref()
                .unwrap_or("__mint.openrouter.default__");
            retry_cloud("openrouter pricing", || openrouter_pricing(agent, marker))?
        }
        None => BTreeMap::new(),
    };
    let mut ppr_results: Vec<PprResult> = Vec::new();
    for lane in &config.postproc {
        if !ppr_filter.is_empty() && !ppr_filter.contains(&lane.id.to_string()) {
            continue;
        }
        eprintln!("[eval] postproc lane '{}' start", lane.id);
        if lane.pricing.as_ref().map(|p| p.unit.as_str()) == Some("openrouter")
            && !or_pricing.contains_key(&lane.model)
        {
            eprintln!(
                "[eval] warn: openrouter model '{}' missing from /models pricing; cost 0",
                lane.model
            );
        }
        for stt in stt_results {
            let call = retry_cloud(&format!("ppr {}", lane.id), || {
                postproc_call(agent, lane, &stt.text, &config.instructions)
            })
            .with_context(|| {
                format!(
                    "postproc '{}' on stt '{}' clip '{}'",
                    lane.id, stt.lane, stt.clip
                )
            });
            // A single failed pass must not sink the whole 315-call matrix:
            // record it as a degenerate pass and keep going.
            let (content, usage, latency) = match call {
                Ok(value) => value,
                Err(error) => {
                    eprintln!(
                        "[eval] ppr FAIL {} stt={} clip={}: {error:#}",
                        lane.id, stt.lane, stt.clip
                    );
                    ppr_results.push(PprResult {
                        lane: lane.id.clone(),
                        stt_lane: stt.lane.clone(),
                        clip: stt.clip.clone(),
                        latency_ms: 0,
                        cost_usd: 0.0,
                        input_tokens: 0,
                        output_tokens: 0,
                        degenerate: true,
                        raw_text: stt.text.clone(),
                        text: String::new(),
                    });
                    continue;
                }
            };
            let cost = ppr_cost_usd(lane, &usage, Some(&or_pricing));
            let out_chars = content.chars().count();
            let in_chars = stt.text.chars().count();
            let degenerate = is_degenerate(in_chars, out_chars);
            eprintln!(
                "[eval] ppr {} stt={} clip={} chars_in={} chars_out={} ms={} cost_usd={:.6} tokens={}/{}/{} degen={}",
                lane.id,
                stt.lane,
                stt.clip,
                in_chars,
                out_chars,
                latency,
                cost,
                usage.prompt_tokens,
                usage.completion_tokens,
                usage.prompt_tokens + usage.completion_tokens,
                degenerate
            );
            ppr_results.push(PprResult {
                lane: lane.id.clone(),
                stt_lane: stt.lane.clone(),
                clip: stt.clip.clone(),
                latency_ms: latency,
                cost_usd: cost,
                input_tokens: usage.prompt_tokens,
                output_tokens: usage.completion_tokens,
                degenerate,
                raw_text: stt.text.clone(),
                text: content,
            });
        }
        // Evict the model so the next local lane fits in VRAM.
        if let Some(base) = &lane.ollama_base {
            let unload_url = format!("{}/api/generate", base.trim_end_matches('/'));
            let payload = serde_json::to_vec(&json!({ "model": lane.model, "keep_alive": 0 }));
            if let Ok(payload) = payload {
                let _ = agent
                    .post(&unload_url)
                    .set("Content-Type", "application/json")
                    .send_bytes(&payload)
                    .map_err(|e| eprintln!("[eval] unload warn {}: {e}", lane.model));
            }
        }
    }
    let postproc_path = out_dir.join("postproc.json");
    fs::write(&postproc_path, serde_json::to_vec_pretty(&ppr_results)?)
        .with_context(|| format!("writing {}", postproc_path.display()))?;

    // ---------------- Scoreboards ----------------
    let board = build_boards(manifest, stt_results, &ppr_results);
    let board_path = out_dir.join("boards.md");
    fs::write(&board_path, &board).with_context(|| format!("writing {}", board_path.display()))?;
    println!("{board}");
    eprintln!(
        "[eval] done: {} stt calls, {} ppr calls; results in {}",
        stt_results.len(),
        ppr_results.len(),
        out_dir.display()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Scoreboards
// ---------------------------------------------------------------------------

fn mean(vals: &[f64]) -> f64 {
    if vals.is_empty() {
        0.0
    } else {
        vals.iter().sum::<f64>() / vals.len() as f64
    }
}

fn build_boards(
    manifest: &Manifest,
    stt_results: &[SttResult],
    ppr_results: &[PprResult],
) -> String {
    let refs: BTreeMap<&str, &str> = manifest
        .clips
        .iter()
        .map(|c| (c.id.as_str(), c.reference.as_str()))
        .collect();
    let mut out = String::new();
    let _ = writeln!(&mut out, "# Gauntlet scoreboard");

    // Per-clip WER/CER for each STT lane.
    let mut stt_rows: Vec<SttRow> = Vec::new();
    for lane in unique_lanes(stt_results) {
        let lane_results: Vec<&SttResult> = stt_results.iter().filter(|r| r.lane == lane).collect();
        let mut wers = Vec::new();
        let mut cers = Vec::new();
        let mut lats = Vec::new();
        let mut rtf = Vec::new();
        let mut costs = 0.0;
        let mut cold_ms = 0.0;
        for r in &lane_results {
            let reference = refs.get(r.clip.as_str()).copied().unwrap_or("");
            let w = wer::wer(reference, &r.text);
            wers.push(w);
            cers.push(wer::cer(reference, &r.text));
            if !r.cold {
                lats.push(r.latency_ms as f64);
                rtf.push(r.latency_ms as f64 / 1000.0 / r.audio_secs.max(0.001));
            }
            if r.cold {
                cold_ms = r.latency_ms as f64 + r.load_ms.unwrap_or(0) as f64;
            }
            costs += r.cost_usd;
        }
        stt_rows.push((
            lane,
            mean(&wers),
            mean(&cers),
            mean(&lats),
            mean(&rtf),
            cold_ms.round() as u128,
            costs,
            lane_results.len() as f64,
        ));
    }
    stt_rows.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    let _ = writeln!(&mut out, "\n## Transcription (word error rate)");
    let _ = writeln!(
        &mut out,
        "| model | mean WER | mean CER | warm ms | RTF | cold ms | cost USD | calls |"
    );
    let _ = writeln!(&mut out, "|---|---|---|---|---|---|---|---|");
    for (lane, w, c, lat, rtf, cold, cost, calls) in &stt_rows {
        let _ = writeln!(
            &mut out,
            "| {lane} | {:.4} | {:.4} | {:.0} | {:.3} | {} | {:.6} | {:.0} |",
            w, c, lat, rtf, cold, cost, calls
        );
    }

    // Per-clip detail for the STT lanes (most informative for accuracy analysis).
    let _ = writeln!(&mut out, "\n## Transcription detail (WER per clip)");
    let names: Vec<&str> = stt_rows.iter().map(|r| r.0.as_str()).collect();
    let _ = writeln!(
        &mut out,
        "| clip | {} |",
        names
            .iter()
            .map(|n| format!("`{n}`"))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    let _ = writeln!(
        &mut out,
        "|{}|",
        std::iter::repeat_n("---", names.len() + 1)
            .collect::<Vec<_>>()
            .join("|")
    );
    for clip in &manifest.clips {
        let mut cells = Vec::new();
        for lane in &names {
            let value = stt_results
                .iter()
                .find(|r| r.lane == *lane && r.clip == clip.id)
                .map(|r| {
                    let reference = refs.get(clip.id.as_str()).copied().unwrap_or("");
                    format!("{:.3}", wer::wer(reference, &r.text))
                })
                .unwrap_or_else(|| "—".to_string());
            cells.push(value);
        }
        let _ = writeln!(&mut out, "| {} | {} |", clip.id, cells.join(" | "));
    }

    // Postproc board.
    let mut ppr_rows: Vec<PprRow> = Vec::new();
    for lane in unique_lanes_ppr(ppr_results) {
        let lane_results: Vec<&PprResult> = ppr_results.iter().filter(|r| r.lane == lane).collect();
        let mut before = Vec::new();
        let mut after = Vec::new();
        let mut lats = Vec::new();
        let mut costs = 0.0;
        let mut degen = 0usize;
        for r in &lane_results {
            let reference = refs.get(r.clip.as_str()).copied().unwrap_or("");
            before.push(wer::wer(reference, &r.raw_text));
            if r.degenerate {
                degen += 1;
            } else {
                after.push(wer::wer(reference, &r.text));
            }
            lats.push(r.latency_ms as f64);
            costs += r.cost_usd;
        }
        ppr_rows.push((
            lane,
            mean(&before),
            mean(&after),
            mean(&after) - mean(&before),
            mean(&lats),
            costs,
            degen,
            lane_results.len(),
        ));
    }
    ppr_rows.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    let _ = writeln!(
        &mut out,
        "\n## Post-processing (final WER across all STT transcripts)"
    );
    let _ = writeln!(
        &mut out,
        "| postproc model | input WER | final WER | delta | warm ms | cost USD | degenerate |"
    );
    let _ = writeln!(&mut out, "|---|---|---|---|---|---|---|");
    for (lane, before, after, delta, lat, cost, degen, n) in &ppr_rows {
        let _ = writeln!(
            &mut out,
            "| {lane} | {before:.4} | {after:.4} | {delta:+.4} | {lat:.0} | {cost:.6} | {degen}/{n} |"
        );
    }

    // Arrangements: best STT x postproc combinations by final WER.
    let _ = writeln!(
        &mut out,
        "\n## Arrangements (STT x postproc, ranked by final WER)"
    );
    let mut arr: Vec<(String, String, f64, f64, f64, f64)> = Vec::new();
    for lane in unique_lanes(stt_results) {
        for ppr in unique_lanes_ppr(ppr_results) {
            let mut wers = Vec::new();
            let mut lats = Vec::new();
            let mut costs = 0.0;
            for r in ppr_results
                .iter()
                .filter(|r| r.lane == ppr && r.stt_lane == lane)
            {
                let reference = refs.get(r.clip.as_str()).copied().unwrap_or("");
                wers.push(wer::wer(reference, &r.text));
                lats.push(r.latency_ms as f64);
                costs += r.cost_usd;
            }
            if wers.is_empty() {
                continue;
            }
            arr.push((
                lane.clone(),
                ppr.clone(),
                mean(&wers),
                mean(&lats),
                costs,
                wers.len() as f64,
            ));
        }
    }
    arr.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
    let _ = writeln!(
        &mut out,
        "| stt | postproc | final WER | ppr ms | ppr cost | calls |"
    );
    let _ = writeln!(&mut out, "|---|---|---|---|---|---|");
    for (s, p, w, lat, cost, n) in arr.into_iter().take(25) {
        let _ = writeln!(
            &mut out,
            "| {s} | {p} | {:.4} | {:.0} | {:.6} | {:.0} |",
            w, lat, cost, n
        );
    }

    out
}

fn unique_lanes(results: &[SttResult]) -> Vec<String> {
    let mut seen = Vec::new();
    for r in results {
        if !seen.contains(&r.lane) {
            seen.push(r.lane.clone());
        }
    }
    seen
}

fn unique_lanes_ppr(results: &[PprResult]) -> Vec<String> {
    let mut seen = Vec::new();
    for r in results {
        if !seen.contains(&r.lane) {
            seen.push(r.lane.clone());
        }
    }
    seen
}

/// STT scoreboard row: (lane, mean WER, mean CER, warm ms, RTF, cold ms, cost, calls).
type SttRow = (String, f64, f64, f64, f64, u128, f64, f64);
/// Postproc scoreboard row:
/// (lane, input WER, final WER, delta, warm ms, cost, degenerate, calls).
type PprRow = (String, f64, f64, f64, f64, f64, usize, usize);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn think_blocks_are_stripped() {
        assert_eq!(
            strip_think_blocks("before thinkingreason responseafter"),
            "beforeafter"
        );
        assert_eq!(
            strip_think_blocks("  thinkingreason response\nCorrected"),
            "Corrected"
        );
        assert_eq!(strip_think_blocks("Corrected thinkingreason"), "Corrected");
        assert_eq!(strip_think_blocks("no tags here"), "no tags here");
        assert!(clean_content("  thinkingonly \n response").is_err());
    }

    #[test]
    fn degenerate_classifies_growth() {
        assert!(!is_degenerate(100, 100));
        assert!(!is_degenerate(100, 500)); // exactly 5x is not degenerate
        assert!(is_degenerate(100, 501));
        assert!(!is_degenerate(10, 200)); // floor of 200 dominates small inputs
        assert!(is_degenerate(10, 201));
        assert!(!is_degenerate(0, 0));
        assert!(is_degenerate(0, 250));
    }

    fn stt_lane(price: PriceSpec) -> SttLane {
        SttLane {
            id: "t".into(),
            kind: "x".into(),
            family: None,
            dir: None,
            variant: None,
            quant: None,
            bin: None,
            model_file: None,
            endpoint: None,
            path: None,
            model: None,
            marker: None,
            scheme: None,
            extra: vec![],
            pricing: Some(price),
        }
    }

    #[test]
    fn cost_per_minute_scales_with_audio_seconds() {
        let lane = stt_lane(PriceSpec {
            unit: "per_min".into(),
            rate: 0.006,
            input: 0.0,
            output: 0.0,
        });
        assert!((stt_cost_usd(&lane, 60.0, 0, 0) - 0.006).abs() < 1e-12);
        assert!((stt_cost_usd(&lane, 11.0, 0, 0) - 11.0 / 60.0 * 0.006).abs() < 1e-12);
        assert_eq!(
            stt_cost_usd(
                &stt_lane(PriceSpec {
                    unit: "zero".into(),
                    rate: 0.0,
                    input: 0.0,
                    output: 0.0,
                }),
                999.0,
                0,
                0
            ),
            0.0
        );
    }

    #[test]
    fn cost_per_token_uses_million_token_rates() {
        let lane = stt_lane(PriceSpec {
            unit: "per_token".into(),
            rate: 0.0,
            input: 1.25,
            output: 5.0,
        });
        // 110 in + 28 out tokens at $1.25M/$5M
        assert!(
            (stt_cost_usd(&lane, 0.0, 110, 28) - (110.0 * 1.25 + 28.0 * 5.0) / 1e6).abs() < 1e-12
        );
    }

    #[test]
    fn proxied_direct_without_prefix_is_unchanged() {
        assert_eq!(
            proxied_with("", "https://api.openai.com/v1"),
            "https://api.openai.com/v1"
        );
        assert_eq!(
            proxied_with("  ", "https://api.openai.com/v1"),
            "https://api.openai.com/v1"
        );
    }

    #[test]
    fn proxied_prefix_routes_https_uri_host_and_path() {
        assert_eq!(
            proxied_with(
                "http://broker.example:4949/proxy/https/",
                "https://api.openai.com/v1/audio/transcriptions"
            ),
            "http://broker.example:4949/proxy/https/api.openai.com/v1/audio/transcriptions"
        );
        assert_eq!(
            proxied_with(
                "http://broker.example:4949/proxy/https",
                "https://openrouter.ai/api/v1/models"
            ),
            "http://broker.example:4949/proxy/https/openrouter.ai/api/v1/models"
        );
    }

    #[test]
    fn proxied_never_touches_non_https_uris() {
        // Local lanes (Ollama) must never be routed through a proxy.
        assert_eq!(
            proxied_with("http://broker.example:4949/proxy/https", "http://127.0.0.1:11434/v1"),
            "http://127.0.0.1:11434/v1"
        );
    }

    #[test]
    fn ppr_per_token_cost() {
        let lane = PostprocLane {
            id: "t".into(),
            endpoint: "http://x".into(),
            model: "m".into(),
            marker: None,
            scheme: None,
            pricing: Some(PriceSpec {
                unit: "per_token".into(),
                rate: 0.0,
                input: 2.5,
                output: 10.0,
            }),
            ollama_base: None,
        };
        let usage = ChatUsage {
            prompt_tokens: 100,
            completion_tokens: 40,
        };
        assert!(
            (ppr_cost_usd(&lane, &usage, None) - (100.0 * 2.5 + 40.0 * 10.0) / 1e6).abs() < 1e-12
        );
    }
}
