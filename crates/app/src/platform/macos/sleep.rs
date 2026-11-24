//! System sleep on macOS: `NSWorkspaceWillSleepNotification` stops an active
//! meeting (DESIGN.md › Meeting pipeline: sleep anyway stops the meeting with
//! a message). Idle sleep during a meeting is already prevented by the
//! meeting capture's `IdleSystemSleepDisabled` activity, so
//! [`SleepWatch::set_meeting_active`] has nothing to hold here.
//!
//! The observer runs on the posting (main) thread and returns only once the
//! meeting's audio is finalized; macOS lets an app delay sleep briefly while
//! it handles this notification. Type-checked only: not yet run on a Mac.

use crate::platform::SleepWatch;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{NSWorkspace, NSWorkspaceWillSleepNotification};
use objc2_foundation::{NSNotification, NSObjectProtocol};
use std::ptr::NonNull;
use std::sync::Arc;

struct Observer(Retained<ProtocolObject<dyn NSObjectProtocol>>);
// The token is only kept alive (and removed on drop); it is never messaged
// from another thread.
unsafe impl Send for Observer {}

pub struct MacSleep {
    _observer: Observer,
}

impl SleepWatch for MacSleep {
    fn set_meeting_active(&self, _active: bool) {}
}

impl Drop for Observer {
    fn drop(&mut self) {
        let center = NSWorkspace::sharedWorkspace().notificationCenter();
        let obj: &objc2::runtime::AnyObject = (*self.0).as_ref();
        unsafe { center.removeObserver(obj) };
    }
}

pub fn watch(on_sleep: Arc<dyn Fn() + Send + Sync>) -> Box<dyn SleepWatch> {
    let center = NSWorkspace::sharedWorkspace().notificationCenter();
    let block = block2::RcBlock::new(move |_n: NonNull<NSNotification>| on_sleep());
    let token = unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(NSWorkspaceWillSleepNotification),
            None,
            None,
            &block,
        )
    };
    Box::new(MacSleep {
        _observer: Observer(token),
    })
}
