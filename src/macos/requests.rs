//! Requests from `cantrip` commands to the running menu-bar app to show one of
//! its windows, which live in the app's own process. The app listens on a
//! socket in its owner-only runtime directory, beside the lock that keeps one
//! app per user, and answers each request after acting on it. A command that
//! finds no app starts it first, so `cantrip settings` and `cantrip actions`
//! behave like choosing Settings… or Recordings and Recovery… from the menu
//! and never leave a window process of their own.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const SOCKET: &str = "app.sock";
/// A request is one short JSON line.
const REQUEST_LIMIT: usize = 512;
/// A connected command has this long to send its request.
const REQUEST_WAIT: Duration = Duration::from_secs(2);
/// The app answers within one tick; this bounds a stalled app.
const REPLY_WAIT: Duration = Duration::from_secs(10);
/// A just-opened app binds its socket within this window.
const STARTUP_WAIT: Duration = Duration::from_secs(20);

/// A window the running app shows on request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "show", rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) enum Show {
    Settings,
    /// Recordings and Recovery, on its recordings.
    Recordings,
    /// Recordings and Recovery, running the setup check.
    CheckSetup,
}

impl Show {
    /// The menu item that shows the same window.
    fn name(self) -> &'static str {
        match self {
            Self::Settings => "Settings",
            Self::Recordings => "Recordings and Recovery",
            Self::CheckSetup => "Check Setup",
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

fn socket_path() -> Result<PathBuf> {
    Ok(crate::hud::macos::runtime_dir()?.join(SOCKET))
}

/// The app's end: requests arrive on the AppKit thread's own tick, without a
/// listener thread, and each is answered once it has been acted on.
pub(crate) struct Listener {
    listener: UnixListener,
    path: PathBuf,
    pending: Vec<(UnixStream, Vec<u8>, Instant)>,
}

impl Listener {
    /// Listen in the runtime directory. Only the holder of the app lock calls
    /// this, so a socket file already there belongs to an app that is gone.
    pub(crate) fn bind() -> Result<Self> {
        Self::bind_at(socket_path()?)
    }

    fn bind_at(path: PathBuf) -> Result<Self> {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("removing {}", path.display()));
            }
        }
        let listener = UnixListener::bind(&path)
            .with_context(|| format!("listening for window requests at {}", path.display()))?;
        listener
            .set_nonblocking(true)
            .context("configuring the window request socket")?;
        Ok(Self {
            listener,
            path,
            pending: Vec::new(),
        })
    }

    /// Requests that arrived since the last call. Malformed or stalled
    /// connections are dropped, which their sender reports as a failure.
    pub(crate) fn poll(&mut self) -> Vec<(Show, Responder)> {
        let now = Instant::now();
        loop {
            match self.listener.accept() {
                Ok((stream, _)) if stream.set_nonblocking(true).is_ok() => {
                    self.pending.push((stream, Vec::new(), now + REQUEST_WAIT));
                }
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        let mut ready = Vec::new();
        self.pending.retain_mut(|(stream, received, deadline)| {
            let mut buffer = [0; REQUEST_LIMIT];
            // A client may half-close after its request; its line still counts.
            let mut ended = false;
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) => {
                        ended = true;
                        break;
                    }
                    Ok(read) => {
                        received.extend_from_slice(&buffer[..read]);
                        if received.len() > REQUEST_LIMIT {
                            return false;
                        }
                    }
                    Err(error) if error.kind() == ErrorKind::Interrupted => {}
                    Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                    Err(_) => return false,
                }
            }
            let Some(end) = received.iter().position(|byte| *byte == b'\n') else {
                return !ended && now < *deadline;
            };
            if let (Ok(show), Ok(stream)) = (
                serde_json::from_slice::<Show>(&received[..end]),
                stream.try_clone(),
            ) {
                ready.push((show, Responder(stream)));
            }
            false
        });
        ready
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Answers one request, after the app has acted on it.
pub(crate) struct Responder(UnixStream);

impl Responder {
    pub(crate) fn reply(mut self, result: Result<()>) {
        let reply = match result {
            Ok(()) => Reply {
                ok: true,
                message: None,
            },
            Err(error) => Reply {
                ok: false,
                message: Some(format!("{error:#}")),
            },
        };
        if let Ok(mut line) = serde_json::to_vec(&reply) {
            line.push(b'\n');
            // A tiny reply fits the socket buffer; a sender that left misses it.
            let _ = self.0.write_all(&line);
        }
    }
}

/// Why a request got no answer.
enum Unanswered {
    /// Nothing listens: no app runs for this runtime directory.
    NotRunning,
    Failed(anyhow::Error),
}

/// Ask the running app to show `window`, opening Cantrip first when no app
/// answers. Returns once the app has shown it, or with the app's reason.
pub(crate) fn show(window: Show) -> Result<()> {
    let path = socket_path()?;
    match ask(&path, window) {
        Ok(result) => return result,
        Err(Unanswered::Failed(error)) => return Err(error),
        Err(Unanswered::NotRunning) => {}
    }
    crate::macos::open_app().context("Opening Cantrip")?;
    let deadline = Instant::now() + STARTUP_WAIT;
    loop {
        std::thread::sleep(Duration::from_millis(200));
        match ask(&path, window) {
            Ok(result) => return result,
            Err(Unanswered::Failed(error)) => return Err(error),
            Err(Unanswered::NotRunning) if Instant::now() < deadline => {}
            Err(Unanswered::NotRunning) => {
                return Err(anyhow!(
                    "Cantrip did not open {} within {} seconds. Open Cantrip from the Applications folder, then choose {}… from its menu.",
                    window.name(),
                    STARTUP_WAIT.as_secs(),
                    window.name()
                ));
            }
        }
    }
}

