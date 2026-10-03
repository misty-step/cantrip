//! Repeatable, text-free speech receipts; see eval/cloud-stt-contracts.md.
//! No credential store, corpus discovery, model downloads, or paid retries.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Cursor, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use transcribe_rs::onnx::parakeet::ParakeetModel;

use super::{wer, LocalModel};

const CATALOG_URL: &str = "https://openrouter.ai/api/v1/models?output_modalities=transcription";
const TRANSCRIPTION_URL: &str = "https://openrouter.ai/api/v1/audio/transcriptions";
const NANOS_PER_USD: f64 = 1_000_000_000.0;
const COMMISSION_CAP_NANOS: u64 = 4_500_000_000;
const MAX_WAV_BYTES: u64 = 25_000_000;
const RESPONSE_LIMIT: u64 = 10_000_000;
const BOUNDARY: &str = "cantrip-living-speech-96cbf54f";

type ClassResult<T> = std::result::Result<T, String>;

#[derive(Parser)]
#[command(
    name = "eval living-speech",
    about = "Evaluate an explicitly reviewed speech corpus without publishing text"
)]
struct Args {
    #[arg(long)]
    config: PathBuf,
    #[arg(long)]
    out: PathBuf,
    #[arg(long)]
    run_id: String,
    #[arg(long)]
    source_revision: String,
    /// Validate corpus, catalog, installed assets and cumulative budget without credentials or paid calls.
    #[arg(long, conflicts_with = "allow_paid")]
    dry_run: bool,
    /// Explicit authorization for this invocation only; does not authorize a recurring paid schedule.
    #[arg(long, required_unless_present = "dry_run")]
    allow_paid: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    schema_version: u32,
    corpus: CorpusConfig,
    models: Vec<ModelConfig>,
    local_parakeet: Option<LocalConfig>,
    budget: BudgetConfig,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusConfig {
    id: String,
    version: String,
    description: String,
    clips: Vec<ClipConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClipConfig {
    id: String,
    file: PathBuf,
    #[serde(rename = "ref")]
    reference: String,
    category: Category,
    source: String,
    license: String,
    reference_reviewed: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Category {
    Dictation,
    PublicSpeech,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelConfig {
    id: String,
    name: String,
    ceiling_usd_per_audio_hour: f64,
    pricing_source: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalConfig {
    id: String,
    name: String,
    model_version: String,
    dir: PathBuf,
    quant: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BudgetConfig {
    commission_id: String,
    ledger_path: PathBuf,
    commission_cap_usd: f64,
    per_run_cap_usd: f64,
}

struct PreparedClip {
    id: String,
    category: Category,
    reference: String,
    wav: Vec<u8>,
    samples: Vec<f32>,
    audio_secs: f64,
}

struct PreparedModel {
    id: String,
    name: String,
    model_version: String,
    reservations: Vec<u64>,
}

struct Prepared {
    corpus: PublicCorpus,
    clips: Vec<PreparedClip>,
    models: Vec<PreparedModel>,
    local: Option<LocalModel>,
    local_metadata: Option<(String, String, String)>,
    local_load_ms: Option<u64>,
    estimated_reservation_nanos: u64,
    budget: BudgetConfig,
    ledger_path: PathBuf,
}

#[derive(Serialize)]
struct PublicCorpus {
    id: String,
    version: String,
    sha256: String,
    clip_count: usize,
    dictation_clip_count: usize,
    audio_secs: f64,
    description: String,
}

#[derive(Serialize)]
struct PublicScorer {
    id: &'static str,
    description: &'static str,
    aggregation: &'static str,
}

#[derive(Serialize)]
struct PublicModel {
    id: String,
    name: String,
    backend: &'static str,
    model_version: String,
    successful_calls: usize,
    failed_calls: usize,
    wer: Option<f64>,
    cer: Option<f64>,
    median_latency_ms: Option<f64>,
    p95_latency_ms: Option<f64>,
    cost_usd: Option<f64>,
    cost_status: &'static str,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Status {
    Ok,
    Error,
    Empty,
}

#[derive(Serialize)]
struct PublicCall {
    model: String,
    clip: String,
    category: Category,
    audio_secs: f64,
    status: Status,
    error_class: Option<String>,
    latency_ms: u64,
    wer: f64,
    cer: f64,
    cost_usd: Option<f64>,
}

#[derive(Serialize)]
struct PublicBudget {
    commission_cap_usd: f64,
    reserved_usd: f64,
    reported_usd: f64,
    unknown_cost_calls: usize,
}

#[derive(Serialize)]
struct PublicRun {
    schema_version: u32,
    run_id: String,
    started_at_unix_ms: u64,
    completed_at_unix_ms: u64,
    source_revision: String,
    corpus: PublicCorpus,
    scorer: PublicScorer,
    models: Vec<PublicModel>,
    calls: Vec<PublicCall>,
    budget: PublicBudget,
    limitations: Vec<String>,
}

// The journal is append-only and locked on its own inode, not on a file later
// replaced by rename. A truncated final record makes the next invocation fail
// closed. A reservation is fsynced before its request can leave this process.
#[derive(Deserialize, Serialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
enum BudgetRecord {
    Commission {
        schema_version: u32,
        commission_id: String,
        cap_nanos: u64,
    },
    Reserve {
        run_id: String,
        model: String,
        clip: String,
        nanos: u64,
    },
    Report {
        run_id: String,
        model: String,
        clip: String,
        cost_usd: f64,
        release_reservation: bool,
    },
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct CallKey {
    run_id: String,
    model: String,
    clip: String,
}

struct Reservation {
    nanos: u64,
    reported_nanos: Option<u64>,
    release_reservation: bool,
}

impl Reservation {
    fn liability(&self) -> u64 {
        match self.reported_nanos {
            Some(cost) if self.release_reservation => cost,
            Some(cost) => self.nanos.max(cost),
            None => self.nanos,
        }
    }
}

#[derive(Default)]
struct RunBudget {
    liability_nanos: u64,
    reservation_nanos: u64,
    reported_usd: f64,
}

#[derive(Default)]
struct BudgetState {
    entries: BTreeMap<CallKey, Reservation>,
    runs: BTreeMap<String, RunBudget>,
    liability_nanos: u64,
    halted: bool,
}

impl BudgetState {
    fn check_capacity(&self, run_id: &str, amount: u64, run_cap: u64) -> ClassResult<()> {
        if self.halted {
            return Err("commission_halted".into());
        }
        let run_total = self.runs.get(run_id).map_or(0, |run| run.liability_nanos);
        if amount == 0
            || amount > run_cap.saturating_sub(run_total)
            || amount > COMMISSION_CAP_NANOS.saturating_sub(self.liability_nanos)
        {
            return Err("budget_insufficient".into());
        }
        Ok(())
    }

    fn apply(&mut self, record: &BudgetRecord) -> ClassResult<()> {
        match record {
            BudgetRecord::Reserve {
                run_id,
                model,
                clip,
                nanos,
            } => {
                let key = CallKey {
                    run_id: run_id.clone(),
                    model: model.clone(),
                    clip: clip.clone(),
                };
                if *nanos == 0
                    || self.halted
                    || *nanos > COMMISSION_CAP_NANOS.saturating_sub(self.liability_nanos)
                    || self.entries.contains_key(&key)
                {
                    return Err("budget_journal_invalid".into());
                }
                self.entries.insert(
                    key,
                    Reservation {
                        nanos: *nanos,
                        reported_nanos: None,
                        release_reservation: false,
                    },
                );
                self.liability_nanos = self
                    .liability_nanos
                    .checked_add(*nanos)
                    .ok_or("budget_journal_invalid")?;
                let run = self.runs.entry(run_id.clone()).or_default();
                run.liability_nanos = run
                    .liability_nanos
                    .checked_add(*nanos)
                    .ok_or("budget_journal_invalid")?;
                run.reservation_nanos = run
                    .reservation_nanos
                    .checked_add(*nanos)
                    .ok_or("budget_journal_invalid")?;
            }
            BudgetRecord::Report {
                run_id,
                model,
                clip,
                cost_usd,
                release_reservation,
            } => {
                if !cost_usd.is_finite() || *cost_usd < 0.0 {
                    return Err("budget_journal_invalid".into());
                }
                let key = CallKey {
                    run_id: run_id.clone(),
                    model: model.clone(),
                    clip: clip.clone(),
                };
                let entry = self.entries.get_mut(&key).ok_or("budget_journal_invalid")?;
                if entry.reported_nanos.is_some() {
                    return Err("budget_journal_invalid".into());
                }
                let old = entry.liability();
                let scaled = (*cost_usd * NANOS_PER_USD).ceil();
                if !scaled.is_finite() || scaled >= u64::MAX as f64 {
                    return Err("budget_journal_invalid".into());
                }
                let reported = scaled as u64;
                entry.reported_nanos = Some(reported);
                entry.release_reservation = *release_reservation;
                if reported > entry.nanos {
                    self.halted = true;
                }
                let new = entry.liability();
                self.liability_nanos = self
                    .liability_nanos
                    .checked_sub(old)
                    .and_then(|remaining| remaining.checked_add(new))
                    .ok_or("budget_journal_invalid")?;
                let run = self.runs.get_mut(run_id).ok_or("budget_journal_invalid")?;
                run.liability_nanos = run
                    .liability_nanos
                    .checked_sub(old)
                    .and_then(|remaining| remaining.checked_add(new))
                    .ok_or("budget_journal_invalid")?;
                run.reported_usd += *cost_usd;
            }
            BudgetRecord::Commission { .. } => return Err("budget_journal_invalid".into()),
        }
        Ok(())
    }
}

struct BudgetLedger {
    file: Option<File>,
    state: BudgetState,
}

impl BudgetLedger {
    fn open(path: &Path, commission_id: &str, create: bool) -> ClassResult<Self> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
        let mut newly_created = false;
        let opened = if create {
            match options.create_new(true).open(path) {
                Ok(file) => {
                    newly_created = true;
                    Ok(file)
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    options.create_new(false).open(path)
                }
                Err(error) => Err(error),
            }
        } else {
            options.open(path)
        };
        let mut file = match opened {
            Ok(file) => file,
            Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    file: None,
                    state: BudgetState::default(),
                });
            }
            Err(_) => return Err("budget_open".into()),
        };
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("budget_locked".into());
        }
        let metadata = file.metadata().map_err(|_| "budget_metadata")?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
            || metadata.nlink() != 1
        {
            return Err("budget_not_private".into());
        }
        let mut state = BudgetState::default();
        if metadata.len() == 0 {
            if !newly_created {
                return Err("budget_journal_invalid".into());
            }
            append_record(
                &mut file,
                &BudgetRecord::Commission {
                    schema_version: 1,
                    commission_id: commission_id.to_owned(),
                    cap_nanos: COMMISSION_CAP_NANOS,
                },
            )?;
            File::open(path.parent().ok_or("budget_parent_missing")?)
                .and_then(|parent| parent.sync_all())
                .map_err(|_| "budget_sync")?;
        } else {
            let mut reader = BufReader::new(&mut file);
            let mut line = String::new();
            let mut first = true;
            loop {
                line.clear();
                let bytes = reader.read_line(&mut line).map_err(|_| "budget_read")?;
                if bytes == 0 {
                    break;
                }
                if !line.ends_with('\n') {
                    return Err("budget_journal_incomplete".into());
                }
                let record: BudgetRecord =
                    serde_json::from_str(&line).map_err(|_| "budget_journal_invalid")?;
                if first {
                    match record {
                        BudgetRecord::Commission {
                            schema_version: 1,
                            commission_id: ref id,
                            cap_nanos: COMMISSION_CAP_NANOS,
                        } if id == commission_id => {}
                        _ => return Err("budget_commission_mismatch".into()),
                    }
                    first = false;
                } else {
                    state.apply(&record)?;
                }
            }
        }
        file.seek(SeekFrom::End(0)).map_err(|_| "budget_seek")?;
        Ok(Self {
            file: Some(file),
            state,
        })
    }

    fn reserve(&mut self, key: &CallKey, amount: u64, run_cap: u64) -> ClassResult<()> {
        self.state.check_capacity(&key.run_id, amount, run_cap)?;
        if self.state.entries.contains_key(key) {
            return Err("budget_duplicate_call".into());
        }
        let record = BudgetRecord::Reserve {
            run_id: key.run_id.clone(),
            model: key.model.clone(),
            clip: key.clip.clone(),
            nanos: amount,
        };
        append_record(self.file.as_mut().ok_or("budget_read_only")?, &record)?;
        self.state.apply(&record)
    }

    fn report(
        &mut self,
        key: &CallKey,
        cost_usd: f64,
        release_reservation: bool,
    ) -> ClassResult<()> {
        let record = BudgetRecord::Report {
            run_id: key.run_id.clone(),
            model: key.model.clone(),
            clip: key.clip.clone(),
            cost_usd,
            release_reservation,
        };
        append_record(self.file.as_mut().ok_or("budget_read_only")?, &record)?;
        self.state.apply(&record)
    }
}

fn append_record(file: &mut File, record: &BudgetRecord) -> ClassResult<()> {
    serde_json::to_writer(&mut *file, record).map_err(|_| "budget_write")?;
    file.write_all(b"\n").map_err(|_| "budget_write")?;
    file.sync_all().map_err(|_| "budget_sync")?;
    Ok(())
}

struct Receipts {
    dir: PathBuf,
    log: File,
}

impl Receipts {
    fn create(path: &Path) -> ClassResult<Self> {
        let parent = fs::canonicalize(
            path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )
        .map_err(|_| "output_parent_missing")?;
        let name = path.file_name().ok_or("output_path_invalid")?;
        let dir = parent.join(name);
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(|_| "output_not_fresh")?;
        fs::DirBuilder::new()
            .mode(0o700)
            .create(dir.join("calls"))
            .map_err(|_| "receipt_create")?;
        let log = new_file(&dir.join("run.log"))?;
        File::open(&parent)
            .and_then(|p| p.sync_all())
            .map_err(|_| "receipt_sync")?;
        Ok(Self { dir, log })
    }

    fn event(&mut self, event: Value) -> ClassResult<()> {
        serde_json::to_writer(&mut self.log, &event).map_err(|_| "receipt_write")?;
        self.log.write_all(b"\n").map_err(|_| "receipt_write")?;
        self.log.sync_all().map_err(|_| "receipt_sync")?;
        Ok(())
    }

    fn write<T: Serialize>(&self, name: &str, value: &T) -> ClassResult<()> {
        let destination = self.dir.join(name);
        let partial = destination.with_extension("partial");
        let mut file = new_file(&partial)?;
        serde_json::to_writer_pretty(&mut file, value).map_err(|_| "receipt_write")?;
        file.write_all(b"\n").map_err(|_| "receipt_write")?;
        file.sync_all().map_err(|_| "receipt_sync")?;
        // Publish a complete inode without replacing any prior immutable receipt.
        fs::hard_link(&partial, &destination).map_err(|_| "receipt_publish")?;
        fs::remove_file(&partial).map_err(|_| "receipt_publish")?;
        File::open(destination.parent().ok_or("receipt_path_invalid")?)
            .and_then(|parent| parent.sync_all())
            .map_err(|_| "receipt_sync")?;
        Ok(())
    }
}

fn new_file(path: &Path) -> ClassResult<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| "receipt_create".into())
}

