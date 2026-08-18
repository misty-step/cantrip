//! Text injection through Wayland typing tools or the clipboard.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Busiest budget any single injection helper may take before it is killed.
///
/// Applied identically to `wtype`, `ydotool`, `wl-copy`, and the paste-shortcut
/// keypress so one policy governs every backend. The reason this is bounded is
/// that the daemon must stay responsive: injection runs on the processing path,
/// and a wedged helper must never stall transcription or the daemon's worker
/// thread indefinitely. Five seconds is well past any legitimate paste of a long
/// transcript while leaving headroom against a hung child.
const INJECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Terminal-safe paste chord. `Ctrl+V` is intercepted by TUIs (OMP shows
/// "Clipboard is empty" and reads a host clipboard that is empty in remote
/// Herdr/SSH sessions). `Ctrl+Shift+V` is the compositor/terminal paste, so
/// the Wayland selection `wl-copy` just wrote is what lands in the focused app.
const PASTE_WTYPE_ARGS: &[&str] = &[
    "-M", "ctrl", "-M", "shift", "-k", "v", "-m", "shift", "-m", "ctrl",
];
/// 29 = LeftCtrl, 42 = LeftShift, 47 = V.
const PASTE_YDOTOOL_ARGS: &[&str] = &["key", "29:1", "42:1", "47:1", "47:0", "42:0", "29:0"];

/// Wait for `child` within `timeout`; on timeout kill and reap it, returning an
/// error so the caller can fall back to the next backend, never blocking forever.
fn wait_child_bounded(child: &mut Child, timeout: Duration) -> Result<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait().context("polling inject helper status")? {
            Some(status) => return Ok(status),
            None => {
                if Instant::now() >= deadline {
                    // Reap the zombie so we never leak a wedged helper behind us.
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!(
                        "inject helper exceeded {}s budget (daemon must stay responsive)",
                        timeout.as_secs()
                    );
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// Spawn `command` and wait for it within `timeout`, returning its exit status.
///
/// Used by the process-boundary tests with fake helpers on `PATH`; backends call
/// [`wait_child`] which applies the shared [`INJECT_TIMEOUT`] policy.
fn run_bounded(command: &mut Command, timeout: Duration) -> Result<ExitStatus> {
    let mut child = command.spawn().context("starting injection helper")?;
    wait_child_bounded(&mut child, timeout)
}

/// Write all of `bytes` into `child`'s stdin within `timeout`.
///
/// The write is bounded for exactly the same reason [`wait_child_bounded`] is:
/// a helper that never reads its input leaves the kernel pipe buffer full, so
/// a blocking `write_all` would stall the daemon's processing path without any
/// bound. Setting the pipe write end non-blocking and polling it against the
/// same deadline means a wedged, non-reading child is killed and reaped instead
/// of blocking forever. On success or on a normal error the local `stdin` is
/// dropped here, closing the pipe so the helper sees EOF (as before).
fn write_stdin_bounded(child: &mut Child, bytes: &[u8], timeout: Duration) -> Result<()> {
    let Some(mut stdin) = child.stdin.take() else {
        return Ok(());
    };
    set_nonblocking(&stdin)?;
    let deadline = Instant::now() + timeout;
    let mut offset = 0;
    while offset < bytes.len() {
        if Instant::now() >= deadline {
            // Reap the zombie so we never leak a wedged helper behind us.
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "inject helper did not drain stdin within {}s (daemon must stay responsive)",
                timeout.as_secs()
            );
        }
        match stdin.write(&bytes[offset..]) {
            Ok(written) if written > 0 => offset += written,
            Ok(_) => {
                // A zero-length write means nothing further can be sent.
                break;
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                // Pipe full because the child is not draining it; wait briefly
                // and retry up to the deadline rather than blocking forever.
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).context("writing text to helper stdin");
            }
        }
    }
    Ok(())
}

/// Mark the write end of `stream`'s pipe as non-blocking so a full pipe cannot
/// block the writer; the caller polls it against a deadline instead.
fn set_nonblocking(stream: &impl AsRawFd) -> Result<()> {
    let fd = stream.as_raw_fd();
    // SAFETY: fcntl(_, F_GETFL/F_SETFL) operates on an owned fd and does not
    // touch memory we do not control; the kernel reports failure via the return.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        bail!(
            "reading stdin pipe flags (errno {})",
            std::io::Error::last_os_error()
        );
    }
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if rc == -1 {
        bail!(
            "setting stdin pipe non-blocking (errno {})",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

/// Select how text is inserted into the focused Wayland application.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum InjectionMode {
    #[default]
    Auto,
    /// Copy, then send one Ctrl+Shift+V to the focused app (paragraph-safe).
    Paste,
    Type,
    Clipboard,
}

/// The backend that inserted text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectionOutcome {
    Typed(&'static str),
    /// Copied to the clipboard and pasted with a single Ctrl+Shift+V.
    Pasted,
    Clipboard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    Paste,
    Wtype,
    Ydotool,
    Clipboard,
}

impl Backend {
    fn name(self) -> &'static str {
        match self {
            Self::Paste => "paste",
            Self::Wtype => "wtype",
            Self::Ydotool => "ydotool",
            Self::Clipboard => "clipboard",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct AvailableBackends {
    wtype: bool,
    ydotool: bool,
    wl_copy: bool,
}

/// True when the daemon may retry via clipboard after the primary mode fails.
/// Only Auto may degrade: Type must never touch the clipboard; Paste is strict.
pub fn allows_clipboard_fallback(mode: InjectionMode) -> bool {
    matches!(mode, InjectionMode::Auto)
}

/// Inject text with the requested backend policy.
pub fn inject(text: &str, mode: InjectionMode) -> Result<InjectionOutcome> {
    let available = AvailableBackends {
        wtype: executable_in_path("wtype"),
        ydotool: executable_in_path("ydotool") && ydotool_socket_exists(),
        wl_copy: executable_in_path("wl-copy"),
    };
    let order = backend_order(mode, available);
    let mut failures = Vec::new();

    // Paste and clipboard-copy carry the text verbatim, so paragraph breaks
    // survive. Typing backends translate control characters into key presses
    // (wtype: \n = Return, \t = Tab, \e = Escape; ydotool likewise), which
    // would make the focused app submit mid-transcript — so the typing
    // fallback flattens controls to spaces and cannot represent paragraphs.
    let typed = normalize_for_typing(text);

    for backend in order {
        let (result, payload) = match backend {
            Backend::Paste => (run_paste(text), text),
            Backend::Wtype => (run_wtype(&typed), typed.as_str()),
            Backend::Ydotool => (run_ydotool(&typed), typed.as_str()),
            Backend::Clipboard => (run_clipboard(text), text),
        };
        match result {
            Ok(()) => {
                tracing::info!(
                    "[Inject] backend={} chars={}",
                    backend.name(),
                    payload.chars().count()
                );
                return Ok(match backend {
                    Backend::Paste => InjectionOutcome::Pasted,
                    Backend::Wtype => InjectionOutcome::Typed("wtype"),
                    Backend::Ydotool => InjectionOutcome::Typed("ydotool"),
                    Backend::Clipboard => InjectionOutcome::Clipboard,
                });
            }
            Err(error) => failures.push(format!("{}: {error}", backend.name())),
        }
    }

    let details = if failures.is_empty() {
        "no backend was available".to_owned()
    } else {
        failures.join("; ")
    };
    match mode {
        InjectionMode::Type => bail!(
            "typing injection failed ({details}); install wtype or ydotool, ensure ydotool has a running daemon, or set injection = \"clipboard\""
        ),
        InjectionMode::Auto => bail!("all injection backends failed ({details})"),
        InjectionMode::Paste => bail!(
            "paste injection failed ({details}); install wl-clipboard and wtype (or a ydotool daemon)"
        ),
        InjectionMode::Clipboard => bail!(
            "clipboard injection failed ({details}); install wl-clipboard so wl-copy is available"
        ),
    }
}

fn backend_order(mode: InjectionMode, available: AvailableBackends) -> Vec<Backend> {
    let paste_ready = available.wl_copy && (available.wtype || available.ydotool);
    match mode {
        InjectionMode::Auto => {
            let mut order = Vec::with_capacity(4);
            if paste_ready {
                order.push(Backend::Paste);
            }
            if available.wtype {
                order.push(Backend::Wtype);
            }
            if available.ydotool {
                order.push(Backend::Ydotool);
            }
            order.push(Backend::Clipboard);
            order
        }
        InjectionMode::Paste => {
            if paste_ready {
                vec![Backend::Paste]
            } else {
                Vec::new()
            }
        }
        InjectionMode::Type => {
            let mut order = Vec::with_capacity(2);
            if available.wtype {
                order.push(Backend::Wtype);
            }
            if available.ydotool {
                order.push(Backend::Ydotool);
            }
            order
        }
        InjectionMode::Clipboard => vec![Backend::Clipboard],
    }
}

/// Names of the backends the injection policy would try for a known tool set.
///
/// `ydotool_ready` means both the executable and a daemon socket are present.
/// The doctor command uses this wrapper around the same order as [`inject`].
pub fn planned_backend_names(
    mode: InjectionMode,
    wtype: bool,
    ydotool_ready: bool,
    wl_copy: bool,
) -> Vec<&'static str> {
    backend_order(
        mode,
        AvailableBackends {
            wtype,
            ydotool: ydotool_ready,
            wl_copy,
        },
    )
    .into_iter()
    .map(Backend::name)
    .collect()
}

/// True when `name` resolves to an executable file on `PATH`.
/// `pub` (not `pub(crate)`): the `cantrip` binary is a separate crate.
pub fn executable_in_path(name: &str) -> bool {
    let Some(path_var) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&path_var).any(|directory| {
        let directory = if directory.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            directory
        };
        let candidate = directory.join(name);
        match candidate.metadata() {
            Ok(metadata) => metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    })
}

/// Candidate ydotool daemon sockets, highest priority first.
/// Shared by injection and `cantrip doctor`.
pub fn ydotool_socket_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::with_capacity(5);
    if let Some(path) = env::var_os("YDOTOOL_SOCKET").filter(|socket| !socket.is_empty()) {
        candidates.push(PathBuf::from(path));
    }
    if let Some(runtime) = env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()) {
        candidates.push(PathBuf::from(runtime).join(".ydotool_socket"));
    } else {
        // Fallback when XDG_RUNTIME_DIR is unset (same layout as typical user sessions).
        candidates.push(PathBuf::from(format!(
            "/run/user/{}/.ydotool_socket",
            unsafe { libc::getuid() }
        )));
    }
    candidates.push(PathBuf::from("/tmp/.ydotool_socket"));
    candidates.push(PathBuf::from("/run/ydotoold.socket"));
    candidates
}

