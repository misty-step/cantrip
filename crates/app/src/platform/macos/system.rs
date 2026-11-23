//! Keychain-held OpenRouter key and the SMAppService login item.
//!
//! Not run on a Mac yet; type-checked only (see `platform::macos` docs).

use cantrip_core::ports::{LoginItem, SecretError, SecretStore};
use objc2_service_management::{SMAppService, SMAppServiceStatus};
use security_framework::passwords;

const SERVICE: &str = "io.mistystep.cantrip";
const ACCOUNT: &str = "openrouter";
/// errSecItemNotFound.
const ITEM_NOT_FOUND: i32 = -25300;

/// Generic password in the login keychain. A read failure other than "not
/// found" is an error, never "no key".
pub struct KeychainSecrets;

impl SecretStore for KeychainSecrets {
    fn get_key(&self) -> Result<Option<String>, SecretError> {
        match passwords::get_generic_password(SERVICE, ACCOUNT) {
            Ok(bytes) => String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| SecretError::Unavailable("stored key is not text".into())),
            Err(e) if e.code() == ITEM_NOT_FOUND => Ok(None),
            Err(e) => Err(SecretError::Unavailable(format!(
                "Keychain error {}",
                e.code()
            ))),
        }
    }

    fn set_key(&self, key: &str) -> Result<(), SecretError> {
        passwords::set_generic_password(SERVICE, ACCOUNT, key.as_bytes())
            .map_err(|e| SecretError::Unavailable(format!("Keychain error {}", e.code())))
    }

    fn delete_key(&self) -> Result<(), SecretError> {
        match passwords::delete_generic_password(SERVICE, ACCOUNT) {
            Ok(()) => Ok(()),
            Err(e) if e.code() == ITEM_NOT_FOUND => Ok(()),
            Err(e) => Err(SecretError::Unavailable(format!(
                "Keychain error {}",
                e.code()
            ))),
        }
    }
}

/// `SMAppService.mainAppService`; the OS is the authority on its state.
pub struct MacLoginItem;

impl LoginItem for MacLoginItem {
    fn enabled(&self) -> bool {
        let service = unsafe { SMAppService::mainAppService() };
        unsafe { service.status() == SMAppServiceStatus::Enabled }
    }

    fn set_enabled(&self, on: bool) -> Result<(), String> {
        let service = unsafe { SMAppService::mainAppService() };
        let result = unsafe {
            if on {
                service.registerAndReturnError()
            } else {
                service.unregisterAndReturnError()
            }
        };
        result.map_err(|e| {
            if on && unsafe { service.status() } == SMAppServiceStatus::RequiresApproval {
                "Approve Cantrip in System Settings › General › Login Items".to_string()
            } else {
                format!("Login item error {}", e.code())
            }
        })
    }
}