pub(super) fn run(raw_args: &[String]) -> Result<()> {
    let args = match Args::try_parse_from(
        std::iter::once("living-speech").chain(raw_args.iter().map(String::as_str)),
    ) {
        Ok(args) => args,
        Err(error) if error.kind() == clap::error::ErrorKind::DisplayHelp => {
            return error
                .print()
                .map_err(|_| anyhow!("living_speech_help_output"));
        }
        Err(_) => {
            return Err(anyhow!(
                "living_speech_cli_invalid; use living-speech --help"
            ))
        }
    };
    if !safe_id(&args.run_id)
        || !(7..=64).contains(&args.source_revision.len())
        || !args
            .source_revision
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(anyhow!("living_speech_run_metadata_invalid"));
    }
    let mut receipts = Receipts::create(&args.out).map_err(|class| anyhow!(class))?;
    let started_at = unix_ms().map_err(|class| anyhow!(class))?;
    receipts.event(json!({"event":"start","run_id":args.run_id,"started_at_unix_ms":started_at,"dry_run":args.dry_run}))
        .map_err(|class| anyhow!(class))?;
    let result = run_inner(&args, started_at, &mut receipts);
    let final_event = match &result {
        Ok(()) => {
            json!({"event":"finish","status":if args.dry_run {"dry_run_complete"} else {"complete"}})
        }
        Err(class) => json!({"event":"finish","status":"error","error_class":class}),
    };
    receipts
        .event(final_event)
        .map_err(|class| anyhow!(class))?;
    result.map_err(|class| anyhow!(class))
}

