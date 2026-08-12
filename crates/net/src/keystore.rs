//! Platform keystore for the long-term endpoint identity (design doc §11.2).
//!
//! The Iroh secret key exists on disk only inside the OS keystore. The
//! fallback is an encrypted file whose key derives from an OS user-specific
//! secret, never a plaintext file in the app directory. None of this is a
//! defence against a local admin on the host machine (§3.1).

use crate::error::Result;

/// Keystore entry name under which the endpoint identity is stored.
pub const IDENTITY_ENTRY: &str = "lumepeer.endpoint.identity";

/// Storage for secret material.
pub trait Keystore: Send {
    /// Loads the endpoint secret key, if one was stored.
    ///
    /// # Errors
    /// [`crate::error::NetError::Keystore`] if the platform store is
    /// unavailable or refuses the read.
    fn load_identity(&self) -> Result<Option<iroh::SecretKey>>;

    /// Stores the endpoint secret key, replacing any previous value.
    ///
    /// # Errors
    /// [`crate::error::NetError::Keystore`] if the write is refused.
    fn store_identity(&self, key: &iroh::SecretKey) -> Result<()>;

    /// Removes the stored identity.
    ///
    /// # Errors
    /// [`crate::error::NetError::Keystore`] if the delete is refused.
    fn delete_identity(&self) -> Result<()>;
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
/// [`crate::error::NetError::Keystore`] if no backend can be opened.
pub fn open() -> Result<Box<dyn Keystore>> {
    todo!("phase 1/4: platform keystore backends per §11.2")
}
