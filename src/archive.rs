use crate::paths;
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const SCHEMA_VERSION: u32 = 1;
static SESSION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) struct Entry<'a> {
    pub source: &'static str,
    pub raw_transcript: &'a str,
    pub postprocessed_transcript: Option<&'a str>,
    pub stt_model: &'a str,
    pub stt_remote: bool,
    pub stt_elapsed_ms: u64,
    pub partial: bool,
    pub postproc_status: &'static str,
    pub postproc_model: Option<&'a str>,
    pub postproc_elapsed_ms: Option<u64>,
    pub postproc_passes: Option<u8>,
    pub postproc_prompt_version: Option<u32>,
    pub postproc_instructions: Option<&'a str>,
}

#[derive(Serialize)]
struct Record<'a> {
    schema_version: u32,
    session_id: &'a str,
    created_at_unix_ms: u64,
    source: &'static str,
    stt: SttRecord<'a>,
    postproc: PostprocRecord<'a>,
    raw_transcript: &'a str,
    postprocessed_transcript: Option<&'a str>,
}

#[derive(Serialize)]
struct SttRecord<'a> {
    model: &'a str,
    backend: &'static str,
    elapsed_ms: u64,
    partial: bool,
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
}

pub(crate) fn save(entry: Entry<'_>) -> Result<PathBuf> {
    save_to(&paths::transcript_history_dir()?, entry)
}

fn save_to(directory: &Path, entry: Entry<'_>) -> Result<PathBuf> {
    ensure_private_directory(directory)?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let created_at_unix_ms = u64::try_from(now.as_millis()).unwrap_or(u64::MAX);
    let sequence = SESSION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let session_id = format!("{:020}-{}-{sequence}", now.as_nanos(), std::process::id());
    let final_path = directory.join(format!("{session_id}.json"));
    let temp_path = directory.join(format!(".{session_id}.tmp"));

    let record = Record {
        schema_version: SCHEMA_VERSION,
        session_id: &session_id,
        created_at_unix_ms,
        source: entry.source,
        stt: SttRecord {
            model: entry.stt_model,
            backend: if entry.stt_remote { "cloud" } else { "local" },
            elapsed_ms: entry.stt_elapsed_ms,
            partial: entry.partial,
        },
        postproc: PostprocRecord {
            status: entry.postproc_status,
            model: entry.postproc_model,
            elapsed_ms: entry.postproc_elapsed_ms,
            passes: entry.postproc_passes,
            prompt_version: entry.postproc_prompt_version,
            custom_instructions: entry.postproc_instructions,
        },
        raw_transcript: entry.raw_transcript,
        postprocessed_transcript: entry.postprocessed_transcript,
    };
    let mut bytes = serde_json::to_vec_pretty(&record).context("serializing transcript history")?;
    bytes.push(b'\n');

    let write_result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temp_path)
            .with_context(|| format!("creating {}", temp_path.display()))?;
        fs::set_permissions(&temp_path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting permissions on {}", temp_path.display()))?;
        file.write_all(&bytes)
            .with_context(|| format!("writing {}", temp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing {}", temp_path.display()))?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }

    if let Err(error) = fs::rename(&temp_path, &final_path) {
        let _ = fs::remove_file(&temp_path);
        return Err(error).with_context(|| {
            format!(
                "publishing transcript history {} -> {}",
                temp_path.display(),
                final_path.display()
            )
        });
    }
    Ok(final_path)
}

fn ensure_private_directory(directory: &Path) -> Result<()> {
    fs::create_dir_all(directory)
        .with_context(|| format!("creating transcript history {}", directory.display()))?;
    let metadata = fs::symlink_metadata(directory)
        .with_context(|| format!("checking transcript history {}", directory.display()))?;
    if metadata.file_type().is_symlink() {
        bail!("transcript history {} is a symlink", directory.display());
    }
    if !metadata.is_dir() {
        bail!(
            "transcript history {} is not a directory",
            directory.display()
        );
    }
    if metadata.uid() != unsafe { libc::getuid() } {
        bail!(
            "transcript history {} is not owned by the current user",
            directory.display()
        );
    }
    if metadata.permissions().mode() & 0o777 != 0o700 {
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting permissions on {}", directory.display()))?;
    }
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
            source: "dictation",
            raw_transcript: raw,
            postprocessed_transcript: cleaned,
            stt_model: "parakeet-test",
            stt_remote: false,
            stt_elapsed_ms: 42,
            partial: false,
            postproc_status: if cleaned.is_some() { "applied" } else { "off" },
            postproc_model: cleaned.map(|_| "cleaner-test"),
            postproc_elapsed_ms: cleaned.map(|_| 7),
            postproc_passes: cleaned.map(|_| 1),
            postproc_prompt_version: cleaned.map(|_| 1),
            postproc_instructions: None,
        }
    }

    #[test]
    fn saves_raw_and_cleaned_text_atomically_with_owner_only_permissions() {
        let directory = test_root("save").join("transcripts");
        let path = save_to(&directory, entry("raw words", Some("Raw words."))).unwrap();

        let record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(record["schema_version"], 1);
        assert_eq!(record["source"], "dictation");
        assert_eq!(record["raw_transcript"], "raw words");
        assert_eq!(record["postprocessed_transcript"], "Raw words.");
        assert_eq!(record["stt"]["model"], "parakeet-test");
        assert_eq!(record["postproc"]["status"], "applied");
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

        let error = save_to(&directory, entry("private words", None)).unwrap_err();
        assert!(error.to_string().contains("is a symlink"));
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);

        fs::remove_dir_all(root).unwrap();
    }
}
