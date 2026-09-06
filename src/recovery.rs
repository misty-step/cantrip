//! Per-take recovery artifacts live in the existing private transcript history.
//! Availability is measured from trusted files, never from a persisted promise.

use crate::archive::{self, Store};
use crate::{paths, stt};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs::{File, Metadata};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Take {
    pub id: String,
    pub created_at_unix_ms: u64,
    pub duration_ms: Option<u64>,
    pub text_available: bool,
    pub audio_available: bool,
    pub partial: bool,
    pub unresolved: bool,
}

#[derive(Debug)]
pub(crate) struct AudioMismatch;

impl std::fmt::Display for AudioMismatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("recording ID already has different retained audio")
    }
}

impl std::error::Error for AudioMismatch {}

pub fn new_id() -> String {
    archive::new_id()
}

pub fn list() -> Result<Vec<Take>> {
    list_in(&Store::open(&paths::transcript_history_dir()?)?)
}

pub fn get(id: &str) -> Result<Take> {
    get_in(&Store::open(&paths::transcript_history_dir()?)?, id)
}

/// Re-establish file and directory durability after an uncertain save.
/// Unlike get(), a successful result is evidence that the returned available
/// artifacts have been synced. Callers must still require the artifact they
/// need; a durably retained transcript is not a substitute for missing audio.
pub fn confirm(id: &str) -> Result<Take> {
    confirm_in(&Store::open(&paths::transcript_history_dir()?)?, id)
}

/// Confirm both source identity and durability before discarding runtime audio.
pub(crate) fn confirm_audio(id: &str, source: &Path) -> Result<Take> {
    confirm_audio_in(&Store::open(&paths::transcript_history_dir()?)?, id, source)
}

pub fn read_text(id: &str) -> Result<String> {
    read_text_in(&Store::open(&paths::transcript_history_dir()?)?, id)
}

pub fn audio_path(id: &str) -> Result<PathBuf> {
    archive::validate_id(id)?;
    let directory = paths::transcript_history_dir()?;
    let store = Store::open(&directory)?;
    let mut file = store
        .open_file(&format!("{id}.wav"))?
        .context("recording audio is unavailable")?;
    audio_duration(&mut file)?;
    Ok(directory.join(format!("{id}.wav")))
}

pub fn persist(
    id: &str,
    duration_ms: u64,
    text: Option<&str>,
    wav: Option<&Path>,
    partial: bool,
    unresolved: bool,
) -> Result<Take> {
    persist_in(
        &Store::open(&paths::transcript_history_dir()?)?,
        id,
        duration_ms,
        text,
        wav,
        partial,
        unresolved,
    )
}

pub fn resolve(id: &str) -> Result<()> {
    resolve_in(&Store::open(&paths::transcript_history_dir()?)?, id)
}

pub fn forget(id: &str) -> Result<()> {
    forget_in(&Store::open(&paths::transcript_history_dir()?)?, id)
}

/// Audio and text in the former global slots have no shared identity. Import
/// them independently, and never unlink an original before durable migration.
pub fn import_legacy() -> Result<()> {
    import_legacy_in(&paths::state_dir()?, &paths::transcript_history_dir()?)
}

fn list_in(store: &Store) -> Result<Vec<Take>> {
    let ids: BTreeSet<_> = store
        .names()?
        .into_iter()
        .filter_map(|name| {
            name.strip_suffix(".json")
                .or_else(|| name.strip_suffix(".wav"))
                .filter(|id| archive::validate_id(id).is_ok())
                .map(str::to_owned)
        })
        .collect();
    let mut takes: Vec<_> = ids.iter().filter_map(|id| get_in(store, id).ok()).collect();
    takes.sort_by(|left, right| {
        right
            .created_at_unix_ms
            .cmp(&left.created_at_unix_ms)
            .then_with(|| right.id.cmp(&left.id))
    });
    Ok(takes)
}

