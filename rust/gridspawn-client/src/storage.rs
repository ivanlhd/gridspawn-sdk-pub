//! Where the signed-in session lives between launches.
//!
//! Protocol 01 §3: refresh tokens must be kept in OS-protected storage, never
//! in plain files or in the game's save data. [`OsTokenStore`] uses the
//! operating system's credential store; when there is none, the client falls
//! back to [`MemoryTokenStore`] and the player signs in on each launch.

use std::{
    fmt,
    sync::{Mutex, MutexGuard},
};

use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use crate::auth::User;

/// Where the signed-in session lives between launches. Calls are synchronous;
/// the client runs them on tokio's blocking pool, since an OS store may wait
/// for the system (for example to unlock a keyring).
pub trait TokenStore: Send + Sync {
    fn load(&self) -> Result<Option<StoredSession>, StoreError>;
    fn save(&self, session: &StoredSession) -> Result<(), StoreError>;
    fn clear(&self) -> Result<(), StoreError>;
    /// Whether a saved session survives the process (default `true`).
    fn persistent(&self) -> bool {
        true
    }
}

/// The signed-in session as stored. `Debug` redacts both tokens; they are
/// zeroed when the value is dropped.
#[derive(Clone, Serialize, Deserialize)]
pub struct StoredSession {
    /// May be empty when a store could not keep it; the client then refreshes.
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds.
    pub access_expires_at: i64,
    pub scopes: Vec<String>,
    pub user: User,
}

impl StoredSession {
    /// What a loaded session needs to be usable.
    pub(crate) fn is_valid(&self) -> bool {
        !self.refresh_token.is_empty() && !self.user.subject.is_empty()
    }
}

impl fmt::Debug for StoredSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredSession")
            .field("access_token", &"****")
            .field("refresh_token", &"****")
            .field("access_expires_at", &self.access_expires_at)
            .field("scopes", &self.scopes)
            .field("user", &self.user)
            .finish()
    }
}

impl Drop for StoredSession {
    fn drop(&mut self) {
        self.access_token.zeroize();
        self.refresh_token.zeroize();
    }
}

/// A token store failure. The message never contains a token.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct StoreError(String);

impl StoreError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Keeps the session in memory only: nothing survives the process, so the
/// player signs in on each launch. The fallback when no OS store is available.
#[derive(Default)]
pub struct MemoryTokenStore {
    session: Mutex<Option<StoredSession>>,
}

impl MemoryTokenStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl TokenStore for MemoryTokenStore {
    fn load(&self) -> Result<Option<StoredSession>, StoreError> {
        Ok(lock(&self.session).clone())
    }

    fn save(&self, session: &StoredSession) -> Result<(), StoreError> {
        *lock(&self.session) = Some(session.clone());
        Ok(())
    }

    fn clear(&self) -> Result<(), StoreError> {
        *lock(&self.session) = None;
        Ok(())
    }

    fn persistent(&self) -> bool {
        false
    }
}

impl fmt::Debug for MemoryTokenStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MemoryTokenStore")
    }
}

#[cfg(feature = "os-store")]
pub use os::OsTokenStore;

#[cfg(feature = "os-store")]
mod os {
    use super::*;
    use crate::config::Environment;

    const SERVICE: &str = "gridspawn";
    /// Windows limits a credential to 2560 bytes. A session larger than this is
    /// saved without its access token (the client refreshes after a restart).
    const MAX_SECRET: usize = 2048;

    /// The operating system's credential store, through the `keyring` crate:
    /// Windows Credential Manager (DPAPI-protected), the macOS Keychain, and the
    /// Secret Service on Linux (GNOME Keyring, KWallet). One entry per
    /// application and environment: service `gridspawn`, account
    /// `<app id>/<environment>`; the value is the session as JSON.
    ///
    /// Not available on Linux without a Secret Service daemon on the session
    /// bus (minimal desktops, containers, WSL without a keyring daemon); plan
    /// for the same on Steam
    /// Deck in Game Mode. Check [`OsTokenStore::available`]; the client's
    /// default store falls back to memory by itself.
    pub struct OsTokenStore {
        account: String,
    }

    impl OsTokenStore {
        pub fn new(app_id: &str, environment: Environment) -> Self {
            Self {
                account: format!("{app_id}/{}", environment.as_str()),
            }
        }

