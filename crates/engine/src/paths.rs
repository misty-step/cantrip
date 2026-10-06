//! Native filesystem locations: unchanged XDG paths on Linux, Application
//! Support and a short owner-private runtime directory on macOS.
//! Explicit absolute XDG_CONFIG_HOME, XDG_DATA_HOME, XDG_STATE_HOME and
//! XDG_RUNTIME_DIR remain supported macOS isolation overrides.

use anyhow::{Context, Result};
#[cfg(target_os = "macos")]
use std::fs::{self, OpenOptions};
#[cfg(target_os = "macos")]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(target_os = "macos")]
use std::path::Path;
use std::path::PathBuf;

/// Linux: `~/.config/cantrip/`. macOS: `~/Library/Application Support/cantrip/`.
pub fn config_dir() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        match directory_override("XDG_CONFIG_HOME")? {
            Some(base) => Ok(base.join("cantrip")),
            None => application_support_dir(),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(dirs::config_dir().context("no config dir")?.join("cantrip"))
    }
}

/// `config.toml` in the native configuration directory.
pub fn config_file() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

/// Linux: `~/.local/share/cantrip/`. macOS shares the native support root
/// with configuration; model data and retained state use separate subdirectories.
pub fn data_dir() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        match directory_override("XDG_DATA_HOME")? {
            Some(base) => Ok(base.join("cantrip")),
            None => application_support_dir(),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(dirs::data_dir().context("no data dir")?.join("cantrip"))
    }
}

/// `models/` in the native data directory.
pub fn models_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("models"))
}

/// Linux: `$XDG_RUNTIME_DIR/cantrip/`, falling back to
/// `/tmp/cantrip-$UID/cantrip/`. macOS: `/private/tmp/cantrip-$UID/cantrip/`.
/// Holds the control socket and in-flight recordings in a 0700 directory.
/// Only an actual tmpfs runtime mount is RAM-backed; temporary paths can use disk.
pub fn runtime_dir() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        if let Some(base) = directory_override("XDG_RUNTIME_DIR")? {
            return Ok(base.join("cantrip"));
        }
        // Darwin's per-user temporary directory may exceed its 104-byte
        // sun_path. This canonical system path is short and independent of
        // TMPDIR; ensure_dir validates and privatizes both owned components.
        Ok(PathBuf::from(format!("/private/tmp/cantrip-{}", unsafe {
            libc::getuid()
        }))
        .join("cantrip"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let base = dirs::runtime_dir().unwrap_or_else(|| {
            PathBuf::from(format!("/tmp/cantrip-{}", unsafe { libc::getuid() }))
        });
        Ok(base.join("cantrip"))
    }
}

/// `cantrip.sock` in the native runtime directory.
pub fn socket_path() -> Result<PathBuf> {
    let path = runtime_dir()?.join("cantrip.sock");
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        anyhow::ensure!(
            path.as_os_str().as_bytes().len() < address.sun_path.len(),
            "native runtime path exceeds Unix socket address capacity"
        );
    }
    Ok(path)
}

/// `hud.lock` in the native runtime directory.
///
/// Single-instance flock target for the HUD: the HUD holds an exclusive
/// lock for its lifetime, and the daemon uses the same lock to detect a
/// missing HUD and respawn it.
pub fn hud_lock_path() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("hud.lock"))
}

/// Linux: `~/.local/state/cantrip/` (data fallback if state is unavailable).
/// macOS: `~/Library/Application Support/cantrip/state/`.
/// Durable operator-facing state that survives reboot.
pub fn state_dir() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        match directory_override("XDG_STATE_HOME")? {
            Some(base) => Ok(base.join("cantrip")),
            None => Ok(application_support_dir()?.join("state")),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let base = dirs::state_dir()
            .or_else(dirs::data_dir)
            .context("no state dir")?;
        Ok(base.join("cantrip"))
    }
}

/// `daemon.log` in the durable state directory.
pub fn daemon_log_path() -> Result<PathBuf> {
    Ok(state_dir()?.join("daemon.log"))
}

/// `transcripts/` in the durable state directory: canonical owner-private
/// history JSON and matching per-take recovery WAV sidecars.
pub fn transcript_history_dir() -> Result<PathBuf> {
    Ok(state_dir()?.join("transcripts"))
}

