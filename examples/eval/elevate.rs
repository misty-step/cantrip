//! Elevate's semantic gate, not Clean's accepted-string oracle.
//!
//! Build on a fresh Boat worker with --no-env. Keep the credential broker native:
//!   pass-env run -e OPENROUTER_API_KEY=workstation/OPENROUTER_MISTY_STEP_CANTRIP_WORKSTATION_SPEECH_EVAL_API_KEY -- python3 examples/eval/elevate_broker.py --out PRIVATE_DIR --port PORT
//!   boat forward ID --reverse --local PORT --remote PORT --bind 127.0.0.1
//!   eval behavior --mode elevate --out PRIVATE_DIR --source-revision SHA --endpoint http://127.0.0.1:PORT/api/v1 --allow-paid
//! Inspect private.json off-log, then provide one hash-bound judgment per output:
//!   eval behavior --mode elevate --out PRIVATE_DIR --review PRIVATE_REVIEW.json
//! Only receipt.json is publishable. Never publish private.json or broker raw files.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{anyhow, ensure, Context, Result};
use cantrip_engine::config::{Mode, PostprocConfig, VocabularyEntry};
use cantrip_engine::{postproc, vocabulary};
use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const MODEL: &str = "openai/gpt-6.1-sol";
const KEY_CEILING_USD: f64 = 4.50;

#[derive(Parser)]
#[command(name = "eval behavior --mode elevate")]
struct Args {
    #[arg(long, value_parser = ["elevate"])]
    mode: String,
    #[arg(long, default_value = "samples/eval/elevate-behavior.json")]
    postproc_manifest: PathBuf,
    /// Absolute, owner-only scratch directory outside the repository; fresh for a run.
    #[arg(long)]
    out: PathBuf,
    #[arg(long)]
    source_revision: Option<String>,
    /// Native allowlisted broker, reverse-forwarded into the worker. No credentials here.
    #[arg(long)]
    endpoint: Option<String>,
    #[arg(long, conflicts_with_all = ["allow_paid", "review"])]
    dry_run: bool,
    #[arg(long, conflicts_with = "review")]
    allow_paid: bool,
    /// Explicit, hash-bound semantic judgments; performs no inference requests.
    #[arg(long)]
    review: Option<PathBuf>,
    /// Opaque paired-comparison schedule. Arm identities remain in the native broker.
    #[arg(long, conflicts_with = "review")]
    schedule: Option<PathBuf>,
}

