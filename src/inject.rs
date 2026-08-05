//! Text injection through Wayland typing tools or the clipboard.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

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

fn ydotool_socket_exists() -> bool {
    let configured = env::var_os("YDOTOOL_SOCKET")
        .filter(|socket| !socket.is_empty())
        .map(|socket| PathBuf::from(socket).exists())
        .unwrap_or(false);
    configured
        || PathBuf::from(format!("/run/user/{}/.ydotool_socket", unsafe {
            libc::getuid()
        }))
        .exists()
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
    let output = Command::new("wtype")
        .arg("--")
        .arg(text)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .context("running wtype")?;
    if !output.status.success() {
        bail!("wtype exited with status {}", output.status);
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

    let output = child.wait_with_output().context("waiting for ydotool")?;
    if !output.status.success() {
        bail!("ydotool exited with status {}", output.status);
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

    let output = child.wait_with_output().context("waiting for wl-copy")?;
    if !output.status.success() {
        bail!("wl-copy exited with status {}", output.status);
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
        let output = Command::new("wtype")
            .args(["-M", "ctrl", "-k", "v"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .context("sending paste shortcut with wtype")?;
        if output.status.success() {
            return Ok(());
        }
    }
    if executable_in_path("ydotool") && ydotool_socket_exists() {
        // 29 = LeftCtrl, 47 = V; down V, up V, up Ctrl.
        let output = Command::new("ydotool")
            .args(["key", "29:1", "47:1", "47:0", "29:0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .context("sending paste shortcut with ydotool")?;
        if output.status.success() {
            return Ok(());
        }
    }
    bail!("cannot send the paste shortcut (install wtype or a ydotool daemon)")
}

#[cfg(test)]
mod tests {
    use super::{backend_order, normalize_for_typing, AvailableBackends, Backend, InjectionMode};

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
}
