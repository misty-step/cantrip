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
    Type,
    Clipboard,
}

/// The backend that inserted text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectionOutcome {
    Typed(&'static str),
    Clipboard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    Wtype,
    Ydotool,
    Clipboard,
}

impl Backend {
    fn name(self) -> &'static str {
        match self {
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
}

/// Inject text with the requested backend policy.
pub fn inject(text: &str, mode: InjectionMode) -> Result<InjectionOutcome> {
    let available = AvailableBackends {
        wtype: executable_in_path("wtype"),
        ydotool: executable_in_path("ydotool") && ydotool_socket_exists(),
    };
    let order = backend_order(mode, available);
    let mut failures = Vec::new();

    for backend in order {
        let result = match backend {
            Backend::Wtype => run_wtype(text),
            Backend::Ydotool => run_ydotool(text),
            Backend::Clipboard => run_clipboard(text),
        };
        match result {
            Ok(()) => {
                tracing::info!(
                    "[Inject] backend={} chars={}",
                    backend.name(),
                    text.chars().count()
                );
                return Ok(match backend {
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
        InjectionMode::Clipboard => bail!(
            "clipboard injection failed ({details}); install wl-clipboard so wl-copy is available"
        ),
    }
}

fn backend_order(mode: InjectionMode, available: AvailableBackends) -> Vec<Backend> {
    match mode {
        InjectionMode::Auto => {
            let mut order = Vec::with_capacity(3);
            if available.wtype {
                order.push(Backend::Wtype);
            }
            if available.ydotool {
                order.push(Backend::Ydotool);
            }
            order.push(Backend::Clipboard);
            order
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

#[cfg(test)]
mod tests {
    use super::{backend_order, AvailableBackends, Backend, InjectionMode};

    #[test]
    fn auto_order_prefers_typing_then_clipboard() {
        assert_eq!(
            backend_order(
                InjectionMode::Auto,
                AvailableBackends {
                    wtype: true,
                    ydotool: true,
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
                },
            ),
            vec![Backend::Ydotool, Backend::Clipboard]
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
                },
            ),
            vec![Backend::Clipboard]
        );
    }
}
