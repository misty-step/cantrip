//! `cantrip e2e …`: headless end-to-end runs of the real controllers over
//! recorded audio, for verification. Only the microphone is replaced (audio is
//! injected from a WAV file, recorded as injected in the output); focus
//! capture, delivery, clipboard, OpenRouter and the Meeting archive are the
//! real ones.
//!
//! ```text
//! cantrip e2e dictate <wav> [--mode raw|clean|elevated] [--post on|off] [--key-env] [--linger <ms>]
//! cantrip e2e meeting <you.wav> <others.wav> --folder <dir> [--key-env]
//! cantrip e2e meeting-retry --folder <dir> [--key-env]
//! ```
//! `meeting-retry` runs the popover's Retry on every recoverable meeting in
//! the staging root named by `CANTRIP_STAGING` (publish-only for
//! `pending-archive`: zero speech calls) and reports each outcome.
//! `--key-env` reads the key from `OPENROUTER_API_KEY` instead of the OS
//! secret store (CI / sandbox hook; the key is never printed). `--linger`
//! keeps the process (and so its X11 clipboard ownership) alive after the
//! outcome and reports the HUD's clipboard claim again at the end.
//! `CANTRIP_STAGING` overrides the staging root (default: a fresh directory
//! under the user's data dir).

use crate::platform;
use cantrip_core::dictation::{Dictation, DictationDeps, TakeConfig};
use cantrip_core::meeting::{MeetingDeps, Meetings};
use cantrip_core::ports::{
    MeetingCapture, MeetingRecording, MeetingTracks, MicError, Recorder, Recording, SecretError,
    SecretStore,
};
use cantrip_core::speech::{Mode, OpenRouter};
use cantrip_core::staging::{unique_suffix, Staging};
use cantrip_core::vocabulary;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub fn run(args: &[String]) -> anyhow::Result<()> {
    match args.first().map(String::as_str) {
        Some("dictate") => dictate(&args[1..]),
        Some("meeting") => meeting(&args[1..]),
        Some("meeting-retry") => meeting_retry(&args[1..]),
        _ => anyhow::bail!(
            "usage: cantrip e2e dictate <wav> … | meeting <you.wav> <others.wav> --folder <dir> | meeting-retry --folder <dir>"
        ),
    }
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn staging() -> anyhow::Result<Staging> {
    let root = match std::env::var_os("CANTRIP_STAGING") {
        Some(p) => PathBuf::from(p),
        None => Staging::default_root().with_file_name(format!("v2-e2e-{}", unique_suffix())),
    };
    Ok(Staging::open(&root)?)
}

fn secrets(args: &[String], platform: &platform::Platform) -> Arc<dyn SecretStore> {
    if args.iter().any(|a| a == "--key-env") {
        Arc::new(EnvKey)
    } else {
        platform.secrets.clone()
    }
}

fn dictate(args: &[String]) -> anyhow::Result<()> {
    let wav = PathBuf::from(
        args.first()
            .ok_or_else(|| anyhow::anyhow!("missing <wav>"))?,
    );
    let mode = match flag(args, "--mode").unwrap_or("clean") {
        "raw" => Mode::Raw,
        "clean" => Mode::Clean,
        "elevated" => Mode::Elevated,
        m => anyhow::bail!("unknown mode {m}"),
    };
    let post = flag(args, "--post").unwrap_or("on") != "off";
    let (tx, _rx) = mpsc::channel();
    let (platform, _tray) = platform::detect(tx)?;
    let staging = staging()?;
    let dictation = Dictation::new(DictationDeps {
        recorder: Arc::new(FileRecorder(wav.clone())),
        desktop: platform.desktop.clone(),
        clipboard: platform.clipboard.clone(),
        secrets: secrets(args, &platform),
        local: platform.local.clone(),
        openrouter: OpenRouter::new(platform.transport.clone()),
        staging: staging.clone(),
        notify: Arc::new(|| {}),
    });
    let started = Instant::now();
    dictation.start(TakeConfig {
        mode,
        post_processing: post,
        vocabulary: vocabulary::seed(),
        microphone: None,
    });
    dictation.stop();
    let finished = dictation.wait_idle(Duration::from_secs(120));
    let hud = dictation.hud();
    let report = dictation.last_report();
    let linger: u64 = flag(args, "--linger")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut claims = Vec::new();
    let until = Instant::now() + Duration::from_millis(linger);
    while Instant::now() < until {
        std::thread::sleep(Duration::from_millis(100));
        let claim = dictation.hud().claim();
        if claims.last() != Some(&claim) {
            claims.push(claim);
            eprintln!("[e2e] claim now: {claim:?}");
        }
    }
    let staged: Vec<_> = staging
        .recoverable_dictations()
        .into_iter()
        .map(|d| json!({"id": d.id, "state": format!("{:?}", d.state)}))
        .collect();
    let out = json!({
        "session": platform.session,
        "audio": wav.display().to_string(),
        "audio_source": "injected WAV file (microphone replaced)",
        "mode": mode.label(),
        "post_processing": post,
        "finished": finished,
        "wall_ms": started.elapsed().as_millis() as u64,
        "hud": hud.text(),
        "hud_claim": hud.claim(),
        "hud_claim_changes_during_linger": claims,
        "outcome": report.as_ref().map(|r| r.outcome.clone()),
        "transcribe_ms": report.as_ref().and_then(|r| r.transcribe_ms),
        "rewrite_ms": report.as_ref().and_then(|r| r.rewrite_ms),
        "total_ms": report.as_ref().map(|r| r.total_ms),
        "text": dictation.last_text(),
        "staged_recovery_items": staged,
        "staging_root": staging.root().display().to_string(),
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn meeting(args: &[String]) -> anyhow::Result<()> {
    let you = PathBuf::from(
        args.first()
            .ok_or_else(|| anyhow::anyhow!("missing <you.wav>"))?,
    );
    let others = PathBuf::from(
        args.get(1)
            .ok_or_else(|| anyhow::anyhow!("missing <others.wav>"))?,
    );
    let folder =
        PathBuf::from(flag(args, "--folder").ok_or_else(|| anyhow::anyhow!("missing --folder"))?);
    let (tx, _rx) = mpsc::channel();
    let (platform, _tray) = platform::detect(tx)?;
    let staging = staging()?;
    let meetings = Meetings::new(MeetingDeps {
        capture: Arc::new(FileMeeting {
            you: you.clone(),
            others: others.clone(),
        }),
        secrets: secrets(args, &platform),
        openrouter: OpenRouter::new(platform.transport.clone()),
        staging: staging.clone(),
        notify: Arc::new(|| {}),
    });
    let started = Instant::now();
    meetings
        .start(None, vocabulary::seed(), folder.clone())
        .map_err(|m| anyhow::anyhow!(m))?;
    meetings.stop();
    let finished = meetings.wait_idle(Duration::from_secs(600));
    let out = json!({
        "session": platform.session,
        "audio_source": "injected WAV files for You and Others (capture replaced)",
        "finished": finished,
        "wall_ms": started.elapsed().as_millis() as u64,
        "finish": format!("{:?}", meetings.last_finish()),
        "published": meetings.last_published().map(|p| p.display().to_string()),
        "recoverable": meetings.recoverable().iter().map(|m| format!("{:?}", m.kind)).collect::<Vec<_>>(),
        "staging_root": staging.root().display().to_string(),
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn meeting_retry(args: &[String]) -> anyhow::Result<()> {
    let folder =
        PathBuf::from(flag(args, "--folder").ok_or_else(|| anyhow::anyhow!("missing --folder"))?);
    if std::env::var_os("CANTRIP_STAGING").is_none() {
        anyhow::bail!("meeting-retry needs CANTRIP_STAGING (the staging root to recover)");
    }
    let (tx, _rx) = mpsc::channel();
    let (platform, _tray) = platform::detect(tx)?;
    let staging = staging()?;
    let meetings = Meetings::new(MeetingDeps {
        capture: Arc::new(FileMeeting {
            you: PathBuf::new(),
            others: PathBuf::new(),
        }),
        secrets: secrets(args, &platform),
        openrouter: OpenRouter::new(platform.transport.clone()),
        staging: staging.clone(),
        notify: Arc::new(|| {}),
    });
    let mut runs = Vec::new();
    for item in meetings.recoverable() {
        meetings.retry(&item.id, folder.clone());
        let finished = meetings.wait_idle(Duration::from_secs(600));
        runs.push(json!({
            "kind_before": format!("{:?}", item.kind),
            "finished": finished,
            "finish": format!("{:?}", meetings.last_finish()),
            "published": meetings.last_published().map(|p| p.display().to_string()),
        }));
    }
    let out = json!({
        "session": platform.session,
        "retries": runs,
        "recoverable_after": meetings.recoverable().iter().map(|m| format!("{:?}", m.kind)).collect::<Vec<_>>(),
        "staging_root": staging.root().display().to_string(),
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

/// The OpenRouter key from the environment (verification hook only).
struct EnvKey;

impl SecretStore for EnvKey {
    fn get_key(&self) -> Result<Option<String>, SecretError> {
        Ok(std::env::var("OPENROUTER_API_KEY")
            .ok()
            .filter(|k| !k.is_empty()))
    }
    fn set_key(&self, _: &str) -> Result<(), SecretError> {
        Err(SecretError::Unavailable("read-only".into()))
    }
    fn delete_key(&self) -> Result<(), SecretError> {
        Err(SecretError::Unavailable("read-only".into()))
    }
}

/// A "microphone" that yields a recorded WAV file.
struct FileRecorder(PathBuf);

struct FileRecording {
    source: PathBuf,
    dest: PathBuf,
}

impl Recorder for FileRecorder {
    fn devices(&self) -> Vec<(String, String)> {
        vec![("file".into(), "Injected WAV".into())]
    }
    fn start(&self, _: Option<&str>, wav_path: &Path) -> Result<Box<dyn Recording>, MicError> {
        if !self.0.is_file() {
            return Err(MicError::Unavailable("no such WAV".into()));
        }
        Ok(Box::new(FileRecording {
            source: self.0.clone(),
            dest: wav_path.to_path_buf(),
        }))
    }
}

impl Recording for FileRecording {
    fn level(&self) -> f32 {
        0.0
    }
    fn stop(self: Box<Self>) -> Result<PathBuf, String> {
        std::fs::copy(&self.source, &self.dest).map_err(|e| e.to_string())?;
        Ok(self.dest)
    }
    fn cancel(self: Box<Self>) {
        let _ = std::fs::remove_file(&self.dest);
    }
}

/// A meeting "capture" that yields two recorded WAV files on one clock.
struct FileMeeting {
    you: PathBuf,
    others: PathBuf,
}

struct FileMeetingRecording {
    sources: (PathBuf, PathBuf),
    dests: (PathBuf, PathBuf),
    started: Instant,
}

impl MeetingCapture for FileMeeting {
    fn start(
        &self,
        _: Option<&str>,
        you_path: &Path,
        others_path: &Path,
    ) -> Result<Box<dyn MeetingRecording>, String> {
        Ok(Box::new(FileMeetingRecording {
            sources: (self.you.clone(), self.others.clone()),
            dests: (you_path.to_path_buf(), others_path.to_path_buf()),
            started: Instant::now(),
        }))
    }
}

impl MeetingRecording for FileMeetingRecording {
    fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
    fn stop(self: Box<Self>) -> Result<MeetingTracks, String> {
        std::fs::copy(&self.sources.0, &self.dests.0).map_err(|e| e.to_string())?;
        std::fs::copy(&self.sources.1, &self.dests.1).map_err(|e| e.to_string())?;
        Ok(MeetingTracks {
            you: self.dests.0,
            others: self.dests.1,
            you_offset_ms: 0,
            others_offset_ms: 0,
        })
    }
}
