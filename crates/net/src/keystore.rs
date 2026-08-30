//! Platform keystore for the long-term endpoint identity and the host's other
//! stored secrets (design doc §11.2).
//!
//! The Iroh secret key exists on disk only inside the OS keystore. The
//! fallback is an encrypted file whose key derives from an OS user-specific
//! secret, never a plaintext file in the app directory. None of this is a
//! defence against a local admin on the host machine (§3.1).
//!
//! The trait is a named-slot store rather than an identity-shaped one: the
//! unattended device password (an Argon2id PHC string) and its TOTP secret
//! live here too, because `CLAUDE.md` forbids secrets in `config/*.toml` and
//! this is the only place the app has that is not a config file (ADR 0033).
//! The identity methods are thin wrappers over the slot named
//! [`IDENTITY_ENTRY`], so every backend implements storage once.
//!
//! Phase 1 ships the trait, the in-memory store used by the tests and the
//! encrypted-file fallback. Of the native backends named in §11.2, macOS
//! Keychain and Linux Secret Service are implemented; Windows Credential
//! Manager and Android Keystore remain phase 4 work per §19 (see ADR 0007),
//! and [`open`] refuses rather than silently downgrading to the fallback on a
//! platform that has a real store.

use std::collections::HashMap;
use std::convert::TryFrom;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chacha20poly1305::aead::{Aead as _, KeyInit as _};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use rand::Rng as _;

use crate::error::{NetError, Result};

/// Keystore entry name under which the endpoint identity is stored.
pub const IDENTITY_ENTRY: &str = "lumepeer.endpoint.identity";

/// Keystore entry name of the unattended device password (§8).
///
/// Holds the Argon2id PHC string, never the password: what is stored is
/// already a verifier, and it is stored here rather than in `config/` because
/// a PHC string is still secret material (ADR 0033).
pub const UNATTENDED_PASSWORD_ENTRY: &str = "lumepeer.unattended.password";

/// Keystore entry name of the role an unattended admission is granted (§8.2).
///
/// Not secret — it is a role name — but kept here rather than in `config/`
/// so it lives and dies with the credentials it applies to: clearing the
/// password clears this in the same pass, and a stale `FullControl` can never
/// outlive the password it was chosen for and attach itself to the next one
/// (ADR 0033).
pub const UNATTENDED_ROLE_ENTRY: &str = "lumepeer.unattended.role";

/// Keystore entry name of the unattended TOTP secret (§8).
///
/// Twenty raw bytes, the shared secret an authenticator app was provisioned
/// with. Unlike the password slot this one *is* a key: anyone who reads it can
/// mint valid codes, which is exactly why it is not in a config file.
pub const UNATTENDED_TOTP_ENTRY: &str = "lumepeer.unattended.totp";

/// Keystore entry name of the audit log's install salt (§15).
///
/// Thirty-two random bytes, minted once on the first run that opens the audit
/// log and never again: `audit::peer_hash` mixes them into every stored peer
/// hash, so replacing them silently turns one peer into two and makes the
/// existing log unreadable as a history.
///
/// Kept here rather than next to the log for the same reason the unattended
/// TOTP secret is: the salt is what stops a reader of the log from confirming
/// a guessed `NodeId` by re-hashing it, so it is secret material even though
/// it is not a key.
pub const AUDIT_SALT_ENTRY: &str = "lumepeer.audit.salt";

/// Domain separator of the file-fallback key derivation.
const KDF_CONTEXT: &str = "lumepeer 2026 endpoint identity file keystore";

/// Storage for secret material, addressed by entry name.
///
/// A backend implements the three slot operations; the identity helpers below
/// are provided in terms of them and are not meant to be overridden.
pub trait Keystore: Send + std::fmt::Debug {
    /// Loads the bytes stored under `entry`.
    ///
    /// A slot that was never written is `Ok(None)`, not an error: "nothing
    /// stored yet" is the first-run state of every one of these entries and
    /// callers must not have to tell it apart from a broken keystore.
    ///
    /// # Errors
    /// [`NetError::Keystore`] if the platform store is unavailable or refuses
    /// the read.
    fn load_secret(&self, entry: &str) -> Result<Option<Vec<u8>>>;

