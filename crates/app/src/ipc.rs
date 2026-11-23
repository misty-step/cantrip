//! Single-instance control socket (cfg-free Unix socket; Linux and macOS).
//!
//! The running app serves one owner-only socket. A second `cantrip` launch —
//! the app-launcher entry on trayless sessions (`cantrip popover`), a
//! compositor-bound command (`cantrip toggle`) — sends one line and exits.
//! Lines: `popover`, `settings`, `toggle`; reply `ok` or `unknown`.

use crate::platform::UiRequest;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};
use std::time::Duration;

const IO_TIMEOUT: Duration = Duration::from_secs(2);

/// The per-user socket path.
pub fn socket_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/".into());
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Cantrip/v2/run/cantrip.sock")
    } else {
        match std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
        {
            Some(dir) => dir.join("cantrip-v2.sock"),
            // No runtime dir: a private dir in Cantrip's own data folder.
            None => home.join(".local/share/cantrip/v2/run/cantrip.sock"),
        }
    }
}

/// Parse one request line.
pub fn parse_command(line: &str) -> Option<UiRequest> {
    match line.trim() {
        "popover" => Some(UiRequest::ShowPopover),
        "settings" => Some(UiRequest::ShowSettings),
        "toggle" => Some(UiRequest::ToggleDictation),
        _ => None,
    }
}

/// The running server; dropping it removes the socket file.
#[derive(Debug)]
pub struct Server {
    path: PathBuf,
    _thread: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Serve `path`. Fails with `AddrInUse` when another live instance answers;
/// a stale socket left by a crash is replaced.
pub fn serve(path: &Path, sink: Sender<UiRequest>) -> io::Result<Server> {
    if let Some(dir) = path.parent() {
        if !dir.exists() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
    }
    let listener = match UnixListener::bind(path) {
        Ok(l) => l,
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            if UnixStream::connect(path).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "Cantrip is already running",
                ));
            }
            // Only ever remove a socket, never another kind of file.
            let meta = std::fs::symlink_metadata(path)?;
            use std::os::unix::fs::FileTypeExt;
            if !meta.file_type().is_socket() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "control socket path is not a socket",
                ));
            }
            std::fs::remove_file(path)?;
            UnixListener::bind(path)?
        }
        Err(e) => return Err(e),
    };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let thread = thread::Builder::new()
        .name("cantrip-ipc".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                if !same_user(&stream) {
                    continue;
                }
                let _ = handle(stream, &sink);
            }
        })?;
    Ok(Server {
        path: path.to_path_buf(),
        _thread: thread,
    })
}

fn handle(stream: UnixStream, sink: &Sender<UiRequest>) -> io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut line = String::new();
    BufReader::new(stream.try_clone()?)
        .take(256)
        .read_line(&mut line)?;
    let mut w = stream;
    match parse_command(&line) {
        Some(req) => {
            let _ = sink.send(req);
            w.write_all(b"ok\n")
        }
        None => w.write_all(b"unknown\n"),
    }
}

/// Reject peers of another user (the socket is already 0600; belt and braces).
fn same_user(stream: &UnixStream) -> bool {
    peer_uid(stream).is_none_or(|uid| uid == unsafe { libc::getuid() })
}

#[cfg(target_os = "linux")]
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    use std::os::unix::io::AsRawFd;
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt with a correctly sized ucred out-parameter.
    let r = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    (r == 0).then_some(cred.uid)
}

#[cfg(not(target_os = "linux"))]
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    use std::os::unix::io::AsRawFd;
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: getpeereid writes two integers.
    let r = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    (r == 0).then_some(uid)
}

/// Send `cmd` to the running instance at the default path.
/// `Ok(false)` when no instance is running.
pub fn send(cmd: &str) -> io::Result<bool> {
    send_to(&socket_path(), cmd)
}

pub fn send_to(path: &Path, cmd: &str) -> io::Result<bool> {
    let mut s = match UnixStream::connect(path) {
        Ok(s) => s,
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(false)
        }
        Err(e) => return Err(e),
    };
    s.set_read_timeout(Some(IO_TIMEOUT))?;
    s.set_write_timeout(Some(IO_TIMEOUT))?;
    s.write_all(format!("{}\n", cmd.trim()).as_bytes())?;
    let mut reply = String::new();
    BufReader::new(s).take(256).read_line(&mut reply)?;
    match reply.trim() {
        "ok" => Ok(true),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Cantrip did not accept {:?} ({other})", cmd.trim()),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn round_trip_and_single_instance() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run/c.sock");
        assert!(!send_to(&path, "popover").unwrap());
        let (tx, rx) = mpsc::channel();
        let server = serve(&path, tx.clone()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(send_to(&path, "popover").unwrap());
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            UiRequest::ShowPopover
        );
        assert!(send_to(&path, "toggle").unwrap());
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            UiRequest::ToggleDictation
        );
        assert!(send_to(&path, "bogus").is_err());
        let second = serve(&path, tx).unwrap_err();
        assert_eq!(second.kind(), io::ErrorKind::AddrInUse);
        drop(server);
        assert!(!path.exists());
    }

    #[test]
    fn stale_socket_is_replaced_but_other_files_are_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.sock");
        drop(UnixListener::bind(&path).unwrap()); // stale socket file remains
        assert!(path.exists());
        let (tx, rx) = mpsc::channel();
        let _server = serve(&path, tx.clone()).unwrap();
        assert!(send_to(&path, "settings").unwrap());
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            UiRequest::ShowSettings
        );

        let file = dir.path().join("notasocket");
        std::fs::write(&file, "keep").unwrap();
        // bind on an existing regular file fails AddrInUse; it must not be removed.
        assert!(serve(&file, tx).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "keep");
    }

    #[test]
    fn commands_parse() {
        assert_eq!(parse_command("popover\n"), Some(UiRequest::ShowPopover));
        assert_eq!(parse_command(" settings "), Some(UiRequest::ShowSettings));
        assert_eq!(parse_command("quit"), None);
    }
}
