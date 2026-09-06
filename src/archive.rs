use crate::paths;
use anyhow::{ensure, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) const SCHEMA_VERSION: u32 = 3;
static SESSION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) struct Entry<'a> {
    pub take_id: Option<&'a str>,
    pub source: &'static str,
    pub raw_transcript: &'a str,
    pub postprocessed_transcript: Option<&'a str>,
    pub audio_duration_ms: Option<u64>,
    pub pipeline_elapsed_ms: u64,
    pub stt_model: &'a str,
    pub stt_remote: bool,
    pub stt_elapsed_ms: u64,
    pub stt_api_cost_usd: Option<f64>,
    pub partial: bool,
    pub cancelled: bool,
    pub postproc_status: &'static str,
    pub postproc_model: Option<&'a str>,
    pub postproc_elapsed_ms: Option<u64>,
    pub postproc_passes: Option<u8>,
    pub postproc_prompt_version: Option<u32>,
    pub postproc_instructions: Option<&'a str>,
    pub postproc_prompt_tokens: Option<u64>,
    pub postproc_completion_tokens: Option<u64>,
    pub postproc_total_tokens: Option<u64>,
    pub postproc_reasoning_tokens: Option<u64>,
    pub postproc_cached_tokens: Option<u64>,
    pub postproc_reported_cost_usd: Option<f64>,
    pub postproc_usage_requests: Option<u8>,
    pub postproc_usage_responses: Option<u8>,
}

#[derive(Serialize)]
struct Record<'a> {
    schema_version: u32,
    session_id: &'a str,
    completed_at_unix_ms: u64,
    source: &'static str,
    audio: AudioRecord,
    pipeline: PipelineRecord,
    stt: SttRecord<'a>,
    postproc: PostprocRecord<'a>,
    raw_transcript: &'a str,
    postprocessed_transcript: Option<&'a str>,
}

#[derive(Serialize)]
struct AudioRecord {
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,
}

#[derive(Serialize)]
struct PipelineRecord {
    elapsed_ms: u64,
}

#[derive(Serialize)]
struct SttRecord<'a> {
    model: &'a str,
    backend: &'static str,
    elapsed_ms: u64,
    partial: bool,
    cancelled: bool,
    /// API billing only. Local inference is `0`; cloud backends are omitted
    /// until their compatible response reports an authoritative charge.
    #[serde(skip_serializing_if = "Option::is_none")]
    api_cost_usd: Option<f64>,
}

#[derive(Serialize)]
struct PostprocRecord<'a> {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    elapsed_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    passes: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_version: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    custom_instructions: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<PostprocUsageRecord>,
}

#[derive(Serialize)]
struct PostprocUsageRecord {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
    reasoning_tokens: u64,
    cached_tokens: u64,
    requests: u8,
    responses_with_usage: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    reported_cost_usd: Option<f64>,
}

pub(crate) fn save(entry: Entry<'_>) -> Result<PathBuf> {
    save_to(&paths::transcript_history_dir()?, entry)
}

