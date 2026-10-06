//! Desktop host lifecycle. Dictation state and durable work live in the engine.

use anyhow::Result;
use cantrip_engine::config::Config;
use std::sync::Arc;

pub fn run(config: Config, preload: bool) -> Result<()> {
    let platform = Arc::new(crate::platform::NativePlatform);
    #[cfg(target_os = "linux")]
    let lifetime = Arc::new(std::sync::atomic::AtomicBool::new(true));
    #[cfg(target_os = "linux")]
    let supervisor_lifetime = lifetime.clone();
    let result = cantrip_engine::engine::run(config, preload, platform, move || {
        #[cfg(target_os = "linux")]
        match cantrip_engine::paths::runtime_dir() {
            Ok(directory) => start_hud_supervisor(directory, supervisor_lifetime),
            Err(error) => tracing::warn!("[Daemon] HUD runtime directory unavailable: {error:#}"),
        }
    });
    #[cfg(target_os = "linux")]
    lifetime.store(false, std::sync::atomic::Ordering::Release);
    result
}

#[cfg(target_os = "linux")]
fn start_hud_supervisor(
    runtime_dir: std::path::PathBuf,
    lifetime: Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};
    const COOLDOWN: Duration = Duration::from_secs(30);
    std::thread::spawn(move || {
        let mut last_spawn = Instant::now()
            .checked_sub(COOLDOWN)
            .unwrap_or_else(Instant::now);
        while lifetime.load(Ordering::Acquire) {
            if last_spawn.elapsed() >= COOLDOWN {
                match crate::hud::acquire_instance_lock() {
                    Ok(Some(lock)) => {
                        drop(lock);
                        last_spawn = Instant::now();
                        match spawn_hud(&runtime_dir) {
                            Ok(()) => tracing::info!("[Daemon] HUD not running; spawned it"),
                            Err(error) => tracing::warn!("[Daemon] spawning HUD failed: {error:#}"),
                        }
                    }
                    Ok(None) => {}
                    Err(error) => tracing::warn!("[Daemon] HUD lock check failed: {error:#}"),
                }
            }
            std::thread::sleep(Duration::from_secs(5));
        }
    });
}

#[cfg(target_os = "linux")]
fn spawn_hud(runtime_dir: &std::path::Path) -> Result<()> {
    use anyhow::Context;
    use std::fs;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let executable = std::env::current_exe().context("locating the cantrip binary")?;
    let log_path = runtime_dir.join("hud.log");
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening HUD log {}", log_path.display()))?;
    let mut command = Command::new(executable);
    command
        .arg("hud")
        .stdin(Stdio::null())
        .stdout(Stdio::from(
            log.try_clone().context("cloning HUD log handle")?,
        ))
        .stderr(Stdio::from(log));
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    command.spawn().context("spawning the HUD")?;
    Ok(())
}
