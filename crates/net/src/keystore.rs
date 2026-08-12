//! Platform keystore for the long-term endpoint identity (design doc §11.2).
//!
//! The Iroh secret key exists on disk only inside the OS keystore. The
//! fallback is an encrypted file whose key derives from an OS user-specific
//! secret, never a plaintext file in the app directory. None of this is a
//! defence against a local admin on the host machine (§3.1).
//!
//! Phase 1 ships the trait, the in-memory store used by the tests and the
//! encrypted-file fallback. The native backends named in §11.2 (Credential
//! Manager, Keychain, Secret Service, Android Keystore) are phase 4 work per
//! §19, and [`open`] refuses rather than silently downgrading to the fallback
//! on a platform that has a real store.

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

/// Domain separator of the file-fallback key derivation.
const KDF_CONTEXT: &str = "lumepeer 2026 endpoint identity file keystore";

/// Storage for secret material.
pub trait Keystore: Send + std::fmt::Debug {
    /// Loads the endpoint secret key, if one was stored.
    ///
    /// # Errors
    /// [`NetError::Keystore`] if the platform store is unavailable or refuses
    /// the read.
    fn load_identity(&self) -> Result<Option<iroh::SecretKey>>;

    /// Stores the endpoint secret key, replacing any previous value.
    ///
    /// # Errors
    /// [`NetError::Keystore`] if the write is refused.
    fn store_identity(&self, key: &iroh::SecretKey) -> Result<()>;

    /// Removes the stored identity.
    ///
    /// # Errors
    /// [`NetError::Keystore`] if the delete is refused.
    fn delete_identity(&self) -> Result<()>;
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
/// [`NetError::Keystore`] on every platform whose native backend is still
/// phase 4 work. Falling back to the encrypted file there would weaken the
/// storage of §11.2 without the user being told, so it is refused instead;
/// callers that accept the fallback construct [`FileKeystore`] explicitly.
pub fn open() -> Result<Box<dyn Keystore>> {
    let backend = platform_backend();
    if backend == Backend::EncryptedFile {
        return Err(NetError::Keystore(
            "no OS keystore on this platform; construct FileKeystore explicitly".to_owned(),
        ));
    }
    Err(NetError::Keystore(format!(
        "{backend:?} backend is phase 4 work (design doc §19)"
    )))
}

/// In-memory keystore. Never persists anything; used by tests and by ephemeral
/// guest sessions that must not leave an identity behind.
#[derive(Debug, Default)]
pub struct MemoryKeystore {
    key: Mutex<Option<iroh::SecretKey>>,
}

impl MemoryKeystore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            key: Mutex::new(None),
        }
    }
}

impl Keystore for MemoryKeystore {
    fn load_identity(&self) -> Result<Option<iroh::SecretKey>> {
        let guard = self
            .key
            .lock()
            .map_err(|_| NetError::Keystore("memory keystore poisoned".to_owned()))?;
        Ok(guard.clone())
    }

    fn store_identity(&self, key: &iroh::SecretKey) -> Result<()> {
        let mut guard = self
            .key
            .lock()
            .map_err(|_| NetError::Keystore("memory keystore poisoned".to_owned()))?;
        *guard = Some(key.clone());
        Ok(())
    }

    fn delete_identity(&self) -> Result<()> {
        let mut guard = self
            .key
            .lock()
            .map_err(|_| NetError::Keystore("memory keystore poisoned".to_owned()))?;
        *guard = None;
        Ok(())
    }
}

/// Encrypted-file fallback of §11.2.
///
/// The file holds `nonce || XChaCha20-Poly1305(secret key)`. The encryption key
/// is derived with BLAKE3 from an OS user-specific secret, so a copy of the file
/// alone does not yield the identity. It is explicitly not a defence against a
/// local admin or against the user's own account (§3.1, §3.3).
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

    fn write_private(&self, bytes: &[u8]) -> Result<()> {
        if let Some(parent) = self.path.parent() {
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
            .open(&self.path)
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
    fn load_identity(&self) -> Result<Option<iroh::SecretKey>> {
        let raw = match fs::read(&self.path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(NetError::Keystore(e.to_string())),
        };
        if raw.len() <= NONCE_BYTES {
            return Err(NetError::Keystore("keystore file truncated".to_owned()));
        }
        let (nonce, ciphertext) = raw.split_at(NONCE_BYTES);
        let plain = self
            .cipher()
            .decrypt(XNonce::from_slice(nonce), ciphertext)
            .map_err(|_| NetError::Keystore("keystore file failed authentication".to_owned()))?;
        let bytes: [u8; 32] = plain
            .as_slice()
            .try_into()
            .map_err(|_| NetError::Keystore("stored identity has the wrong length".to_owned()))?;
        Ok(Some(iroh::SecretKey::from_bytes(&bytes)))
    }

    fn store_identity(&self, key: &iroh::SecretKey) -> Result<()> {
        let mut nonce = [0u8; NONCE_BYTES];
        rand::rng().fill_bytes(&mut nonce);
        let ciphertext = self
            .cipher()
            .encrypt(XNonce::from_slice(&nonce), key.to_bytes().as_slice())
            .map_err(|_| NetError::Keystore("could not encrypt the identity".to_owned()))?;
        let mut out = Vec::with_capacity(NONCE_BYTES + ciphertext.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ciphertext);
        self.write_private(&out)
    }

    fn delete_identity(&self) -> Result<()> {
        match fs::remove_file(&self.path) {
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

    #[test]
    fn open_refuses_instead_of_downgrading_to_the_file_fallback() {
        assert!(matches!(open(), Err(NetError::Keystore(_))));
    }
}