fn save_to(directory: &Path, entry: Entry<'_>) -> Result<PathBuf> {
    let store = Store::open(directory)?;
    let generated_id;
    let session_id = match entry.take_id {
        Some(id) => id,
        None => {
            generated_id = new_id();
            &generated_id
        }
    };
    validate_id(session_id)?;
    let record = Record {
        schema_version: SCHEMA_VERSION,
        session_id,
        completed_at_unix_ms: now_ms(),
        source: entry.source,
        audio: AudioRecord {
            duration_ms: entry.audio_duration_ms,
        },
        pipeline: PipelineRecord {
            elapsed_ms: entry.pipeline_elapsed_ms,
        },
        stt: SttRecord {
            model: entry.stt_model,
            backend: if entry.stt_remote { "cloud" } else { "local" },
            elapsed_ms: entry.stt_elapsed_ms,
            partial: entry.partial,
            cancelled: entry.cancelled,
            api_cost_usd: entry.stt_api_cost_usd,
        },
        postproc: PostprocRecord {
            status: entry.postproc_status,
            model: entry.postproc_model,
            elapsed_ms: entry.postproc_elapsed_ms,
            passes: entry.postproc_passes,
            prompt_version: entry.postproc_prompt_version,
            custom_instructions: entry.postproc_instructions,
            usage: entry
                .postproc_prompt_tokens
                .map(|prompt_tokens| PostprocUsageRecord {
                    prompt_tokens,
                    completion_tokens: entry.postproc_completion_tokens.unwrap_or_default(),
                    total_tokens: entry.postproc_total_tokens.unwrap_or_default(),
                    reasoning_tokens: entry.postproc_reasoning_tokens.unwrap_or_default(),
                    cached_tokens: entry.postproc_cached_tokens.unwrap_or_default(),
                    requests: entry.postproc_usage_requests.unwrap_or_default(),
                    responses_with_usage: entry.postproc_usage_responses.unwrap_or_default(),
                    reported_cost_usd: entry.postproc_reported_cost_usd,
                }),
        },
        raw_transcript: entry.raw_transcript,
        postprocessed_transcript: entry.postprocessed_transcript,
    };
    let incoming = serde_json::to_value(record).context("serializing transcript history")?;
    let complete = !entry.partial && !entry.cancelled && final_text(&incoming).is_some();
    let existing = store.record_for_update(session_id, complete)?;
    // A retry cannot replace any usable prior text until its complete result
    // is ready for one atomic publication. Keep bounded provenance, not a
    // recursively nested or ever-growing second transcript history.
    let preserve = existing
        .as_ref()
        .is_some_and(|old| final_text(old).is_some() && !complete);
    let mut recovery = existing
        .as_ref()
        .and_then(|old| old.get("recovery"))
        .filter(|value| value.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let created_at = existing
        .as_ref()
        .map(created_at_ms)
        .filter(|timestamp| *timestamp > 0)
        .unwrap_or_else(now_ms);
    recovery["created_at_unix_ms"] = json!(created_at);
    recovery["retry_incomplete"] = json!(!complete);
    recovery["unresolved"] = json!(!complete || recovery["unresolved"].as_bool().unwrap_or(false));
    recovery["latest_attempt"] = json!({
        "source": entry.source,
        "completed_at_unix_ms": now_ms(),
        "partial": entry.partial,
        "cancelled": entry.cancelled,
        "complete": complete,
    });
    if recovery.get("origin").is_none() {
        let origin = existing
            .as_ref()
            .filter(|old| {
                old.get("stt").is_some()
                    || final_text(old).is_some()
                    || old.get("source").and_then(Value::as_str) != Some("recovery")
            })
            .unwrap_or(&incoming);
        recovery["origin"] = json!({
            "source": origin.get("source"),
            "completed_at_unix_ms": origin.get("completed_at_unix_ms"),
            "stt_model": origin.pointer("/stt/model"),
            "stt_backend": origin.pointer("/stt/backend"),
        });
    }
    let mut result = if preserve {
        existing.context("missing preserved transcript")?
    } else {
        recovery["partial"] = json!(entry.partial || entry.cancelled);
        incoming
    };
    result["schema_version"] = json!(SCHEMA_VERSION);
    result["recovery"] = recovery;
    store.write_record(session_id, &result)?;
    Ok(directory.join(format!("{session_id}.json")))
}

pub(crate) fn new_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = SESSION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:020}-{}-{sequence}", std::process::id())
}

pub(crate) fn now_ms() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    u64::try_from(millis).unwrap_or(u64::MAX)
}

pub(crate) fn validate_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "invalid recording ID"
    );
    Ok(())
}

pub(crate) fn final_text(record: &Value) -> Option<&str> {
    record
        .get("postprocessed_transcript")
        .and_then(Value::as_str)
        .or_else(|| record.get("raw_transcript").and_then(Value::as_str))
        .filter(|text| !text.trim().is_empty())
}

