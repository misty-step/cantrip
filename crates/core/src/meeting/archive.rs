//! The append-only, exclusive Meeting-folder archive (DESIGN.md › Publication
//! invariants 1–3). Everything here touches the user's Meeting folder, so it
//! is deliberately small and paranoid:
//!
//! - disjointness from staging is judged by filesystem identity (dev, inode)
//!   after symlink resolution, never by string prefix;
//! - the chosen folder is never created; `cantrip-meeting/YYYY/MM` are created
//!   one level at a time, each existing level checked before anything is
//!   created or written inside it;
//! - notes are created with `O_CREAT|O_EXCL|O_NOFOLLOW` and written in place:
//!   no temp files, no renames, no deletes, no truncation, no write-opens of
//!   pre-existing files;
//! - verification opens with `O_NOFOLLOW|O_NONBLOCK`, so a symlink, FIFO,
//!   socket or device at the path is refused without being followed, read or
//!   waited on.

use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// Publication subdirectory under the Meeting folder.
pub const MEETING_SUBDIR: &str = "cantrip-meeting";

/// Why a publication did not happen. Every variant leaves staging intact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Blocked {
    /// The folder or a publication subdirectory resolves into, onto or around
    /// staging. Nothing was created, written or deleted.
    Overlap,
    /// The folder is missing, not a directory or not writable.
    Unwritable(String),
    /// A note was written but did not verify; it is left alone.
    VerifyFailed(String),
    /// A note verified (regular file, matching digest) but it or one of its
    /// directories up to and including the Meeting folder did not flush to
    /// stable storage; it is left alone and staging is kept.
    FlushFailed(String),
}

/// The directory-flush boundary of publication. The real implementation
/// fsyncs; tests inject `EIO` on a chosen directory (e.g. the Meeting folder)
/// to prove that staging is never deleted before the whole chain is durable.
pub trait ArchiveFs: Send + Sync {
    fn sync_dir(&self, dir: &Path) -> io::Result<()>;
}

/// `fsync(2)` on the directory.
pub struct RealFs;

impl ArchiveFs for RealFs {
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        File::open(dir)?.sync_all()
    }
}

/// Test-only fault injection for verification and flush.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    Verify,
    Flush,
}

fn identity(path: &Path) -> io::Result<(u64, u64)> {
    let m = fs::metadata(path)?;
    Ok((m.dev(), m.ino()))
}

/// True when `staging` is, contains or lies inside `dir` (both resolved,
/// compared by filesystem identity). Errors mean "cannot prove disjoint".
pub fn overlaps(staging: &Path, dir: &Path) -> io::Result<bool> {
    let s = fs::canonicalize(staging)?;
    let d = fs::canonicalize(dir)?;
    let sid = identity(&s)?;
    let did = identity(&d)?;
    // staging is or lies inside dir
    for a in s.ancestors() {
        if identity(a)? == did {
            return Ok(true);
        }
    }
    // dir lies inside staging (staging contains dir)
    for a in d.ancestors() {
        if identity(a)? == sid {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Settings-time check of a chosen folder (invariant 1a). A folder that does
/// not exist yet is judged by its nearest existing ancestor lying inside
/// staging; it is never created.
pub fn folder_choice_overlaps(folder: &Path, staging: &Path) -> io::Result<bool> {
    if fs::metadata(folder).is_ok() {
        return overlaps(staging, folder);
    }
    let s = fs::canonicalize(staging)?;
    let sid = identity(&s)?;
    for a in folder.ancestors().skip(1) {
        if let Ok(c) = fs::canonicalize(a) {
            for b in c.ancestors() {
                if identity(b)? == sid {
                    return Ok(true);
                }
            }
            return Ok(false);
        }
    }
    Ok(false)
}

fn check_disjoint(staging: &Path, dir: &Path) -> Result<(), Blocked> {
    match overlaps(staging, dir) {
        Ok(false) => Ok(()),
        Ok(true) => Err(Blocked::Overlap),
        Err(e) => Err(Blocked::Unwritable(e.kind().to_string())),
    }
}

/// Resolve (creating only what is missing below the chosen folder) the
/// directory `<folder>/cantrip-meeting/<yyyy>/<mm>`, enforcing invariant 1 at
/// every level before creating or writing inside it. Returns the directories
/// from the folder down (all of them are flushed before staging is deleted).
pub fn prepare_dir(
    folder: &Path,
    staging: &Path,
    yyyy: &str,
    mm: &str,
) -> Result<Vec<PathBuf>, Blocked> {
    match fs::metadata(folder) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return Err(Blocked::Unwritable("not a directory".into())),
        Err(e) => return Err(Blocked::Unwritable(e.kind().to_string())),
    }
    check_disjoint(staging, folder)?;
    let mut chain = vec![folder.to_path_buf()];
    let mut cur = folder.to_path_buf();
    for name in [MEETING_SUBDIR, yyyy, mm] {
        let next = cur.join(name);
        match fs::metadata(&next) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Err(Blocked::Unwritable("not a directory".into())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if fs::symlink_metadata(&next).is_ok() {
                    // A dangling symlink: never create through it.
                    return Err(Blocked::Unwritable("dangling link".into()));
                }
                // `cur` was checked disjoint just before this point.
                match fs::create_dir(&next) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(Blocked::Unwritable(e.kind().to_string())),
                }
                let _ = crate::staging::fsync_dir(&cur);
            }
            Err(e) => return Err(Blocked::Unwritable(e.kind().to_string())),
        }
        check_disjoint(staging, &next)?;
        chain.push(next.clone());
        cur = next;
    }
    Ok(chain)
}