fn run_inner(args: &Args, started_at: u64, receipts: &mut Receipts) -> ClassResult<()> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(120))
        .redirects(0)
        .build();
    let mut prepared = prepare(&args.config, &receipts.dir, &agent)?;
    let mut ledger = BudgetLedger::open(
        &prepared.ledger_path,
        &prepared.budget.commission_id,
        !args.dry_run,
    )?;
    if ledger.state.runs.contains_key(&args.run_id) {
        return Err("run_id_already_spent".into());
    }
    let run_cap = cap_nanos(prepared.budget.per_run_cap_usd)?;
    let capacity =
        ledger
            .state
            .check_capacity(&args.run_id, prepared.estimated_reservation_nanos, run_cap);
    let report = json!({
        "schema_version":1,
        "run_id":args.run_id,
        "status":if capacity.is_ok() {"ready"} else {"blocked"},
        "error_class":capacity.as_ref().err(),
        "corpus":prepared.corpus,
        "models":prepared.models.iter().map(|model| json!({"id":model.id,"name":model.name,"model_version":model.model_version,"estimated_reservation_usd":usd(model.reservations.iter().sum())})).collect::<Vec<_>>(),
        "local_model":prepared.local_metadata.as_ref().map(|(id,name,version)| json!({"id":id,"name":name,"model_version":version})),
        "paid_request_count":prepared.models.len() * prepared.clips.len(),
        "local_request_count":if prepared.local.is_some() {prepared.clips.len()} else {0},
        "paid_audio_secs":prepared.corpus.audio_secs * prepared.models.len() as f64,
        "estimated_reservation_usd":usd(prepared.estimated_reservation_nanos),
        "commission_cap_usd":4.5,
        "per_run_cap_usd":prepared.budget.per_run_cap_usd,
        "commission_liability_before_usd":usd(ledger.state.liability_nanos),
        "commission_remaining_usd":usd(COMMISSION_CAP_NANOS.saturating_sub(ledger.state.liability_nanos)),
        "credentials_checked":false,
        "paid_calls_made":0
    });
    receipts.write("preflight.json", &report)?;
    receipts.event(json!({"event":"preflight","status":if capacity.is_ok() {"ready"} else {"blocked"},"paid_request_count":prepared.models.len()*prepared.clips.len(),"audio_secs":prepared.corpus.audio_secs,"estimated_reservation_usd":usd(prepared.estimated_reservation_nanos)}))?;
    capacity?;
    if args.dry_run {
        println!(
            "{}",
            serde_json::to_string(&report).map_err(|_| "preflight_serialization")?
        );
        return Ok(());
    }
    if !args.allow_paid {
        return Err("paid_authorization_required".into());
    }
    let key = std::env::var("OPENROUTER_API_KEY").map_err(|_| "credentials_unavailable")?;
    if key.is_empty() || !key.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err("credentials_invalid".into());
    }
    let authorization = format!("Bearer {key}");
    let mut calls = Vec::with_capacity(
        (prepared.models.len() + usize::from(prepared.local.is_some())) * prepared.clips.len(),
    );
    let mut halted: Option<String> = None;
    let mut unknown_cost_calls = 0;
    let mut reported_usd = 0.0;
    let mut receipt_error = false;
    for model in &prepared.models {
        for (clip, reservation) in prepared.clips.iter().zip(&model.reservations) {
            let outcome = if let Some(class) = &halted {
                Outcome::error(class.clone())
            } else {
                let call_key = CallKey {
                    run_id: args.run_id.clone(),
                    model: model.id.clone(),
                    clip: clip.id.clone(),
                };
                match ledger.reserve(&call_key, *reservation, run_cap) {
                    Err(class) => {
                        halted = Some(class.clone());
                        Outcome::error(class)
                    }
                    Ok(()) => {
                        if receipts.event(json!({"event":"reserved","model":model.id,"clip":clip.id,"reserved_usd":usd(*reservation)})).is_err() {
                            receipt_error = true;
                            halted = Some("receipt_storage".into());
                            Outcome::error("receipt_storage".into())
                        } else {
                            let outcome = cloud_call(&agent, &authorization, &model.id, clip);
                            if let Some(cost) = outcome.cost_usd {
                                reported_usd += cost;
                                if let Err(class) = ledger.report(&call_key, cost, outcome.status == Status::Ok) {
                                    halted = Some(class);
                                } else if ledger.state.halted {
                                    halted = Some("pricing_ceiling_exceeded".into());
                                }
                            } else {
                                unknown_cost_calls += 1;
                            }
                            outcome
                        }
                    }
                }
            };
            let call = scored_call(&model.id, clip, outcome);
            if record_call(receipts, calls.len(), &call).is_err() {
                receipt_error = true;
                halted = Some("receipt_storage".into());
            }
            calls.push(call);
        }
    }
    if let (Some(local), Some((id, _, _))) = (&mut prepared.local, &prepared.local_metadata) {
        for clip in &prepared.clips {
            let started = Instant::now();
            let outcome = match local.transcribe(&clip.samples) {
                Ok(text) => Outcome::text(text, Some(0.0), elapsed_ms(started)),
                Err(_) => Outcome {
                    latency_ms: elapsed_ms(started),
                    cost_usd: Some(0.0),
                    ..Outcome::error("local_transcription".into())
                },
            };
            let call = scored_call(id, clip, outcome);
            if record_call(receipts, calls.len(), &call).is_err() {
                receipt_error = true;
            }
            calls.push(call);
        }
    }
    let mut models: Vec<PublicModel> = prepared
        .models
        .iter()
        .map(|model| {
            summarize(
                &model.id,
                &model.name,
                "openrouter",
                &model.model_version,
                &calls,
            )
        })
        .collect();
    if let Some((id, name, version)) = &prepared.local_metadata {
        models.push(summarize(id, name, "local", version, &calls));
    }
    let budget = ledger.state.runs.get(&args.run_id);
    let failures = calls
        .iter()
        .filter(|call| call.status != Status::Ok)
        .count();
    let mut limitations = vec![
        "English only. ASCII lowercase normalization keeps alphanumerics, apostrophes and hyphens; WER can exceed 100%. Results are not comparable to external benchmark normalization or corpora.".into(),
        "Corpus is explicitly selected and reference-reviewed, not a representative population sample. Dictation and public-speech clip categories remain separate in the per-call data.".into(),
        "Audio, references, provider transcripts, source paths and original take identifiers are withheld. The corpus digest includes exact references and audio bytes but excludes filesystem paths.".into(),
        "Cloud requests use the configured model ID; model_version is the canonical slug observed in the public transcription catalog at preflight, not proof of immutable provider weights.".into(),
        "Sequential calls, common language en and response_format json; no paid retries, diarization options, vocabulary prompts or post-processing.".into(),
        "Latency percentiles include successful calls only (nearest-rank p95); errors and empty outputs contribute full deletion to macro-mean WER/CER and count as failed calls. Unsubmitted blocked calls have zero elapsed time, not a measured service latency.".into(),
        "Conservative reservations round audio up to whole seconds using explicitly reviewed per-hour ceiling rates, not ambiguous catalog prompt units. Failure, empty output and missing cost never release reservations. Unknown usage is never treated as zero.".into(),
        "Budget reserved_usd is this run's remaining accounted liability after successful reported-cost settlements; reported_usd is observed usage.cost only. The private cumulative commission ledger, not prior unrelated API-key usage, controls the $4.50 cap.".into(),
        format!("Host: {} {}; available execution threads: {}. Competing host load and network/provider load were not isolated or sampled.", std::env::consts::OS, std::env::consts::ARCH, std::thread::available_parallelism().map_or_else(|_| "unknown".to_owned(), |count| count.get().to_string())),
        format!("Cumulative commission accounted liability after this run: ${:.9}; original conservative reservations for this run: ${:.9}.", usd(ledger.state.liability_nanos), budget.map_or(0.0, |run| usd(run.reservation_nanos))),
    ];
    if let Some(load_ms) = prepared.local_load_ms {
        limitations.push(format!("Local Parakeet uses the explicitly selected installed model; its execution provider was not independently instrumented. Model loading ({load_ms} ms) is excluded from per-clip latency. Local-zero means zero API charges, not zero hardware or electricity cost."));
    }
    if failures > 0 {
        limitations.push(format!("Incomplete quality run: {failures} scheduled calls failed, were empty, or were blocked. Every configured model/clip pair remains in the scores; this run must not be labeled all-successful."));
    }
    if unknown_cost_calls > 0 {
        limitations.push(format!("{unknown_cost_calls} submitted paid calls have unknown cost; their conservative reservations remain committed to the commission."));
    }
    if let Some(class) = &halted {
        limitations.push(format!(
            "Further paid calls were halted with error class {class}."
        ));
    }
    if receipt_error {
        limitations.push("Receipt storage failed during the run; later paid requests were prevented. Available in-memory call accounting is retained here if final storage succeeds.".into());
    }
    let result = PublicRun {
        schema_version: 1,
        run_id: args.run_id.clone(),
        started_at_unix_ms: started_at,
        completed_at_unix_ms: unix_ms()?,
        source_revision: args.source_revision.clone(),
        corpus: prepared.corpus,
        scorer: PublicScorer {
            id: "cantrip-ascii-v1",
            description: "ASCII case-folding; word tokens keep alphanumerics, apostrophes and hyphens; CER drops spaces. Levenshtein edit distance divided by reference length.",
            aggregation: "macro-mean over clips; error/empty output scored as full deletion",
        },
        models,
        calls,
        budget: PublicBudget {
            commission_cap_usd: 4.5,
            reserved_usd: budget.map_or(0.0, |run| usd(run.liability_nanos)),
            reported_usd,
            unknown_cost_calls,
        },
        limitations,
    };
    receipts.write("results.json", &result)?;
    if failures > 0 || unknown_cost_calls > 0 || halted.is_some() || receipt_error {
        Err("incomplete_run".into())
    } else {
        Ok(())
    }
}

