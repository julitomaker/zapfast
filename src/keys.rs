//! API keys for AI providers, kept in the OS keyring and shared by every
//! feature that calls them (Projects' analysis, transcription, pictures).
//! The service and account names match `projects::engine`'s, so a key added
//! in one place works in the other.

use std::sync::Arc;

const SERVICE: &str = "com.julitomaker.whatspro.ai";

fn store() -> anyhow::Result<Arc<dyn keyring_core::api::CredentialStoreApi>> {
    #[cfg(target_os = "linux")]
    let store = zbus_secret_service_keyring_store::Store::new();
    #[cfg(target_os = "macos")]
    let store = apple_native_keyring_store::keychain::Store::new();
    #[cfg(windows)]
    let store = windows_native_keyring_store::Store::new();
    Ok(store.map_err(|error| anyhow::anyhow!("{error}"))?
        as Arc<dyn keyring_core::api::CredentialStoreApi>)
}

fn entry(account: &str) -> anyhow::Result<keyring_core::Entry> {
    store()?
        .build(SERVICE, account, None)
        .map_err(|error| anyhow::anyhow!("{error}"))
}

/// The key saved for `account` (`"OpenAi"`, `"Anthropic"`, `"Gemini"`…).
pub fn get(account: &str) -> Option<String> {
    entry(account)
        .ok()?
        .get_password()
        .ok()
        .filter(|key| !key.trim().is_empty())
}

/// Saves `key` for `account`; an empty key removes it.
pub fn set(account: &str, key: &str) -> anyhow::Result<()> {
    let entry = entry(account)?;
    if key.trim().is_empty() {
        match entry.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(error) => Err(anyhow::anyhow!("{error}")),
        }
    } else {
        entry
            .set_password(key.trim())
            .map_err(|error| anyhow::anyhow!("{error}"))
    }
}