pub(crate) fn created_at_ms(record: &Value) -> u64 {
    record
        .pointer("/recovery/created_at_unix_ms")
        .or_else(|| record.get("completed_at_unix_ms"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

pub(crate) fn partial(record: &Value) -> bool {
    record
        .pointer("/recovery/partial")
        .or_else(|| record.pointer("/stt/partial"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// One owner-private history directory. All mutations and reads hold its
/// advisory lock; fd-relative operations cannot be redirected by replacing
/// the directory path while a write is in flight.
pub(crate) struct Store {
    directory: File,
}

impl Store {
    pub(crate) fn open(directory: &Path) -> Result<Self> {
        check_ancestors(directory)?;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder
            .create(directory)
            .with_context(|| format!("creating private history {}", directory.display()))?;
        check_ancestors(directory)?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(directory)
            .context("opening private history directory")?;
        let metadata = file.metadata().context("checking history directory")?;
        ensure!(
            metadata.is_dir() && metadata.uid() == unsafe { libc::getuid() },
            "history directory is not owned by the current user"
        );
        file.set_permissions(fs::Permissions::from_mode(0o700))
            .context("setting history directory permissions")?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error()).context("locking transcript history");
        }
        file.sync_all().context("syncing history directory")?;
        // An earlier failed mkdir/fsync can leave apparently existing but
        // non-durable ancestors. Re-establish the entire directory chain too.
        for parent in directory
            .ancestors()
            .skip(1)
            .filter(|path| !path.as_os_str().is_empty())
        {
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC)
                .open(parent)
                .and_then(|parent| parent.sync_all())
                .context("syncing history parent directory")?;
        }
        Ok(Self { directory: file })
    }

    pub(crate) fn names(&self) -> Result<Vec<String>> {
        let path = format!("/proc/self/fd/{}", self.directory.as_raw_fd());
        fs::read_dir(path)
            .context("listing transcript history")?
            .filter_map(|entry| match entry {
                Ok(entry) => entry.file_name().into_string().ok().map(Ok),
                Err(error) => Some(Err(
                    anyhow::Error::new(error).context("reading history entry")
                )),
            })
            .collect()
    }

    pub(crate) fn open_file(&self, name: &str) -> Result<Option<File>> {
        let name = file_name(name)?;
        let fd = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(error).context("opening private history artifact");
        }
        // SAFETY: openat returned a new descriptor owned by this File.
        let file = unsafe { File::from_raw_fd(fd) };
        check_file(&file, true)?;
        Ok(Some(file))
    }

    pub(crate) fn read_record(&self, id: &str) -> Result<Option<Value>> {
        self.record_for_update(id, false)
    }

    pub(crate) fn record_for_update(&self, id: &str, complete: bool) -> Result<Option<Value>> {
        validate_id(id)?;
        let Some(mut file) = self.open_file(&format!("{id}.json"))? else {
            return Ok(None);
        };
        match serde_json::from_reader::<_, Value>(&mut file) {
            Ok(record) if record.is_object() => {
                if let Some(record_id) = record.get("session_id").and_then(Value::as_str) {
                    ensure!(
                        record_id == id,
                        "transcript history ID does not match its filename"
                    );
                    return Ok(Some(record));
                }
            }
            Err(error) if error.is_io() => {
                return Err(error).context("reading transcript history");
            }
            _ => {}
        }
        ensure!(complete, "invalid transcript history JSON");
        // Only a complete replacement may repair malformed metadata. The
        // original bytes remain a private artifact, even if publication of
        // the replacement fails. Content addressing avoids duplicate copies
        // when the same interrupted repair is attempted again.
        let backup = self.preserve_corrupt_record(id, &mut file)?;
        Ok(Some(json!({
            "schema_version": SCHEMA_VERSION,
            "session_id": id,
            "source": "recovery",
            "recovery": {
                "created_at_unix_ms": now_ms(),
                "unresolved": true,
                "corrupt_record": backup,
            },
        })))
    }

    fn preserve_corrupt_record(&self, id: &str, file: &mut File) -> Result<String> {
        let expected = checksum(file)?;
        let name = format!("{id}.json.corrupt-{expected:x}");
        if let Some(mut existing) = self.open_file(&name)? {
            ensure!(
                checksum(&mut existing)? == expected,
                "corrupt-history backup has changed"
            );
            existing
                .sync_all()
                .context("syncing corrupt-history backup")?;
            self.sync()?;
            return Ok(name);
        }
        file.seek(SeekFrom::Start(0))
            .context("rewinding corrupt history")?;
        self.publish(&name, |output| {
            let mut hasher = Sha256::new();
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let length = file.read(&mut buffer).context("reading corrupt history")?;
                if length == 0 {
                    break;
                }
                output
                    .write_all(&buffer[..length])
                    .context("preserving corrupt history")?;
                hasher.update(&buffer[..length]);
            }
            ensure!(
                hasher.finalize() == expected,
                "corrupt history changed while preserving it"
            );
            Ok(())
        })?;
        Ok(name)
    }

    pub(crate) fn write_record(&self, id: &str, record: &Value) -> Result<()> {
        validate_id(id)?;
        self.publish(&format!("{id}.json"), |file| {
            serde_json::to_writer_pretty(&mut *file, record)
                .context("serializing transcript history")?;
            file.write_all(b"\n")
                .context("writing transcript history")?;
            Ok(())
        })
    }

    pub(crate) fn publish(
        &self,
        name: &str,
        write: impl FnOnce(&mut File) -> Result<()>,
    ) -> Result<()> {
        // Reject existing symlinks, hardlinks and non-private files. Rename
        // replaces the directory entry, never writes through the target.
        self.open_file(name)?;
        let destination = file_name(name)?;
        let temporary = file_name(&format!(".{}.tmp", new_id()))?;
        let fd = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                temporary.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("creating history temporary file");
        }
        // SAFETY: openat returned a new descriptor owned by this File.
        let mut file = unsafe { File::from_raw_fd(fd) };
        let result = (|| {
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .context("setting artifact permissions")?;
            write(&mut file)?;
            file.sync_all().context("syncing history artifact")?;
            if unsafe {
                libc::renameat(
                    self.directory.as_raw_fd(),
                    temporary.as_ptr(),
                    self.directory.as_raw_fd(),
                    destination.as_ptr(),
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error()).context("publishing history artifact");
            }
            self.sync()
        })();
        if result.is_err() {
            // After a successful rename the temp is already absent; the
            // published artifact is deliberately left discoverable.
            unsafe {
                libc::unlinkat(self.directory.as_raw_fd(), temporary.as_ptr(), 0);
            }
        }
        result
    }

    pub(crate) fn remove(&self, name: &str) -> Result<()> {
        if self.open_file(name)?.is_none() {
            return Ok(());
        }
        let name = file_name(name)?;
        if unsafe { libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            return Err(std::io::Error::last_os_error()).context("removing history artifact");
        }
        self.sync()
    }

    pub(crate) fn sync(&self) -> Result<()> {
        self.directory
            .sync_all()
            .context("syncing transcript history")
    }
}