fn prepare(path: &Path, out: &Path, agent: &ureq::Agent) -> ClassResult<Prepared> {
    let path = fs::canonicalize(path).map_err(|_| "config_missing")?;
    let base = path.parent().ok_or("config_parent_missing")?;
    let config: Config = serde_json::from_reader(File::open(&path).map_err(|_| "config_read")?)
        .map_err(|_| "config_invalid")?;
    if config.schema_version != 1
        || !safe_id(&config.corpus.id)
        || !public_label(&config.corpus.version, 100)
        || !public_label(&config.corpus.description, 2000)
        || !safe_id(&config.budget.commission_id)
        || config.budget.commission_cap_usd != 4.5
    {
        return Err("config_metadata_invalid".into());
    }
    cap_nanos(config.budget.per_run_cap_usd)?;
    if config.models.is_empty()
        || config.models.len() + usize::from(config.local_parakeet.is_some()) < 5
    {
        return Err("at_least_five_models_required".into());
    }
    let mut model_ids = BTreeSet::new();
    for model in &config.models {
        if !model_id(&model.id)
            || !public_label(&model.name, 200)
            || !model_ids.insert(model.id.as_str())
        {
            return Err("model_metadata_invalid".into());
        }
        if !model.ceiling_usd_per_audio_hour.is_finite()
            || model.ceiling_usd_per_audio_hour <= 0.0
            || !model.pricing_source.starts_with("https://")
            || !public_label(&model.pricing_source, 2000)
        {
            return Err("price_ceiling_unbounded".into());
        }
    }
    if let Some(local) = &config.local_parakeet {
        if !safe_id(&local.id)
            || !public_label(&local.name, 200)
            || !public_label(&local.model_version, 200)
            || !model_ids.insert(local.id.as_str())
            || !matches!(local.quant.as_str(), "int8" | "int4" | "fp16" | "fp32")
        {
            return Err("local_metadata_invalid".into());
        }
    }
    let ledger_path = selected_future_path(base, &config.budget.ledger_path)?;
    if ledger_path.starts_with(out) {
        return Err("budget_inside_public_output".into());
    }
    let parent = fs::metadata(ledger_path.parent().ok_or("budget_parent_missing")?)
        .map_err(|_| "budget_parent_missing")?;
    if parent.uid() != unsafe { libc::geteuid() } || parent.permissions().mode() & 0o077 != 0 {
        return Err("budget_parent_not_private".into());
    }
    let mut clip_ids = BTreeSet::new();
    let mut clips = Vec::with_capacity(config.corpus.clips.len());
    let mut hash = Sha256::new();
    hash_part(&mut hash, b"cantrip-reviewed-corpus-v1");
    hash_part(&mut hash, config.corpus.id.as_bytes());
    hash_part(&mut hash, config.corpus.version.as_bytes());
    let mut dictation_count = 0;
    let mut audio_secs = 0.0;
    for clip in &config.corpus.clips {
        if !safe_id(&clip.id) || !clip_ids.insert(clip.id.as_str()) {
            return Err("clip_id_invalid".into());
        }
        if !clip.reference_reviewed
            || wer::words(&clip.reference).is_empty()
            || wer::chars(&clip.reference).is_empty()
        {
            return Err("reference_unreviewed_or_empty".into());
        }
        if clip.source.trim().is_empty() || clip.license.trim().is_empty() {
            return Err("clip_provenance_missing".into());
        }
        let selected = selected_path(base, &clip.file)?;
        let mut file = File::open(&selected).map_err(|_| "clip_read")?;
        let metadata = file.metadata().map_err(|_| "clip_read")?;
        if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_WAV_BYTES {
            return Err("clip_size_invalid".into());
        }
        let mut wav = Vec::with_capacity(metadata.len() as usize);
        Read::by_ref(&mut file)
            .take(MAX_WAV_BYTES + 1)
            .read_to_end(&mut wav)
            .map_err(|_| "clip_read")?;
        if wav.len() as u64 > MAX_WAV_BYTES {
            return Err("clip_size_invalid".into());
        }
        let mut reader =
            hound::WavReader::new(Cursor::new(&wav)).map_err(|_| "clip_wav_invalid")?;
        let spec = reader.spec();
        if spec.channels != 1
            || spec.sample_rate != 16_000
            || spec.bits_per_sample != 16
            || spec.sample_format != hound::SampleFormat::Int
        {
            return Err("clip_requires_16khz_mono_pcm16".into());
        }
        let mut samples = if config.local_parakeet.is_some() {
            Vec::with_capacity(reader.duration() as usize)
        } else {
            Vec::new()
        };
        let mut sample_count = 0_u64;
        for sample in reader.samples::<i16>() {
            let sample = sample.map_err(|_| "clip_wav_invalid")?;
            sample_count += 1;
            if config.local_parakeet.is_some() {
                samples.push(f32::from(sample) / 32768.0);
            }
        }
        if sample_count == 0 {
            return Err("clip_audio_empty".into());
        }
        let secs = sample_count as f64 / 16_000.0;
        audio_secs += secs;
        dictation_count += usize::from(clip.category == Category::Dictation);
        hash_part(&mut hash, clip.id.as_bytes());
        hash_part(
            &mut hash,
            match clip.category {
                Category::Dictation => b"dictation",
                Category::PublicSpeech => b"public-speech",
            },
        );
        hash_part(&mut hash, clip.reference.as_bytes());
        hash_part(&mut hash, clip.source.as_bytes());
        hash_part(&mut hash, clip.license.as_bytes());
        hash_part(&mut hash, &wav);
        clips.push(PreparedClip {
            id: clip.id.clone(),
            category: clip.category,
            reference: clip.reference.clone(),
            wav,
            samples,
            audio_secs: secs,
        });
    }
    if clips.is_empty() || dictation_count == 0 {
        return Err("real_dictation_required".into());
    }
    let versions = catalog_versions(agent)?;
    let mut models = Vec::with_capacity(config.models.len());
    let mut estimated_reservation_nanos = 0_u64;
    for model in config.models {
        let version = versions
            .get(&model.id)
            .ok_or("model_not_in_transcription_catalog")?;
        let reservations: Vec<u64> = clips
            .iter()
            .map(|clip| reservation_nanos(model.ceiling_usd_per_audio_hour, clip.audio_secs))
            .collect::<ClassResult<_>>()?;
        for amount in &reservations {
            estimated_reservation_nanos = estimated_reservation_nanos
                .checked_add(*amount)
                .ok_or("reservation_unbounded")?;
        }
        models.push(PreparedModel {
            id: model.id,
            name: model.name,
            model_version: version.clone(),
            reservations,
        });
    }
    let (local, local_metadata, local_load_ms) = match config.local_parakeet {
        Some(local) => {
            let dir = selected_path(base, &local.dir)?;
            if !dir.is_dir() {
                return Err("local_model_missing".into());
            }
            let started = Instant::now();
            let model = ParakeetModel::load(&dir, &super::parse_quant(Some(&local.quant)))
                .map_err(|_| "local_model_load")?;
            (
                Some(LocalModel::Parakeet(model)),
                Some((local.id, local.name, local.model_version)),
                Some(elapsed_ms(started)),
            )
        }
        None => (None, None, None),
    };
    Ok(Prepared {
        corpus: PublicCorpus {
            id: config.corpus.id,
            version: config.corpus.version,
            sha256: format!("{:x}", hash.finalize()),
            clip_count: clips.len(),
            dictation_clip_count: dictation_count,
            audio_secs,
            description: config.corpus.description,
        },
        clips,
        models,
        local,
        local_metadata,
        local_load_ms,
        estimated_reservation_nanos,
        budget: config.budget,
        ledger_path,
    })
}

