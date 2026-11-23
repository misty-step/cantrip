//! Linux platform. OWNER: linux fork.
#![allow(dead_code)]
use super::{Platform, Tray, UiRequest};
use std::sync::mpsc::Sender;

pub mod atspi;
pub mod clipboard;
pub mod desktop;
pub mod hotkeys;
pub mod window;
pub mod x11;

pub fn platform(requests: Sender<UiRequest>) -> anyhow::Result<(Platform, Option<Box<dyn Tray>>)> {
    let _ = requests;
    todo!()
}
