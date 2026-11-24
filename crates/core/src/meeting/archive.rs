//! The append-only, exclusive Meeting-folder archive (DESIGN.md › Publication
//! invariants 1–3). Everything here touches the user's Meeting folder, so it
//! is deliberately small and paranoid:
//!
//! - disjointness from staging is judged by filesystem identity (dev, inode)
//!   after symlink resolution, never by string prefix;
//! - the chosen folder is never created; `cantrip-meeting/YYYY/MM` are created
//!   one level at a time, each existing level checked before anything is
//!   created or written inside it;
//! - a note's complete bytes are written and fsynced *before* it gets a name,
//!   then it appears in one step that can never replace an existing name:
//!   Linux writes an unnamed `O_TMPFILE` inode in the `MM` directory and
//!   `linkat`s it; macOS writes a copy inside the meeting's own staging
//!   directory and moves it with `renameatx_np(RENAME_EXCL)`. `EEXIST` steps to
//!   the next name. Where neither is supported (`EOPNOTSUPP`, `EXDEV`, no
//!   `/proc`, …) the note is created with `O_CREAT|O_EXCL|O_NOFOLLOW` and
//!   written in place. Either way: no temp file names in the Meeting folder, no
//!   renames or deletes there, no truncation, no write-opens of pre-existing
//!   files;
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

/// Outcome of one exclusive publication attempt.
#[derive(Debug, PartialEq, Eq)]
pub enum Create {
    Created,
    Taken,
}

/// Errors meaning "this filesystem (or `/proc`) cannot publish atomically
/// here": the note is then created directly instead. Anything else is a real
/// failure (permissions, space, I/O) and blocks publication.
fn atomic_unsupported(e: &io::Error) -> bool {
    // `ENOTSUP` and `EOPNOTSUPP` are one value on Linux, two on macOS.
    const UNSUPPORTED: [i32; 6] = [
        libc::EOPNOTSUPP,
        libc::ENOTSUP,
        libc::EISDIR,
        libc::EINVAL,
        libc::EXDEV,
        libc::ENOENT,
    ];
    e.raw_os_error().is_some_and(|n| UNSUPPORTED.contains(&n))
}

fn c_path(p: &Path) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(p.as_os_str().as_bytes())
        .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}

/// Linux: an unnamed regular-file inode in `dir` (`O_TMPFILE`). It has no
/// name in the directory, so nothing is visible there until it is linked.
#[cfg(target_os = "linux")]
pub fn open_unnamed_in(dir: &Path) -> io::Result<File> {
    use std::os::fd::FromRawFd;
    let c = c_path(dir)?;
    // SAFETY: `c` is a valid NUL-terminated path; the returned descriptor is
    // owned by the `File` below and nowhere else.
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_TMPFILE | libc::O_WRONLY | libc::O_CLOEXEC,
            // Owner-only, the same mode a directly created note gets.
            0o600 as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a freshly opened descriptor we exclusively own.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// No unnamed inodes outside Linux.
#[cfg(not(target_os = "linux"))]
pub fn open_unnamed_in(_dir: &Path) -> io::Result<File> {
    Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP))
}

/// The filesystem boundary of publication. The real implementation fsyncs,
/// opens `O_TMPFILE` inodes and writes straight through; tests inject `EIO`
/// on a chosen directory (e.g. the Meeting folder) to prove that staging is
/// never deleted before the whole chain is durable, simulate a filesystem
/// without `O_TMPFILE`, and slow or split the note's write to prove no
/// partial note is ever visible.
pub trait ArchiveFs: Send + Sync {
    fn sync_dir(&self, dir: &Path) -> io::Result<()>;

    /// Open an unnamed inode in `dir` (Linux `O_TMPFILE`).
    fn open_unnamed(&self, dir: &Path) -> io::Result<File> {
        open_unnamed_in(dir)
    }

    /// Write the note's complete bytes into a freshly created file.
    fn write_note(&self, f: &mut File, bytes: &[u8]) -> io::Result<()> {
        f.write_all(bytes)
    }
}

/// `fsync(2)` on the directory; real `O_TMPFILE`; straight writes.
pub struct RealFs;

impl ArchiveFs for RealFs {
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        File::open(dir)?.sync_all()
    }
}

/// Exclusively create `path` and write `bytes` into it, then fsync it. Never
/// opens an existing file. A write error after creation leaves the
/// incomplete file in place (it will fail verification). This is the
/// fallback where atomic publication is unsupported: the name exists before
/// its bytes do.
fn create_direct(path: &Path, bytes: &[u8], afs: &dyn ArchiveFs) -> io::Result<Create> {
    let mut f = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Ok(Create::Taken),
        Err(e) => return Err(e),
    };
    afs.write_note(&mut f, bytes)?;
    f.sync_all()?;
    Ok(Create::Created)
}

