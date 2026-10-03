//! Local scheduling and metrics-only review PRs. The speech ledger remains authoritative.

use std::fs::{self, DirBuilder, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};

const REPO: &str = "misty-step/cantrip";
const ORIGIN: &str = "https://github.com/misty-step/cantrip.git";
const SERVICE: &str = include_str!("../../contrib/cantrip-speech-eval.service");
const TIMER: &str = include_str!("../../contrib/cantrip-speech-eval.timer");

#[derive(Parser)]
#[command(name = "eval install-speech-schedule")]
struct InstallArgs {
    #[arg(long)]
    config: PathBuf,
    #[arg(long)]
    out_root: PathBuf,
    #[arg(long)]
    source_revision: String,
    #[arg(long)]
    publish_checkout: PathBuf,
}

#[derive(Parser)]
#[command(name = "eval scheduled-speech")]
struct RunArgs {
    #[arg(long)]
    schedule: Option<PathBuf>,
    #[arg(long, conflicts_with = "allow_paid")]
    dry_run: bool,
    /// Activating the installed service authorizes this paid invocation and metrics-only PR.
    #[arg(long, required_unless_present = "dry_run")]
    allow_paid: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Schedule {
    schema_version: u32,
    config: PathBuf,
    out_root: PathBuf,
    source_revision: String,
    publish_checkout: PathBuf,
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("schedule_home_missing"))
}

fn state_dir() -> Result<PathBuf> {
    Ok(std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or(home()?.join(".local/state"))
        .join("cantrip/speech-eval"))
}

fn private_dir(path: &Path) -> Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|_| anyhow!("schedule_directory_failed"))?;
    Ok(())
}

fn quiet(command: &mut Command, class: &str) -> Result<()> {
    let status = command
        .env_remove("OPENROUTER_API_KEY")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|_| anyhow!("{class}"))?;
    if !status.success() {
        bail!("{class}");
    }
    Ok(())
}

fn git(checkout: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(checkout);
    command
}

fn verify_origin(checkout: &Path) -> Result<()> {
    let output = git(checkout)
        .args(["remote", "get-url", "origin"])
        .env_remove("OPENROUTER_API_KEY")
        .output()
        .map_err(|_| anyhow!("schedule_origin_failed"))?;
    if !output.status.success() || output.stdout.strip_suffix(b"\n") != Some(ORIGIN.as_bytes()) {
        bail!("schedule_origin_mismatch");
    }
    Ok(())
}

/// Install only; never reload, enable, start, reset the ledger, or change a speech key.
pub(super) fn install(raw_args: &[String]) -> Result<()> {
    let args = InstallArgs::try_parse_from(
        std::iter::once("install-speech-schedule").chain(raw_args.iter().map(String::as_str)),
    )?;
    if !args.config.is_absolute()
        || !args.out_root.is_absolute()
        || !args.publish_checkout.is_absolute()
        || args.source_revision.is_empty()
        || !args
            .source_revision
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("schedule_install_paths_or_revision_invalid");
    }
    // Corpus review, installed models and durable budget are validated by the normal dry run.
    let schedule = Schedule {
        schema_version: 1,
        config: args.config,
        out_root: args.out_root,
        source_revision: args.source_revision,
        publish_checkout: args.publish_checkout,
    };
    if !schedule.publish_checkout.exists() {
        if let Some(parent) = schedule.publish_checkout.parent() {
            private_dir(parent)?;
        }
        quiet(
            Command::new("git")
                .args(["clone", "--depth", "1", ORIGIN])
                .arg(&schedule.publish_checkout),
            "schedule_clone_failed",
        )?;
    }
    verify_origin(&schedule.publish_checkout)?;
    let home = home()?;
    let bin = home.join(".local/bin");
    if !bin.join("pass-env").is_file() {
        bail!("schedule_pass_env_missing");
    }
    fs::create_dir_all(&bin).map_err(|_| anyhow!("schedule_binary_directory_failed"))?;
    let unit_dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or(home.join(".config"))
        .join("systemd/user");
    fs::create_dir_all(&unit_dir).map_err(|_| anyhow!("schedule_unit_directory_failed"))?;
    // Preserve another owner's unit or direct modifications. Drop-ins remain untouched.
    for (name, contents) in [
        ("cantrip-speech-eval.service", SERVICE),
        ("cantrip-speech-eval.timer", TIMER),
    ] {
        let target = unit_dir.join(name);
        match fs::read(&target) {
            Ok(existing) if existing != contents.as_bytes() => {
                bail!("schedule_unit_owner_conflict")
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o644)
                    .open(target)
                    .map_err(|_| anyhow!("schedule_unit_install_failed"))?;
                use std::io::Write;
                file.write_all(contents.as_bytes())
                    .map_err(|_| anyhow!("schedule_unit_install_failed"))?;
            }
            Err(_) => bail!("schedule_unit_inspection_failed"),
        }
    }
    let state = state_dir()?;
    private_dir(&state)?;
    private_dir(&schedule.out_root)?;
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(state.join("schedule.json"))
        .map_err(|_| anyhow!("schedule_config_install_failed"))?;
    serde_json::to_writer_pretty(file, &schedule)
        .map_err(|_| anyhow!("schedule_config_install_failed"))?;
    let installed = bin.join("cantrip-speech-eval");
    let staged = bin.join(format!(
        ".cantrip-speech-eval-install-{}",
        std::process::id()
    ));
    fs::copy(std::env::current_exe()?, &staged)
        .map_err(|_| anyhow!("schedule_binary_install_failed"))?;
    fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))?;
    fs::rename(staged, installed).map_err(|_| anyhow!("schedule_binary_install_failed"))?;
    println!("[STT] schedule_installed; activation_deferred; commission_ledger_unchanged");
    Ok(())
}

