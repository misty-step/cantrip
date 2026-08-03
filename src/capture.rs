//! Audio capture through PipeWire's `pw-record` command.

use anyhow::{anyhow, bail, Context, Result};
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const STOP_TIMEOUT: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// A running `pw-record` process and its output path.
///
/// `stop`/`cancel` consume the recorder and disarm the drop guard. If the
/// recorder is dropped any other way (daemon shutdown, panic), `Drop` kills
/// the child so no orphaned pw-record keeps holding the microphone, and
/// removes the partial recording.
pub struct Recorder {
    child: Child,
    wav_path: PathBuf,
    started_at: Instant,
    disarmed: bool,
}

impl Recorder {
    /// Start a 16 kHz, mono, signed 16-bit WAV recording.
    pub fn start(wav_path: &Path, source: Option<&str>) -> Result<Self> {
        let args = pw_record_args(wav_path, source);
        let child = Command::new("pw-record")
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("starting pw-record for recording {}", wav_path.display()))?;

        tracing::info!("[Capture] recording started");
        Ok(Self {
            child,
            wav_path: wav_path.to_path_buf(),
            started_at: Instant::now(),
            disarmed: false,
        })
    }

    /// Stop the process cleanly and return the completed WAV path.
    pub fn stop(mut self) -> Result<PathBuf> {
        self.disarmed = true;
        stop_child(&mut self.child).with_context(|| "stopping pw-record")?;
        verify_wav(&self.wav_path)?;
        tracing::info!(
            "[Capture] recording stopped after {} ms",
            self.started_at.elapsed().as_millis()
        );
        Ok(std::mem::take(&mut self.wav_path))
    }

    /// Stop the process and remove its partial recording.
    pub fn cancel(mut self) -> Result<()> {
        self.disarmed = true;
        let elapsed = self.started_at.elapsed();
        let stop_result = cancel_child(&mut self.child);
        if let Err(error) = stop_result {
            if let Err(remove_error) = remove_recording(&self.wav_path) {
                tracing::warn!(
                    "[Capture] failed to remove canceled recording {}: {}",
                    self.wav_path.display(),
                    remove_error
                );
            }
            return Err(error).context("canceling pw-record");
        }
        remove_recording(&self.wav_path)?;

        tracing::info!(
            "[Capture] recording canceled after {} ms",
            elapsed.as_millis()
        );
        Ok(())
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }
        tracing::warn!("[Capture] recorder dropped while running; killing pw-record");
        if cancel_child(&mut self.child).is_err() {
            let _ = unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGKILL) };
            let _ = self.child.wait();
        }
        if let Err(error) = remove_recording(&self.wav_path) {
            tracing::warn!(
                "[Capture] failed to remove abandoned recording {}: {}",
                self.wav_path.display(),
                error
            );
        }
    }
}

fn pw_record_args(wav_path: &Path, source: Option<&str>) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("--rate"),
        OsString::from("16000"),
        OsString::from("--channels"),
        OsString::from("1"),
        OsString::from("--format"),
        OsString::from("s16"),
    ];
    if let Some(source) = source {
        args.push(OsString::from("--target"));
        args.push(OsString::from(source));
    }
    args.push(wav_path.as_os_str().to_owned());
    args
}

fn stop_child(child: &mut Child) -> Result<()> {
    if let Some(status) = child.try_wait().context("checking pw-record state")? {
        return Err(already_exited_error(child, status)?);
    }

    send_sigint(child).or_else(|signal_error| {
        if let Some(status) = child
            .try_wait()
            .context("checking pw-record after SIGINT failure")?
        {
            return Err(already_exited_error(child, status)?);
        }
        Err(signal_error)
    })?;

    if let Some(status) = wait_for_exit(child)? {
        // pw-record exits with status 1 on SIGINT by design (verified against
        // PipeWire 1.5.85); the WAV is still finalized. Exit status is not a
        // success signal here — verify_wav() on the produced file is.
        if !status.success() {
            let stderr = read_stderr(child)?;
            tracing::debug!(
                "[Capture] pw-record exit after SIGINT: {status}; stderr: {}",
                display_stderr(&stderr)
            );
        }
        return Ok(());
    }

    let pid = child.id();
    let kill_result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    let kill_error = if kill_result == -1 {
        Some(std::io::Error::last_os_error())
    } else {
        None
    };
    let status = child
        .wait()
        .context("waiting for pw-record after SIGKILL")?;
    let stderr = read_stderr(child)?;
    if let Some(error) = kill_error {
        bail!(
            "pw-record did not stop within {} seconds; SIGKILL failed: {error}; status {status}; stderr: {}",
            STOP_TIMEOUT.as_secs(),
            display_stderr(&stderr)
        );
    }
    bail!(
        "pw-record did not stop within {} seconds; sent SIGKILL; status {status}; stderr: {}",
        STOP_TIMEOUT.as_secs(),
        display_stderr(&stderr)
    );
}