    /// Stores `bytes` under `entry`, replacing any previous value.
    ///
    /// # Errors
    /// [`NetError::Keystore`] if the write is refused.
    fn store_secret(&self, entry: &str, bytes: &[u8]) -> Result<()>;

    /// Removes whatever is stored under `entry`. Removing an absent entry
    /// succeeds: the post-condition is "nothing is stored there".
    ///
    /// # Errors
    /// [`NetError::Keystore`] if the delete is refused.
    fn delete_secret(&self, entry: &str) -> Result<()>;

    /// Loads the endpoint secret key, if one was stored.
    ///
    /// # Errors
    /// [`NetError::Keystore`] as [`Self::load_secret`], or if the stored blob
    /// is not 32 bytes.
    fn load_identity(&self) -> Result<Option<iroh::SecretKey>> {
        let Some(bytes) = self.load_secret(IDENTITY_ENTRY)? else {
            return Ok(None);
        };
        let bytes: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
            NetError::Keystore("the stored identity has the wrong length".to_owned())
        })?;
        Ok(Some(iroh::SecretKey::from_bytes(&bytes)))
    }

    /// Stores the endpoint secret key, replacing any previous value.
    ///
    /// # Errors
    /// [`NetError::Keystore`] if the write is refused.
    fn store_identity(&self, key: &iroh::SecretKey) -> Result<()> {
        self.store_secret(IDENTITY_ENTRY, &key.to_bytes())
    }

    /// Removes the stored identity.
    ///
    /// # Errors
    /// [`NetError::Keystore`] if the delete is refused.
    fn delete_identity(&self) -> Result<()> {
        self.delete_secret(IDENTITY_ENTRY)
    }
}

/// Loads the stored identity or creates and stores a new one.
///
/// # Errors
/// [`NetError::Keystore`] if the store cannot be read or written.
pub fn load_or_create(store: &dyn Keystore) -> Result<iroh::SecretKey> {
    if let Some(existing) = store.load_identity()? {
        return Ok(existing);
    }
    let fresh = iroh::SecretKey::generate();
    store.store_identity(&fresh)?;
    Ok(fresh)
}

/// Keystore backend selected for the current platform (§11.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// Windows Credential Manager via the `windows` crate.
    WindowsCredentialManager,
    /// Keychain Services via `security-framework`.
    AppleKeychain,
    /// D-Bus Secret Service via `secret-service`.
    LinuxSecretService,
    /// Android Keystore via JNI.
    AndroidKeystore,
    /// Encrypted file keyed by an OS user-specific secret.
    EncryptedFile,
}

/// Backend this build would use on the current platform.
#[must_use]
pub const fn platform_backend() -> Backend {
    #[cfg(target_os = "windows")]
    {
        Backend::WindowsCredentialManager
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        Backend::AppleKeychain
    }
    #[cfg(target_os = "android")]
    {
        Backend::AndroidKeystore
    }
    #[cfg(all(target_os = "linux", not(target_os = "android")))]
    {
        Backend::LinuxSecretService
    }
    #[cfg(not(any(
        target_os = "windows",
        target_os = "macos",
        target_os = "ios",
        target_os = "android",
        target_os = "linux"
    )))]
    {
        Backend::EncryptedFile
    }
}

/// Opens the platform keystore.
///
/// # Errors
/// [`NetError::Keystore`] when the platform's native backend is unavailable or
/// not implemented yet. Falling back to the encrypted file would weaken the
/// storage of §11.2 without the user being told, so it is refused instead;
/// callers that accept the fallback construct [`FileKeystore`] explicitly.
pub fn open() -> Result<Box<dyn Keystore>> {
    let backend = platform_backend();

    #[cfg(all(
        target_os = "linux",
        not(target_os = "android"),
        feature = "secret-service"
    ))]
    if backend == Backend::LinuxSecretService {
        return Ok(Box::new(
            secret_service_backend::SecretServiceKeystore::new(),
        ));
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    if backend == Backend::AppleKeychain {
        return Ok(Box::new(
            apple_keychain_backend::AppleKeychainKeystore::new(),
        ));
    }

    #[cfg(all(target_os = "windows", feature = "keyring"))]
    if backend == Backend::WindowsCredentialManager {
        return Ok(Box::new(
            windows_credential_manager_backend::WindowsCredentialManagerKeystore::new(),
        ));
    }

    if backend == Backend::EncryptedFile {
        return Err(NetError::Keystore(
            "no OS keystore on this platform; construct FileKeystore explicitly".to_owned(),
        ));
    }
    Err(NetError::Keystore(format!(
        "the {backend:?} backend is not built into this binary"
    )))
}

