//! Text injection through Wayland typing tools or the clipboard.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
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

/// Wait for `child` to exit within [`INJECT_TIMEOUT`], killing it if it overruns.
fn wait_child(child: &mut Child) -> Result<ExitStatus> {
    wait_child_bounded(child, INJECT_TIMEOUT)
}

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

/// Select how text is inserted into the focused Wayland application.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum InjectionMode {
    #[default]
    Auto,
    /// Copy, then send one Ctrl+V to the focused app (paragraph-safe).
    Paste,
    Type,
    Clipboard,
}

/// The backend that inserted text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectionOutcome {
    Typed(&'static str),
    /// Copied to the clipboard and pasted with a single Ctrl+V.
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

fn run_ydotool(text: &str) -> Result<()> {
    let mut child = Command::new("ydotool")
        .args(["type", "--file", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("starting ydotool")?;

    let write_result = child
        .stdin
        .take()
        .context("opening ydotool stdin")
        .and_then(|mut stdin| {
            stdin
                .write_all(text.as_bytes())
                .context("writing text to ydotool")
        });
    if let Err(error) = write_result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    // stdin is dropped here, closing the pipe; ydotool sees EOF on its input.

    let status = wait_child(&mut child).context("waiting for ydotool")?;
    if !status.success() {
        bail!("ydotool exited with status {}", status);
    }
    Ok(())
}

fn run_clipboard(text: &str) -> Result<()> {
    let mut child = Command::new("wl-copy")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("starting wl-copy")?;

    let write_result = child
        .stdin
        .take()
        .context("opening wl-copy stdin")
        .and_then(|mut stdin| {
            stdin
                .write_all(text.as_bytes())
                .context("writing text to wl-copy")
        });
    if let Err(error) = write_result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    // stdin is dropped here, closing the pipe; wl-copy sees EOF on its input.

    let status = wait_child(&mut child).context("waiting for wl-copy")?;
    if !status.success() {
        bail!("wl-copy exited with status {}", status);
    }
    Ok(())
}

/// Copy the text, then send one Ctrl+V to the focused app. Nothing is typed
/// into a live window, so losing focus mid-composition is harmless: the
/// clipboard was fully written before the single paste keypress. The text
/// stays on the clipboard (wl-copy keeps holding the selection) for the
/// user to paste again by hand.
fn run_paste(text: &str) -> Result<()> {
    run_clipboard(text)?;
    send_paste_shortcut()
}

/// Emit a Ctrl+V keypress through wtype (or a ydotool key sequence).
fn send_paste_shortcut() -> Result<()> {
    if executable_in_path("wtype") {
        let status = run_bounded(
            Command::new("wtype")
                .args(["-M", "ctrl", "-k", "v"])
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
        // 29 = LeftCtrl, 47 = V; down V, up V, up Ctrl.
        let status = run_bounded(
            Command::new("ydotool")
                .args(["key", "29:1", "47:1", "47:0", "29:0"])
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
        ydotool_socket_candidates, AvailableBackends, Backend, InjectionMode,
    };
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, Instant};

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
}