#[derive(Deserialize)]
struct Catalog {
    data: Vec<CatalogModel>,
}

#[derive(Deserialize)]
struct CatalogModel {
    id: String,
    canonical_slug: String,
    architecture: CatalogArchitecture,
}

#[derive(Deserialize)]
struct CatalogArchitecture {
    input_modalities: Vec<String>,
    output_modalities: Vec<String>,
}

fn catalog_versions(agent: &ureq::Agent) -> ClassResult<BTreeMap<String, String>> {
    let response = agent.get(CATALOG_URL).call().map_err(http_error_class)?;
    if response.status() != 200 {
        return Err(format!("catalog_http_{}", response.status()));
    }
    let catalog: Catalog = serde_json::from_reader(response.into_reader().take(RESPONSE_LIMIT))
        .map_err(|_| "catalog_shape")?;
    let mut versions = BTreeMap::new();
    for model in catalog.data {
        if model
            .architecture
            .input_modalities
            .iter()
            .any(|modality| modality == "audio")
            && model
                .architecture
                .output_modalities
                .iter()
                .any(|modality| modality == "transcription")
            && model_id(&model.id)
            && model_id(&model.canonical_slug)
            && versions.insert(model.id, model.canonical_slug).is_some()
        {
            return Err("catalog_duplicate_model".into());
        }
    }
    Ok(versions)
}

