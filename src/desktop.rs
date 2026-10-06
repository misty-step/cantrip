//! Native desktop/session evidence. Unsupported evidence never becomes a permit.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
pub(crate) use linux::{connect_unix, peer_pid, wayland_socket_path, Guard};
#[cfg(target_os = "macos")]
pub(crate) use macos::{peer_pid, Guard, UNSUPPORTED_DELIVERY};
