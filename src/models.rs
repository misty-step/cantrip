//! Download and install local Parakeet model files.

use crate::paths::{ensure_dir, models_dir};
use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const PROGRESS_BYTES: u64 = 50 * 1024 * 1024;

pub struct ModelSpec {
    pub name: &'static str,
    pub url: &'static str,
    pub dir_name: &'static str,
    pub expect: &'static [&'static str],
    pub sha256: &'static str,
}

pub const PARAKEET_V3_INT8: ModelSpec = ModelSpec {
    name: "parakeet",
    url: "https://blob.handy.computer/parakeet-v3-int8.tar.gz",
    dir_name: "parakeet-tdt-0.6b-v3-int8",
    expect: &[
        "encoder-model.int8.onnx",
        "decoder_joint-model.int8.onnx",
        "nemo128.onnx",
        "vocab.txt",
    ],
    sha256: "43d37191602727524a7d8c6da0eef11c4ba24320f5b4730f1a2497befc2efa77",
};

pub const MODEL_NAMES: &[&str] = &["parakeet-tdt-0.6b-v3-int8"];

fn spec(name: &str) -> Option<&'static ModelSpec> {
    if PARAKEET_V3_INT8.dir_name == name {
        Some(&PARAKEET_V3_INT8)
    } else {
        None
    }
}

/// Look up a local model by name, failing with the list of valid names.
pub fn require(name: &str) -> Result<&'static ModelSpec> {
    spec(name).with_context(|| {
        format!(
            "unknown local stt.model '{name}'; valid names: {}",
            MODEL_NAMES.join(", ")
        )
    })
}

pub fn installed(spec: &ModelSpec) -> Result<Option<PathBuf>> {
    let root = models_dir().context("locating model directory")?;
    installed_in(&root, spec)
}

#[derive(Debug)]
pub struct InstallCancelled;

impl std::fmt::Display for InstallCancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("model installation cancelled")
    }
}

impl std::error::Error for InstallCancelled {}

pub fn ensure_model(spec: &ModelSpec, cancel: Option<&AtomicBool>) -> Result<PathBuf> {
    check_cancel(cancel)?;
    if spec.expect.is_empty() {
        bail!("model {} has no expected files", spec.name);
    }

    let root = ensure_dir(models_dir().context("locating model directory")?)?;
    if let Some(dir) = installed_in(&root, spec)? {
        return Ok(dir);
    }

    tracing::info!("[Models] downloading {} from {}", spec.name, spec.url);
    let extract_path = create_temp_dir(&root, "extract")?;
    let (archive_path, mut archive_file) = match create_temp_file(&root, "download") {
        Ok(file) => file,
        Err(error) => {
            fs::remove_dir_all(&extract_path).with_context(|| {
                format!(
                    "removing temporary model directory {}",
                    extract_path.display()
                )
            })?;
            return Err(error);
        }
    };
    let artifacts = TempArtifacts {
        archive: archive_path.clone(),
        extract: extract_path.clone(),
        cleanup_attempted: false,
    };

    let result = (|| -> Result<PathBuf> {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout_read(Duration::from_secs(5))
            .timeout_write(Duration::from_secs(5))
            .build();
        let response = agent
            .get(spec.url)
            .call()
            .with_context(|| format!("downloading model {} from {}", spec.name, spec.url))?;
        let mut reader = response.into_reader();
        let mut buffer = [0_u8; 64 * 1024];
        let mut downloaded = 0_u64;
        let mut next_progress = PROGRESS_BYTES;
        loop {
            check_cancel(cancel)?;
            let read = reader
                .read(&mut buffer)
                .with_context(|| format!("reading model download for {}", spec.name))?;
            if read == 0 {
                break;
            }
            check_cancel(cancel)?;
            archive_file
                .write_all(&buffer[..read])
                .with_context(|| format!("writing model download for {}", spec.name))?;
            downloaded += read as u64;
            while downloaded >= next_progress {
                tracing::info!(
                    "[Models] downloaded {} MB for {}",
                    next_progress / (1024 * 1024),
                    spec.name
                );
                next_progress += PROGRESS_BYTES;
            }
        }
        archive_file
            .flush()
            .with_context(|| format!("flushing model download for {}", spec.name))?;
        drop(archive_file);

        verify_archive(&archive_path, spec, cancel)?;

        let archive_file = File::open(&archive_path).with_context(|| {
            format!(
                "opening downloaded model archive {}",
                archive_path.display()
            )
        })?;
        let decoder = GzDecoder::new(CancellableReader {
            inner: archive_file,
            cancel,
        });
        let mut archive = tar::Archive::new(decoder);
        archive
            .unpack(&extract_path)
            .with_context(|| format!("extracting model archive for {}", spec.name))?;

        let source_dir =
            find_model_dir(&extract_path, spec.expect[0], cancel)?.ok_or_else(|| {
                anyhow::anyhow!(
                    "model archive {} does not contain {}",
                    spec.name,
                    spec.expect[0]
                )
            })?;
        let destination = root.join(spec.dir_name);
        publish_model(&source_dir, &destination, spec, cancel)?;
        tracing::info!("[Models] installed {}", spec.name);
        Ok(destination)
    })();
    artifacts.cleanup()?;
    if result.is_err() {
        // Errors before the atomic publish leave the previous model intact.
        // A simultaneous user cancellation remains a cancellation only after
        // temporary artifacts have actually been removed.
        check_cancel(cancel)?;
    }
    result
}