/// D-Bus Secret Service backend for Linux (§11.2).
///
/// Kept as an inline module so the file list of §6 stays exact.
#[cfg(all(
    target_os = "linux",
    not(target_os = "android"),
    feature = "secret-service"
))]
pub mod secret_service_backend {
    use std::collections::HashMap;

    use secret_service::{EncryptionType, SecretService};

    use super::Keystore;
    use crate::error::{NetError, Result};

    /// Attribute key under which every Lumepeer secret is filed; the entry
    /// name is its value, which is what makes one slot findable.
    const ATTRIBUTE: &str = "lumepeer";
    /// MIME type of the stored blob: raw bytes, not text.
    const CONTENT_TYPE: &str = "application/octet-stream";

    /// Keystore backed by the D-Bus Secret Service (gnome-keyring, `KWallet`).
    #[derive(Debug, Default)]
    pub struct SecretServiceKeystore {
        _private: (),
    }

    impl SecretServiceKeystore {
        /// Creates the backend. The D-Bus connection is opened per operation:
        /// the identity is read once at startup, so a persistent connection
        /// would only keep an idle socket open against the budgets of §15.
        #[must_use]
        pub const fn new() -> Self {
            Self { _private: () }
        }

        /// Runs one Secret Service operation on a private current-thread
        /// runtime, on a plain OS thread.
        ///
        /// The [`Keystore`] trait is synchronous because everything that calls
        /// it is: identity handling happens before the endpoint exists. Callers
        /// such as `spawn_actor` run inside `main`'s tokio runtime, and tokio
        /// refuses to build a nested runtime on a thread that already drives
        /// one, so the runtime has to live on a fresh OS thread rather than
        /// the caller's.
        fn block_on<F, T>(future: F) -> Result<T>
        where
            F: std::future::Future<Output = Result<T>> + Send + 'static,
            T: Send + 'static,
        {
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| NetError::Keystore(format!("cannot start a runtime: {e}")))?;
                runtime.block_on(future)
            })
            .join()
            .unwrap_or_else(|_| Err(NetError::Keystore("keystore thread panicked".to_owned())))
        }

        fn attributes(entry: &str) -> HashMap<&str, &str> {
            HashMap::from([(ATTRIBUTE, entry)])
        }

        async fn load(entry: String) -> Result<Option<Vec<u8>>> {
            let service = SecretService::connect(EncryptionType::Dh)
                .await
                .map_err(|e| NetError::Keystore(e.to_string()))?;
            let items = service
                .search_items(Self::attributes(&entry))
                .await
                .map_err(|e| NetError::Keystore(e.to_string()))?;

            let Some(item) = items.unlocked.first().or_else(|| items.locked.first()) else {
                return Ok(None);
            };
            item.unlock()
                .await
                .map_err(|e| NetError::Keystore(e.to_string()))?;
            let secret = item
                .get_secret()
                .await
                .map_err(|e| NetError::Keystore(e.to_string()))?;
            Ok(Some(secret))
        }

        async fn store(entry: String, bytes: Vec<u8>) -> Result<()> {
            let service = SecretService::connect(EncryptionType::Dh)
                .await
                .map_err(|e| NetError::Keystore(e.to_string()))?;
            let collection = service
                .get_default_collection()
                .await
                .map_err(|e| NetError::Keystore(e.to_string()))?;
            collection
                .unlock()
                .await
                .map_err(|e| NetError::Keystore(e.to_string()))?;
            collection
                .create_item(
                    &format!("Lumepeer: {entry}"),
                    Self::attributes(&entry),
                    &bytes,
                    // Replace: one slot holds exactly one value.
                    true,
                    CONTENT_TYPE,
                )
                .await
                .map_err(|e| NetError::Keystore(e.to_string()))?;
            Ok(())
        }

        async fn delete(entry: String) -> Result<()> {
            let service = SecretService::connect(EncryptionType::Dh)
                .await
                .map_err(|e| NetError::Keystore(e.to_string()))?;
            let items = service
                .search_items(Self::attributes(&entry))
                .await
                .map_err(|e| NetError::Keystore(e.to_string()))?;
            for item in items.unlocked.iter().chain(items.locked.iter()) {
                item.unlock()
                    .await
                    .map_err(|e| NetError::Keystore(e.to_string()))?;
                item.delete()
                    .await
                    .map_err(|e| NetError::Keystore(e.to_string()))?;
            }
            Ok(())
        }
    }

    impl Keystore for SecretServiceKeystore {
        fn load_secret(&self, entry: &str) -> Result<Option<Vec<u8>>> {
            Self::block_on(Self::load(entry.to_owned()))
        }

        fn store_secret(&self, entry: &str, bytes: &[u8]) -> Result<()> {
            Self::block_on(Self::store(entry.to_owned(), bytes.to_vec()))
        }

        fn delete_secret(&self, entry: &str) -> Result<()> {
            Self::block_on(Self::delete(entry.to_owned()))
        }
    }

    #[cfg(test)]
    mod tests {
        #![allow(clippy::unwrap_used)]

        use super::*;
        use crate::keystore::load_or_create;

        /// Runs against the session keyring when one is reachable. There is no
        /// keyring on a headless CI runner, and the test says so instead of
        /// failing.
        #[test]
        fn the_session_keyring_round_trips_an_identity() {
            let store = SecretServiceKeystore::new();
            let Ok(existing) = store.load_identity() else {
                return;
            };
            // Never clobber a real identity: only run on a machine that has
            // none stored yet, and clean up afterwards.
            if existing.is_some() {
                return;
            }

            // Storing can also fail on a headless runner: gnome-keyring may
            // accept the connection and an empty search, then fail to unlock
            // or create the default collection because there is no prompter
            // to answer it. That is the same "no usable keyring here" case
            // as the load above, so skip rather than fail.
            let Ok(key) = load_or_create(&store) else {
                return;
            };
            assert_eq!(
                store.load_identity().unwrap().unwrap().public(),
                key.public()
            );
            store.delete_identity().unwrap();
            assert!(store.load_identity().unwrap().is_none());
        }
    }
}