/// Candidate file name `n` (1-based) for a note.
pub fn candidate_name(stamp: &str, slug: &str, partial: bool, n: u32) -> String {
    match (partial, n) {
        (false, 1) => format!("{stamp}-{slug}.md"),
        (false, n) => format!("{stamp}-{slug}-{n}.md"),
        (true, 1) => format!("{stamp}-{slug}-partial.md"),
        (true, n) => format!("{stamp}-{slug}-partial-{n}.md"),
    }
}

/// Outcome of one exclusive create.
pub enum Create {
    Created,
    Taken,
}

/// Exclusively create `path` and write `bytes` into it, then fsync it. Never
/// opens an existing file. A write error after creation leaves the
/// incomplete file in place (it will fail verification).
pub fn create_exclusive(path: &Path, bytes: &[u8]) -> io::Result<Create> {
    let mut f = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Ok(Create::Taken),
        Err(e) => return Err(e),
    };
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(Create::Created)
}

pub fn digest(bytes: &[u8]) -> String {
    let d = Sha256::digest(bytes);
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// Verify a published note: a regular file (never following a symlink, never
/// blocking on a FIFO/socket/device) whose content digest matches, then flush
/// it and every directory of its chain (given from the Meeting folder down;
/// flushed deepest first) to stable storage. Any flush error is
/// [`Blocked::FlushFailed`]: the caller keeps staging.
pub fn verify_and_flush(
    path: &Path,
    want_digest: &str,
    dirs: &[PathBuf],
    fault: Option<Fault>,
    afs: &dyn ArchiveFs,
) -> Result<(), Blocked> {
    let fail = |m: &str| Err(Blocked::VerifyFailed(m.to_string()));
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_file() => {}
        Ok(_) => return fail("not a regular file"),
        Err(e) => return fail(&e.kind().to_string()),
    }
    let mut f = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(f) => f,
        Err(e) => return fail(&e.kind().to_string()),
    };
    match f.metadata() {
        Ok(m) if m.file_type().is_file() => {}
        _ => return fail("not a regular file"),
    }
    let mut bytes = Vec::new();
    if let Err(e) = f.read_to_end(&mut bytes) {
        return fail(&e.kind().to_string());
    }
    if fault == Some(Fault::Verify) || digest(&bytes) != want_digest {
        return fail("digest mismatch");
    }
    let flush_failed = |m: &str| Err(Blocked::FlushFailed(m.to_string()));
    if fault == Some(Fault::Flush) || f.sync_all().is_err() {
        return flush_failed("flush failed");
    }
    for d in dirs.iter().rev() {
        if let Err(e) = afs.sync_dir(d) {
            return flush_failed(&e.kind().to_string());
        }
    }
    Ok(())
}

/// Invariant (c) for a recorded publish path
/// `<folder>/cantrip-meeting/<yyyy>/<mm>/<note>`: every directory of its
/// chain, from the Meeting folder down to the note's own directory, must be
/// disjoint from staging. Returns that whole chain (Meeting folder first), so
/// a recorded-path Retry flushes exactly what a first publication flushes. A
/// path not in that layout cannot be accepted.
pub fn recorded_chain(path: &Path, staging: &Path) -> Result<Vec<PathBuf>, Blocked> {
    let layout = || Blocked::Unwritable("unexpected layout".into());
    let mm = path.parent().ok_or_else(layout)?;
    let yyyy = mm.parent().ok_or_else(layout)?;
    let sub = yyyy.parent().ok_or_else(layout)?;
    let folder = sub.parent().ok_or_else(layout)?;
    if sub.file_name() != Some(std::ffi::OsStr::new(MEETING_SUBDIR)) {
        return Err(layout());
    }
    let chain: Vec<PathBuf> = [folder, sub, yyyy, mm]
        .iter()
        .map(|p| p.to_path_buf())
        .collect();
    for d in &chain {
        check_disjoint(staging, d)?;
    }
    Ok(chain)
}

/// True when `a` and `b` are the same directory by filesystem identity.
pub fn same_dir(a: &Path, b: &Path) -> bool {
    matches!((identity(a), identity(b)), (Ok(x), Ok(y)) if x == y)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn overlap_is_by_identity_in_every_direction() {
        let t = tempfile::tempdir().unwrap();
        let staging = t.path().join("stage");
        let inside = staging.join("x");
        fs::create_dir_all(&inside).unwrap();
        let other = t.path().join("other");
        fs::create_dir(&other).unwrap();
        assert!(overlaps(&staging, &staging).unwrap());
        assert!(overlaps(&staging, &inside).unwrap());
        assert!(overlaps(&staging, t.path()).unwrap());
        assert!(!overlaps(&staging, &other).unwrap());
        let link = t.path().join("link");
        symlink(&inside, &link).unwrap();
        assert!(overlaps(&staging, &link).unwrap());
    }

    #[test]
    fn candidate_names_append_counters() {
        assert_eq!(candidate_name("S", "x", false, 1), "S-x.md");
        assert_eq!(candidate_name("S", "x", false, 2), "S-x-2.md");
        assert_eq!(candidate_name("S", "x", true, 1), "S-x-partial.md");
        assert_eq!(candidate_name("S", "x", true, 3), "S-x-partial-3.md");
    }

    #[test]
    fn missing_folder_choice_judged_by_existing_ancestor() {
        let t = tempfile::tempdir().unwrap();
        let staging = t.path().join("stage");
        fs::create_dir(&staging).unwrap();
        assert!(folder_choice_overlaps(&staging.join("a/b"), &staging).unwrap());
        assert!(!folder_choice_overlaps(&t.path().join("elsewhere/a"), &staging).unwrap());
    }
}
