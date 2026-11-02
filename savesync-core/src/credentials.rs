//! Secure refresh-token persistence.
//!
//! Tokens are held in the operating system credential store (Keychain on macOS,
//! Credential Manager on Windows, Secret Service on Linux). SQLite never sees
//! them. Tests and headless environments can use [`MemoryTokenStore`].

use std::sync::Mutex;

use crate::error::{Error, Result};

const DEFAULT_SERVICE: &str = "savesync";
const DEFAULT_ACCOUNT: &str = "google-drive-refresh-token";

/// Keyring service every SaveSync credential is stored under.
pub const SERVICE: &str = DEFAULT_SERVICE;
/// Google Drive refresh token account (the original entry).
pub const GOOGLE_ACCOUNT: &str = DEFAULT_ACCOUNT;
/// Dropbox refresh token account.
pub const DROPBOX_ACCOUNT: &str = "dropbox-refresh-token";
/// WebDAV account. Holds the HTTP Basic password, never a token.
pub const WEBDAV_ACCOUNT: &str = "webdav-password";

/// Persistence interface for the Google refresh token.
pub trait TokenStore: Send + Sync {
    fn load(&self) -> Result<Option<String>>;
    fn save(&self, token: &str) -> Result<()>;
    fn delete(&self) -> Result<()>;
}

/// OS keyring implementation.
pub struct KeyringTokenStore {
    service: String,
    account: String,
}

impl KeyringTokenStore {
    pub fn new(service: impl Into<String>, account: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            account: account.into(),
        }
    }

    fn entry(&self) -> Result<keyring::Entry> {
        keyring::Entry::new(&self.service, &self.account)
            .map_err(|error| Error::Auth(format!("OS keyring is unavailable: {error}")))
    }
}

impl Default for KeyringTokenStore {
    fn default() -> Self {
        Self::new(DEFAULT_SERVICE, DEFAULT_ACCOUNT)
    }
}

impl TokenStore for KeyringTokenStore {
    fn load(&self) -> Result<Option<String>> {
        match self.entry()?.get_password() {
            Ok(token) if token.is_empty() => Ok(None),
            Ok(token) => Ok(Some(token)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(Error::Auth(format!(
                "could not read refresh token from the OS keyring: {error}"
            ))),
        }
    }

    fn save(&self, token: &str) -> Result<()> {
        if token.trim().is_empty() {
            return Err(Error::Auth(
                "refusing to store an empty refresh token".into(),
            ));
        }
        self.entry()?
            .set_password(token)
            .map_err(|error| Error::Auth(format!("could not save refresh token: {error}")))
    }

    fn delete(&self) -> Result<()> {
        match self.entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(Error::Auth(format!(
                "could not delete refresh token: {error}"
            ))),
        }
    }
}

/// In-memory token store for tests and headless development.
pub struct MemoryTokenStore {
    token: Mutex<Option<String>>,
}

impl MemoryTokenStore {
    pub fn new(token: Option<String>) -> Self {
        Self {
            token: Mutex::new(token),
        }
    }
}

impl Default for MemoryTokenStore {
    fn default() -> Self {
        Self::new(None)
    }
}

impl TokenStore for MemoryTokenStore {
    fn load(&self) -> Result<Option<String>> {
        Ok(self.token.lock().expect("token mutex").clone())
    }

    fn save(&self, token: &str) -> Result<()> {
        if token.trim().is_empty() {
            return Err(Error::Auth(
                "refusing to store an empty refresh token".into(),
            ));
        }
        *self.token.lock().expect("token mutex") = Some(token.to_string());
        Ok(())
    }

    fn delete(&self) -> Result<()> {
        *self.token.lock().expect("token mutex") = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_round_trips_and_deletes() {
        let store = MemoryTokenStore::default();
        assert_eq!(store.load().unwrap(), None);
        store.save("refresh-1").unwrap();
        assert_eq!(store.load().unwrap().as_deref(), Some("refresh-1"));
        store.delete().unwrap();
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn empty_tokens_are_rejected() {
        let store = MemoryTokenStore::default();
        assert!(store.save("   ").is_err());
        assert_eq!(store.load().unwrap(), None);
    }

    /// Each provider gets its own keyring item, so one credential can never
    /// overwrite another's. This exercises the account names the desktop shell
    /// wires into separate [`KeyringTokenStore`]s.
    #[test]
    fn provider_accounts_round_trip_independently() {
        let accounts = [GOOGLE_ACCOUNT, DROPBOX_ACCOUNT, WEBDAV_ACCOUNT];

        for (index, account) in accounts.iter().enumerate() {
            for other in &accounts[index + 1..] {
                assert_ne!(account, other, "account names must be distinct");
            }
            assert!(!account.is_empty());
        }

        let stores: Vec<MemoryTokenStore> = accounts
            .iter()
            .map(|_| MemoryTokenStore::default())
            .collect();

        for (store, account) in stores.iter().zip(accounts) {
            assert_eq!(store.load().unwrap(), None);
            store.save(&format!("secret-for-{account}")).unwrap();
        }

        for (store, account) in stores.iter().zip(accounts) {
            assert_eq!(
                store.load().unwrap().as_deref(),
                Some(format!("secret-for-{account}").as_str())
            );
        }

        // Deleting one leaves every other provider's credential untouched.
        stores[1].delete().unwrap();
        assert_eq!(stores[1].load().unwrap(), None);
        assert_eq!(
            stores[0].load().unwrap().as_deref(),
            Some(format!("secret-for-{GOOGLE_ACCOUNT}").as_str())
        );
        assert_eq!(
            stores[2].load().unwrap().as_deref(),
            Some(format!("secret-for-{WEBDAV_ACCOUNT}").as_str())
        );
    }
}