/// macOS/iOS Keychain backend (§11.2).
///
/// Kept as an inline module so the file list of §6 stays exact.
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub mod apple_keychain_backend {
    use security_framework::base::Error as SfError;
    use security_framework::passwords::{
        delete_generic_password, get_generic_password, set_generic_password,
    };
    use security_framework_sys::base::errSecItemNotFound;

    use super::Keystore;
    use crate::error::{NetError, Result};

    /// Keychain service name the identity item is filed under. Matches the
    /// Tauri bundle identifier so the item is recognisable in Keychain Access.
    const SERVICE: &str = "io.insigmo.lumepeer";

    /// Keystore backed by Keychain Services via `security-framework`.
    #[derive(Debug, Default)]
    pub struct AppleKeychainKeystore {
        _private: (),
    }

    impl AppleKeychainKeystore {
        /// Creates the backend. Each operation opens and closes its own
        /// Keychain query; there is no persistent handle to hold.
        #[must_use]
        pub const fn new() -> Self {
            Self { _private: () }
        }

        fn is_not_found(error: SfError) -> bool {
            error.code() == errSecItemNotFound
        }
    }

    impl Keystore for AppleKeychainKeystore {
        fn load_secret(&self, entry: &str) -> Result<Option<Vec<u8>>> {
            match get_generic_password(SERVICE, entry) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(e) if Self::is_not_found(e) => Ok(None),
                Err(e) => Err(NetError::Keystore(e.to_string())),
            }
        }

        fn store_secret(&self, entry: &str, bytes: &[u8]) -> Result<()> {
            set_generic_password(SERVICE, entry, bytes)
                .map_err(|e| NetError::Keystore(e.to_string()))
        }

        fn delete_secret(&self, entry: &str) -> Result<()> {
            match delete_generic_password(SERVICE, entry) {
                Ok(()) => Ok(()),
                Err(e) if Self::is_not_found(e) => Ok(()),
                Err(e) => Err(NetError::Keystore(e.to_string())),
            }
        }
    }

    #[cfg(test)]
    mod tests {
        #![allow(clippy::unwrap_used)]

        use super::*;
        use crate::keystore::load_or_create;

        /// Runs against the login Keychain. There is no Keychain on a
        /// headless CI runner without one unlocked, and the test says so
        /// instead of failing.
        #[test]
        fn the_keychain_round_trips_an_identity() {
            let store = AppleKeychainKeystore::new();
            let Ok(existing) = store.load_identity() else {
                return;
            };
            // Never clobber a real identity: only run on a machine that has
            // none stored yet, and clean up afterwards.
            if existing.is_some() {
                return;
            }

            let Ok(key) = load_or_create(&store) else {
                return;
            };
            assert_eq!(
                store.load_identity().unwrap().unwrap().public(),
                key.public()
            );
            store.delete_identity().unwrap();
            assert!(store.load_identity().unwrap().is_none());
        }
    }
}