pub(super) fn run(raw_args: &[String]) -> Result<()> {
    let args = RunArgs::try_parse_from(
        std::iter::once("scheduled-speech").chain(raw_args.iter().map(String::as_str)),
    )?;
    let schedule_path = match args.schedule {
        Some(path) => path,
        None => state_dir()?.join("schedule.json"),
    };
    let schedule: Schedule = serde_json::from_reader(
        fs::File::open(schedule_path).map_err(|_| anyhow!("schedule_config_unavailable"))?,
    )
    .map_err(|_| anyhow!("schedule_config_invalid"))?;
    if schedule.schema_version != 1 {
        bail!("schedule_schema_unsupported");
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    let run_id = format!("weekly-{now}-{}", std::process::id());
    let out = schedule.out_root.join(&run_id);
    let branch = format!("speech-eval/{run_id}");
    if !args.dry_run {
        verify_origin(&schedule.publish_checkout)?;
        quiet(
            git(&schedule.publish_checkout).args(["fetch", "origin", "master"]),
            "schedule_fetch_failed",
        )?;
        quiet(
            git(&schedule.publish_checkout).args(["switch", "--create", &branch, "origin/master"]),
            "schedule_branch_failed",
        )?;
    }
    let mut child = Command::new(std::env::current_exe()?);
    child
        .arg("living-speech")
        .arg("--config")
        .arg(&schedule.config)
        .arg("--out")
        .arg(&out)
        .arg("--run-id")
        .arg(&run_id)
        .arg("--source-revision")
        .arg(&schedule.source_revision)
        .arg(if args.dry_run {
            "--dry-run"
        } else {
            "--allow-paid"
        })
        .stdin(Stdio::null());
    if !args.dry_run {
        let key = std::env::var("OPENROUTER_API_KEY")
            .map_err(|_| anyhow!("schedule_credential_missing"))?;
        // pass-env preserves a terminal LF. Remove exactly one transport newline only in
        // the child environment; the living-speech credential validator is unchanged.
        child.env("OPENROUTER_API_KEY", key.strip_suffix('\n').unwrap_or(&key));
    }
    let status = child
        .status()
        .map_err(|_| anyhow!("schedule_runner_failed"))?;
    // A partial run is still evidence. Publish it when a complete sanitized receipt exists.
    if !args.dry_run && out.join("results.json").is_file() {
        let public = schedule.publish_checkout.join("site/public/evals");
        fs::create_dir_all(public.join("runs"))?;
        let result_path = format!("site/public/evals/runs/{run_id}.json");
        let log_path = format!("site/public/evals/runs/{run_id}.log");
        fs::copy(
            out.join("results.json"),
            schedule.publish_checkout.join(&result_path),
        )?;
        fs::copy(
            out.join("run.log"),
            schedule.publish_checkout.join(&log_path),
        )?;
        fs::copy(out.join("results.json"), public.join("latest.json"))?;
        quiet(
            git(&schedule.publish_checkout).args([
                "add",
                "--",
                &result_path,
                &log_path,
                "site/public/evals/latest.json",
            ]),
            "schedule_stage_failed",
        )?;
        quiet(
            git(&schedule.publish_checkout).args([
                "commit",
                "--only",
                "-m",
                &format!("eval: publish {run_id}"),
                "--",
                &result_path,
                &log_path,
                "site/public/evals/latest.json",
            ]),
            "schedule_commit_failed",
        )?;
        quiet(
            git(&schedule.publish_checkout).args(["push", "--set-upstream", "origin", &branch]),
            "schedule_push_failed",
        )?;
        quiet(Command::new("gh").args(["pr", "create", "--repo", REPO, "--base", "master", "--head", &branch, "--title", &format!("Speech evaluation: {run_id}"), "--body", "Scheduled private-corpus run. Numeric results and sanitized logs only; audio, references and provider transcripts remain local. Review completeness, budget and scoring caveats before merging. Merge uses the normal site CI/deploy route."]), "schedule_pr_failed")?;
        println!("[STT] schedule_review_pr_created; run_id={run_id}");
    }
    println!(
        "[STT] schedule_run_finished; run_id={run_id}; dry_run={}; success={}",
        args.dry_run,
        status.success()
    );
    if !status.success() {
        bail!("schedule_run_incomplete; retained_receipts");
    }
    Ok(())
}