fn get_in(store: &Store, id: &str) -> Result<Take> {
    archive::validate_id(id)?;
    // A corrupt JSON record must not hide an independently durable WAV, and
    // a corrupt/missing WAV must never produce a recoverable-audio claim.
    let json_file = store.open_file(&format!("{id}.json")).ok().flatten();
    let mut audio_file = store.open_file(&format!("{id}.wav")).ok().flatten();
    ensure!(
        json_file.is_some() || audio_file.is_some(),
        "recording does not exist or its artifacts are untrusted"
    );
    let fallback_created_at = json_file
        .as_ref()
        .or(audio_file.as_ref())
        .and_then(|file| file.metadata().ok())
        .map(|metadata| modified_ms(&metadata))
        .unwrap_or(0);
    let audio_duration = audio_file
        .as_mut()
        .and_then(|file| audio_duration(file).ok());
    let record = store.read_record(id).ok().flatten();
    let created_at = record.as_ref().map(archive::created_at_ms).unwrap_or(0);
    Ok(Take {
        id: id.to_owned(),
        created_at_unix_ms: if created_at == 0 {
            fallback_created_at
        } else {
            created_at
        },
        duration_ms: audio_duration.or_else(|| {
            record
                .as_ref()
                .and_then(|record| record.pointer("/audio/duration_ms"))
                .and_then(Value::as_u64)
        }),
        text_available: record.as_ref().and_then(archive::final_text).is_some(),
        audio_available: audio_duration.is_some(),
        partial: record.as_ref().is_some_and(archive::partial),
        unresolved: audio_file.is_some()
            || record.is_none()
            || record
                .as_ref()
                .and_then(|record| record.pointer("/recovery/unresolved"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
    })
}

fn confirm_in(store: &Store, id: &str) -> Result<Take> {
    archive::validate_id(id)?;
    let mut present = false;
    for extension in ["json", "wav"] {
        if let Some(file) = store.open_file(&format!("{id}.{extension}"))? {
            file.sync_all()
                .context("confirming recording artifact durability")?;
            present = true;
        }
    }
    ensure!(present, "recording does not exist");
    store.sync()?;
    get_in(store, id)
}

fn confirm_audio_in(store: &Store, id: &str, source: &Path) -> Result<Take> {
    archive::validate_id(id)?;
    let mut retained = store
        .open_file(&format!("{id}.wav"))?
        .context("recording audio is unavailable")?;
    let mut source = archive::open_source(source)?;
    if !same_audio(&mut retained, &mut source)? {
        return Err(AudioMismatch.into());
    }
    confirm_in(store, id)
}

fn read_text_in(store: &Store, id: &str) -> Result<String> {
    let record = store
        .read_record(id)?
        .context("recording transcript is unavailable")?;
    archive::final_text(&record)
        .map(str::to_owned)
        .context("recording transcript is unavailable")
}

fn persist_in(
    store: &Store,
    id: &str,
    duration_ms: u64,
    text: Option<&str>,
    wav: Option<&Path>,
    partial: bool,
    unresolved: bool,
) -> Result<Take> {
    archive::validate_id(id)?;
    let audio_name = format!("{id}.wav");
    // Publish the sidecar first. Even a later JSON/fsync failure leaves an
    // artifact discoverable by get/list instead of a false all-or-nothing lie.
    let audio_result = if let Some(source) = wav {
        let result = match store.open_file(&audio_name) {
            Ok(Some(mut retained)) => match archive::open_source(source)
                .and_then(|mut source| same_audio(&mut retained, &mut source))
            {
                Ok(matches) => {
                    if !matches {
                        return Err(AudioMismatch.into());
                    }
                    retain_audio(store, &audio_name, source)
                }
                Err(error) => Err(error),
            },
            Ok(None) => retain_audio(store, &audio_name, source),
            Err(error) => Err(error),
        };
        Some(result)
    } else {
        None
    };
    let supplied = text.filter(|text| !text.trim().is_empty());
    let complete =
        supplied.is_some() && !partial && audio_result.as_ref().is_none_or(|result| result.is_ok());
    let record_result = (|| -> Result<()> {
        let mut record = store.record_for_update(id, complete)?.unwrap_or_else(|| {
            json!({
                "schema_version": archive::SCHEMA_VERSION,
                "session_id": id,
                "source": "recovery",
                "recovery": { "created_at_unix_ms": archive::now_ms() },
            })
        });
        let created_at = archive::created_at_ms(&record);
        let old_partial = archive::partial(&record);
        let prior_unresolved = record
            .pointer("/recovery/unresolved")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !record.get("recovery").is_some_and(Value::is_object) {
            record["recovery"] = json!({});
        }
        record["recovery"]["created_at_unix_ms"] = json!(if created_at == 0 {
            archive::now_ms()
        } else {
            created_at
        });
        record["recovery"]["partial"] = json!(old_partial);
        record["recovery"]["unresolved"] = json!(unresolved || prior_unresolved);
        if supplied.is_some() || unresolved {
            record["recovery"]["retry_incomplete"] = json!(!complete);
        }
        if let Some(text) = supplied {
            let old_text = archive::final_text(&record);
            if old_text != Some(text) && (old_text.is_none() || complete) {
                // The rich pipeline archive normally already has the exact
                // final text. This is the durable fallback when that write
                // failed; do not attribute replacement text to old model data.
                if record["recovery"].get("origin").is_none() {
                    record["recovery"]["origin"] = json!({
                        "source": record.get("source"),
                        "completed_at_unix_ms": record.get("completed_at_unix_ms"),
                        "stt_model": record.pointer("/stt/model"),
                        "stt_backend": record.pointer("/stt/backend"),
                    });
                }
                if let Some(object) = record.as_object_mut() {
                    object.remove("stt");
                    object.remove("postproc");
                    object.remove("pipeline");
                }
                record["source"] = json!("recovery");
                record["raw_transcript"] = json!(text);
                record["postprocessed_transcript"] = Value::Null;
                record["completed_at_unix_ms"] = json!(archive::now_ms());
                record["recovery"]["partial"] = json!(partial);
            } else if complete {
                record["recovery"]["partial"] = json!(false);
            }
        }
        let actual_duration = audio_result
            .as_ref()
            .and_then(|result| result.as_ref().ok())
            .copied();
        let duration = actual_duration
            .or_else(|| record.pointer("/audio/duration_ms").and_then(Value::as_u64))
            .or_else(|| (duration_ms > 0).then_some(duration_ms));
        if !record.get("audio").is_some_and(Value::is_object) {
            record["audio"] = json!({});
        }
        record["audio"]["duration_ms"] = json!(duration);
        record["schema_version"] = json!(archive::SCHEMA_VERSION);
        store.write_record(id, &record)
    })();
    match (audio_result.transpose(), record_result) {
        (Err(audio), Err(record)) => Err(anyhow::anyhow!(
            "retaining audio failed: {audio:#}; saving transcript failed: {record:#}"
        )),
        (Err(error), _) | (_, Err(error)) => Err(error),
        _ => get_in(store, id),
    }
}

fn retain_audio(store: &Store, name: &str, source: &Path) -> Result<u64> {
    if let Some(mut existing) = store.open_file(name)? {
        existing.sync_all().context("syncing retained recording")?;
        store.sync()?;
        return audio_duration(&mut existing);
    }
    let mut input = archive::open_source(source)?;
    let before = input
        .metadata()
        .context("reading source recording metadata")?;
    store.publish(name, |output| {
        let copied = std::io::copy(&mut input, output).context("retaining recording audio")?;
        let after = input
            .metadata()
            .context("rechecking source recording metadata")?;
        ensure!(
            copied == before.len() && same_file_version(&before, &after),
            "recording changed while retaining audio"
        );
        Ok(())
    })?;
    let mut saved = store
        .open_file(name)?
        .context("retained recording disappeared")?;
    audio_duration(&mut saved)
}

fn audio_duration(file: &mut File) -> Result<u64> {
    let metadata = stt::wav_metadata_from(file).context("recording audio is corrupt")?;
    ensure!(metadata.frames > 0, "recording audio is empty");
    Ok(metadata.duration_ms)
}

fn same_audio(left: &mut File, right: &mut File) -> Result<bool> {
    let left_metadata = left.metadata().context("checking retained audio")?;
    let right_metadata = right.metadata().context("checking retry audio")?;
    if left_metadata.dev() == right_metadata.dev() && left_metadata.ino() == right_metadata.ino() {
        return Ok(true);
    }
    if left_metadata.len() != right_metadata.len() {
        return Ok(false);
    }
    left.seek(SeekFrom::Start(0))?;
    right.seek(SeekFrom::Start(0))?;
    let mut left_bytes = [0_u8; 65_536];
    let mut right_bytes = [0_u8; 65_536];
    let mut remaining = left_metadata.len();
    while remaining > 0 {
        let length = remaining.min(left_bytes.len() as u64) as usize;
        left.read_exact(&mut left_bytes[..length])?;
        right.read_exact(&mut right_bytes[..length])?;
        if left_bytes[..length] != right_bytes[..length] {
            return Ok(false);
        }
        remaining -= length as u64;
    }
    ensure!(
        same_file_version(&left_metadata, &left.metadata()?)
            && same_file_version(&right_metadata, &right.metadata()?),
        "recording changed while checking retry audio"
    );
    Ok(true)
}

fn same_file_version(before: &Metadata, after: &Metadata) -> bool {
    before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.len() == after.len()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec()
}

fn resolve_in(store: &Store, id: &str) -> Result<()> {
    let mut record = store.read_record(id)?.context("recording does not exist")?;
    ensure!(
        archive::final_text(&record).is_some()
            && !archive::partial(&record)
            && !record
                .pointer("/recovery/retry_incomplete")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        "recording has no complete durable transcript from a successful attempt"
    );
    if !record.get("recovery").is_some_and(Value::is_object) {
        record["recovery"] = json!({});
    }
    record["recovery"]["unresolved"] = json!(false);
    // Re-publish/fsync the complete text before removing its recovery audio.
    store.write_record(id, &record)?;
    store.remove(&format!("{id}.wav"))
}

fn forget_in(store: &Store, id: &str) -> Result<()> {
    get_in(store, id)?;
    match store.read_record(id) {
        Ok(Some(mut record))
            if archive::final_text(&record).is_some() && !archive::partial(&record) =>
        {
            if !record.get("recovery").is_some_and(Value::is_object) {
                record["recovery"] = json!({});
            }
            record["recovery"]["unresolved"] = json!(false);
            record["recovery"]["retry_incomplete"] = json!(false);
            if let Some(recovery) = record.get_mut("recovery").and_then(Value::as_object_mut) {
                recovery.remove("corrupt_record");
            }
            store.write_record(id, &record)?;
        }
        _ => store.remove(&format!("{id}.json"))?,
    }
    store.remove(&format!("{id}.wav"))?;
    let prefix = format!("{id}.json.corrupt-");
    for name in store
        .names()?
        .into_iter()
        .filter(|name| name.starts_with(&prefix))
    {
        store.remove(&name)?;
    }
    Ok(())
}

fn import_legacy_in(state: &Path, history: &Path) -> Result<()> {
    let legacy = Store::open(state)?;
    let store = Store::open(history)?;
    let mut errors = Vec::new();
    for (kind, name) in [
        ("audio", "last-failed.wav"),
        ("text", "last-transcript.txt"),
    ] {
        let result = (|| -> Result<()> {
            let Some(mut original) = legacy.open_file(name)? else {
                return Ok(());
            };
            let metadata = original.metadata().context("checking legacy artifact")?;
            let id = legacy_id(kind, &metadata);
            let mut text = String::new();
            let duration = if kind == "audio" {
                audio_duration(&mut original).context("legacy audio is unavailable")?
            } else {
                original
                    .read_to_string(&mut text)
                    .context("reading legacy transcript")?;
                ensure!(!text.trim().is_empty(), "legacy transcript is empty");
                0
            };
            let source = state.join(name);
            let take = persist_in(
                &store,
                &id,
                duration,
                (kind == "text").then_some(text.as_str()),
                (kind == "audio").then_some(source.as_path()),
                false,
                true,
            )?;
            ensure!(
                if kind == "audio" {
                    take.audio_available
                } else {
                    take.text_available
                },
                "legacy artifact was not durably imported"
            );
            let mut record = store
                .read_record(&id)?
                .context("imported record disappeared")?;
            record["source"] = json!(format!("legacy-{kind}"));
            record["recovery"]["created_at_unix_ms"] = json!(modified_ms(&metadata));
            record["recovery"]["legacy_import"] = json!({ "kind": kind });
            store.write_record(&id, &record)?;
            let current = legacy
                .open_file(name)?
                .context("legacy artifact disappeared")?;
            ensure!(
                same_file_version(&metadata, &current.metadata()?)
                    && same_file_version(&metadata, &original.metadata()?),
                "legacy artifact changed during migration"
            );
            legacy.remove(name)
        })();
        if let Err(error) = result {
            errors.push(format!("legacy {kind}: {error:#}"));
        }
    }
    ensure!(errors.is_empty(), "{}", errors.join("; "));
    Ok(())
}

fn legacy_id(kind: &str, metadata: &Metadata) -> String {
    // Stable across a restart halfway through migration, distinct for
    // unrelated legacy slots and for a later file occupying the slot.
    format!(
        "legacy-{kind}-{:x}-{:x}-{:x}-{:x}-{:x}",
        metadata.dev(),
        metadata.ino(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.len()
    )
}

fn modified_ms(metadata: &Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::{symlink, PermissionsExt};

    struct Fixture {
        root: PathBuf,
        history: PathBuf,
        wav: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("cantrip-recovery-{}", new_id()));
            fs::create_dir(&root).unwrap();
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
            let history = root.join("transcripts");
            let wav = root.join("source.wav");
            write_wav(&wav, 42);
            Self { root, history, wav }
        }

        fn store(&self) -> Store {
            Store::open(&self.history).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn write_wav(path: &Path, sample: i16) {
        let mut writer = hound::WavWriter::create(
            path,
            hound::WavSpec {
                channels: 1,
                sample_rate: 16_000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for _ in 0..160 {
            writer.write_sample(sample).unwrap();
        }
        writer.finalize().unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn identities_are_unique_and_path_like_ids_cannot_retarget_artifacts() {
        let identities: BTreeSet<_> = (0..256).map(|_| new_id()).collect();
        assert_eq!(identities.len(), 256);
        let fixture = Fixture::new();
        let store = fixture.store();
        for invalid in [
            "",
            ".",
            "..",
            "../another",
            "/absolute",
            "take.wav",
            "a/b",
            "a\0b",
        ] {
            assert!(persist_in(&store, invalid, 10, Some("private"), None, false, true).is_err());
            assert!(get_in(&store, invalid).is_err());
        }
        assert!(list_in(&store).unwrap().is_empty());
    }

    #[test]
    fn failed_and_partial_retries_keep_previous_evidence_until_complete_replacement() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let id = new_id();
        persist_in(
            &store,
            &id,
            10,
            Some("first partial"),
            Some(&fixture.wav),
            true,
            true,
        )
        .unwrap();
        let retained = fixture.history.join(format!("{id}.wav"));
        let original_bytes = fs::read(&retained).unwrap();
        persist_in(&store, &id, 10, None, Some(&retained), false, true).unwrap();
        assert_eq!(read_text_in(&store, &id).unwrap(), "first partial");
        assert!(resolve_in(&store, &id).is_err());
        persist_in(
            &store,
            &id,
            10,
            Some("different partial"),
            Some(&retained),
            true,
            true,
        )
        .unwrap();
        assert_eq!(read_text_in(&store, &id).unwrap(), "first partial");
        assert!(get_in(&store, &id).unwrap().partial);
        assert!(resolve_in(&store, &id).is_err());
        let other_wav = fixture.root.join("different.wav");
        write_wav(&other_wav, 43);
        assert!(persist_in(
            &store,
            &id,
            10,
            Some("wrong recording"),
            Some(&other_wav),
            false,
            true
        )
        .is_err());
        assert_eq!(read_text_in(&store, &id).unwrap(), "first partial");
        assert_eq!(fs::read(&retained).unwrap(), original_bytes);
        persist_in(
            &store,
            &id,
            10,
            Some("complete recovered text"),
            Some(&retained),
            false,
            true,
        )
        .unwrap();
        assert_eq!(
            read_text_in(&store, &id).unwrap(),
            "complete recovered text"
        );
        assert_eq!(fs::read(&retained).unwrap(), original_bytes);
        resolve_in(&store, &id).unwrap();
        let take = get_in(&store, &id).unwrap();
        assert!(take.text_available);
        assert!(!take.audio_available);
        assert!(!take.partial);
        assert!(!take.unresolved);
    }

    #[test]
    fn unsuccessful_retry_cannot_resolve_using_an_older_complete_transcript() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let id = new_id();
        persist_in(
            &store,
            &id,
            10,
            Some("older complete"),
            Some(&fixture.wav),
            false,
            true,
        )
        .unwrap();
        persist_in(&store, &id, 10, None, None, false, true).unwrap();
        assert!(resolve_in(&store, &id).is_err());
        persist_in(
            &store,
            &id,
            10,
            Some("cancelled new text"),
            None,
            true,
            true,
        )
        .unwrap();
        assert_eq!(read_text_in(&store, &id).unwrap(), "older complete");
        assert!(resolve_in(&store, &id).is_err());
        assert!(get_in(&store, &id).unwrap().audio_available);
    }

    #[test]
    fn unrelated_success_does_not_remove_an_unresolved_take() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let failed = new_id();
        let success = new_id();
        let before =
            persist_in(&store, &failed, 10, None, Some(&fixture.wav), false, true).unwrap();
        persist_in(
            &store,
            &success,
            10,
            Some("complete text"),
            Some(&fixture.wav),
            false,
            true,
        )
        .unwrap();
        resolve_in(&store, &success).unwrap();
        assert_eq!(get_in(&store, &failed).unwrap(), before);
        let listed = list_in(&store).unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed
            .iter()
            .any(|take| take.id == failed && take.unresolved && take.audio_available));
        assert!(listed
            .windows(2)
            .all(|pair| pair[0].created_at_unix_ms >= pair[1].created_at_unix_ms));
    }

    #[test]
    fn partial_persistence_is_discoverable_after_either_artifact_write_fails() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let audio_only = new_id();
        fs::create_dir(fixture.history.join(format!("{audio_only}.json"))).unwrap();
        assert!(persist_in(
            &store,
            &audio_only,
            10,
            Some("not published"),
            Some(&fixture.wav),
            true,
            true
        )
        .is_err());
        let take = get_in(&store, &audio_only).unwrap();
        assert!(take.audio_available);
        assert!(!take.text_available);
        assert!(take.unresolved);
        let text_only = new_id();
        assert!(persist_in(
            &store,
            &text_only,
            10,
            Some("saved despite missing audio"),
            Some(&fixture.root.join("missing.wav")),
            false,
            true
        )
        .is_err());
        let take = get_in(&store, &text_only).unwrap();
        assert!(!take.audio_available);
        assert!(take.text_available);
        assert_eq!(
            read_text_in(&store, &text_only).unwrap(),
            "saved despite missing audio"
        );
        let blocked_audio = new_id();
        fs::create_dir(fixture.history.join(format!("{blocked_audio}.wav"))).unwrap();
        assert!(persist_in(
            &store,
            &blocked_audio,
            10,
            Some("saved despite blocked audio"),
            Some(&fixture.wav),
            false,
            true
        )
        .is_err());
        let take = get_in(&store, &blocked_audio).unwrap();
        assert!(!take.audio_available);
        assert_eq!(
            read_text_in(&store, &blocked_audio).unwrap(),
            "saved despite blocked audio"
        );
    }

    #[test]
    fn artifact_availability_comes_from_valid_files_not_stored_flags() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let id = new_id();
        persist_in(
            &store,
            &id,
            10,
            Some("saved text"),
            Some(&fixture.wav),
            false,
            true,
        )
        .unwrap();
        store.remove(&format!("{id}.wav")).unwrap();
        let mut record = store.read_record(&id).unwrap().unwrap();
        record["recovery"]["audio_available"] = json!(true);
        store.write_record(&id, &record).unwrap();
        assert!(!get_in(&store, &id).unwrap().audio_available);
        store
            .publish(&format!("{id}.wav"), |file| {
                file.write_all(b"RIFF corrupt")?;
                Ok(())
            })
            .unwrap();
        assert!(!get_in(&store, &id).unwrap().audio_available);
        store
            .publish(&format!("{id}.json"), |file| {
                file.write_all(b"corrupt JSON")?;
                Ok(())
            })
            .unwrap();
        assert!(!get_in(&store, &id).unwrap().text_available);
        assert!(read_text_in(&store, &id).is_err());
        store.remove(&format!("{id}.wav")).unwrap();
        retain_audio(&store, &format!("{id}.wav"), &fixture.wav).unwrap();
        let take = get_in(&store, &id).unwrap();
        assert!(take.audio_available);
        assert!(!take.text_available);
        assert!(take.unresolved);
    }

    #[test]
    fn history_is_private_and_untrusted_links_cannot_expose_or_replace_files() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let id = new_id();
        persist_in(
            &store,
            &id,
            10,
            Some("private text"),
            Some(&fixture.wav),
            false,
            true,
        )
        .unwrap();
        assert_eq!(
            fs::metadata(&fixture.history).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for suffix in ["json", "wav"] {
            assert_eq!(
                fs::metadata(fixture.history.join(format!("{id}.{suffix}")))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let outside = fixture.root.join("outside");
        fs::write(&outside, b"unrelated evidence").unwrap();
        let linked = new_id();
        symlink(&outside, fixture.history.join(format!("{linked}.json"))).unwrap();
        assert!(persist_in(
            &store,
            &linked,
            10,
            Some("must not replace"),
            None,
            false,
            true
        )
        .is_err());
        assert!(read_text_in(&store, &linked).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"unrelated evidence");
        let hardlinked = new_id();
        fs::hard_link(
            fixture.history.join(format!("{id}.wav")),
            fixture.history.join(format!("{hardlinked}.wav")),
        )
        .unwrap();
        assert!(get_in(&store, &hardlinked).is_err());
        assert!(!get_in(&store, &id).unwrap().audio_available);
    }

    #[test]
    fn historical_rich_archives_remain_readable_and_forget_keeps_successful_text() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let id = new_id();
        store
            .write_record(
                &id,
                &json!({
                    "schema_version": 2,
                    "session_id": id,
                    "completed_at_unix_ms": 1_234,
                    "source": "dictation",
                    "raw_transcript": "raw words",
                    "postprocessed_transcript": "Clean words.",
                    "audio": { "duration_ms": 5_000 },
                    "stt": { "model": "historical-model", "partial": false },
                    "postproc": { "custom_instructions": "historical provenance" },
                }),
            )
            .unwrap();
        assert_eq!(read_text_in(&store, &id).unwrap(), "Clean words.");
        let take = get_in(&store, &id).unwrap();
        assert_eq!(take.created_at_unix_ms, 1_234);
        assert_eq!(take.duration_ms, Some(5_000));
        assert!(take.text_available);
        assert!(!take.audio_available);
        persist_in(&store, &id, 10, None, Some(&fixture.wav), false, true).unwrap();
        forget_in(&store, &id).unwrap();
        assert_eq!(read_text_in(&store, &id).unwrap(), "Clean words.");
        let record = store.read_record(&id).unwrap().unwrap();
        assert_eq!(record["stt"]["model"], "historical-model");
        assert_eq!(
            record["postproc"]["custom_instructions"],
            "historical provenance"
        );
        assert!(!get_in(&store, &id).unwrap().unresolved);
        let incomplete = new_id();
        persist_in(
            &store,
            &incomplete,
            10,
            Some("partial words"),
            Some(&fixture.wav),
            true,
            true,
        )
        .unwrap();
        forget_in(&store, &incomplete).unwrap();
        assert!(get_in(&store, &incomplete).is_err());
    }

    #[test]
    fn legacy_audio_and_text_migrate_as_distinct_durable_takes() {
        let fixture = Fixture::new();
        fs::copy(&fixture.wav, fixture.root.join("last-failed.wav")).unwrap();
        {
            let legacy = Store::open(&fixture.root).unwrap();
            legacy
                .publish("last-transcript.txt", |file| {
                    file.write_all(b"unrelated last transcript")?;
                    Ok(())
                })
                .unwrap();
        }
        import_legacy_in(&fixture.root, &fixture.history).unwrap();
        assert!(!fixture.root.join("last-failed.wav").exists());
        assert!(!fixture.root.join("last-transcript.txt").exists());
        let first_ids;
        {
            let store = fixture.store();
            let takes = list_in(&store).unwrap();
            assert_eq!(takes.len(), 2);
            let audio = takes.iter().find(|take| take.audio_available).unwrap();
            let text = takes.iter().find(|take| take.text_available).unwrap();
            assert_eq!(text.duration_ms, None);
            assert_ne!(audio.id, text.id);
            assert!(!audio.text_available);
            assert!(!text.audio_available);
            assert_eq!(
                read_text_in(&store, &text.id).unwrap(),
                "unrelated last transcript"
            );
            assert_eq!(
                store.read_record(&audio.id).unwrap().unwrap()["source"],
                "legacy-audio"
            );
            assert_eq!(
                store.read_record(&text.id).unwrap().unwrap()["source"],
                "legacy-text"
            );
            first_ids = takes
                .into_iter()
                .map(|take| take.id)
                .collect::<BTreeSet<_>>();
        }
        import_legacy_in(&fixture.root, &fixture.history).unwrap();
        let store = fixture.store();
        assert_eq!(
            list_in(&store)
                .unwrap()
                .into_iter()
                .map(|take| take.id)
                .collect::<BTreeSet<_>>(),
            first_ids
        );
    }

    #[test]
    fn legacy_failure_preserves_its_original_without_blocking_the_other_import() {
        let fixture = Fixture::new();
        {
            let legacy = Store::open(&fixture.root).unwrap();
            legacy
                .publish("last-failed.wav", |file| {
                    file.write_all(b"corrupt legacy recording")?;
                    Ok(())
                })
                .unwrap();
            legacy
                .publish("last-transcript.txt", |file| {
                    file.write_all(b"independently recoverable text")?;
                    Ok(())
                })
                .unwrap();
        }
        assert!(import_legacy_in(&fixture.root, &fixture.history).is_err());
        assert_eq!(
            fs::read(fixture.root.join("last-failed.wav")).unwrap(),
            b"corrupt legacy recording"
        );
        assert!(!fixture.root.join("last-transcript.txt").exists());
        let store = fixture.store();
        let takes = list_in(&store).unwrap();
        assert_eq!(takes.len(), 1);
        assert_eq!(
            read_text_in(&store, &takes[0].id).unwrap(),
            "independently recoverable text"
        );
        assert!(!takes[0].audio_available);
    }

    #[test]
    fn interrupted_legacy_import_keeps_original_until_canonical_publication_succeeds() {
        let fixture = Fixture::new();
        let original = fixture.root.join("last-failed.wav");
        fs::copy(&fixture.wav, &original).unwrap();
        let id = legacy_id("audio", &fs::metadata(&original).unwrap());
        {
            let legacy = Store::open(&fixture.root).unwrap();
            legacy
                .publish("last-transcript.txt", |file| {
                    file.write_all(b"independent text")?;
                    Ok(())
                })
                .unwrap();
            let _store = fixture.store();
            fs::create_dir(fixture.history.join(format!("{id}.json"))).unwrap();
        }
        assert!(import_legacy_in(&fixture.root, &fixture.history).is_err());
        assert!(original.exists());
        assert!(!fixture.root.join("last-transcript.txt").exists());
        {
            let store = fixture.store();
            let take = get_in(&store, &id).unwrap();
            assert!(take.audio_available);
            assert!(!take.text_available);
        }
        fs::remove_dir(fixture.history.join(format!("{id}.json"))).unwrap();
        import_legacy_in(&fixture.root, &fixture.history).unwrap();
        assert!(!original.exists());
        let store = fixture.store();
        assert!(get_in(&store, &id).unwrap().audio_available);
        assert_eq!(list_in(&store).unwrap().len(), 2);
    }

    #[test]
    fn safe_complete_recovery_preserves_corrupt_history_once_per_distinct_content() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let id = new_id();
        persist_in(
            &store,
            &id,
            10,
            Some("old text"),
            Some(&fixture.wav),
            false,
            true,
        )
        .unwrap();
        let corrupt = b"{ malformed private history \xff";
        store
            .publish(&format!("{id}.json"), |file| {
                file.write_all(corrupt)?;
                Ok(())
            })
            .unwrap();
        let before = confirm_in(&store, &id).unwrap();
        assert!(before.audio_available);
        assert!(!before.text_available);
        assert!(persist_in(
            &store,
            &id,
            10,
            Some("incomplete retry"),
            Some(&fixture.wav),
            true,
            true
        )
        .is_err());
        assert_eq!(
            fs::read(fixture.history.join(format!("{id}.json"))).unwrap(),
            corrupt
        );
        persist_in(
            &store,
            &id,
            10,
            Some("complete recovery"),
            Some(&fixture.wav),
            false,
            true,
        )
        .unwrap();
        let record = store.read_record(&id).unwrap().unwrap();
        let backup = record["recovery"]["corrupt_record"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(fs::read(fixture.history.join(&backup)).unwrap(), corrupt);
        assert_eq!(read_text_in(&store, &id).unwrap(), "complete recovery");
        assert_eq!(list_in(&store).unwrap().len(), 1);
        // The same corrupted contents after another interrupted repair must
        // not accumulate another copy or overwrite existing forensic bytes.
        store
            .publish(&format!("{id}.json"), |file| {
                file.write_all(corrupt)?;
                Ok(())
            })
            .unwrap();
        persist_in(
            &store,
            &id,
            10,
            Some("second complete recovery"),
            Some(&fixture.wav),
            false,
            true,
        )
        .unwrap();
        let prefix = format!("{id}.json.corrupt-");
        assert_eq!(
            store
                .names()
                .unwrap()
                .iter()
                .filter(|name| name.starts_with(&prefix))
                .count(),
            1
        );
        assert_eq!(fs::read(fixture.history.join(&backup)).unwrap(), corrupt);
        resolve_in(&store, &id).unwrap();
        assert!(fixture.history.join(&backup).exists());
        forget_in(&store, &id).unwrap();
        assert!(!fixture.history.join(&backup).exists());
        assert_eq!(
            read_text_in(&store, &id).unwrap(),
            "second complete recovery"
        );
    }

    #[test]
    fn durability_confirmation_is_not_inferred_from_artifact_presence() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let id = new_id();
        fs::create_dir(fixture.history.join(format!("{id}.json"))).unwrap();
        assert!(persist_in(&store, &id, 10, None, Some(&fixture.wav), false, true).is_err());
        assert!(get_in(&store, &id).unwrap().audio_available);
        // A present WAV is not enough to certify a take with an untrusted
        // artifact occupying its canonical JSON path.
        assert!(confirm_in(&store, &id).is_err());
        fs::remove_dir(fixture.history.join(format!("{id}.json"))).unwrap();
        let confirmed = confirm_in(&store, &id).unwrap();
        assert!(confirmed.audio_available);
        assert!(!confirmed.text_available);
        store.remove(&format!("{id}.wav")).unwrap();
        assert!(confirm_in(&store, &id).is_err());
    }

    #[test]
    fn complete_repair_does_not_overwrite_a_different_valid_recording_identity() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let id = new_id();
        let unrelated = new_id();
        store
            .write_record(
                &id,
                &json!({
                    "session_id": unrelated,
                    "raw_transcript": "unrelated private evidence",
                }),
            )
            .unwrap();
        let before = fs::read(fixture.history.join(format!("{id}.json"))).unwrap();
        assert!(persist_in(&store, &id, 10, Some("new words"), None, false, true).is_err());
        assert_eq!(
            fs::read(fixture.history.join(format!("{id}.json"))).unwrap(),
            before
        );
    }

    #[test]
    fn audio_presence_uses_sample_frames_not_rounded_milliseconds() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let short = fixture.root.join("sub-millisecond.wav");
        let mut writer = hound::WavWriter::create(&short, spec).unwrap();
        writer.write_sample(42_i16).unwrap();
        writer.finalize().unwrap();
        let id = new_id();
        let take = persist_in(&store, &id, 0, None, Some(&short), false, true).unwrap();
        assert_eq!(take.duration_ms, Some(0));
        assert!(take.audio_available);
        let empty = fixture.root.join("empty.wav");
        hound::WavWriter::create(&empty, spec)
            .unwrap()
            .finalize()
            .unwrap();
        let empty_id = new_id();
        assert!(persist_in(&store, &empty_id, 0, None, Some(&empty), false, true).is_err());
        assert!(!get_in(&store, &empty_id).unwrap().audio_available);
    }

    #[test]
    fn audio_confirmation_requires_the_exact_source_not_just_a_durable_take() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let id = new_id();
        persist_in(
            &store,
            &id,
            10,
            Some("original transcript"),
            Some(&fixture.wav),
            false,
            true,
        )
        .unwrap();
        let different = fixture.root.join("different.wav");
        write_wav(&different, 43);
        assert!(confirm_in(&store, &id).unwrap().audio_available);
        assert!(confirm_audio_in(&store, &id, &different)
            .unwrap_err()
            .is::<AudioMismatch>());
        assert!(persist_in(
            &store,
            &id,
            10,
            Some("unrelated transcript"),
            Some(&different),
            false,
            true,
        )
        .unwrap_err()
        .is::<AudioMismatch>());
        assert_eq!(read_text_in(&store, &id).unwrap(), "original transcript");
        assert_eq!(
            fs::read(fixture.history.join(format!("{id}.wav"))).unwrap(),
            fs::read(&fixture.wav).unwrap()
        );
        assert!(
            confirm_audio_in(&store, &id, &fixture.wav)
                .unwrap()
                .audio_available
        );
        assert!(different.exists(), "unrelated source remains available");
    }
}