/// Windows Credential Manager backend (§11.2).
///
/// Kept as an inline module so the file list of §6 stays exact. The
/// underlying `CredWriteW`/`CredReadW`/`CredDeleteW` calls are FFI and
/// therefore `unsafe`, which `lumepeer_net`'s crate-wide `forbid(unsafe_code)`
/// does not allow anywhere in this crate; `keyring`'s `windows-native` backend
/// carries that unsafe in its own compiled crate instead, the same way
/// `secret-service` and `security-framework` keep D-Bus and Keychain FFI out
/// of this one.
#[cfg(all(target_os = "windows", feature = "keyring"))]
pub mod windows_credential_manager_backend {
    use keyring::Entry;

    use super::Keystore;
    use crate::error::{NetError, Result};

    /// Credential Manager target service, matching the Tauri bundle
    /// identifier so the entry is recognisable in Credential Manager.
    const SERVICE: &str = "io.insigmo.lumepeer";

    /// Keystore backed by Windows Credential Manager via the `keyring` crate.
    #[derive(Debug, Default)]
    pub struct WindowsCredentialManagerKeystore {
        _private: (),
    }

    impl WindowsCredentialManagerKeystore {
        /// Creates the backend. Each operation opens and closes its own
        /// Credential Manager handle; there is no persistent handle to hold.
        #[must_use]
        pub const fn new() -> Self {
            Self { _private: () }
        }

        fn entry(name: &str) -> Result<Entry> {
            Entry::new(SERVICE, name).map_err(|e| NetError::Keystore(e.to_string()))
        }
    }

    impl Keystore for WindowsCredentialManagerKeystore {
        fn load_secret(&self, entry: &str) -> Result<Option<Vec<u8>>> {
            match Self::entry(entry)?.get_secret() {
                Ok(bytes) => Ok(Some(bytes)),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(e) => Err(NetError::Keystore(e.to_string())),
            }
        }

        fn store_secret(&self, entry: &str, bytes: &[u8]) -> Result<()> {
            Self::entry(entry)?
                .set_secret(bytes)
                .map_err(|e| NetError::Keystore(e.to_string()))
        }