#[derive(Deserialize)]
struct TranscriptionResponse {
    #[serde(default)]
    text: Value,
    #[serde(default)]
    usage: Option<Value>,
}

struct Outcome {
    text: String,
    status: Status,
    error_class: Option<String>,
    latency_ms: u64,
    cost_usd: Option<f64>,
}

impl Outcome {
    fn error(class: String) -> Self {
        Self {
            text: String::new(),
            status: Status::Error,
            error_class: Some(class),
            latency_ms: 0,
            cost_usd: None,
        }
    }

    fn text(text: String, cost_usd: Option<f64>, latency_ms: u64) -> Self {
        let empty = wer::words(&text).is_empty();
        Self {
            text,
            status: if empty { Status::Empty } else { Status::Ok },
            error_class: if empty {
                Some("empty_output".into())
            } else {
                None
            },
            latency_ms,
            cost_usd,
        }
    }
}

fn cloud_call(
    agent: &ureq::Agent,
    authorization: &str,
    model: &str,
    clip: &PreparedClip,
) -> Outcome {
    // Never include a private filename or source path in the upload metadata.
    let prefix = format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\n{model}\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"language\"\r\n\r\nen\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"response_format\"\r\n\r\njson\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\nContent-Type: audio/wav\r\n\r\n");
    let suffix = format!("\r\n--{BOUNDARY}--\r\n");
    // Stream the already-reviewed bytes instead of reopening a mutable source
    // or allocating/copying another complete audio buffer for every model.
    let length = prefix.len() + clip.wav.len() + suffix.len();
    let body = Cursor::new(prefix.as_bytes())
        .chain(Cursor::new(&clip.wav))
        .chain(Cursor::new(suffix.as_bytes()));
    let started = Instant::now();
    let response = agent
        .post(TRANSCRIPTION_URL)
        .set("Authorization", authorization)
        .set(
            "Content-Type",
            &format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .set("Content-Length", &length.to_string())
        .send(body);
    let mut outcome = match response {
        Ok(response) | Err(ureq::Error::Status(_, response)) => parse_response(response),
        Err(error) => Outcome::error(http_error_class(error)),
    };
    outcome.latency_ms = elapsed_ms(started);
    outcome
}

fn parse_response(response: ureq::Response) -> Outcome {
    let status = response.status();
    let parsed: TranscriptionResponse =
        match serde_json::from_reader(response.into_reader().take(RESPONSE_LIMIT)) {
            Ok(parsed) => parsed,
            Err(_) => {
                return Outcome::error(if status == 200 {
                    "response_shape".into()
                } else {
                    format!("http_{status}")
                })
            }
        };
    let cost = parsed
        .usage
        .as_ref()
        .and_then(|usage| usage.get("cost"))
        .and_then(Value::as_f64)
        .filter(|cost| cost.is_finite() && *cost >= 0.0);
    if status != 200 {
        return Outcome {
            cost_usd: cost,
            ..Outcome::error(format!("http_{status}"))
        };
    }
    match parsed.text {
        Value::String(text) => Outcome::text(text, cost, 0),
        _ => Outcome {
            cost_usd: cost,
            ..Outcome::error("response_shape".into())
        },
    }
}

fn http_error_class(error: ureq::Error) -> String {
    match error {
        ureq::Error::Status(status, _) => format!("http_{status}"),
        ureq::Error::Transport(error) => {
            let mut class = format!("transport_{:?}", error.kind());
            class.make_ascii_lowercase();
            class
        }
    }
}

fn scored_call(model: &str, clip: &PreparedClip, outcome: Outcome) -> PublicCall {
    let (word_error, char_error) = if outcome.status == Status::Ok {
        (
            wer::wer(&clip.reference, &outcome.text),
            wer::cer(&clip.reference, &outcome.text),
        )
    } else {
        (1.0, 1.0)
    };
    PublicCall {
        model: model.to_owned(),
        clip: clip.id.clone(),
        category: clip.category,
        audio_secs: clip.audio_secs,
        status: outcome.status,
        error_class: outcome.error_class,
        latency_ms: outcome.latency_ms,
        wer: word_error,
        cer: char_error,
        cost_usd: outcome.cost_usd,
    }
}

fn record_call(receipts: &mut Receipts, index: usize, call: &PublicCall) -> ClassResult<()> {
    receipts.write(&format!("calls/{index:06}.json"), call)?;
    receipts.event(json!({"event":"call","model":call.model,"clip":call.clip,"status":call.status,"error_class":call.error_class,"latency_ms":call.latency_ms,"wer":call.wer,"cer":call.cer,"cost_usd":call.cost_usd}))
}

fn summarize(
    id: &str,
    name: &str,
    backend: &'static str,
    version: &str,
    calls: &[PublicCall],
) -> PublicModel {
    let mut count = 0;
    let mut successful = 0;
    let mut word_error = 0.0;
    let mut char_error = 0.0;
    let mut latencies = Vec::new();
    let mut total_cost = Some(0.0);
    for call in calls.iter().filter(|call| call.model == id) {
        count += 1;
        word_error += call.wer;
        char_error += call.cer;
        if call.status == Status::Ok {
            successful += 1;
            latencies.push(call.latency_ms);
        }
        total_cost = total_cost.zip(call.cost_usd).map(|(sum, cost)| sum + cost);
    }
    latencies.sort_unstable();
    let median = if latencies.is_empty() {
        None
    } else {
        let mid = latencies.len() / 2;
        Some(if latencies.len() % 2 == 0 {
            (latencies[mid - 1] as f64 + latencies[mid] as f64) / 2.0
        } else {
            latencies[mid] as f64
        })
    };
    let p95 = if latencies.is_empty() {
        None
    } else {
        Some(latencies[(latencies.len() * 95).div_ceil(100) - 1] as f64)
    };
    PublicModel {
        id: id.to_owned(),
        name: name.to_owned(),
        backend,
        model_version: version.to_owned(),
        successful_calls: successful,
        failed_calls: count - successful,
        wer: (count > 0).then(|| word_error / count as f64),
        cer: (count > 0).then(|| char_error / count as f64),
        median_latency_ms: median,
        p95_latency_ms: p95,
        cost_usd: total_cost,
        cost_status: if backend == "local" {
            "local-zero"
        } else if total_cost.is_some() {
            "reported"
        } else {
            "unknown"
        },
    }
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && id.as_bytes()[0].is_ascii_alphanumeric()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn model_id(id: &str) -> bool {
    let Some((provider, model)) = id.split_once('/') else {
        return false;
    };
    !provider.is_empty()
        && !model.is_empty()
        && id.len() <= 200
        && provider
            .bytes()
            .chain(model.bytes())
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn public_label(label: &str, max: usize) -> bool {
    !label.trim().is_empty() && label.len() <= max && !label.chars().any(char::is_control)
}

fn selected_path(base: &Path, path: &Path) -> ClassResult<PathBuf> {
    fs::canonicalize(if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    })
    .map_err(|_| "selected_path_missing".into())
}

fn selected_future_path(base: &Path, path: &Path) -> ClassResult<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let parent = fs::canonicalize(path.parent().ok_or("budget_parent_missing")?)
        .map_err(|_| "budget_parent_missing")?;
    Ok(parent.join(path.file_name().ok_or("budget_path_invalid")?))
}