fn checksum(file: &mut File) -> Result<sha2::digest::Output<Sha256>> {
    file.seek(SeekFrom::Start(0))
        .context("rewinding history artifact")?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let length = file
            .read(&mut buffer)
            .context("reading history artifact checksum")?;
        if length == 0 {
            break;
        }
        hasher.update(&buffer[..length]);
    }
    Ok(hasher.finalize())
}

fn file_name(name: &str) -> Result<CString> {
    ensure!(
        !name.is_empty() && name != "." && name != ".." && !name.contains('/'),
        "invalid history artifact name"
    );
    CString::new(name).context("invalid history artifact name")
}

pub(crate) fn check_ancestors(path: &Path) -> Result<()> {
    for ancestor in path.ancestors().filter(|path| !path.as_os_str().is_empty()) {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) => ensure!(
                !metadata.file_type().is_symlink(),
                "private artifact path is a symlink"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("checking private artifact path"),
        }
    }
    Ok(())
}

pub(crate) fn open_source(path: &Path) -> Result<File> {
    check_ancestors(path)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .context("opening recovery source")?;
    check_file(&file, false)?;
    Ok(file)
}

fn check_file(file: &File, private: bool) -> Result<()> {
    let metadata = file.metadata().context("checking history artifact")?;
    ensure!(
        metadata.is_file() && metadata.uid() == unsafe { libc::getuid() } && metadata.nlink() == 1,
        "history artifact is not a regular file owned exclusively by the current user"
    );
    ensure!(
        !private || metadata.permissions().mode() & 0o077 == 0,
        "history artifact is not owner-private"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::os::unix::fs::symlink;

    fn test_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "cantrip-archive-{name}-{}-{}",
            std::process::id(),
            SESSION_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn entry<'a>(raw: &'a str, cleaned: Option<&'a str>) -> Entry<'a> {
        Entry {
            take_id: None,
            source: "dictation",
            raw_transcript: raw,
            postprocessed_transcript: cleaned,
            audio_duration_ms: Some(1_500),
            pipeline_elapsed_ms: 49,
            stt_model: "parakeet-test",
            stt_remote: false,
            stt_elapsed_ms: 42,
            stt_api_cost_usd: Some(0.0),
            partial: false,
            cancelled: false,
            postproc_status: if cleaned.is_some() { "applied" } else { "off" },
            postproc_model: cleaned.map(|_| "cleaner-test"),
            postproc_elapsed_ms: cleaned.map(|_| 7),
            postproc_passes: cleaned.map(|_| 1),
            postproc_prompt_version: cleaned.map(|_| 1),
            postproc_instructions: None,
            postproc_prompt_tokens: cleaned.map(|_| 100),
            postproc_completion_tokens: cleaned.map(|_| 20),
            postproc_total_tokens: cleaned.map(|_| 120),
            postproc_reasoning_tokens: cleaned.map(|_| 5),
            postproc_cached_tokens: cleaned.map(|_| 10),
            postproc_reported_cost_usd: cleaned.map(|_| 0.001),
            postproc_usage_requests: cleaned.map(|_| 1),
            postproc_usage_responses: cleaned.map(|_| 1),
        }
    }

    #[test]
    fn saves_raw_and_cleaned_text_atomically_with_owner_only_permissions() {
        let directory = test_root("save").join("transcripts");
        let path = save_to(&directory, entry("raw words", Some("Raw words."))).unwrap();

        let record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(record["source"], "dictation");
        assert_eq!(record["raw_transcript"], "raw words");
        assert_eq!(record["postprocessed_transcript"], "Raw words.");
        assert_eq!(record["stt"]["model"], "parakeet-test");
        assert_eq!(record["postproc"]["status"], "applied");
        assert_eq!(record["audio"]["duration_ms"], 1_500);
        assert_eq!(record["pipeline"]["elapsed_ms"], 49);
        assert_eq!(record["stt"]["api_cost_usd"], 0.0);
        assert_eq!(record["postproc"]["usage"]["total_tokens"], 120);
        assert_eq!(record["postproc"]["usage"]["reported_cost_usd"], 0.001);
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(fs::read_dir(&directory).unwrap().all(|item| !item
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")));

        fs::remove_dir_all(directory.parent().unwrap()).unwrap();
    }

    #[test]
    fn rejects_a_symlink_archive_directory() {
        let root = test_root("symlink");
        let target = root.join("target");
        let directory = root.join("transcripts");
        fs::create_dir_all(&target).unwrap();
        symlink(&target, &directory).unwrap();

        assert!(save_to(&directory, entry("private words", None)).is_err());
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retry_preserves_prior_text_and_metadata_until_a_complete_result_is_ready() {
        let root = test_root("retry");
        let directory = root.join("transcripts");
        let id = new_id();
        let mut first = entry("first raw", Some("First complete."));
        first.take_id = Some(&id);
        let path = save_to(&directory, first).unwrap();
        let created_at;
        {
            let store = Store::open(&directory).unwrap();
            let mut original = store.read_record(&id).unwrap().unwrap();
            created_at = created_at_ms(&original);
            original["recovery"]["unresolved"] = json!(true);
            original["recovery"]["operator_metadata"] = json!("preserve");
            store.write_record(&id, &original).unwrap();
        }
        let mut retry = entry("a later incomplete attempt", None);
        retry.take_id = Some(&id);
        retry.source = "recover";
        retry.partial = true;
        assert_eq!(save_to(&directory, retry).unwrap(), path);
        {
            let store = Store::open(&directory).unwrap();
            let preserved = store.read_record(&id).unwrap().unwrap();
            assert_eq!(final_text(&preserved), Some("First complete."));
            assert_eq!(preserved["postproc"]["usage"]["reported_cost_usd"], 0.001);
            assert_eq!(preserved["recovery"]["operator_metadata"], "preserve");
            assert_eq!(preserved["recovery"]["retry_incomplete"], true);
        }
        let mut cancelled = entry("never delivered", None);
        cancelled.take_id = Some(&id);
        cancelled.cancelled = true;
        save_to(&directory, cancelled).unwrap();
        {
            let store = Store::open(&directory).unwrap();
            assert_eq!(
                final_text(&store.read_record(&id).unwrap().unwrap()),
                Some("First complete.")
            );
        }
        let mut complete = entry("recovered raw", Some("Recovered complete."));
        complete.take_id = Some(&id);
        complete.source = "recover";
        save_to(&directory, complete).unwrap();
        {
            let store = Store::open(&directory).unwrap();
            let replaced = store.read_record(&id).unwrap().unwrap();
            assert_eq!(final_text(&replaced), Some("Recovered complete."));
            assert_eq!(created_at_ms(&replaced), created_at);
            assert_eq!(replaced["recovery"]["unresolved"], true);
            assert_eq!(replaced["recovery"]["retry_incomplete"], false);
            assert_eq!(replaced["recovery"]["origin"]["source"], "dictation");
            assert_eq!(replaced["recovery"]["operator_metadata"], "preserve");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_atomic_replacement_keeps_the_previous_transcript_readable() {
        let root = test_root("atomic");
        let store = Store::open(&root).unwrap();
        let id = new_id();
        store
            .write_record(
                &id,
                &json!({
                    "session_id": id,
                    "raw_transcript": "previous durable evidence",
                }),
            )
            .unwrap();
        let result = store.publish(&format!("{id}.json"), |file| {
            file.write_all(b"incomplete replacement")?;
            anyhow::bail!("simulated write failure")
        });
        assert!(result.is_err());
        assert_eq!(
            final_text(&store.read_record(&id).unwrap().unwrap()),
            Some("previous durable evidence")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn complete_archive_repairs_corruption_without_erasing_its_original_bytes() {
        let root = test_root("repair");
        let id = new_id();
        let corrupt = b"{ malformed private history";
        {
            let store = Store::open(&root).unwrap();
            store
                .publish(&format!("{id}.json"), |file| {
                    file.write_all(corrupt)?;
                    Ok(())
                })
                .unwrap();
        }
        let mut interrupted = entry("returned but cancelled", None);
        interrupted.take_id = Some(&id);
        interrupted.cancelled = true;
        assert!(save_to(&root, interrupted).is_err());
        assert_eq!(fs::read(root.join(format!("{id}.json"))).unwrap(), corrupt);
        let mut complete = entry("new complete words", Some("New complete words."));
        complete.take_id = Some(&id);
        complete.source = "recover";
        save_to(&root, complete).unwrap();
        {
            let store = Store::open(&root).unwrap();
            let record = store.read_record(&id).unwrap().unwrap();
            assert_eq!(final_text(&record), Some("New complete words."));
            assert_eq!(record["source"], "recover");
            assert_eq!(record["stt"]["model"], "parakeet-test");
            let backup = record["recovery"]["corrupt_record"].as_str().unwrap();
            assert_eq!(fs::read(root.join(backup)).unwrap(), corrupt);
            assert_eq!(
                fs::metadata(root.join(backup))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(root).unwrap();
    }
}