/// First existing ydotool socket from [`ydotool_socket_candidates`], if any.
pub fn find_ydotool_socket() -> Option<PathBuf> {
    ydotool_socket_candidates()
        .into_iter()
        .find(|path| path.exists())
}

fn ydotool_socket_exists() -> bool {
    find_ydotool_socket().is_some()
}

/// Replace control characters with a single space so a transcript can never
/// act as keys: wtype maps `\n` to Return, `\t` to Tab, `\e` to Escape (and
/// ydotool maps newlines to Enter), which would make the focused app submit
/// or navigate mid-injection. Consecutive controls collapse into one space.
fn normalize_for_typing(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for character in text.chars() {
        if character.is_control() {
            pending_space = true;
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(character);
        }
    }
    if pending_space {
        out.push(' ');
    }
    out
}

fn run_wtype(text: &str) -> Result<()> {
    let status = run_bounded(
        Command::new("wtype")
            .arg("--")
            .arg(text)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
        INJECT_TIMEOUT,
    )
    .context("running wtype")?;
    if !status.success() {
        bail!("wtype exited with status {}", status);
    }
    Ok(())
}

/// Spawn `command` with a piped stdin, write `text` into it, then wait for the
/// child to exit — with **both** the write and the wait bounded by `timeout`.
///
/// Shared by `ydotool` and `wl-copy`, the two backends that feed the helper its
/// payload over stdin. Bounding the write matters as much as bounding the wait:
/// a helper that never reads leaves the pipe full, and without a bound the
/// write alone would stall the daemon forever before the wait ever ran.
fn run_writer_backend(
    command: &mut Command,
    text: &str,
    name: &str,
    timeout: Duration,
) -> Result<()> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("starting {name}"))?;

    write_stdin_bounded(&mut child, text.as_bytes(), timeout)
        .with_context(|| format!("writing text to {name}"))?;

    let status =
        wait_child_bounded(&mut child, timeout).with_context(|| format!("waiting for {name}"))?;
    if !status.success() {
        bail!("{name} exited with status {}", status);
    }
    Ok(())
}