fn hash_part(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}

fn cap_nanos(cap: f64) -> ClassResult<u64> {
    if !cap.is_finite() || cap <= 0.0 || cap > 4.5 {
        return Err("per_run_cap_invalid".into());
    }
    let nanos = (cap * NANOS_PER_USD).floor() as u64;
    if nanos == 0 {
        Err("per_run_cap_invalid".into())
    } else {
        Ok(nanos)
    }
}

fn reservation_nanos(rate_per_hour: f64, audio_secs: f64) -> ClassResult<u64> {
    let nanos = (rate_per_hour * audio_secs.ceil() / 3600.0 * NANOS_PER_USD).ceil();
    if !rate_per_hour.is_finite()
        || rate_per_hour <= 0.0
        || !audio_secs.is_finite()
        || audio_secs <= 0.0
        || !nanos.is_finite()
        || nanos < 1.0
        || nanos > COMMISSION_CAP_NANOS as f64
    {
        return Err("reservation_unbounded".into());
    }
    Ok(nanos as u64)
}

fn usd(nanos: u64) -> f64 {
    nanos as f64 / NANOS_PER_USD
}

fn unix_ms() -> ClassResult<u64> {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| "clock_invalid")?
            .as_millis(),
    )
    .map_err(|_| "clock_invalid".into())
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct PrivateTemp(PathBuf);

    impl PrivateTemp {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "cantrip-speech-budget-{}-{}-{}",
                std::process::id(),
                unix_ms().unwrap(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for PrivateTemp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn key(run: &str, clip: &str) -> CallKey {
        CallKey {
            run_id: run.into(),
            model: "vendor/model".into(),
            clip: clip.into(),
        }
    }

    #[test]
    fn interrupted_receipt_is_never_published_under_its_final_name() {
        struct Interrupted;
        impl Serialize for Interrupted {
            fn serialize<S: serde::Serializer>(
                &self,
                serializer: S,
            ) -> std::result::Result<S::Ok, S::Error> {
                use serde::ser::SerializeSeq;
                let mut sequence = serializer.serialize_seq(Some(2))?;
                sequence.serialize_element(&1_u32)?;
                Err(serde::ser::Error::custom("interrupted receipt"))
            }
        }

        let temp = PrivateTemp::new();
        let receipts = Receipts::create(&temp.0.join("run")).unwrap();
        assert!(receipts.write("results.json", &Interrupted).is_err());
        assert!(!receipts.dir.join("results.json").exists());
    }

    #[test]
    fn existing_empty_journal_cannot_restart_commission_spending() {
        let temp = PrivateTemp::new();
        let path = temp.0.join("commission.jsonl");
        drop(new_file(&path).unwrap());
        assert!(BudgetLedger::open(&path, "commission", true).is_err());
    }

    #[test]
    fn impossible_reservation_history_is_rejected_even_after_zero_cost_release() {
        let temp = PrivateTemp::new();
        let path = temp.0.join("commission.jsonl");
        let mut file = new_file(&path).unwrap();
        for record in [
            BudgetRecord::Commission {
                schema_version: 1,
                commission_id: "commission".into(),
                cap_nanos: COMMISSION_CAP_NANOS,
            },
            BudgetRecord::Reserve {
                run_id: "run-a".into(),
                model: "vendor/model".into(),
                clip: "dictation-01".into(),
                nanos: COMMISSION_CAP_NANOS + 1,
            },
            BudgetRecord::Report {
                run_id: "run-a".into(),
                model: "vendor/model".into(),
                clip: "dictation-01".into(),
                cost_usd: 0.0,
                release_reservation: true,
            },
        ] {
            append_record(&mut file, &record).unwrap();
        }
        drop(file);
        assert!(BudgetLedger::open(&path, "commission", false).is_err());
    }

    #[test]
    fn malformed_recognition_cannot_hide_reported_charge_or_ceiling_overrun() {
        let temp = PrivateTemp::new();
        let path = temp.0.join("commission.jsonl");
        let mut ledger = BudgetLedger::open(&path, "commission", true).unwrap();
        let call_key = key("run-a", "dictation-01");
        ledger.reserve(&call_key, 100_000_000, 200_000_000).unwrap();
        let outcome = parse_response(
            ureq::Response::new(200, "OK", r#"{"text":null,"usage":{"cost":0.11}}"#).unwrap(),
        );
        assert!(outcome.status == Status::Error);
        ledger
            .report(
                &call_key,
                outcome
                    .cost_usd
                    .expect("reported bill must survive invalid text"),
                outcome.status == Status::Ok,
            )
            .unwrap();
        drop(ledger);
        let mut ledger = BudgetLedger::open(&path, "commission", false).unwrap();
        assert_eq!(ledger.state.liability_nanos, 110_000_000);
        assert!(ledger
            .reserve(&key("run-b", "extra"), 1, COMMISSION_CAP_NANOS)
            .is_err());
    }

    #[test]
    fn failed_http_output_cannot_become_recognition_or_hide_its_charge() {
        let outcome = parse_response(
            ureq::Response::new(
                503,
                "Service Unavailable",
                r#"{"text":"provider diagnostic, not a transcript","usage":{"cost":0.05}}"#,
            )
            .unwrap(),
        );
        assert!(outcome.status == Status::Error);
        assert_eq!(outcome.error_class.as_deref(), Some("http_503"));
        assert_eq!(outcome.cost_usd, Some(0.05));
    }

    #[test]
    fn reservations_survive_failure_empty_unknown_and_restart() {
        let temp = PrivateTemp::new();
        let path = temp.0.join("commission.jsonl");
        let mut ledger = BudgetLedger::open(&path, "commission", true).unwrap();
        // Failure and missing usage deliberately have no report event; an
        // empty response has real usage, but cannot release its reservation.
        ledger
            .reserve(&key("run-a", "failed"), 1_000_000_000, COMMISSION_CAP_NANOS)
            .unwrap();
        ledger
            .reserve(
                &key("run-a", "unknown"),
                1_000_000_000,
                COMMISSION_CAP_NANOS,
            )
            .unwrap();
        ledger
            .reserve(&key("run-a", "empty"), 1_000_000_000, COMMISSION_CAP_NANOS)
            .unwrap();
        ledger.report(&key("run-a", "empty"), 0.1, false).unwrap();
        drop(ledger);
        let mut ledger = BudgetLedger::open(&path, "commission", false).unwrap();
        assert_eq!(ledger.state.liability_nanos, 3_000_000_000);
        assert!(ledger
            .reserve(
                &key("run-b", "over-cap"),
                1_500_000_001,
                COMMISSION_CAP_NANOS
            )
            .is_err());
        assert!(ledger
            .reserve(&key("run-a", "over-run-cap"), 1, 3_000_000_000)
            .is_err());
        ledger
            .reserve(
                &key("run-b", "exact-cap"),
                1_500_000_000,
                COMMISSION_CAP_NANOS,
            )
            .unwrap();
        assert!(ledger
            .reserve(&key("run-c", "extra"), 1, COMMISSION_CAP_NANOS)
            .is_err());
    }

    #[test]
    fn exclusive_owner_and_incomplete_journal_prevent_spending() {
        let temp = PrivateTemp::new();
        let path = temp.0.join("commission.jsonl");
        let ledger = BudgetLedger::open(&path, "commission", true).unwrap();
        assert!(
            matches!(BudgetLedger::open(&path, "commission", true), Err(class) if class == "budget_locked")
        );
        drop(ledger);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"event\":\"reserve\"").unwrap();
        file.sync_all().unwrap();
        assert!(
            matches!(BudgetLedger::open(&path, "commission", true), Err(class) if class == "budget_journal_incomplete")
        );
    }

    #[test]
    fn successful_usage_is_authoritative_but_ceiling_overrun_halts_commission() {
        let temp = PrivateTemp::new();
        let path = temp.0.join("commission.jsonl");
        let mut ledger = BudgetLedger::open(&path, "commission", true).unwrap();
        ledger
            .reserve(&key("run-a", "ok"), 100_000_000, 200_000_000)
            .unwrap();
        ledger.report(&key("run-a", "ok"), 0.025, true).unwrap();
        assert_eq!(ledger.state.liability_nanos, 25_000_000);
        ledger
            .reserve(&key("run-a", "over-ceiling"), 100_000_000, 200_000_000)
            .unwrap();
        ledger
            .report(&key("run-a", "over-ceiling"), 0.101, true)
            .unwrap();
        drop(ledger);
        let mut ledger = BudgetLedger::open(&path, "commission", false).unwrap();
        assert_eq!(ledger.state.liability_nanos, 126_000_000);
        assert!(ledger
            .reserve(&key("run-b", "blocked"), 1, COMMISSION_CAP_NANOS)
            .is_err());
    }

    #[test]
    fn per_hour_reservations_round_duration_and_never_adopt_ambiguous_units() {
        assert_eq!(reservation_nanos(0.1, 3600.0).unwrap(), 100_000_000);
        assert_eq!(reservation_nanos(1.0, 19.219).unwrap(), 5_555_556);
        for rate in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(reservation_nanos(rate, 20.0).is_err());
        }
    }

    fn clip() -> PreparedClip {
        PreparedClip {
            id: "dictation-01".into(),
            category: Category::Dictation,
            reference: "private reference alpha beta".into(),
            wav: Vec::new(),
            samples: Vec::new(),
            audio_secs: 2.0,
        }
    }

    #[test]
    fn failed_and_empty_calls_cannot_disappear_from_quality_or_improve_latency() {
        let mut reference_clip = clip();
        let success = scored_call(
            "vendor/model",
            &reference_clip,
            Outcome::text(reference_clip.reference.clone(), Some(0.01), 80),
        );
        reference_clip.id = "dictation-02".into();
        let failure = scored_call(
            "vendor/model",
            &reference_clip,
            Outcome::error("http_503".into()),
        );
        reference_clip.id = "dictation-03".into();
        let empty = scored_call(
            "vendor/model",
            &reference_clip,
            Outcome::text("...".into(), Some(0.01), 1),
        );
        let calls = vec![success, failure, empty];
        let model = summarize(
            "vendor/model",
            "Model",
            "openrouter",
            "vendor/model-version",
            &calls,
        );
        assert_eq!(model.successful_calls, 1);
        assert_eq!(model.failed_calls, 2);
        assert_eq!(model.wer, Some(2.0 / 3.0));
        assert_eq!(model.cer, Some(2.0 / 3.0));
        assert_eq!(model.median_latency_ms, Some(80.0));
        assert_eq!(model.p95_latency_ms, Some(80.0));
        assert_eq!(model.cost_usd, None);
    }

    #[test]
    fn missing_or_invalid_usage_never_becomes_zero_cost() {
        let reference_clip = clip();
        for body in [
            r#"{"text":"recognized words"}"#,
            r#"{"text":"recognized words","usage":null}"#,
            r#"{"text":"recognized words","usage":{"cost":null}}"#,
            r#"{"text":"recognized words","usage":{"cost":"0.01"}}"#,
            r#"{"text":"recognized words","usage":{"cost":-1}}"#,
        ] {
            let outcome = parse_response(ureq::Response::new(200, "OK", body).unwrap());
            let call = scored_call("vendor/model", &reference_clip, outcome);
            assert!(call.status == Status::Ok);
            assert_eq!(call.cost_usd, None);
            assert_eq!(
                summarize(
                    "vendor/model",
                    "Model",
                    "openrouter",
                    "vendor/model-version",
                    &[call]
                )
                .cost_usd,
                None
            );
        }
        let empty = scored_call(
            "vendor/model",
            &reference_clip,
            parse_response(
                ureq::Response::new(200, "OK", r#"{"text":"","usage":{"cost":0.01}}"#).unwrap(),
            ),
        );
        assert!(empty.status == Status::Empty);
        assert_eq!(empty.wer, 1.0);
        assert_eq!(empty.cer, 1.0);
        assert_eq!(empty.cost_usd, Some(0.01));
    }

    #[test]
    fn public_receipts_discard_reference_transcript_and_http_body() {
        let temp = PrivateTemp::new();
        let mut receipts = Receipts::create(&temp.0.join("run")).unwrap();
        let private_body = "provider private response /secret/source/path original-take-identifier";
        let error_response = ureq::Response::new(503, "Service Unavailable", private_body).unwrap();
        let clip = clip();
        let calls = [
            scored_call("vendor/model", &clip, parse_response(ureq::Response::new(200, "OK", r#"{"text":"private provider transcript","usage":{"cost":0.01,"seconds":2}}"#).unwrap())),
            scored_call("vendor/model", &clip, parse_response(error_response)),
        ];
        for (index, call) in calls.iter().enumerate() {
            record_call(&mut receipts, index, call).unwrap();
        }
        assert_eq!(
            fs::metadata(&receipts.dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(receipts.dir.join("run.log"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let log = fs::read_to_string(receipts.dir.join("run.log")).unwrap();
        let serialized = serde_json::to_string(&calls).unwrap();
        for private in [
            "private reference",
            "private provider transcript",
            "provider private response",
            "/secret/source/path",
            "original-take-identifier",
        ] {
            assert!(!log.contains(private));
            assert!(!serialized.contains(private));
        }
        assert!(log.contains("http_503"));
        assert_eq!(calls[0].cost_usd, Some(0.01));
        assert_eq!(calls[1].cost_usd, None);
    }
}
