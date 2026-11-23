//! Linux platform. OWNER: linux fork.
use super::{Platform, Tray, UiRequest};
use std::sync::mpsc::Sender;

pub fn platform(requests: Sender<UiRequest>) -> anyhow::Result<(Platform, Option<Box<dyn Tray>>)> {
    let _ = requests;
    todo!()
}
