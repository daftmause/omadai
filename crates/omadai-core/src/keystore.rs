//! Credential storage: Secret Service on Linux, an in-memory fake for tests, and a stub elsewhere.

use crate::secret::Secret;
use std::fmt;

pub const SERVICE: &str = "omadai";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeystoreError {
    /// The default collection is locked; omadai never asks to unlock it.
    Locked,
    /// No Secret Service could be reached.
    Unavailable(String),
    /// The Secret Service has no collection under the `default` alias.
    NoDefaultCollection,
    NotFound,
    AlreadyExists,
    NotSupported(&'static str),
    Backend(String),
}

impl fmt::Display for KeystoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeystoreError::Locked => f.write_str("keyring locked: unlock your login keyring, then retry (omadai never prompts to unlock it)"),
            KeystoreError::Unavailable(e) => write!(f, "keyring unavailable: {e}"),
            KeystoreError::NoDefaultCollection => f.write_str("keyring has no default collection"),
            KeystoreError::NotFound => f.write_str("no such credential in the keyring"),
            KeystoreError::AlreadyExists => f.write_str("a credential with that label already exists"),
            KeystoreError::NotSupported(what) => write!(f, "{what} is not supported yet"),
            KeystoreError::Backend(e) => write!(f, "keyring error: {e}"),
        }
    }
}

impl std::error::Error for KeystoreError {}

/// One stored credential's identity; carries no secret.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Entry {
    pub platform: String,
    pub label: String,
}

/// Credentials addressed by platform and label, under service `omadai`.
pub trait Keystore: Send + Sync {
    /// A short name for `doctor`.
    fn kind(&self) -> &'static str;
    /// Whether the default collection is locked, read without prompting.
    fn is_locked(&self) -> Result<bool, KeystoreError>;
    fn get(&self, platform: &str, label: &str) -> Result<Option<Secret>, KeystoreError>;
    /// Fails with `AlreadyExists` rather than replacing a stored credential.
    fn put(&self, platform: &str, label: &str, secret: &Secret) -> Result<(), KeystoreError>;
    /// Returns whether anything was deleted.
    fn delete(&self, platform: &str, label: &str) -> Result<bool, KeystoreError>;
    /// Relabels in place, so the secret is never read into this process.
    fn rename(&self, platform: &str, from: &str, to: &str) -> Result<(), KeystoreError>;
    fn list(&self) -> Result<Vec<Entry>, KeystoreError>;
}

/// The keystore the product uses; a build with the `test-fakes` feature gets the in-memory fake.
pub fn open_default() -> Box<dyn Keystore> {
    #[cfg(feature = "test-fakes")]
    {
        Box::new(memory::MemoryKeystore::new())
    }
    #[cfg(all(not(feature = "test-fakes"), target_os = "linux"))]
    {
        Box::new(secret_service_store::SecretServiceKeystore)
    }
    #[cfg(all(not(feature = "test-fakes"), not(target_os = "linux")))]
    {
        Box::new(unsupported::UnsupportedKeystore)
    }
}

pub mod memory {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    type Items = BTreeMap<(String, String), Secret>;

    /// Holds credentials in process memory only; for tests and test builds.
    #[derive(Default)]
    pub struct MemoryKeystore {
        items: Mutex<Items>,
        locked: AtomicBool,
    }

    impl MemoryKeystore {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn set_locked(&self, locked: bool) {
            self.locked.store(locked, Ordering::SeqCst);
        }

        fn check(&self) -> Result<std::sync::MutexGuard<'_, Items>, KeystoreError> {
            if self.locked.load(Ordering::SeqCst) {
                return Err(KeystoreError::Locked);
            }
            self.items
                .lock()
                .map_err(|_| KeystoreError::Backend("poisoned".into()))
        }
    }

    fn key(platform: &str, label: &str) -> (String, String) {
        (platform.to_string(), label.to_string())
    }

    impl Keystore for MemoryKeystore {
        fn kind(&self) -> &'static str {
            "in-memory (test build, nothing is persisted)"
        }

        fn is_locked(&self) -> Result<bool, KeystoreError> {
            Ok(self.locked.load(Ordering::SeqCst))
        }

        fn get(&self, platform: &str, label: &str) -> Result<Option<Secret>, KeystoreError> {
            Ok(self
                .check()?
                .get(&key(platform, label))
                .map(|s| Secret::new(s.expose().to_string())))
        }

        fn put(&self, platform: &str, label: &str, secret: &Secret) -> Result<(), KeystoreError> {
            let mut items = self.check()?;
            if items.contains_key(&key(platform, label)) {
                return Err(KeystoreError::AlreadyExists);
            }
            items.insert(
                key(platform, label),
                Secret::new(secret.expose().to_string()),
            );
            Ok(())
        }

        fn delete(&self, platform: &str, label: &str) -> Result<bool, KeystoreError> {
            Ok(self.check()?.remove(&key(platform, label)).is_some())
        }

        fn rename(&self, platform: &str, from: &str, to: &str) -> Result<(), KeystoreError> {
            let mut items = self.check()?;
            if items.contains_key(&key(platform, to)) {
                return Err(KeystoreError::AlreadyExists);
            }
            let s = items
                .remove(&key(platform, from))
                .ok_or(KeystoreError::NotFound)?;
            items.insert(key(platform, to), s);
            Ok(())
        }

        fn list(&self) -> Result<Vec<Entry>, KeystoreError> {
            Ok(self
                .check()?
                .keys()
                .map(|(p, l)| Entry {
                    platform: p.clone(),
                    label: l.clone(),
                })
                .collect())
        }
    }
}

