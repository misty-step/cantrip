//! Download and install local Parakeet model files.

use crate::paths::{ensure_dir, models_dir};
use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const PROGRESS_BYTES: u64 = 50 * 1024 * 1024;

pub struct ModelSpec {
    pub name: &'static str,
    pub url: &'static str,
    pub dir_name: &'static str,
    pub expect: &'static [&'static str],
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
};

pub fn installed(spec: &ModelSpec) -> Result<Option<PathBuf>> {
    let root = models_dir().context("locating model directory")?;
    installed_in(&root, spec)
}

pub fn ensure_model(spec: &ModelSpec) -> Result<PathBuf> {
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
            let _ = fs::remove_dir_all(&extract_path);
            return Err(error);
        }
    };
    let _artifacts = TempArtifacts {
        archive: archive_path.clone(),
        extract: extract_path.clone(),
    };

    let response = ureq::get(spec.url)
        .call()
        .with_context(|| format!("downloading model {} from {}", spec.name, spec.url))?;
    let mut reader = response.into_reader();
    let mut buffer = [0_u8; 64 * 1024];
    let mut downloaded = 0_u64;
    let mut next_progress = PROGRESS_BYTES;
    loop {
        let read = reader
            .read(&mut buffer)
            .with_context(|| format!("reading model download for {}", spec.name))?;
        if read == 0 {
            break;
        }
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

    let archive_file = File::open(&archive_path).with_context(|| {
        format!(
            "opening downloaded model archive {}",
            archive_path.display()
        )
    })?;
    let decoder = GzDecoder::new(archive_file);
    let mut archive = tar::Archive::new(decoder);
    archive
        .unpack(&extract_path)
        .with_context(|| format!("extracting model archive for {}", spec.name))?;

    let source_dir = find_model_dir(&extract_path, spec.expect[0])?.ok_or_else(|| {
        anyhow::anyhow!(
            "model archive {} does not contain {}",
            spec.name,
            spec.expect[0]
        )
    })?;
    let destination = root.join(spec.dir_name);
    if destination.exists() {
        remove_existing(&destination)
            .with_context(|| format!("removing incomplete model {}", destination.display()))?;
    }
    fs::rename(&source_dir, &destination).with_context(|| {
        format!(
            "installing model {} at {}",
            spec.name,
            destination.display()
        )
    })?;

    let Some(dir) = installed_in(&root, spec)? else {
        let _ = fs::remove_dir_all(&destination);
        bail!(
            "model {} is missing expected files after installation",
            spec.name
        );
    };
    tracing::info!("[Models] installed {}", spec.name);
    Ok(dir)
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

fn find_model_dir(root: &Path, first_expected: &str) -> Result<Option<PathBuf>> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
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

fn remove_existing(path: &Path) -> Result<()> {
    if path.is_dir() {
        fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))?;
    } else {
        fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}

fn create_temp_file(base: &Path, label: &str) -> Result<(PathBuf, File)> {
    for attempt in 0..100_u32 {
        let path = temporary_path(base, label, attempt);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
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
}

impl Drop for TempArtifacts {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.archive);
        let _ = fs::remove_dir_all(&self.extract);
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