fn run_ydotool(text: &str) -> Result<()> {
    // ydotool reads the transcript over stdin (`--file -`).
    run_writer_backend(
        Command::new("ydotool").args(["type", "--file", "-"]),
        text,
        "ydotool",
        INJECT_TIMEOUT,
    )
}

fn run_clipboard(text: &str) -> Result<()> {
    // wl-copy reads the clipboard payload over stdin.
    run_writer_backend(
        &mut Command::new("wl-copy"),
        text,
        "wl-copy",
        INJECT_TIMEOUT,
    )
}

/// Copy the text, then send one Ctrl+Shift+V to the focused app. Nothing is
/// typed into a live window, so losing focus mid-composition is harmless: the
/// clipboard was fully written before the single paste keypress. The text
/// stays on the clipboard (wl-copy keeps holding the selection) for the
/// user to paste again by hand.
fn run_paste(text: &str) -> Result<()> {
    run_clipboard(text)?;
    send_paste_shortcut()
}

/// Emit a Ctrl+Shift+V keypress through wtype (or a ydotool key sequence).
fn send_paste_shortcut() -> Result<()> {
    if executable_in_path("wtype") {
        let status = run_bounded(
            Command::new("wtype")
                .args(PASTE_WTYPE_ARGS)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
            INJECT_TIMEOUT,
        )
        .context("sending paste shortcut with wtype")?;
        if status.success() {
            return Ok(());
        }
    }
    if executable_in_path("ydotool") && ydotool_socket_exists() {
        let status = run_bounded(
            Command::new("ydotool")
                .args(PASTE_YDOTOOL_ARGS)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
            INJECT_TIMEOUT,
        )
        .context("sending paste shortcut with ydotool")?;
        if status.success() {
            return Ok(());
        }
    }
    bail!("cannot send the paste shortcut (install wtype or a ydotool daemon)")
}