/// How a prepared note will be published.
enum How {
    /// Linux: complete, fsynced bytes in an unnamed inode of the `MM`
    /// directory, published by `linkat` (which never replaces a name).
    #[cfg(target_os = "linux")]
    Unnamed(File),
    /// macOS: a complete, fsynced copy in the meeting's own staging
    /// directory, published by `renameatx_np(RENAME_EXCL)`.
    #[cfg(target_os = "macos")]
    StagedCopy(PathBuf),
    /// No atomic publication here: exclusive create, then write.
    Direct,
}

/// A note ready to appear under a name in the `MM` directory: whole and in
/// one step where the filesystem supports it (DESIGN.md › Publication
/// invariant 2). The primary staged `note.md` is never touched.
pub struct Prepared<'a> {
    bytes: &'a [u8],
    afs: &'a dyn ArchiveFs,
    how: How,
}

/// Write `bytes` somewhere no Meeting-folder listing can see and fsync them,
/// before any name is chosen. `dir` is the `MM` directory; `scratch` is the
/// meeting's own staging directory (macOS only; never inside the Meeting
/// folder).
pub fn prepare<'a>(
    dir: &Path,
    scratch: &Path,
    bytes: &'a [u8],
    afs: &'a dyn ArchiveFs,
) -> io::Result<Prepared<'a>> {
    let how = prepare_how(dir, scratch, bytes, afs)?;
    Ok(Prepared { bytes, afs, how })
}

#[cfg(target_os = "linux")]
fn prepare_how(dir: &Path, _scratch: &Path, bytes: &[u8], afs: &dyn ArchiveFs) -> io::Result<How> {
    match afs.open_unnamed(dir) {
        Ok(mut f) => {
            afs.write_note(&mut f, bytes)?;
            f.sync_all()?;
            Ok(How::Unnamed(f))
        }
        Err(e) if atomic_unsupported(&e) => Ok(How::Direct),
        Err(e) => Err(e),
    }
}

#[cfg(target_os = "macos")]
fn prepare_how(_dir: &Path, scratch: &Path, bytes: &[u8], afs: &dyn ArchiveFs) -> io::Result<How> {
    let tmp = scratch.join(format!("publish-{}.tmp", crate::staging::unique_suffix()));
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&tmp)?;
    match afs.write_note(&mut f, bytes).and_then(|()| f.sync_all()) {
        Ok(()) => Ok(How::StagedCopy(tmp)),
        Err(e) => {
            drop(f);
            // Inside staging only, never the Meeting folder.
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn prepare_how(_: &Path, _: &Path, _: &[u8], _: &dyn ArchiveFs) -> io::Result<How> {
    Ok(How::Direct)
}

impl Prepared<'_> {
    /// True when the next [`Prepared::publish_at`] is atomic.
    pub fn atomic(&self) -> bool {
        !matches!(self.how, How::Direct)
    }

    /// Publish the prepared note at `path` without ever replacing or opening
    /// anything already there: `Taken` when the name exists (the prepared
    /// bytes stay ready for the next name).
    pub fn publish_at(&mut self, path: &Path) -> io::Result<Create> {
        match self.publish_atomic(path) {
            Some(Ok(c)) => Ok(c),
            Some(Err(e)) if atomic_unsupported(&e) => {
                self.discard();
                create_direct(path, self.bytes, self.afs)
            }
            Some(Err(e)) => Err(e),
            None => create_direct(path, self.bytes, self.afs),
        }
    }

    /// `None` when this note is published directly.
    fn publish_atomic(&mut self, path: &Path) -> Option<io::Result<Create>> {
        match &self.how {
            #[cfg(target_os = "linux")]
            How::Unnamed(f) => {
                use std::os::fd::AsRawFd;
                Some(link_unnamed(f.as_raw_fd(), path))
            }
            #[cfg(target_os = "macos")]
            How::StagedCopy(tmp) => {
                let r = rename_excl(tmp, path);
                if matches!(r, Ok(Create::Created)) {
                    // Consumed by the rename: nothing left to unlink.
                    self.how = How::Direct;
                }
                Some(r)
            }
            How::Direct => None,
        }
    }

    /// Drop the prepared bytes (an unnamed inode simply vanishes; a macOS
    /// staged copy is unlinked inside staging, never in the Meeting folder).
    fn discard(&mut self) {
        #[cfg(target_os = "macos")]
        if let How::StagedCopy(tmp) = &self.how {
            let _ = fs::remove_file(tmp);
        }
        self.how = How::Direct;
    }
}

impl Drop for Prepared<'_> {
    fn drop(&mut self) {
        self.discard();
    }
}