fn sha256_hex(path: &Path, cancel: Option<&AtomicBool>) -> Result<String> {
    let mut file = File::open(path)
        .with_context(|| format!("opening model archive {} for checksum", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        check_cancel(cancel)?;
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("reading model archive {} for checksum", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let mut hex = String::with_capacity(64);
    for byte in hasher.finalize() {
        hex.push_str(&format!("{byte:02x}"));
    }
    Ok(hex)
}

fn verify_archive(path: &Path, spec: &ModelSpec, cancel: Option<&AtomicBool>) -> Result<()> {
    let actual = sha256_hex(path, cancel)
        .with_context(|| format!("hashing model archive {} for {}", path.display(), spec.name))?;
    if actual != spec.sha256 {
        let _ = fs::remove_file(path);
        bail!(
            "model {} archive checksum mismatch: expected sha256 {}, got {}",
            spec.name,
            spec.sha256,
            actual
        );
    }
    Ok(())
}

fn installed_in(root: &Path, spec: &ModelSpec) -> Result<Option<PathBuf>> {
    if spec.expect.is_empty() {
        bail!("model {} has no expected files", spec.name);
    }
    let dir = root.join(spec.dir_name);
    if !dir.is_dir() {
        return Ok(None);
    }
    if spec.expect.iter().all(|name| dir.join(name).is_file()) {
        Ok(Some(dir))
    } else {
        Ok(None)
    }
}

fn find_model_dir(
    root: &Path,
    first_expected: &str,
    cancel: Option<&AtomicBool>,
) -> Result<Option<PathBuf>> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        check_cancel(cancel)?;
        if dir.join(first_expected).is_file() {
            return Ok(Some(dir));
        }
        for entry in fs::read_dir(&dir)
            .with_context(|| format!("walking extracted model directory {}", dir.display()))?
        {
            let entry = entry
                .with_context(|| format!("reading extracted model directory {}", dir.display()))?;
            if entry
                .file_type()
                .with_context(|| format!("reading {}", entry.path().display()))?
                .is_dir()
            {
                pending.push(entry.path());
            }
        }
    }
    Ok(None)
}

fn check_cancel(cancel: Option<&AtomicBool>) -> Result<()> {
    if cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed)) {
        return Err(InstallCancelled.into());
    }
    Ok(())
}

struct CancellableReader<'a, R> {
    inner: R,
    cancel: Option<&'a AtomicBool>,
}

impl<R: Read> Read for CancellableReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        check_cancel(self.cancel).map_err(std::io::Error::other)?;
        let count = self.inner.read(buffer)?;
        check_cancel(self.cancel).map_err(std::io::Error::other)?;
        Ok(count)
    }
}

fn publish_model(
    source: &Path,
    destination: &Path,
    spec: &ModelSpec,
    cancel: Option<&AtomicBool>,
) -> Result<()> {
    for name in spec.expect {
        check_cancel(cancel)?;
        let path = source.join(name);
        anyhow::ensure!(
            path.is_file(),
            "model {} is missing expected file {name}",
            spec.name
        );
        File::open(&path)?.sync_all()?;
    }
    File::open(source)?.sync_all()?;
    check_cancel(cancel)?;
    if fs::symlink_metadata(destination).is_ok() {
        // renameat2(RENAME_EXCHANGE) atomically exchanges even nonempty
        // directories. A failed exchange leaves the old model untouched.
        // The old tree then lives at the owned extraction path for cleanup.
        let source = CString::new(source.as_os_str().as_bytes())?;
        let destination = CString::new(destination.as_os_str().as_bytes())?;
        let result = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                destination.as_ptr(),
                libc::RENAME_EXCHANGE,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error())
                .context("atomically replacing the model; existing model retained");
        }
    } else {
        fs::rename(source, destination).context("publishing verified model")?;
    }
    Ok(())
}

fn create_temp_file(base: &Path, label: &str) -> Result<(PathBuf, File)> {
    for attempt in 0..100_u32 {
        let path = temporary_path(base, label, attempt);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("creating temporary file {}", path.display()))
            }
        }
    }
    bail!("unable to create temporary model download file")
}

fn create_temp_dir(base: &Path, label: &str) -> Result<PathBuf> {
    for attempt in 0..100_u32 {
        let path = temporary_path(base, label, attempt);
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("creating temporary directory {}", path.display()))
            }
        }
    }
    bail!("unable to create temporary model extraction directory")
}

fn temporary_path(base: &Path, label: &str, attempt: u32) -> PathBuf {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    base.join(format!(
        ".cantrip-{label}-{}-{now}-{attempt}.tmp",
        std::process::id()
    ))
}

struct TempArtifacts {
    archive: PathBuf,
    extract: PathBuf,
    cleanup_attempted: bool,
}

