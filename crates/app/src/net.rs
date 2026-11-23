//! HTTPS transport to OpenRouter (cfg-free: rustls on Linux, native-tls on
//! macOS, chosen in Cargo.toml). Implements `cantrip_core::ports::Transport`.
//!
//! Error mapping is what the core's fail-closed rules depend on:
//! - DNS failure, refused / unreachable connect, or a connect-phase timeout →
//!   `Offline` (no byte of the request left the machine);
//! - any timeout after the connection exists → `Timeout`;
//! - everything else → `Failed`.
//!
//! HTTP 4xx/5xx are *replies* (`Ok(HttpReply)`), with their bodies, so the core
//! decides retryability. Nothing here logs bodies, keys or audio.

use cantrip_core::ports::{HttpReply, Transport, TransportError};
use std::io;
use std::time::Duration;

/// Bound on DNS + TCP + TLS setup. Not an inference deadline: a server that
/// accepted the connection may take as long as it needs (unless the caller
/// passes a total `timeout`).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Replies are small JSON documents; refuse absurd bodies.
const MAX_REPLY_BYTES: u64 = 8 * 1024 * 1024;

pub struct UreqTransport {
    agent: ureq::Agent,
}

impl Default for UreqTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl UreqTransport {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_resolve(Some(CONNECT_TIMEOUT))
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .user_agent("cantrip/2")
            .build();
        UreqTransport {
            agent: ureq::Agent::new_with_config(config),
        }
    }
}

impl Transport for UreqTransport {
    fn post_json(
        &self,
        url: &str,
        key: &str,
        body: &serde_json::Value,
        timeout: Option<Duration>,
    ) -> Result<HttpReply, TransportError> {
        // Serialize up front so the request carries a Content-Length.
        let bytes =
            serde_json::to_vec(body).map_err(|_| TransportError::Failed("encode".into()))?;
        let result = self
            .agent
            .post(url)
            .config()
            .timeout_global(timeout)
            .build()
            .header("Authorization", &format!("Bearer {key}"))
            .header("Content-Type", "application/json")
            .send(&bytes[..]);
        let mut response = result.map_err(map_error)?;
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after);
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_REPLY_BYTES)
            .read_to_string()
            .map_err(map_error)?;
        Ok(HttpReply {
            status,
            body,
            retry_after,
        })
    }
}

/// `Retry-After` as delta-seconds (the HTTP-date form is accepted only when it
/// can be read as a plain integer; dates map to `None` and the core's own
/// backoff applies).
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    let v = value.trim();
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs.min(3600)));
    }
    v.parse::<f64>()
        .ok()
        .filter(|s| s.is_finite() && *s >= 0.0)
        .map(|s| Duration::from_secs_f64(s.min(3600.0)))
}

fn map_error(e: ureq::Error) -> TransportError {
    use ureq::Timeout as T;
    match e {
        ureq::Error::HostNotFound | ureq::Error::ConnectionFailed => {
            TransportError::Offline(class(&e))
        }
        ureq::Error::Timeout(T::Resolve | T::Connect) => TransportError::Offline(class(&e)),
        ureq::Error::Timeout(_) => TransportError::Timeout,
        ureq::Error::Io(ref io) if is_connect_failure(io) => TransportError::Offline(class(&e)),
        ureq::Error::Io(ref io) if io.kind() == io::ErrorKind::TimedOut => TransportError::Timeout,
        other => TransportError::Failed(class(&other)),
    }
}

fn is_connect_failure(e: &io::Error) -> bool {
    use io::ErrorKind as K;
    matches!(
        e.kind(),
        K::ConnectionRefused | K::NetworkUnreachable | K::HostUnreachable | K::AddrNotAvailable
    ) || is_resolve_failure(e)
}

/// std's resolver reports getaddrinfo failures as uncategorized I/O errors
/// with this message prefix; nothing was sent.
fn is_resolve_failure(e: &io::Error) -> bool {
    e.to_string().contains("failed to lookup address")
}