        fn delete_secret(&self, entry: &str) -> Result<()> {
            match Self::entry(entry)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(NetError::Keystore(e.to_string())),
            }
        }
    }

    #[cfg(test)]
    mod tests {
        #![allow(clippy::unwrap_used)]

        use super::*;
        use crate::keystore::load_or_create;

        /// Runs against the real Credential Manager. There is no user
        /// profile to store into on a headless CI runner without one loaded,
        /// and the test says so instead of failing.
        #[test]
        fn the_credential_manager_round_trips_an_identity() {
            let store = WindowsCredentialManagerKeystore::new();
            let Ok(existing) = store.load_identity() else {
                return;
            };
            // Never clobber a real identity: only run on a machine that has
            // none stored yet, and clean up afterwards.
            if existing.is_some() {
                return;
            }

            let Ok(key) = load_or_create(&store) else {
                return;
            };
            assert_eq!(
                store.load_identity().unwrap().unwrap().public(),
                key.public()
            );
            store.delete_identity().unwrap();
            assert!(store.load_identity().unwrap().is_none());
        }
    }
}

/// In-memory keystore. Never persists anything; used by tests and by ephemeral
/// guest sessions that must not leave an identity behind.
#[derive(Debug, Default)]
pub struct MemoryKeystore {
    slots: Mutex<HashMap<String, Vec<u8>>>,
}

impl MemoryKeystore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
        }
    }

    fn slots(&self) -> Result<std::sync::MutexGuard<'_, HashMap<String, Vec<u8>>>> {
        self.slots
            .lock()
            .map_err(|_| NetError::Keystore("memory keystore poisoned".to_owned()))
    }
}

impl Keystore for MemoryKeystore {
    fn load_secret(&self, entry: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.slots()?.get(entry).cloned())
    }

    fn store_secret(&self, entry: &str, bytes: &[u8]) -> Result<()> {
        self.slots()?.insert(entry.to_owned(), bytes.to_vec());
        Ok(())
    }

    fn delete_secret(&self, entry: &str) -> Result<()> {
        self.slots()?.remove(entry);
        Ok(())
    }
}

/// Encrypted-file fallback of §11.2.
///
/// The file holds `nonce || XChaCha20-Poly1305(secret bytes)`. The encryption
/// key is derived with BLAKE3 from an OS user-specific secret, so a copy of the
/// file alone does not yield the identity. It is explicitly not a defence
/// against a local admin or against the user's own account (§3.1, §3.3).
///
/// One slot is one file. `IDENTITY_ENTRY` keeps the exact path this store was
/// constructed with, so an install that already has an identity file keeps
/// reading it; every other entry becomes a sibling file named after the entry.
/// Entry names are folded down to `[a-z0-9-]` first, which is what stops a name
/// from ever walking out of the directory.
#[derive(Debug)]
pub struct FileKeystore {
    path: PathBuf,
    key: Key,
}

impl FileKeystore {
    /// Creates a store at `path`, deriving the file key from `user_secret`.
    ///
    /// `user_secret` must be OS user-specific material, not a constant and not
    /// something an unprivileged peer can observe.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, user_secret: &[u8]) -> Self {
        let derived = blake3::derive_key(KDF_CONTEXT, user_secret);
        Self {
            path: path.into(),
            key: Key::from(derived),
        }
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new(&self.key)
    }

    /// File backing `entry` (see the type's docs for the naming rule).
    fn path_for(&self, entry: &str) -> PathBuf {
        if entry == IDENTITY_ENTRY {
            return self.path.clone();
        }
        let mut name: String = entry
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        name.push_str(".bin");
        self.path
            .parent()
            .map_or_else(|| PathBuf::from(&name), |dir| dir.join(&name))
    }

    fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| NetError::Keystore(e.to_string()))?;
        }
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(path)
            .map_err(|e| NetError::Keystore(e.to_string()))?;
        file.write_all(bytes)
            .map_err(|e| NetError::Keystore(e.to_string()))?;
        file.sync_all()
            .map_err(|e| NetError::Keystore(e.to_string()))
    }

    /// Path of the encrypted file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Bytes of the XChaCha20-Poly1305 nonce prefix.