impl TempArtifacts {
    fn remove(&self) -> Result<()> {
        for (path, result) in [
            (&self.archive, fs::remove_file(&self.archive)),
            (&self.extract, fs::remove_dir_all(&self.extract)),
        ] {
            if let Err(error) = result {
                if error.kind() != ErrorKind::NotFound {
                    return Err(error).with_context(|| {
                        format!("removing temporary model files at {}", path.display())
                    });
                }
            }
        }
        Ok(())
    }

    fn cleanup(mut self) -> Result<()> {
        self.cleanup_attempted = true;
        self.remove()
    }
}

impl Drop for TempArtifacts {
    fn drop(&mut self) {
        if !self.cleanup_attempted {
            let _ = self.remove();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_requires_every_expected_file() {
        let root = test_root();
        let spec = ModelSpec {
            name: "test",
            url: "unused-in-installed-test",
            dir_name: "model",
            expect: &["encoder.onnx", "vocab.txt"],
            sha256: "unused-in-installed-test",
        };
        let dir = root.join(spec.dir_name);
        fs::create_dir_all(&dir).expect("create model directory");
        fs::write(dir.join(spec.expect[0]), b"model").expect("create encoder");
        assert!(installed_in(&root, &spec)
            .expect("inspect partial model")
            .is_none());

        fs::write(dir.join(spec.expect[1]), b"vocabulary").expect("create vocabulary");
        assert_eq!(
            installed_in(&root, &spec).expect("inspect complete model"),
            Some(dir)
        );
        fs::remove_dir_all(root).expect("remove test model");
    }

    #[test]
    fn registry_lookup_uses_directory_name() {
        assert_eq!(
            spec("parakeet-tdt-0.6b-v3-int8").map(|model| model.dir_name),
            Some("parakeet-tdt-0.6b-v3-int8")
        );
        assert!(spec("missing-model").is_none());
        assert_eq!(MODEL_NAMES, &["parakeet-tdt-0.6b-v3-int8"]);
    }

    #[test]
    fn sha256_matches_known_vectors() {
        let root = test_root();
        let path = root.join("archive");

        fs::write(&path, b"abc").expect("write abc archive");
        assert_eq!(
            sha256_hex(&path, None).expect("hash abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );

        fs::write(&path, b"").expect("write empty archive");
        assert_eq!(
            sha256_hex(&path, None).expect("hash empty"),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );

        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn verify_archive_accepts_matching_digest() {
        let root = test_root();
        let path = root.join("archive");
        fs::write(&path, b"abc").expect("write archive");
        let spec = checksum_test_spec();

        verify_archive(&path, &spec, None).expect("accept matching digest");
        assert!(path.exists());

        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn verify_archive_deletes_mismatched_archive() {
        let root = test_root();
        let path = root.join("archive");
        fs::write(&path, b"wrong bytes").expect("write mismatched archive");
        let spec = checksum_test_spec();

        let error = verify_archive(&path, &spec, None).expect_err("reject mismatched digest");
        assert!(error.to_string().contains("checksum mismatch"));
        assert!(!path.exists());

        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn verify_archive_deletes_truncated_archive() {
        let root = test_root();
        let path = root.join("archive");
        fs::write(&path, b"ab").expect("write truncated archive");
        let spec = checksum_test_spec();

        let error = verify_archive(&path, &spec, None).expect_err("reject truncated archive");
        assert!(error.to_string().contains("checksum mismatch"));
        assert!(!path.exists());

        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn cancelled_and_incomplete_replacements_preserve_the_previous_model() {
        let root = test_root();
        let old = root.join("installed");
        let next = root.join("candidate");
        fs::create_dir_all(&old).expect("old directory");
        fs::create_dir_all(&next).expect("candidate directory");
        fs::write(old.join("encoder.onnx"), b"previous model").expect("previous");
        let spec = checksum_test_spec();
        let cancel = AtomicBool::new(false);
        assert!(publish_model(&next, &old, &spec, Some(&cancel)).is_err());
        assert_eq!(
            fs::read(old.join("encoder.onnx")).expect("retained"),
            b"previous model"
        );
        fs::write(next.join("encoder.onnx"), b"verified replacement").expect("candidate");
        cancel.store(true, Ordering::Relaxed);
        assert!(publish_model(&next, &old, &spec, Some(&cancel)).is_err());
        assert_eq!(
            fs::read(old.join("encoder.onnx")).expect("retained after cancellation"),
            b"previous model"
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn cancellation_interrupts_archive_reads_without_consuming_more_bytes() {
        let cancel = AtomicBool::new(true);
        let mut reader = CancellableReader {
            inner: std::io::Cursor::new(b"model archive"),
            cancel: Some(&cancel),
        };
        assert!(reader.read(&mut [0; 4]).is_err());
        assert_eq!(reader.inner.position(), 0);
    }

    fn checksum_test_spec() -> ModelSpec {
        ModelSpec {
            name: "test",
            url: "unused-in-checksum-test",
            dir_name: "model",
            expect: &["encoder.onnx"],
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        }
    }

    fn test_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "cantrip-models-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("create test root");
        root
    }
}
