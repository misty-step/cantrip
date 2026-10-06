//! macOS destination permits are explicitly unavailable with public AX APIs.
//!
//! NSWorkspace activation notifications and AXObserver focus notifications are
//! useful interruption signals, but neither provides a documented complete
//! screen-lock history or an initial unlocked-state attestation. Frontmost-app
//! equality after processing is not continuous destination/session evidence.
//!
//! Apple documents NSWorkspace session notifications as user-session switches:
//! <https://developer.apple.com/documentation/appkit/nsworkspace/sessiondidresignactivenotification>.
//! SessionGetInfo's sessionHasGraphicAccess means graphics are available, not
//! that a session is unlocked: <https://github.com/apple-oss-distributions/Security/blob/main/OSX/libsecurity_authorization/lib/AuthSession.h>.
//! IOConsoleLocked and CGSSessionScreenIsLocked are private IOKit keys:
//! <https://github.com/apple-oss-distributions/xnu/blob/main/iokit/IOKit/IOKitKeysPrivate.h>.
//! An AX/window snapshot, HUD occlusion, undocumented distributed notification,
//! or absence of a private lock key cannot turn unknown evidence into permission.
//!
//! Do not start a partial observer that could be mistaken for a verified permit.
//! Copy is independent of this capability and uses native NSPasteboard directly.

use std::{io, mem, os::fd::AsRawFd, os::unix::net::UnixStream};

pub(crate) const UNSUPPORTED_DELIVERY: &str =
    "Keyboard delivery is unavailable on macOS. Choose Clipboard in Settings, then paste manually.";

/// A captured refusal, never a frontmost-app snapshot masquerading as a permit.
#[derive(Clone)]
pub(crate) struct Guard {
    reason: &'static str,
}

impl Guard {
    pub(crate) fn capture() -> Self {
        Self {
            reason: UNSUPPORTED_DELIVERY,
        }
    }

    pub(crate) fn denial(&self) -> &'static str {
        self.reason
    }
}

/// Authenticated operational sender identity only; it grants no input permit.
pub(crate) fn peer_pid(socket: &UnixStream) -> io::Result<i32> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: socket is live and both output pointers refer to writable storage.
    if unsafe { libc::getpeereid(socket.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if uid != unsafe { libc::geteuid() } {
        return Err(io::Error::other("Untrusted desktop socket"));
    }
    let mut pid: libc::pid_t = 0;
    let mut size = mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: Darwin LOCAL_PEERPID writes one pid_t. Validate returned size
    // rather than accepting partially initialized or differently typed data.
    if unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut size,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if size as usize != mem::size_of::<libc::pid_t>() || pid <= 0 {
        return Err(io::Error::other("Invalid desktop peer identity"));
    }
    Ok(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_peer_identity_comes_from_the_socket_not_environment() {
        let (socket, _peer) = UnixStream::pair().unwrap();
        assert_eq!(peer_pid(&socket).unwrap(), std::process::id() as i32);
    }
}