        /// Whether the OS store answers on this machine (a read of a probe
        /// entry that is never written). May wait for the system, so call it
        /// off the game's main thread.
        pub fn available() -> bool {
            match keyring::Entry::new(SERVICE, "availability-probe") {
                Ok(entry) => matches!(entry.get_secret(), Ok(_) | Err(keyring::Error::NoEntry)),
                Err(_) => false,
            }
        }

        fn entry(&self) -> Result<keyring::Entry, StoreError> {
            keyring::Entry::new(SERVICE, &self.account).map_err(failure)
        }
    }

    fn failure(error: keyring::Error) -> StoreError {
        // keyring's messages name the platform failure, never the secret.
        StoreError::new(format!("OS credential store: {error}"))
    }

    impl TokenStore for OsTokenStore {
        fn load(&self) -> Result<Option<StoredSession>, StoreError> {
            match self.entry()?.get_secret() {
                Ok(mut bytes) => {
                    let session = serde_json::from_slice::<StoredSession>(&bytes);
                    bytes.zeroize();
                    // An unreadable entry is treated as absent; it is overwritten on the next save.
                    Ok(session.ok())
                }
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(error) => Err(failure(error)),
            }
        }

        fn save(&self, session: &StoredSession) -> Result<(), StoreError> {
            let encode = |session: &StoredSession| {
                serde_json::to_vec(session)
                    .map_err(|_| StoreError::new("the session could not be encoded"))
            };
            let mut bytes = encode(session)?;
            if bytes.len() > MAX_SECRET {
                bytes.zeroize();
                let mut compact = session.clone();
                compact.access_token.zeroize();
                compact.access_token.clear();
                compact.access_expires_at = 0;
                bytes = encode(&compact)?;
            }
            let result = self.entry()?.set_secret(&bytes).map_err(failure);
            bytes.zeroize();
            result
        }

        fn clear(&self) -> Result<(), StoreError> {
            match self.entry()?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(error) => Err(failure(error)),
            }
        }
    }

    impl fmt::Debug for OsTokenStore {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("OsTokenStore")
                .field("service", &SERVICE)
                .field("account", &self.account)
                .finish()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> StoredSession {
        StoredSession {
            access_token: "access-secret".into(),
            refresh_token: "refresh-secret".into(),
            access_expires_at: 1,
            scopes: vec!["identity.basic".into()],
            user: User {
                display_name: "Player".into(),
                subject: "sub".into(),
                avatar_object_path: None,
            },
        }
    }

    #[test]
    fn memory_store_round_trip() {
        let store = MemoryTokenStore::new();
        assert!(store.load().unwrap().is_none());
        store.save(&session()).unwrap();
        assert_eq!(
            store.load().unwrap().unwrap().refresh_token,
            "refresh-secret"
        );
        store.clear().unwrap();
        assert!(store.load().unwrap().is_none());
        assert!(!store.persistent());
    }

    #[test]
    fn debug_redacts_tokens() {
        let text = format!("{:?}", session());
        assert!(!text.contains("secret"), "{text}");
        assert!(text.contains("Player"));
    }

    #[cfg(feature = "os-store")]
    #[test]
    fn os_store_round_trip_when_available() {
        // The answer depends on the machine (false without a keyring daemon).
        let available = OsTokenStore::available();
        eprintln!("OS token store available: {available}");
        if !available {
            return;
        }
        let store = OsTokenStore::new("gridspawn-client-selftest", crate::Environment::Development);
        store.clear().unwrap();
        assert!(store.load().unwrap().is_none());
        store.save(&session()).unwrap();
        let loaded = store.load().unwrap().expect("saved");
        assert_eq!(loaded.access_token, "access-secret");
        assert_eq!(loaded.refresh_token, "refresh-secret");
        assert_eq!(loaded.user, session().user);

        // Too large for one credential: saved without the access token.
        let mut large = session();
        large.access_token = "a".repeat(3000);
        store.save(&large).unwrap();
        let loaded = store.load().unwrap().expect("saved");
        assert!(loaded.access_token.is_empty());
        assert_eq!(loaded.access_expires_at, 0);
        assert_eq!(loaded.refresh_token, "refresh-secret");

        store.clear().unwrap();
        store.clear().unwrap();
        assert!(store.load().unwrap().is_none());
        assert!(store.persistent());
    }
}