fn cancel_child(child: &mut Child) -> Result<()> {
    if child
        .try_wait()
        .context("checking pw-record state")?
        .is_some()
    {
        return Ok(());
    }

    if let Err(signal_error) = send_sigint(child) {
        if child
            .try_wait()
            .context("checking pw-record after SIGINT failure")?
            .is_some()
        {
            return Ok(());
        }
        return Err(signal_error);
    }

    if wait_for_exit(child)?.is_some() {
        return Ok(());
    }

    let pid = child.id();
    let kill_result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    let status = child
        .wait()
        .context("waiting for pw-record after SIGKILL")?;
    let stderr = read_stderr(child)?;
    if kill_result == -1 {
        let error = std::io::Error::last_os_error();
        bail!(
            "pw-record did not stop within {} seconds; SIGKILL failed: {error}; status {status}; stderr: {}",
            STOP_TIMEOUT.as_secs(),
            display_stderr(&stderr)
        );
    }
    bail!(
        "pw-record did not stop within {} seconds; sent SIGKILL; status {status}; stderr: {}",
        STOP_TIMEOUT.as_secs(),
        display_stderr(&stderr)
    );
}

fn send_sigint(child: &Child) -> Result<()> {
    let pid = child.id();
    let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGINT) };
    if result == -1 {
        return Err(anyhow!(std::io::Error::last_os_error()))
            .with_context(|| format!("sending SIGINT to pw-record process {pid}"));
    }
    Ok(())
}

/// Return `Some(status)` when the process exits before the timeout.
fn wait_for_exit(child: &mut Child) -> Result<Option<ExitStatus>> {
    let deadline = Instant::now() + STOP_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().context("waiting for pw-record")? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn already_exited_error(child: &mut Child, status: ExitStatus) -> Result<anyhow::Error> {
    let stderr = read_stderr(child)?;
    Ok(anyhow!(
        "pw-record exited before stop ({status}); stderr: {}",
        display_stderr(&stderr)
    ))
}

fn read_stderr(child: &mut Child) -> Result<String> {
    let Some(mut stderr) = child.stderr.take() else {
        return Ok(String::new());
    };
    let mut output = String::new();
    stderr
        .read_to_string(&mut output)
        .context("reading pw-record stderr")?;
    Ok(output.trim().to_owned())
}

fn display_stderr(stderr: &str) -> &str {
    if stderr.is_empty() {
        "(no stderr output)"
    } else {
        stderr
    }
}

fn verify_wav(path: &Path) -> Result<()> {
    let metadata =
        fs::metadata(path).with_context(|| format!("checking recorded WAV {}", path.display()))?;
    if metadata.len() <= 44 {
        bail!(
            "recorded WAV {} is {} bytes; expected more than 44 bytes",
            path.display(),
            metadata.len()
        );
    }
    Ok(())
}

fn remove_recording(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing recording {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::pw_record_args;
    use std::path::Path;

    fn strings(args: Vec<std::ffi::OsString>) -> Vec<String> {
        args.into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn pw_record_args_without_source() {
        assert_eq!(
            strings(pw_record_args(Path::new("/tmp/recording.wav"), None)),
            vec![
                "--rate",
                "16000",
                "--channels",
                "1",
                "--format",
                "s16",
                "/tmp/recording.wav"
            ]
        );
    }

    #[test]
    fn pw_record_args_with_source() {
        assert_eq!(
            strings(pw_record_args(
                Path::new("/tmp/recording.wav"),
                Some("alsa_input.pci-1"),
            )),
            vec![
                "--rate",
                "16000",
                "--channels",
                "1",
                "--format",
                "s16",
                "--target",
                "alsa_input.pci-1",
                "/tmp/recording.wav"
            ]
        );
    }
}