/// Create a directory and missing parents. macOS additionally validates the
/// native roots and opens owned application directories without following links,
/// then normalizes their permissions to 0700.
pub fn ensure_dir(dir: PathBuf) -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let runtime = runtime_dir()?;
        if dir.starts_with(&runtime) {
            ensure_private_directory(runtime.parent().context("no runtime parent")?)?;
        }
        // Privatize the application root as well as its requested descendant.
        // Derive it from the selected path, not from another location that
        // could point at real user data during an isolated invocation.
        if let Some(root) = dir
            .ancestors()
            .find(|path| path.file_name().is_some_and(|name| name == "cantrip"))
        {
            if root != dir.as_path() {
                ensure_private_directory(root)?;
            }
        }
        ensure_private_directory(&dir)?;
    }
    #[cfg(not(target_os = "macos"))]
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// Normalize an owned regular native file by its open descriptor. macOS mode
/// bits do not restrict extended ACL grants; never normalize a substituted path.
#[cfg(target_os = "macos")]
pub fn privatize_file(file: &std::fs::File) -> Result<()> {
    let metadata = file
        .metadata()
        .context("checking private application file")?;
    anyhow::ensure!(
        metadata.is_file() && metadata.nlink() == 1 && metadata.uid() == unsafe { libc::getuid() },
        "application file is not a singly linked regular file owned by the current user"
    );
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .context("setting private application file permissions")?;
    crate::archive::clear_inherited_acl(file).context("privatizing application file ACL")?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn application_support_dir() -> Result<PathBuf> {
    // Resolve only the OS-selected home trust boundary. Native homes can use
    // legitimate /Users symlinks; history/artifact symlinks remain forbidden.
    let home = dirs::home_dir()
        .context("no home directory")?
        .canonicalize()
        .context("resolving native home directory")?;
    crate::archive::check_ancestors(&home)?;
    let metadata = fs::metadata(&home).context("checking native home directory")?;
    anyhow::ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::getuid() }
            && metadata.permissions().mode() & 0o022 == 0,
        "native home directory is not a trusted directory owned by the current user"
    );
    Ok(home.join("Library/Application Support/cantrip"))
}

#[cfg(target_os = "macos")]
fn directory_override(variable: &str) -> Result<Option<PathBuf>> {
    let Some(value) = std::env::var_os(variable).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let path = PathBuf::from(value);
    anyhow::ensure!(
        path.is_absolute(),
        "{variable} must be an absolute directory path"
    );
    let base = canonical_directory(&path).with_context(|| format!("resolving {variable}"))?;
    let mut existing = base.as_path();
    let metadata = loop {
        match fs::metadata(existing) {
            Ok(metadata) => break metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                existing = existing
                    .parent()
                    .context("override has no existing ancestor")?;
            }
            Err(error) => return Err(error).with_context(|| format!("checking {variable}")),
        }
    };
    let owner = metadata.uid();
    let mode = metadata.permissions().mode();
    anyhow::ensure!(
        metadata.is_dir()
            && (owner == unsafe { libc::getuid() }
                || (owner == 0 && (variable != "XDG_RUNTIME_DIR" || existing != base.as_path())))
            && (mode & 0o022 == 0 || (owner == 0 && mode & libc::S_ISVTX as u32 != 0)),
        "{variable} is not a trusted directory"
    );
    Ok(Some(base))
}

#[cfg(target_os = "macos")]
fn canonical_directory(path: &Path) -> Result<PathBuf> {
    match path.canonicalize() {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .context("directory has no existing ancestor")?;
            let name = path.file_name().context("directory has no filename")?;
            let mut canonical = canonical_directory(parent)?;
            canonical.push(name);
            Ok(canonical)
        }
        Err(error) => Err(error).context("canonicalizing directory"),
    }
}

#[cfg(target_os = "macos")]
fn ensure_private_directory(directory: &Path) -> Result<()> {
    crate::archive::check_ancestors(directory)?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)
        .with_context(|| format!("creating private directory {}", directory.display()))?;
    crate::archive::check_ancestors(directory)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(directory)
        .context("opening private application directory")?;
    let metadata = file
        .metadata()
        .context("checking private application directory")?;
    anyhow::ensure!(
        metadata.is_dir() && metadata.uid() == unsafe { libc::getuid() },
        "application directory is not owned by the current user"
    );
    file.set_permissions(fs::Permissions::from_mode(0o700))
        .context("setting private application directory permissions")?;
    crate::archive::clear_inherited_acl(&file).context("privatizing application directory ACL")?;
    Ok(())
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn explicit_native_base_is_canonicalized_but_artifact_links_are_not_followed() {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("cantrip-paths-{}", crate::recovery::new_id()));
        fs::create_dir(&root).unwrap();
        let actual = root.join("actual");
        let alias = root.join("explicit-base");
        fs::create_dir(&actual).unwrap();
        symlink(&actual, &alias).unwrap();
        let selected = canonical_directory(&alias.join("missing/cantrip")).unwrap();
        assert_eq!(selected, actual.join("missing/cantrip"));
        ensure_private_directory(&selected).unwrap();
        assert_eq!(
            fs::metadata(&selected).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let victim = root.join("victim");
        fs::create_dir(&victim).unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o755)).unwrap();
        let artifact_link = selected.join("state");
        symlink(&victim, &artifact_link).unwrap();
        assert!(ensure_private_directory(&artifact_link).is_err());
        assert_eq!(
            fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
            0o755
        );
        fs::remove_dir_all(root).unwrap();
    }
}
