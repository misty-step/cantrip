//! The desktop composition root; shared workflow depends only on engine ports.

use anyhow::Result;
use cantrip_engine::ports::{DeliveryPermit, Platform, Recorder};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;

pub(crate) struct NativePlatform;

impl Platform for NativePlatform {
    fn start_recording(&self, wav: &Path, source: Option<&str>) -> Result<Box<dyn Recorder>> {
        Ok(Box::new(crate::capture::Recorder::start(wav, source)?))
    }

    fn prepare_delivery(&self) {
        crate::inject::DeliveryGuard::prepare();
    }

    fn delivery_permit(&self) -> Arc<dyn DeliveryPermit> {
        Arc::new(crate::inject::DeliveryGuard::capture())
    }

    fn handoff_color(&self, slot: usize) -> [u8; 3] {
        crate::theme::load().target(slot)
    }

    fn sender_identity(&self, stream: &UnixStream) -> String {
        sender_identity(stream)
    }
}

#[cfg(target_os = "linux")]
fn sender_identity(stream: &UnixStream) -> String {
    let Ok(pid) = crate::desktop::peer_pid(stream) else {
        return "unknown".to_owned();
    };
    let exe = std::fs::read_link(format!("/proc/{pid}/exe"))
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "?".to_owned());
    let mut chain = Vec::new();
    let mut current = pid;
    for _ in 0..3 {
        let Some(parent) = parent_pid(current) else {
            break;
        };
        if parent <= 1 {
            break;
        }
        let name = std::fs::read_to_string(format!("/proc/{parent}/comm")).unwrap_or_default();
        chain.push(format!("{}:{parent}", name.trim()));
        current = parent;
    }
    format!("pid={pid} exe={exe} parents={}", chain.join("<-"))
}

#[cfg(target_os = "linux")]
fn parent_pid(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

#[cfg(target_os = "macos")]
fn sender_identity(stream: &UnixStream) -> String {
    match crate::desktop::peer_pid(stream) {
        Ok(pid) => format!("pid={pid}"),
        Err(_) => "unknown".to_owned(),
    }
}