fn ask(path: &Path, window: Show) -> std::result::Result<Result<()>, Unanswered> {
    let mut stream = match UnixStream::connect(path) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) =>
        {
            return Err(Unanswered::NotRunning)
        }
        Err(error) => {
            return Err(Unanswered::Failed(
                anyhow::Error::new(error).context("Reaching the running Cantrip"),
            ))
        }
    };
    let exchange = || -> Result<Reply> {
        stream.set_read_timeout(Some(REPLY_WAIT))?;
        stream.set_write_timeout(Some(REPLY_WAIT))?;
        let mut line = serde_json::to_vec(&window)?;
        line.push(b'\n');
        stream.write_all(&line)?;
        let mut reply = Vec::new();
        stream
            .take(REQUEST_LIMIT as u64 * 8)
            .read_to_end(&mut reply)?;
        Ok(serde_json::from_slice(&reply)?)
    };
    match exchange() {
        Ok(Reply { ok: true, .. }) => Ok(Ok(())),
        Ok(Reply { message, .. }) => Ok(Err(anyhow!(
            "Cantrip did not show {}: {}",
            window.name(),
            message.unwrap_or_else(|| "no reason given".to_owned())
        ))),
        Err(error) => Err(Unanswered::Failed(error.context(format!(
            "The running Cantrip did not answer the request to show {}; quit and reopen Cantrip",
            window.name()
        )))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::DirBuilderExt;

    struct Directory(PathBuf);

    impl Directory {
        fn new() -> Self {
            // Unix socket paths are short; the per-user temporary directory may not be.
            let path = PathBuf::from("/private/tmp")
                .join(format!("ct-req-{}", cantrip_engine::recovery::new_id()));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .unwrap();
            Self(path)
        }
    }

    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn serve_until_answered(listener: &mut Listener, result: impl Fn() -> Result<()>) -> Show {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some((show, responder)) = listener.poll().pop() {
                responder.reply(result());
                return show;
            }
            assert!(Instant::now() < deadline, "the request never arrived");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_request_is_answered_after_the_app_acts_on_it() {
        let directory = Directory::new();
        let path = directory.0.join(SOCKET);
        let mut listener = Listener::bind_at(path.clone()).unwrap();

        let asking = path.clone();
        let client = std::thread::spawn(move || ask(&asking, Show::Settings));
        assert_eq!(
            serve_until_answered(&mut listener, || Ok(())),
            Show::Settings
        );
        assert!(matches!(client.join().unwrap(), Ok(Ok(()))));

        let asking = path.clone();
        let client = std::thread::spawn(move || ask(&asking, Show::CheckSetup));
        assert_eq!(
            serve_until_answered(&mut listener, || Err(anyhow!("window refused"))),
            Show::CheckSetup
        );
        assert!(matches!(client.join().unwrap(), Ok(Err(_))));
    }

    #[test]
    fn without_a_listening_app_the_request_reports_it_is_not_running() {
        let directory = Directory::new();
        let path = directory.0.join(SOCKET);
        assert!(matches!(
            ask(&path, Show::Settings),
            Err(Unanswered::NotRunning)
        ));
        // A socket file left by an app that exited answers nothing, and the
        // next app replaces it.
        drop(UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        assert!(matches!(
            ask(&path, Show::Settings),
            Err(Unanswered::NotRunning)
        ));
        let mut listener = Listener::bind_at(path.clone()).unwrap();
        let client = std::thread::spawn(move || ask(&path, Show::Settings));
        serve_until_answered(&mut listener, || Ok(()));
        assert!(matches!(client.join().unwrap(), Ok(Ok(()))));
    }

    #[test]
    fn malformed_requests_are_dropped_without_an_answer() {
        let directory = Directory::new();
        let path = directory.0.join(SOCKET);
        let mut listener = Listener::bind_at(path.clone()).unwrap();
        let mut stream = UnixStream::connect(&path).unwrap();
        stream.write_all(b"{\"show\":\"terminal\"}\n").unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            assert!(listener.poll().is_empty());
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).unwrap();
        assert!(reply.is_empty());
    }

    #[test]
    fn a_client_that_half_closes_after_its_request_is_still_answered() {
        let directory = Directory::new();
        let path = directory.0.join(SOCKET);
        let mut listener = Listener::bind_at(path.clone()).unwrap();
        let mut stream = UnixStream::connect(&path).unwrap();
        stream.write_all(b"{\"show\":\"settings\"}\n").unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        assert_eq!(
            serve_until_answered(&mut listener, || Ok(())),
            Show::Settings
        );
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).unwrap();
        let reply: Reply = serde_json::from_slice(&reply).unwrap();
        assert!(reply.ok);

        // A half-closed client without a complete line is dropped at once.
        let mut partial = UnixStream::connect(&path).unwrap();
        partial.write_all(b"{\"show\":").unwrap();
        partial.shutdown(std::net::Shutdown::Write).unwrap();
        let deadline = Instant::now() + Duration::from_millis(300);
        while Instant::now() < deadline {
            assert!(listener.poll().is_empty());
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(listener.pending.is_empty());
    }
}