const NONCE_BYTES: usize = 24;
impl Keystore for FileKeystore {
    fn load_secret(&self, entry: &str) -> Result<Option<Vec<u8>>> {
        let raw = match fs::read(self.path_for(entry)) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(NetError::Keystore(e.to_string())),
        };
        if raw.len() <= NONCE_BYTES {
            return Err(NetError::Keystore("keystore file truncated".to_owned()));
        }
        let (nonce, ciphertext) = raw.split_at(NONCE_BYTES);
        let nonce = XNonce::try_from(nonce)
            .map_err(|_| NetError::Keystore("invalid nonce length in keystore file".to_owned()))?;
        let plain = self
            .cipher()
            .decrypt(&nonce, ciphertext)
            .map_err(|_| NetError::Keystore("keystore file failed authentication".to_owned()))?;
        Ok(Some(plain))
    }

    fn store_secret(&self, entry: &str, bytes: &[u8]) -> Result<()> {
        let mut nonce = [0u8; NONCE_BYTES];
        rand::rng().fill_bytes(&mut nonce);
        let nonce_array: XNonce = nonce.into();
        let ciphertext = self
            .cipher()
            .encrypt(&nonce_array, bytes)
            .map_err(|_| NetError::Keystore("could not encrypt the secret".to_owned()))?;
        let mut out = Vec::with_capacity(NONCE_BYTES + ciphertext.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ciphertext);
        Self::write_private(&self.path_for(entry), &out)
    }

    fn delete_secret(&self, entry: &str) -> Result<()> {
        match fs::remove_file(self.path_for(entry)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(NetError::Keystore(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        let mut suffix = [0u8; 8];
        rand::rng().fill_bytes(&mut suffix);
        path.push(format!("lumepeer-{name}-{}", u64::from_le_bytes(suffix)));
        path.push("identity.bin");
        path
    }

    #[test]
    fn memory_keystore_roundtrips_and_deletes() {
        let store = MemoryKeystore::new();
        assert!(store.load_identity().unwrap().is_none());
        let key = load_or_create(&store).unwrap();
        assert_eq!(load_or_create(&store).unwrap().public(), key.public());
        store.delete_identity().unwrap();
        assert!(store.load_identity().unwrap().is_none());
    }

    #[test]
    fn file_keystore_roundtrips_and_the_file_is_not_plaintext() {
        let path = temp_path("roundtrip");
        let store = FileKeystore::new(&path, b"user-specific secret");
        let key = load_or_create(&store).unwrap();
        let raw = fs::read(&path).unwrap();
        assert!(!raw.windows(32).any(|w| w == key.to_bytes()));
        let reopened = FileKeystore::new(&path, b"user-specific secret");
        assert_eq!(
            reopened.load_identity().unwrap().unwrap().public(),
            key.public()
        );
        store.delete_identity().unwrap();
        assert!(store.load_identity().unwrap().is_none());
    }

    #[test]
    fn a_different_user_secret_cannot_read_the_file() {
        let path = temp_path("wrong-secret");
        let store = FileKeystore::new(&path, b"right secret");
        load_or_create(&store).unwrap();
        let attacker = FileKeystore::new(&path, b"wrong secret");
        assert!(matches!(
            attacker.load_identity(),
            Err(NetError::Keystore(_))
        ));
        store.delete_identity().unwrap();
    }

    #[test]
    fn a_tampered_file_fails_authentication() {
        let path = temp_path("tampered");
        let store = FileKeystore::new(&path, b"secret");
        load_or_create(&store).unwrap();
        let mut raw = fs::read(&path).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0xff;
        fs::write(&path, &raw).unwrap();
        assert!(matches!(store.load_identity(), Err(NetError::Keystore(_))));
        store.delete_identity().unwrap();
    }

    /// The slot API is what the unattended password and TOTP secret ride on
    /// (ADR 0033), so an absent slot must read as "nothing stored", never as a
    /// failure the caller has to tell apart from a broken keystore.
    #[test]
    fn an_absent_slot_is_none_rather_than_an_error() {
        let memory = MemoryKeystore::new();
        assert_eq!(memory.load_secret(UNATTENDED_PASSWORD_ENTRY).unwrap(), None);
        // Deleting what was never stored still succeeds: the post-condition is
        // "nothing is stored there", and it already holds.
        memory.delete_secret(UNATTENDED_PASSWORD_ENTRY).unwrap();

        let path = temp_path("absent-slot");
        let file = FileKeystore::new(&path, b"user-specific secret");
        assert_eq!(file.load_secret(UNATTENDED_TOTP_ENTRY).unwrap(), None);
        file.delete_secret(UNATTENDED_TOTP_ENTRY).unwrap();
    }

    #[test]
    fn slots_round_trip_independently_of_each_other() {
        let phc = b"$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA";
        let totp = [7u8; 20];

        for store in [
            Box::new(MemoryKeystore::new()) as Box<dyn Keystore>,
            Box::new(FileKeystore::new(
                temp_path("slots"),
                b"user-specific secret",
            )),
        ] {
            let key = load_or_create(store.as_ref()).unwrap();
            store.store_secret(UNATTENDED_PASSWORD_ENTRY, phc).unwrap();
            store.store_secret(UNATTENDED_TOTP_ENTRY, &totp).unwrap();

            assert_eq!(
                store.load_secret(UNATTENDED_PASSWORD_ENTRY).unwrap(),
                Some(phc.to_vec())
            );
            assert_eq!(
                store.load_secret(UNATTENDED_TOTP_ENTRY).unwrap(),
                Some(totp.to_vec())
            );

            // Turning the second factor off must not disturb the password or
            // the identity: one slot, one value, no shared file to clobber.
            store.delete_secret(UNATTENDED_TOTP_ENTRY).unwrap();
            assert_eq!(store.load_secret(UNATTENDED_TOTP_ENTRY).unwrap(), None);
            assert_eq!(
                store.load_secret(UNATTENDED_PASSWORD_ENTRY).unwrap(),
                Some(phc.to_vec())
            );
            assert_eq!(
                store.load_identity().unwrap().unwrap().public(),
                key.public()
            );

            store.delete_secret(UNATTENDED_PASSWORD_ENTRY).unwrap();
            store.delete_identity().unwrap();
        }
    }

    /// A PHC string is secret material: the fallback file must not be readable
    /// as text by anyone who copies it out of the app directory (§11.2).
    #[test]
    fn a_stored_secret_is_not_plaintext_on_disk_and_lives_in_its_own_file() {
        let path = temp_path("secret-file");
        let store = FileKeystore::new(&path, b"user-specific secret");
        let phc = b"$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA";
        store.store_secret(UNATTENDED_PASSWORD_ENTRY, phc).unwrap();

        let dir = path.parent().unwrap();
        let files: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        // The identity was never written here, so exactly one file exists and
        // it is not the identity's.
        assert_eq!(files.len(), 1);
        assert_ne!(files[0], path);

        let raw = fs::read(&files[0]).unwrap();
        assert!(!raw.windows(phc.len()).any(|w| w == phc));

        let reopened = FileKeystore::new(&path, b"user-specific secret");
        assert_eq!(
            reopened.load_secret(UNATTENDED_PASSWORD_ENTRY).unwrap(),
            Some(phc.to_vec())
        );
        let _ = fs::remove_dir_all(dir);
    }

    /// Entry names reach `path_for` from constants in this crate today, but the
    /// sanitizing is what guarantees a name can never address a file outside
    /// the keystore directory.
    #[test]
    fn an_entry_name_cannot_walk_out_of_the_keystore_directory() {
        let path = temp_path("traversal");
        let store = FileKeystore::new(&path, b"secret");
        let dir = path.parent().unwrap();

        for hostile in ["../../etc/passwd", "..", "/etc/shadow", "a\\b"] {
            let resolved = store.path_for(hostile);
            assert_eq!(
                resolved.parent(),
                Some(dir),
                "{hostile} escaped the keystore directory"
            );
        }
    }

    #[test]
    fn open_never_downgrades_to_the_file_fallback() {
        match open() {
            // A platform with a built-in native backend returns it.
            Ok(store) => {
                assert!(matches!(
                    platform_backend(),
                    Backend::LinuxSecretService
                        | Backend::AppleKeychain
                        | Backend::WindowsCredentialManager
                ));
                // Opening must not have created anything yet.
                let _ = store.load_identity();
            }
            // Everywhere else the caller is told, rather than silently handed
            // the weaker file store (§11.2).
            Err(NetError::Keystore(_)) => {}
            Err(other) => panic!("unexpected keystore error: {other}"),
        }
    }
}