#[cfg(target_os = "linux")]
pub mod secret_service_store {
    use super::*;
    use secret_service::EncryptionType;
    use secret_service::Error as SsError;
    use secret_service::blocking::{Collection, Item, SecretService};
    use std::collections::HashMap;

    /// The freedesktop Secret Service, reached through its default collection; connects per call.
    pub struct SecretServiceKeystore;

    fn backend(e: SsError) -> KeystoreError {
        match e {
            SsError::Locked => KeystoreError::Locked,
            SsError::Unavailable => {
                KeystoreError::Unavailable("no Secret Service provider found".into())
            }
            SsError::NoResult => KeystoreError::NotFound,
            other => KeystoreError::Backend(other.to_string()),
        }
    }

    fn attrs<'a>(platform: &'a str, label: &'a str) -> HashMap<&'a str, &'a str> {
        HashMap::from([
            ("service", SERVICE),
            ("platform", platform),
            ("label", label),
        ])
    }

    fn item_label(platform: &str, label: &str) -> String {
        format!("omadai: {platform} ({label})")
    }

    /// Connects and opens the default collection, failing on a locked one before anything else touches it.
    fn with_unlocked<T>(
        f: impl FnOnce(&Collection<'_>) -> Result<T, KeystoreError>,
    ) -> Result<T, KeystoreError> {
        let ss = SecretService::connect(EncryptionType::Dh)
            .map_err(|e| KeystoreError::Unavailable(e.to_string()))?;
        let collection = ss.get_default_collection().map_err(|e| match e {
            SsError::NoResult => KeystoreError::NoDefaultCollection,
            other => backend(other),
        })?;
        if collection.is_locked().map_err(backend)? {
            return Err(KeystoreError::Locked);
        }
        f(&collection)
    }

    fn unlocked(item: &Item<'_>) -> Result<(), KeystoreError> {
        if item.is_locked().map_err(backend)? {
            Err(KeystoreError::Locked)
        } else {
            Ok(())
        }
    }

    impl Keystore for SecretServiceKeystore {
        fn kind(&self) -> &'static str {
            "Secret Service (default collection)"
        }

        fn is_locked(&self) -> Result<bool, KeystoreError> {
            match with_unlocked(|_| Ok(())) {
                Ok(()) => Ok(false),
                Err(KeystoreError::Locked) => Ok(true),
                Err(e) => Err(e),
            }
        }

        fn get(&self, platform: &str, label: &str) -> Result<Option<Secret>, KeystoreError> {
            with_unlocked(|c| {
                let items = c.search_items(attrs(platform, label)).map_err(backend)?;
                let Some(item) = items.first() else {
                    return Ok(None);
                };
                unlocked(item)?;
                Secret::from_bytes(item.get_secret().map_err(backend)?)
                    .map(Some)
                    .ok_or_else(|| KeystoreError::Backend("stored credential is not UTF-8".into()))
            })
        }

        fn put(&self, platform: &str, label: &str, secret: &Secret) -> Result<(), KeystoreError> {
            with_unlocked(|c| {
                if !c
                    .search_items(attrs(platform, label))
                    .map_err(backend)?
                    .is_empty()
                {
                    return Err(KeystoreError::AlreadyExists);
                }
                c.create_item(
                    &item_label(platform, label),
                    attrs(platform, label),
                    secret.expose().as_bytes(),
                    false,
                    "text/plain",
                )
                .map_err(backend)?;
                Ok(())
            })
        }

        fn delete(&self, platform: &str, label: &str) -> Result<bool, KeystoreError> {
            with_unlocked(|c| {
                let items = c.search_items(attrs(platform, label)).map_err(backend)?;
                for item in &items {
                    unlocked(item)?;
                    item.delete().map_err(backend)?;
                }
                Ok(!items.is_empty())
            })
        }

        fn rename(&self, platform: &str, from: &str, to: &str) -> Result<(), KeystoreError> {
            with_unlocked(|c| {
                if !c
                    .search_items(attrs(platform, to))
                    .map_err(backend)?
                    .is_empty()
                {
                    return Err(KeystoreError::AlreadyExists);
                }
                let items = c.search_items(attrs(platform, from)).map_err(backend)?;
                let item = items.first().ok_or(KeystoreError::NotFound)?;
                unlocked(item)?;
                item.set_attributes(attrs(platform, to)).map_err(backend)?;
                if let Err(e) = item.set_label(&item_label(platform, to)) {
                    // The config still names the old label, so the item must stay findable by it.
                    let _ = item.set_attributes(attrs(platform, from));
                    return Err(backend(e));
                }
                Ok(())
            })
        }

        fn list(&self) -> Result<Vec<Entry>, KeystoreError> {
            with_unlocked(|c| {
                let items = c
                    .search_items(HashMap::from([("service", SERVICE)]))
                    .map_err(backend)?;
                let mut out = Vec::new();
                for item in &items {
                    let a = item.get_attributes().map_err(backend)?;
                    if let (Some(p), Some(l)) = (a.get("platform"), a.get("label")) {
                        out.push(Entry {
                            platform: p.clone(),
                            label: l.clone(),
                        });
                    }
                }
                out.sort();
                Ok(out)
            })
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Touches the real session keyring, so it runs only when asked for twice over.
        #[test]
        #[ignore = "touches the real Secret Service; set OMADAI_TEST_REAL_KEYRING=1 and pass --ignored"]
        fn real_keyring_round_trip() {
            if std::env::var_os("OMADAI_TEST_REAL_KEYRING").as_deref()
                != Some(std::ffi::OsStr::new("1"))
            {
                eprintln!("skipped: OMADAI_TEST_REAL_KEYRING is not 1");
                return;
            }
            let ks = SecretServiceKeystore;
            let label = format!("omadai-test-{}", std::process::id());
            let _ = ks.delete("openai", &label);
            ks.put("openai", &label, &Secret::new("not-a-real-key".into()))
                .unwrap();
            assert_eq!(
                ks.get("openai", &label).unwrap().unwrap().expose(),
                "not-a-real-key"
            );
            ks.rename("openai", &label, &format!("{label}-b")).unwrap();
            assert!(
                ks.list()
                    .unwrap()
                    .iter()
                    .any(|e| e.label == format!("{label}-b"))
            );
            assert!(ks.delete("openai", &format!("{label}-b")).unwrap());
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub mod unsupported {
    use super::*;

    /// Placeholder until the macOS Keychain store lands.
    pub struct UnsupportedKeystore;

    const WHAT: &str = "credential storage on this OS";

    impl Keystore for UnsupportedKeystore {
        fn kind(&self) -> &'static str {
            "none (not supported yet on this OS)"
        }
        fn is_locked(&self) -> Result<bool, KeystoreError> {
            Err(KeystoreError::NotSupported(WHAT))
        }
        fn get(&self, _: &str, _: &str) -> Result<Option<Secret>, KeystoreError> {
            Err(KeystoreError::NotSupported(WHAT))
        }
        fn put(&self, _: &str, _: &str, _: &Secret) -> Result<(), KeystoreError> {
            Err(KeystoreError::NotSupported(WHAT))
        }
        fn delete(&self, _: &str, _: &str) -> Result<bool, KeystoreError> {
            Err(KeystoreError::NotSupported(WHAT))
        }
        fn rename(&self, _: &str, _: &str, _: &str) -> Result<(), KeystoreError> {
            Err(KeystoreError::NotSupported(WHAT))
        }
        fn list(&self) -> Result<Vec<Entry>, KeystoreError> {
            Err(KeystoreError::NotSupported(WHAT))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::memory::MemoryKeystore;
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn memory_round_trip() {
        let ks = MemoryKeystore::new();
        ks.put("openai", "a", &Secret::new("k1".into())).unwrap();
        assert_eq!(
            ks.put("openai", "a", &Secret::new("k2".into())),
            Err(KeystoreError::AlreadyExists)
        );
        ks.rename("openai", "a", "b").unwrap();
        assert!(ks.get("openai", "a").unwrap().is_none());
        assert_eq!(ks.get("openai", "b").unwrap().unwrap().expose(), "k1");
        assert_eq!(
            ks.list().unwrap(),
            vec![Entry {
                platform: "openai".into(),
                label: "b".into()
            }]
        );
        assert!(ks.delete("openai", "b").unwrap());
        assert!(!ks.delete("openai", "b").unwrap());
    }

    #[test]
    fn locked_store_fails_fast() {
        let ks = MemoryKeystore::new();
        ks.set_locked(true);
        let start = Instant::now();
        assert_eq!(ks.get("openai", "a").err(), Some(KeystoreError::Locked));
        assert_eq!(
            ks.put("openai", "a", &Secret::new("k".into())),
            Err(KeystoreError::Locked)
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(
            KeystoreError::Locked
                .to_string()
                .starts_with("keyring locked")
        );
    }
}