#[cfg(test)]
mod tests {
    use super::{
        allows_clipboard_fallback, backend_order, normalize_for_typing, run_bounded,
        run_writer_backend, ydotool_socket_candidates, AvailableBackends, Backend, InjectionMode,
        PASTE_WTYPE_ARGS, PASTE_YDOTOOL_ARGS,
    };
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, Instant};

    #[test]
    fn paste_shortcut_is_ctrl_shift_v() {
        assert_eq!(
            PASTE_WTYPE_ARGS,
            ["-M", "ctrl", "-M", "shift", "-k", "v", "-m", "shift", "-m", "ctrl"]
        );
        assert_eq!(
            PASTE_YDOTOOL_ARGS,
            ["key", "29:1", "42:1", "47:1", "47:0", "42:0", "29:0"]
        );
    }

    /// Write an executable `name` into `dir` whose body is `body`, so tests can
    /// put a deterministic fake helper on `PATH` (a real subprocess boundary).
    fn write_fake_executable(dir: &Path, name: &str, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[test]
    fn auto_order_prefers_paste_then_typing_then_clipboard() {
        assert_eq!(
            backend_order(
                InjectionMode::Auto,
                AvailableBackends {
                    wtype: true,
                    ydotool: true,
                    wl_copy: true,
                },
            ),
            vec![
                Backend::Paste,
                Backend::Wtype,
                Backend::Ydotool,
                Backend::Clipboard
            ]
        );
    }

    #[test]
    fn auto_order_skips_paste_without_wl_copy() {
        assert_eq!(
            backend_order(
                InjectionMode::Auto,
                AvailableBackends {
                    wtype: true,
                    ydotool: true,
                    wl_copy: false,
                },
            ),
            vec![Backend::Wtype, Backend::Ydotool, Backend::Clipboard]
        );
    }

    #[test]
    fn auto_order_skips_unavailable_typers() {
        assert_eq!(
            backend_order(
                InjectionMode::Auto,
                AvailableBackends {
                    wtype: false,
                    ydotool: true,
                    wl_copy: true,
                },
            ),
            vec![Backend::Paste, Backend::Ydotool, Backend::Clipboard]
        );
    }

    #[test]
    fn paste_mode_requires_wl_copy_and_a_keyboard_backend() {
        assert_eq!(
            backend_order(
                InjectionMode::Paste,
                AvailableBackends {
                    wtype: true,
                    ydotool: false,
                    wl_copy: true,
                },
            ),
            vec![Backend::Paste]
        );
        assert_eq!(
            backend_order(
                InjectionMode::Paste,
                AvailableBackends {
                    wtype: false,
                    ydotool: false,
                    wl_copy: true,
                },
            ),
            Vec::<Backend>::new()
        );
        assert_eq!(
            backend_order(
                InjectionMode::Paste,
                AvailableBackends {
                    wtype: true,
                    ydotool: true,
                    wl_copy: false,
                },
            ),
            Vec::<Backend>::new()
        );
    }

    #[test]
    fn type_order_never_includes_clipboard() {
        assert_eq!(
            backend_order(
                InjectionMode::Type,
                AvailableBackends {
                    wtype: false,
                    ydotool: false,
                    wl_copy: true,
                },
            ),
            Vec::<Backend>::new()
        );
        assert_eq!(
            backend_order(
                InjectionMode::Type,
                AvailableBackends {
                    wtype: true,
                    ydotool: true,
                    wl_copy: true,
                },
            ),
            vec![Backend::Wtype, Backend::Ydotool]
        );
    }

    #[test]
    fn ydotool_candidates_prefer_env_then_runtime() {
        // Snapshot-free: only check ordering rules against the current env.
        let list = ydotool_socket_candidates();
        assert!(!list.is_empty());
        if let Some(env_socket) = std::env::var_os("YDOTOOL_SOCKET").filter(|s| !s.is_empty()) {
            assert_eq!(list[0], PathBuf::from(env_socket));
        }
        assert!(
            list.iter()
                .any(|p| p.ends_with(".ydotool_socket") || p.ends_with("ydotoold.socket")),
            "expected known socket basenames: {list:?}"
        );
    }

    #[test]
    fn clipboard_fallback_only_for_auto() {
        assert!(allows_clipboard_fallback(InjectionMode::Auto));
        assert!(!allows_clipboard_fallback(InjectionMode::Type));
        assert!(!allows_clipboard_fallback(InjectionMode::Paste));
        assert!(!allows_clipboard_fallback(InjectionMode::Clipboard));
    }

    #[test]
    fn clipboard_order_contains_only_clipboard() {
        assert_eq!(
            backend_order(
                InjectionMode::Clipboard,
                AvailableBackends {
                    wtype: true,
                    ydotool: true,
                    wl_copy: true,
                },
            ),
            vec![Backend::Clipboard]
        );
    }

    #[test]
    fn normalize_replaces_control_chars_with_spaces() {
        assert_eq!(normalize_for_typing("hello\nworld"), "hello world");
        assert_eq!(normalize_for_typing("a\r\nb"), "a b");
        assert_eq!(normalize_for_typing("a\tb"), "a b");
        assert_eq!(normalize_for_typing("a\x1bb"), "a b");
        assert_eq!(normalize_for_typing("a\x0cb"), "a b");
    }

    #[test]
    fn normalize_collapses_consecutive_controls() {
        assert_eq!(normalize_for_typing("one\n\n\ntwo"), "one two");
        assert_eq!(normalize_for_typing("a\n\t\rb"), "a b");
    }

    #[test]
    fn normalize_leaves_plain_text_alone() {
        let text = "So yesterday, I was thinking about the Exa API and the CLI.";
        assert_eq!(normalize_for_typing(text), text);
    }

    #[test]
    fn normalize_handles_leading_and_trailing_controls() {
        assert_eq!(normalize_for_typing("\nstart"), " start");
        assert_eq!(normalize_for_typing("end\n"), "end ");
        assert_eq!(normalize_for_typing("\n"), " ");
        assert_eq!(normalize_for_typing(""), "");
    }

    #[test]
    fn sleeping_fake_helper_on_path_times_out() {
        // Process-boundary proof that a wedged helper cannot stall the daemon:
        // a real child process that sleeps far past the budget is killed and
        // reported as a timeout rather than blocking forever.
        let dir = std::env::temp_dir().join(format!(
            "cantrip-inject-fake-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_fake_executable(&dir, "cantrip-inject-fake", "#!/bin/sh\nsleep 30\n");

        let budget = Duration::from_millis(200);
        let path_var = std::env::var("PATH").unwrap_or_default();
        let mut cmd = Command::new("cantrip-inject-fake");
        // Fake resolved via PATH so the spawn boundary is a real subprocess.
        cmd.env("PATH", format!("{}:{}", dir.display(), path_var));

        let started = Instant::now();
        let result = run_bounded(&mut cmd, budget);
        let elapsed = started.elapsed();

        assert!(result.is_err(), "expected the sleeping fake to time out");
        assert!(
            elapsed < Duration::from_secs(5),
            "timeout was not actually bounded: took {elapsed:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn input_writing_backend_times_out_when_helper_never_reads() {
        // Process-boundary proof for the *stdin-writing* backends (ydotool and
        // wl-copy), the path the previous no-stdin test did not cover. A real
        // child that never reads leaves the pipe buffer full; forwarding a
        // payload bigger than that buffer must time out (kill + reap) rather
        // than block the daemon on the write forever.
        let dir = std::env::temp_dir().join(format!(
            "cantrip-inject-fake-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_fake_executable(&dir, "cantrip-inject-reader-fake", "#!/bin/sh\nsleep 30\n");

        let budget = Duration::from_millis(200);
        let path_var = std::env::var("PATH").unwrap_or_default();
        let mut cmd = Command::new("cantrip-inject-reader-fake");
        // Resolved via PATH so the spawn boundary is a real subprocess.
        cmd.env("PATH", format!("{}:{}", dir.display(), path_var));

        // Larger than the kernel pipe buffer (usually 64 KiB) so the write end
        // genuinely fills if the child never reads.
        let payload = "x".repeat(1 << 20);

        let started = Instant::now();
        let result = run_writer_backend(&mut cmd, &payload, "fake-reader", budget);
        let elapsed = started.elapsed();

        assert!(
            result.is_err(),
            "expected the non-reading sleeping fake to time out"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "write phase was not actually bounded: took {elapsed:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