#[derive(Deserialize)]
struct Manifest {
    schema: String,
    corpus_version: String,
    include: PathBuf,
    vocabulary: Vec<VocabularyEntry>,
    inherited_checks: BTreeMap<String, Checks>,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Included {
    cases: Vec<Case>,
}

// Existing Clean `accepted` strings are deliberately not deserialized or scored.
#[derive(Deserialize)]
struct Case {
    id: String,
    category: String,
    input: String,
    #[serde(default)]
    checks: Checks,
}

#[derive(Default, Deserialize)]
struct Checks {
    #[serde(default)]
    exact: Vec<String>,
    #[serde(default)]
    lexical_any: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
enum Failure {
    Request,
    SpeechAct,
    FactAdded,
    FactOmitted,
    Number,
    Negation,
    Modality,
    Literal,
    LexicalUpgrade,
    Formatting,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Schedule {
    schema: String,
    calls: Vec<ScheduledCall>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScheduledCall {
    id: Option<String>,
    case: String,
    iteration: u8,
}

#[derive(Deserialize, Serialize)]
struct Call {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    schedule_id: Option<String>,
    case: String,
    category: String,
    iteration: u8,
    latency_ms: u128,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
    cost_usd: Option<f64>,
    alias_substitutions: usize,
    source_sha256: String,
    output_sha256: Option<String>,
    failures: BTreeSet<Failure>,
    reviewed: bool,
}

#[derive(Deserialize, Serialize)]
struct Receipt {
    schema: String,
    claim_type: String,
    source_revision: String,
    corpus_version: String,
    corpus_sha256: String,
    inherited_corpus_sha256: String,
    inherited_cases: usize,
    added_cases: usize,
    #[serde(default)]
    planned_calls: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    comparison_schedule_sha256: Option<String>,
    model: String,
    reasoning_effort: String,
    production_path: String,
    system_prompt_sha256: String,
    operator_prompt_sha256: String,
    timeout_ms: u64,
    calls: Vec<Call>,
    accounting: Value,
    summary: Value,
    semantic_reviewer: Option<String>,
    limitations: Vec<String>,
}

#[derive(Deserialize, Serialize)]
struct PrivateOutput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    schedule_id: Option<String>,
    case: String,
    iteration: u8,
    source: String,
    alias_corrected_source: String,
    text: String,
    output_sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Review {
    reviewer: String,
    calls: Vec<Judgment>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Judgment {
    case: String,
    iteration: u8,
    output_sha256: String,
    /// Empty only after reviewing every semantic dimension in the manifest.
    failures: BTreeSet<Failure>,
}

fn hash(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes.as_ref()))
}

fn write_private(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .context("opening private evaluation artifact")?;
    file.write_all(&bytes)?;
    file.sync_all().context("syncing evaluation artifact")
}

fn private_out(path: &Path, fresh: bool) -> Result<()> {
    ensure!(path.is_absolute(), "--out must be absolute private scratch");
    if fresh {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .context("--out must be a fresh private directory")?;
    }
    let out = fs::canonicalize(path)?;
    let repo = fs::canonicalize(std::env::current_dir()?)?;
    ensure!(
        !out.starts_with(repo),
        "private outputs must be outside the repository"
    );
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "--out must be an owner-only directory"
    );
    Ok(())
}

fn broker_accounting(endpoint: &str) -> Result<Value> {
    let port = endpoint
        .strip_prefix("http://127.0.0.1:")
        .and_then(|suffix| suffix.strip_suffix("/api/v1"))
        .and_then(|port| port.parse::<u16>().ok())
        .filter(|port| *port > 0);
    ensure!(
        port.is_some(),
        "--endpoint must be a loopback credential broker"
    );
    let response = ureq::get(&format!("{endpoint}/accounting"))
        .timeout(std::time::Duration::from_secs(30))
        .call()
        .map_err(|_| anyhow!("credential_broker_accounting_unavailable"))?;
    let accounting: Value = serde_json::from_reader(response.into_reader())
        .map_err(|_| anyhow!("credential_broker_accounting_shape"))?;
    ensure!(
        accounting["model"] == MODEL
            && accounting["reasoning_effort"] == "low"
            && accounting["absolute_key_ceiling_usd"].as_f64() == Some(KEY_CEILING_USD),
        "credential broker does not implement the authorized evaluation bounds"
    );
    Ok(accounting)
}

fn literal_present(text: &str, literal: &str) -> bool {
    text.match_indices(literal).any(|(start, matched)| {
        let word = |c: char| c.is_alphanumeric() || matches!(c, '_' | '/' | '-');
        let before = text[..start].chars().next_back();
        let mut after = text[start + matched.len()..].chars();
        let next = after.next();
        !before.is_some_and(|c| word(c) || c == '.')
            && !next.is_some_and(word)
            && !(next == Some('.') && after.next().is_some_and(word))
    })
}

fn deterministic(checks: &Checks, text: &str) -> BTreeSet<Failure> {
    let mut failures = BTreeSet::new();
    if checks
        .exact
        .iter()
        .any(|literal| !literal_present(text, literal))
    {
        failures.insert(Failure::Literal);
    }
    if !checks.lexical_any.is_empty() {
        let lower = text.to_lowercase();
        if !checks
            .lexical_any
            .iter()
            .any(|term| literal_present(&lower, &term.to_lowercase()))
        {
            failures.insert(Failure::LexicalUpgrade);
        }
    }
    failures
}

fn summarize(receipt: &mut Receipt) {
    let count = receipt.calls.len();
    let planned = if receipt.planned_calls == 0 {
        receipt.inherited_cases + receipt.added_cases
    } else {
        receipt.planned_calls
    };
    let failures: Vec<_> = receipt.calls.iter().filter(|call| !call.failures.is_empty())
        .map(|call| json!({"case": call.case, "iteration": call.iteration, "categories": call.failures}))
        .collect();
    let reviewed = receipt.calls.iter().filter(|call| call.reviewed).count();
    let mut latencies: Vec<_> = receipt.calls.iter().map(|call| call.latency_ms).collect();
    latencies.sort_unstable();
    let percentile = |percent: usize| {
        latencies
            .get((count * percent).div_ceil(100).saturating_sub(1))
            .copied()
    };
    let known_cost: f64 = receipt.calls.iter().filter_map(|call| call.cost_usd).sum();
    receipt.summary = json!({
        "scheduled": planned,
        "submitted": count,
        "reviewed": reviewed,
        "passed": receipt.calls.iter().filter(|call| call.reviewed && call.failures.is_empty()).count(),
        "failed": failures.len(),
        "failures": failures,
        "input_tokens": receipt.calls.iter().filter_map(|call| call.input_tokens).sum::<u64>(),
        "output_tokens": receipt.calls.iter().filter_map(|call| call.output_tokens).sum::<u64>(),
        "reported_cost_usd": known_cost,
        "unknown_cost_calls": receipt.calls.iter().filter(|call| call.cost_usd.is_none()).count(),
        "latency_ms": {"p50": percentile(50), "p95": percentile(95), "max": latencies.last()},
        "gate": if count != planned || reviewed != count { "unverified" }
            else if failures.is_empty() { "pass" } else { "fail" }
    });
}

fn apply_review(out: &Path, path: &Path) -> Result<()> {
    let mut receipt: Receipt = serde_json::from_slice(&fs::read(out.join("receipt.json"))?)?;
    ensure!(
        receipt.semantic_reviewer.is_none(),
        "receipt already reviewed"
    );
    let private: Vec<PrivateOutput> = serde_json::from_slice(&fs::read(out.join("private.json"))?)?;
    let review: Review = serde_json::from_slice(&fs::read(path)?)?;
    ensure!(
        !review.reviewer.trim().is_empty(),
        "semantic reviewer must be identified"
    );
    ensure!(
        review.calls.len() == private.len(),
        "every completed output requires semantic review"
    );
    let mut seen = BTreeSet::new();
    for judgment in review.calls {
        ensure!(
            seen.insert((judgment.case.clone(), judgment.iteration)),
            "duplicate semantic judgment"
        );
        let raw = private
            .iter()
            .find(|raw| raw.case == judgment.case && raw.iteration == judgment.iteration)
            .ok_or_else(|| anyhow!("semantic judgment names an unknown output"))?;
        ensure!(
            judgment.output_sha256 == raw.output_sha256 && raw.output_sha256 == hash(&raw.text),
            "semantic judgment does not match retained output bytes"
        );
        let call = receipt
            .calls
            .iter_mut()
            .find(|call| call.case == judgment.case && call.iteration == judgment.iteration)
            .ok_or_else(|| anyhow!("semantic judgment has no call receipt"))?;
        ensure!(
            call.output_sha256.as_ref() == Some(&raw.output_sha256),
            "call/output hash mismatch"
        );
        call.failures.extend(judgment.failures);
        call.reviewed = true;
    }
    receipt.semantic_reviewer = Some(review.reviewer);
    summarize(&mut receipt);
    write_private(&out.join("receipt.json"), &receipt)?;
    eprintln!(
        "[eval] Elevate reviewed={} failed={} gate={}",
        receipt.summary["reviewed"], receipt.summary["failed"], receipt.summary["gate"]
    );
    Ok(())
}

pub(super) fn run(args: &[String]) -> Result<()> {
    let args =
        Args::try_parse_from(std::iter::once("behavior".to_owned()).chain(args.iter().cloned()))?;
    ensure!(args.mode == "elevate", "this gate only evaluates Elevate");
    private_out(&args.out, args.review.is_none())?;
    if let Some(review) = &args.review {
        return apply_review(&args.out, review);
    }
    ensure!(
        args.dry_run || args.allow_paid,
        "choose --dry-run or --allow-paid"
    );
    let source_revision = args.source_revision.as_deref().unwrap_or_default();
    ensure!(
        source_revision.len() == 40 && source_revision.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "--source-revision must identify the built production source"
    );
    let manifest_bytes = fs::read(&args.postproc_manifest)?;
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes)?;
    ensure!(
        manifest.schema == "cantrip.eval.elevate.v1",
        "unsupported Elevate corpus schema"
    );
    let included_path = args
        .postproc_manifest
        .parent()
        .unwrap_or(Path::new("."))
        .join(&manifest.include);
    let included_bytes = fs::read(included_path)?;
    let mut included: Included = serde_json::from_slice(&included_bytes)?;
    let inherited_cases = included.cases.len();
    for (id, checks) in manifest.inherited_checks {
        included
            .cases
            .iter_mut()
            .find(|case| case.id == id)
            .ok_or_else(|| anyhow!("checks name an unknown inherited case"))?
            .checks = checks;
    }
    let added_cases = manifest.cases.len();
    ensure!(
        added_cases == 8 && inherited_cases > 0,
        "run all inherited cases plus eight Elevate cases"
    );
    included.cases.extend(manifest.cases);
    let mut ids = BTreeSet::new();
    for case in &included.cases {
        ensure!(
            !case.input.trim().is_empty() && ids.insert(&case.id),
            "empty or duplicate case"
        );
        ensure!(
            case.id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
            "invalid public case ID"
        );
        ensure!(
            case.checks
                .exact
                .iter()
                .all(|literal| !literal.is_empty() && literal_present(&case.input, literal)),
            "protected literal must occur in its synthetic source"
        );
    }
    let case_by_id: BTreeMap<&str, &Case> = included
        .cases
        .iter()
        .map(|case| (case.id.as_str(), case))
        .collect();
    let schedule_bytes = args.schedule.as_ref().map(fs::read).transpose()?;
    let planned = if let Some(bytes) = &schedule_bytes {
        let schedule: Schedule = serde_json::from_slice(bytes)?;
        ensure!(
            schedule.schema == "cantrip.eval.elevate-schedule.v1"
                && schedule.calls.len() == 246
                && inherited_cases == 33,
            "comparison requires exactly 41 cases x 3 repeats x 2 arms"
        );
        let mut schedule_ids = BTreeSet::new();
        let mut counts = BTreeMap::new();
        for call in &schedule.calls {
            ensure!(
                case_by_id.contains_key(call.case.as_str()) && (1..=3).contains(&call.iteration),
                "comparison schedule has an unknown case or repeat"
            );
            let id = call.id.as_deref().unwrap_or_default();
            ensure!(
                id.len() == 32
                    && id.bytes().all(|byte| byte.is_ascii_hexdigit())
                    && schedule_ids.insert(id),
                "comparison schedule IDs must be unique opaque hashes"
            );
            *counts
                .entry((call.case.as_str(), call.iteration))
                .or_insert(0) += 1;
        }
        ensure!(
            counts.len() == 123 && counts.values().all(|count| *count == 2),
            "every case/repeat requires two scheduled calls"
        );
        schedule.calls
    } else {
        included
            .cases
            .iter()
            .map(|case| ScheduledCall {
                id: None,
                case: case.id.clone(),
                iteration: 1,
            })
            .collect()
    };
    let mut cfg = PostprocConfig {
        mode: Mode::Elevate,
        ..PostprocConfig::default()
    };
    cfg.elevate.model = MODEL.to_owned();
    cfg.elevate.reasoning_effort = Some("low".to_owned());
    cfg.api_key_id = None;
    cfg.decision_model = None;
    if let Some(endpoint) = &args.endpoint {
        cfg.endpoint = endpoint.clone();
    }
    let prompt = postproc::build_mode_system_prompt(
        &cfg,
        manifest.vocabulary.iter().map(VocabularyEntry::term),
    );
    let mut receipt = Receipt {
        schema: "cantrip.eval.elevate-receipt.v1".to_owned(),
        claim_type: "measured_unless_explicitly_unverified".to_owned(),
        source_revision: source_revision.to_owned(),
        corpus_version: manifest.corpus_version,
        corpus_sha256: hash(manifest_bytes),
        inherited_corpus_sha256: hash(included_bytes),
        inherited_cases,
        added_cases,
        planned_calls: planned.len(),
        comparison_schedule_sha256: schedule_bytes.as_ref().map(hash),
        model: MODEL.to_owned(),
        reasoning_effort: "low".to_owned(),
        production_path: "vocabulary::apply then postproc::refine (production prompt, request serialization, response normalization, usage and shared deadline)".to_owned(),
        system_prompt_sha256: hash(prompt),
        operator_prompt_sha256: hash(postproc::ELEVATE_SYSTEM_PROMPT),
        timeout_ms: cfg.timeout_ms,
        calls: Vec::new(),
        accounting: Value::Null,
        summary: Value::Null,
        semantic_reviewer: None,
        limitations: vec![
            "One sequential sample per case; no paid retries or paid judge. This light gate is not a reliability estimate or native UI/dictation proof.".to_owned(),
            "Semantic judgments are explicit builder-agent review, not an independent blinded judge. Clean accepted strings are never scored.".to_owned(),
            "The native broker adds only disclosed test-only generation/price bounds; system/user/model/reasoning remain the production request.".to_owned(),
        ],
    };
    if args.schedule.is_some() {
        receipt.schema = "cantrip.eval.elevate-comparison-private.v1".to_owned();
        receipt.limitations = vec![
            "One precommitted paired comparison: 41 cases x 3 repeats x 2 opaque routes. No paid retries or model judge.".to_owned(),
            "The native broker inserts only the fixed candidate addendum for its precommitted opaque route; all requests originate in production postproc::refine.".to_owned(),
            "Independent blinded semantic grades are primary. Exact-number checks are diagnostics; preserve numeric values, meaningful modality and literal content without imposing Clean wording.".to_owned(),
        ];
    }
    let mut private = Vec::new();
    if args.dry_run {
        summarize(&mut receipt);
        write_private(&args.out.join("receipt.json"), &receipt)?;
        eprintln!(
            "[eval] Elevate dry-run inherited={} added={} planned={} paid_calls=0",
            inherited_cases,
            added_cases,
            planned.len()
        );
        return Ok(());
    }
    let endpoint = args
        .endpoint
        .as_deref()
        .ok_or_else(|| anyhow!("--allow-paid requires the native credential broker --endpoint"))?;
    receipt.accounting = broker_accounting(endpoint)?;
    if let Some(schedule_hash) = &receipt.comparison_schedule_sha256 {
        ensure!(
            receipt.accounting["comparison"]["worker_schedule_sha256"].as_str()
                == Some(schedule_hash.as_str()),
            "native broker/client schedule mismatch"
        );
    }
    for scheduled in planned {
        let case = case_by_id[scheduled.case.as_str()];
        if let Some(id) = &scheduled.id {
            cfg.endpoint = format!("{endpoint}/eval/{id}");
        }
        let (source, substitutions) = vocabulary::apply(&case.input, &manifest.vocabulary);
        let started = Instant::now();
        // No key lookup, evaluator request copy, provider fallback or paid retry.
        let refinement = postproc::refine(&source, &cfg, &manifest.vocabulary, None, None);
        let latency_ms = started.elapsed().as_millis();
        let mut call = Call {
            schedule_id: scheduled.id.clone(),
            case: case.id.clone(),
            category: case.category.clone(),
            iteration: scheduled.iteration,
            latency_ms,
            input_tokens: None,
            output_tokens: None,
            reasoning_tokens: None,
            cost_usd: None,
            alias_substitutions: substitutions,
            source_sha256: hash(&case.input),
            output_sha256: None,
            failures: BTreeSet::new(),
            reviewed: false,
        };
        match refinement {
            Ok(refined) => {
                if let Some(usage) = refined.usage {
                    call.input_tokens = Some(usage.prompt_tokens);
                    call.output_tokens = Some(usage.completion_tokens);
                    call.reasoning_tokens = Some(usage.reasoning_tokens);
                    call.cost_usd = usage.reported_cost_usd;
                }
                let output_hash = hash(&refined.text);
                call.output_sha256 = Some(output_hash.clone());
                call.failures = deterministic(&case.checks, &refined.text);
                private.push(PrivateOutput {
                    schedule_id: scheduled.id,
                    case: case.id.clone(),
                    iteration: scheduled.iteration,
                    alias_corrected_source: source.into_owned(),
                    source: case.input.clone(),
                    text: refined.text,
                    output_sha256: output_hash,
                });
            }
            Err(_) => {
                call.failures.insert(Failure::Request);
            }
        }
        eprintln!(
            "[eval] Elevate case={} iteration={} ms={} deterministic_failures={}",
            call.case,
            call.iteration,
            call.latency_ms,
            call.failures.len()
        );
        let request_failed = call.failures.contains(&Failure::Request);
        receipt.calls.push(call);
        write_private(&args.out.join("private.json"), &private)?;
        let accounting = broker_accounting(endpoint);
        if let Ok(value) = &accounting {
            receipt.accounting = value.clone();
        }
        summarize(&mut receipt);
        write_private(&args.out.join("receipt.json"), &receipt)?;
        accounting?;
        if request_failed {
            break;
        } // Unknown charge or refused guard never starts another paid call.
    }
    eprintln!(
        "[eval] Elevate submitted={} review_required={} gate=unverified",
        receipt.calls.len(),
        private.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transformative_output_uses_literals_and_terms_not_clean_wording() {
        let checks = Checks {
            exact: vec!["3".into(), "retry_take".into()],
            lexical_any: vec!["idempotent".into()],
        };
        assert!(deterministic(&checks, "Make retry_take idempotent; retain 3 workers.").is_empty());
        let failed = deterministic(&checks, "Retry the task with 13 workers.");
        assert!(failed.contains(&Failure::Literal));
        assert!(failed.contains(&Failure::LexicalUpgrade));
    }

    #[test]
    fn protected_literals_do_not_match_substrings_of_other_identifiers_or_numbers() {
        assert!(!literal_present("13", "3"));
        assert!(!literal_present("other_retry_take", "retry_take"));
        assert!(!literal_present("version 0.1.12", "12"));
        assert!(!literal_present("3.7 workers", "3"));
        assert!(!literal_present(
            "/var/lib/cantrip/takes.json.bak",
            "/var/lib/cantrip/takes.json"
        ));
        assert!(literal_present(
            "Keep /var/lib/cantrip/takes.json.",
            "/var/lib/cantrip/takes.json"
        ));
    }
}