/// An error class for logs: no URLs with secrets, no bodies.
fn class(e: &ureq::Error) -> String {
    match e {
        ureq::Error::Io(io) => format!("io:{:?}", io.kind()),
        ureq::Error::Timeout(t) => format!("timeout:{t:?}"),
        ureq::Error::StatusCode(s) => format!("status:{s}"),
        ureq::Error::HostNotFound => "host-not-found".into(),
        ureq::Error::ConnectionFailed => "connection-failed".into(),
        ureq::Error::Tls(m) => format!("tls:{m}"),
        other => {
            let s = format!("{other:?}");
            s.split(['(', ' ', '{'])
                .next()
                .unwrap_or("error")
                .to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::thread;

    /// One-shot HTTP stub: returns (base url, join handle yielding the
    /// request headers + body it saw).
    fn stub(reply: &'static str, delay: Duration) -> (String, thread::JoinHandle<String>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/api/v1/chat/completions", l.local_addr().unwrap());
        let h = thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut head = String::new();
            let mut len = 0usize;
            loop {
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
                head.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap();
            thread::sleep(delay);
            let _ = s.write_all(reply.as_bytes());
            head + &String::from_utf8_lossy(&body)
        });
        (url, h)
    }

    #[test]
    fn ok_reply_returns_status_body_and_sends_bearer_json() {
        let (url, h) = stub(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\n{\"ok\":true}",
            Duration::ZERO,
        );
        let t = UreqTransport::new();
        let r = t
            .post_json(&url, "k-test", &serde_json::json!({"a": 1}), None)
            .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "{\"ok\":true}");
        let seen = h.join().unwrap().to_ascii_lowercase();
        assert!(seen.contains("authorization: bearer k-test"));
        assert!(seen.contains("content-type: application/json"));
        assert!(seen.ends_with("{\"a\":1}"));
    }

    #[test]
    fn server_error_is_a_reply_with_body_and_retry_after() {
        let (url, h) = stub(
            "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 7\r\nContent-Length: 4\r\n\r\nbusy",
            Duration::ZERO,
        );
        let r = UreqTransport::new()
            .post_json(&url, "k", &serde_json::json!({}), None)
            .unwrap();
        assert_eq!(r.status, 503);
        assert_eq!(r.body, "busy");
        assert_eq!(r.retry_after, Some(Duration::from_secs(7)));
        h.join().unwrap();
    }

    #[test]
    fn refused_connection_is_offline() {
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let url = format!("http://127.0.0.1:{port}/x");
        let e = UreqTransport::new()
            .post_json(&url, "k", &serde_json::json!({}), None)
            .unwrap_err();
        assert!(matches!(e, TransportError::Offline(_)), "{e:?}");
    }

    #[test]
    fn unresolvable_host_is_offline() {
        let e = UreqTransport::new()
            .post_json(
                "http://cantrip-no-such-host.invalid/x",
                "k",
                &serde_json::json!({}),
                None,
            )
            .unwrap_err();
        assert!(matches!(e, TransportError::Offline(_)), "{e:?}");
    }

    #[test]
    fn slow_server_hits_the_total_timeout() {
        let (url, _h) = stub(
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}",
            Duration::from_millis(1500),
        );
        let start = std::time::Instant::now();
        let e = UreqTransport::new()
            .post_json(
                &url,
                "k",
                &serde_json::json!({}),
                Some(Duration::from_millis(300)),
            )
            .unwrap_err();
        assert_eq!(e, TransportError::Timeout);
        assert!(start.elapsed() < Duration::from_millis(1200));
    }

    #[test]
    fn retry_after_parsing() {
        assert_eq!(parse_retry_after(" 12 "), Some(Duration::from_secs(12)));
        assert_eq!(parse_retry_after("1.5"), Some(Duration::from_millis(1500)));
        assert_eq!(parse_retry_after("999999"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), None);
        assert_eq!(parse_retry_after("-3"), None);
    }

    #[test]
    fn class_never_contains_urls() {
        let c = class(&ureq::Error::BadUri("http://secret/".into()));
        assert!(!c.contains("secret"), "{c}");
    }
}
