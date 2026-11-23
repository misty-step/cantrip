//! HTTPS transport for macOS: ureq over the system TLS stack (Security
//! framework via native-tls), so the build needs no C toolchain for the Apple
//! target. The integrator may replace this with the shared `crate::net`
//! transport once that module selects native-tls on macOS.

use cantrip_core::ports::{HttpReply, Transport, TransportError};
use std::time::Duration;

pub struct NativeTlsTransport;

impl Transport for NativeTlsTransport {
    fn post_json(
        &self,
        url: &str,
        key: &str,
        body: &serde_json::Value,
        timeout: Option<Duration>,
    ) -> Result<HttpReply, TransportError> {
        let tls = ureq::tls::TlsConfig::builder()
            .provider(ureq::tls::TlsProvider::NativeTls)
            .build();
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .tls_config(tls)
            .http_status_as_error(false)
            .timeout_global(timeout)
            .build()
            .into();
        let result = agent
            .post(url)
            .header("Authorization", &format!("Bearer {key}"))
            .header("X-Title", "Cantrip")
            .send_json(body);
        match result {
            Ok(mut resp) => {
                let status = resp.status().as_u16();
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .map(Duration::from_secs);
                let body = resp
                    .body_mut()
                    .with_config()
                    .limit(64 * 1024 * 1024)
                    .read_to_string()
                    .map_err(|e| TransportError::Failed(error_class(&e)))?;
                Ok(HttpReply {
                    status,
                    body,
                    retry_after,
                })
            }
            Err(ureq::Error::Timeout(_)) => Err(TransportError::Timeout),
            Err(e @ (ureq::Error::HostNotFound | ureq::Error::ConnectionFailed)) => {
                Err(TransportError::Offline(error_class(&e)))
            }
            Err(ureq::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionRefused
                        | std::io::ErrorKind::NetworkUnreachable
                        | std::io::ErrorKind::HostUnreachable
                ) =>
            {
                Err(TransportError::Offline(e.kind().to_string()))
            }
            Err(e) => Err(TransportError::Failed(error_class(&e))),
        }
    }
}

/// Error class only: never URLs with secrets or bodies.
fn error_class(e: &ureq::Error) -> String {
    match e {
        ureq::Error::Io(io) => format!("io: {}", io.kind()),
        ureq::Error::Timeout(_) => "timeout".into(),
        ureq::Error::HostNotFound => "host not found".into(),
        ureq::Error::ConnectionFailed => "connection failed".into(),
        ureq::Error::Tls(_) => "tls".into(),
        other => format!("http client: {}", std::any::type_name_of_val(other)),
    }
}