/// `linkat(AT_FDCWD, "/proc/self/fd/N", AT_FDCWD, path, AT_SYMLINK_FOLLOW)`:
/// gives the unnamed inode its name in one step; `EEXIST` if anything (a
/// file, a directory, a dangling symlink) already has that name.
#[cfg(target_os = "linux")]
fn link_unnamed(fd: std::os::fd::RawFd, path: &Path) -> io::Result<Create> {
    let from = c_path(Path::new(&format!("/proc/self/fd/{fd}")))?;
    let to = c_path(path)?;
    // SAFETY: both are valid NUL-terminated paths; `fd` is open for the
    // duration of the call.
    let r = unsafe {
        libc::linkat(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::AT_SYMLINK_FOLLOW,
        )
    };
    if r == 0 {
        return Ok(Create::Created);
    }
    let e = io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::EEXIST) {
        Ok(Create::Taken)
    } else {
        Err(e)
    }
}

/// `renameatx_np(…, RENAME_EXCL)`: moves the staged copy to `path` in one
/// step; `EEXIST` (never a replacement) if the name exists.
#[cfg(target_os = "macos")]
fn rename_excl(from: &Path, to: &Path) -> io::Result<Create> {
    let f = c_path(from)?;
    let t = c_path(to)?;
    // SAFETY: both are valid NUL-terminated paths.
    let r = unsafe {
        libc::renameatx_np(
            libc::AT_FDCWD,
            f.as_ptr(),
            libc::AT_FDCWD,
            t.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if r == 0 {
        return Ok(Create::Created);
    }
    let e = io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::EEXIST) {
        Ok(Create::Taken)
    } else {
        Err(e)
    }
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

    /// Published notes are owner-only (`0600`) on the atomic path and on the
    /// direct exclusive-create fallback (a `0644` regression shows under any
    /// usual umask: `022`, `002`), on any host filesystem.
    #[test]
    fn published_notes_are_owner_only_on_every_path() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("mm");
        let scratch = t.path().join("staging");
        fs::create_dir(&dir).unwrap();
        fs::create_dir(&scratch).unwrap();
        let note = b"# a note\n";
        let atomic = dir.join("atomic.md");
        let mut prepared = prepare(&dir, &scratch, note, &RealFs).unwrap();
        // A host filesystem without `O_TMPFILE` (`EOPNOTSUPP`: some overlay,
        // network and FUSE mounts) prepares the direct path instead; the mode
        // contract holds either way. Atomicity itself is asserted by the
        // dedicated atomic-publication tests.
        if !prepared.atomic() {
            eprintln!("note: O_TMPFILE unsupported here; checked the direct path twice");
        }
        assert_eq!(prepared.publish_at(&atomic).unwrap(), Create::Created);
        drop(prepared);

        let direct = dir.join("direct.md");
        assert_eq!(
            create_direct(&direct, note, &RealFs).unwrap(),
            Create::Created
        );

        for p in [&atomic, &direct] {
            assert_eq!(fs::read(p).unwrap(), note);
            let mode = fs::metadata(p).unwrap().mode() & 0o777;
            assert_eq!(mode, 0o600, "{p:?}: {mode:o}");
        }
    }

    /// The atomic path on the real filesystem: wherever the host supports
    /// `O_TMPFILE` (ext4, xfs, btrfs, tmpfs), publication must be atomic and
    /// the note owner-only. Hosts that refuse it are reported and skipped.
    #[cfg(target_os = "linux")]
    #[test]
    fn real_fs_publishes_atomically_where_o_tmpfile_works() {
        use std::os::unix::ffi::OsStrExt;
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("mm");
        fs::create_dir(&dir).unwrap();
        let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).unwrap();
        // SAFETY: probe open of an unnamed inode; closed at once.
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_TMPFILE | libc::O_WRONLY, 0o600) };
        if fd < 0 {
            eprintln!(
                "skip: O_TMPFILE unsupported on this host filesystem ({})",
                io::Error::last_os_error()
            );
            return;
        }
        // SAFETY: fd was just opened above.
        unsafe { libc::close(fd) };
        let mut prepared = prepare(&dir, t.path(), b"n", &RealFs).unwrap();
        assert!(
            prepared.atomic(),
            "O_TMPFILE works here: publish atomically"
        );
        let p = dir.join("n.md");
        assert_eq!(prepared.publish_at(&p).unwrap(), Create::Created);
        assert_eq!(fs::metadata(&p).unwrap().mode() & 0o777, 0o600);
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
