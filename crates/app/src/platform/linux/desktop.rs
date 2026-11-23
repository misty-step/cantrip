//! PLACEHOLDER (linux-system fork): the real implementation is owned by the
//! linux-desktop fork. Until then every target is uncertain → copy-only.

use cantrip_core::ports::*;

pub struct LinuxDesktop;

impl LinuxDesktop {
    pub fn new() -> Self {
        LinuxDesktop
    }
}

impl Desktop for LinuxDesktop {
    fn capture_target(&self) -> Option<Target> {
        None
    }
    fn focus_now(&self) -> Option<FocusNow> {
        None
    }
    fn app_alive(&self, _app: &AppIdentity) -> bool {
        false
    }
    fn missing_delivery_permission(&self) -> Option<String> {
        None
    }
    fn session_epoch(&self) -> u64 {
        0
    }
    fn field_support(&self, _target: &Target) -> FieldSupport {
        FieldSupport::Gone
    }
    fn read_field(&self, _target: &Target) -> Option<FieldState> {
        None
    }
    fn insert_text(&self, _target: &Target, _text: &str) -> InsertResult {
        InsertResult::Refused
    }
    fn can_address_paste(&self, _app: &AppIdentity) -> bool {
        false
    }
    fn post_paste(&self, _target: &Target) -> PasteResult {
        PasteResult::Unaddressable
    }
}
