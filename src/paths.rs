//! Filesystem locations. XDG-compliant, all under the `cantrip` name.

use anyhow::{Context, Result};
use std::path::PathBuf;

/// `~/.config/cantrip/`
pub fn config_dir() -> Result<PathBuf> {
    Ok(dirs::config_dir().context("no config dir")?.join("cantrip"))
}

/// `~/.config/cantrip/config.toml`
pub fn config_file() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

/// `~/.local/share/cantrip/`
pub fn data_dir() -> Result<PathBuf> {
    Ok(dirs::data_dir().context("no data dir")?.join("cantrip"))
}

/// `~/.local/share/cantrip/models/`
pub fn models_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("models"))
}

/// `$XDG_RUNTIME_DIR/cantrip/` (falls back to `/tmp/cantrip-$UID`).
/// Holds the control socket and in-flight recordings. Runtime dirs are
/// tmpfs and per-user (0700), so recordings never touch disk.
pub fn runtime_dir() -> Result<PathBuf> {
    let base = dirs::runtime_dir()
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/cantrip-{}", unsafe { libc::getuid() })));
    Ok(base.join("cantrip"))
}

/// `$XDG_RUNTIME_DIR/cantrip/cantrip.sock`
pub fn socket_path() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("cantrip.sock"))
}

/// `$XDG_RUNTIME_DIR/cantrip/hud.lock`
///
/// Single-instance flock target for the HUD: the HUD holds an exclusive
/// lock for its lifetime, and the daemon uses the same lock to detect a
/// missing HUD and respawn it.
pub fn hud_lock_path() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("hud.lock"))
}

/// Create a directory (and parents) if missing, then return it.
pub fn ensure_dir(dir: PathBuf) -> Result<PathBuf> {
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}
