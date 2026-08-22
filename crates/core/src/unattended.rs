//! Unattended access: device password, brute-force lockout and TOTP 2FA
//! (design doc §8; ADR 0021).
//!
//! The invite model of §7 assumes a person at the host answering the consent
//! dialog. Unattended access removes that person, so the host must decide on
//! cryptographic evidence alone: a device password (Argon2id) and optionally a
//! time-based one-time code (RFC 6238). Both factors are verified here, in
//! the TCB, never in the UI or on the network side; a failure is an
//! [`UnattendedError`], never a panic.
//!
//! Brute force is answered with a lockout: after
//! [`UNATTENDED_MAX_FAILED_ATTEMPTS`] failed [`UnattendedAccess::verify_full`]
//! calls every further attempt is refused for
//! [`UNATTENDED_LOCKOUT_DURATION_SECS`], including one with the correct
//! credentials. A success resets the counter.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use hmac::{Hmac, Mac};
use sha1::Sha1;

use crate::constants::{
    UNATTENDED_LOCKOUT_DURATION_SECS, UNATTENDED_MAX_FAILED_ATTEMPTS, UNATTENDED_TOTP_STEP_SECS,
};

/// Everything that can go wrong while verifying unattended credentials (§18).
///
/// Deliberately coarse: `BadPassword` and `BadCode` do not say how close a
/// guess was, and `LockedOut` carries only the remaining seconds — nothing
/// that helps an attacker iterate faster.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UnattendedError {
    /// No password has been configured; unattended access is off (§8).
    #[error("unattended access is not configured")]
    NotConfigured,
    /// The presented password is wrong.
    #[error("password rejected")]
    BadPassword,
    /// A password was required but not presented.
    #[error("a password is required")]
    MissingPassword,
    /// The presented TOTP code is wrong or outside the acceptance window.
    #[error("code rejected")]
    BadCode,
    /// A TOTP code was required but not presented.
    #[error("a one-time code is required")]
    MissingCode,
    /// Every verification is refused until the lockout expires.
    #[error("locked out for {remaining_secs}s after repeated failures")]
    LockedOut {
        /// Seconds until verification attempts are accepted again.
        remaining_secs: u64,
    },
    /// The stored hash could not be parsed; the password must be re-set.
    #[error("stored password hash is corrupt")]
    CorruptStore,
    /// The platform random generator failed while salting a new hash.
    #[error("cannot generate password salt")]
    SaltGeneration,
}

/// Convenience alias for unattended results.
pub type Result<T> = core::result::Result<T, UnattendedError>;

/// RFC 6238 TOTP over HMAC-SHA1 with 6-digit codes.
///
/// SHA1 appears here only because RFC 6238 and every mainstream authenticator
/// app pin it for TOTP; nothing else in the workspace uses it (ADR 0021).
/// `generate` takes a Unix timestamp in seconds so callers can verify against
/// their own clock; there is no `now()` hidden inside.
#[derive(Debug, Clone)]
pub struct Totp {
    secret: Vec<u8>,
}

impl Totp {
    /// Builds a generator over `secret`.
    #[must_use]
    pub fn new(secret: &[u8]) -> Self {
        Self {
            secret: secret.to_vec(),
        }
    }

    /// The 6-digit code valid at Unix time `unix_secs`.
    ///
    /// # Errors
    /// [`UnattendedError::BadCode`] if the HMAC cannot be keyed — only
    /// possible for an empty secret, which the constructor accepts but the
    /// RFC forbids; never a panic on hostile input.
    pub fn generate(&self, unix_secs: u64) -> Result<String> {
        let counter = unix_secs / UNATTENDED_TOTP_STEP_SECS;
        let mac = <Hmac<Sha1> as hmac::Mac>::new_from_slice(&self.secret)
            .map_err(|_| UnattendedError::BadCode)?
            .chain_update(counter.to_be_bytes());
        let digest = hmac::Mac::finalize(mac).into_bytes();

        // Dynamic truncation per RFC 4226 §5.3.
        let offset = usize::from(digest[digest.len() - 1] & 0x0f);
        let binary = u32::from_be_bytes([
            digest[offset] & 0x7f,
            digest[offset + 1],
            digest[offset + 2],
            digest[offset + 3],
        ]);
        Ok(format!("{:06}", binary % 1_000_000))
    }

    /// Verifies `code` for the step containing `unix_secs`, accepting the
    /// neighbouring step on each side for clock drift.
    ///
    /// # Errors
    /// [`UnattendedError::BadCode`] unless one of the accepted steps matches.
    pub fn verify(&self, code: &str, unix_secs: u64) -> Result<()> {
        if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
            return Err(UnattendedError::BadCode);
        }
        let step = i64::try_from(UNATTENDED_TOTP_STEP_SECS).unwrap_or(30);
        for drift in [0i64, -1, 1] {
            let candidate =
                (i64::try_from(unix_secs).unwrap_or(i64::MAX) + drift * step).clamp(0, i64::MAX);
            let candidate = u64::try_from(candidate).unwrap_or(0);
            if self.generate(candidate)? == code {
                return Ok(());
            }
        }
        Err(UnattendedError::BadCode)
    }
}

/// Unattended-access credentials of this host (§8, ADR 0021).
///
/// The password never survives in the clear: only an Argon2id PHC string is
/// kept, meant to live in the OS keystore next to the node identity
/// (`crates/net::keystore`), not in a config file. The failure counter and
/// lockout are in-memory; a restart clears them, which is acceptable because
/// the attacker still faces the password itself and each guess costs one full
/// Argon2id evaluation.
#[derive(Debug, Default)]
pub struct UnattendedAccess {
    /// Argon2id PHC string, `None` while unattended access is off.
    password_hash: Option<String>,
    /// Optional second factor secret.
    totp_secret: Option<[u8; 20]>,
    /// Failed [`Self::verify_full`] calls since the last success.
    failed_attempts: u32,
    /// Until when every verification is refused, regardless of credentials.
    locked_until: Option<Instant>,
}

impl UnattendedAccess {
    /// Creates an access gate with nothing configured: everything is denied.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            password_hash: None,
            totp_secret: None,
            failed_attempts: 0,
            locked_until: None,
        }
    }

    /// Whether a password is configured and unattended access may be offered.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.password_hash.is_some()
    }

    /// The stored PHC string, for persistence into the keystore.
    #[must_use]
    pub fn stored_secret(&self) -> Option<&str> {
        self.password_hash.as_deref()
    }

    /// Restores a previously persisted PHC string (from the keystore).
    pub fn restore_password_hash(&mut self, phc: &str) {
        self.password_hash = Some(phc.to_owned());
    }

    /// Installs (or replaces) the device password from the clear text.
    ///
    /// # Errors
    /// [`UnattendedError::SaltGeneration`] when the platform CSPRNG fails;
    /// the previous hash stays in place.
    pub fn set_password(&mut self, password: &str) -> Result<()> {
        // The salt comes from the workspace CSPRNG the same way session ids
        // do (`session.rs`): 16 random bytes, base64'd into a PHC salt. This
        // dodges the two-`rand_core`-versions conflict that feeding a `rand`
        // RNG straight into `SaltString::generate` would hit (ADR 0021).
        use rand::RngExt as _;
        let mut bytes = [0u8; 16];
        rand::rng().fill(&mut bytes);
        let salt = SaltString::encode_b64(&bytes).map_err(|_| UnattendedError::SaltGeneration)?;
        let hash = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map_err(|_| UnattendedError::SaltGeneration)?;
        self.password_hash = Some(hash.to_string());
        Ok(())
    }

    /// Enables or replaces the second factor with a 20-byte secret.
    pub fn set_totp_secret(&mut self, secret: [u8; 20]) {
        self.totp_secret = Some(secret);
    }

    /// The second factor, for provisioning an authenticator app.
    #[must_use]
    pub fn totp(&self) -> Option<Totp> {
        self.totp_secret.as_ref().map(|s| Totp::new(s))
    }

    /// Whether verification is currently locked out.
    #[must_use]
    pub fn locked_out(&self) -> bool {
        self.lockout_remaining_secs().is_some()
    }

    /// Remaining lockout seconds, if locked.
    fn lockout_remaining_secs(&self) -> Option<u64> {
        let left = self.locked_until?.saturating_duration_since(Instant::now());
        if left.is_zero() {
            None
        } else {
            Some(left.as_secs())
        }
    }

    /// Full gate: password plus, when provisioned, the TOTP code against the
    /// real clock. Applies lockout bookkeeping around both factors.
    ///
    /// # Errors
    /// The union of [`UnattendedError`]; a missing factor is
    /// `MissingPassword`/`MissingCode`, never a silent pass.
    pub fn verify_full(&mut self, password: Option<&str>, code: Option<&str>) -> Result<()> {
        if !self.enabled() {
            return Err(UnattendedError::NotConfigured);
        }
        if let Some(remaining) = self.lockout_remaining_secs() {
            return Err(UnattendedError::LockedOut {
                remaining_secs: remaining,
            });
        }
        // Take both verdicts first so neither factor leaks information about
        // the other through timing ordering; only then bookkeep.
        let password_ok = match password {
            None => Err(UnattendedError::MissingPassword),
            Some(password) => self.check_password(password),
        };
        let code_ok = match (&self.totp_secret, code) {
            (None, _) => Ok(()),
            (Some(_), None) => Err(UnattendedError::MissingCode),
            (Some(secret), Some(code)) => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                Totp::new(secret).verify(code, now)
            }
        };

        match (password_ok, code_ok) {
            (Ok(()), Ok(())) => {
                self.failed_attempts = 0;
                self.locked_until = None;
                Ok(())
            }
            (Err(e), _) | (_, Err(e)) => {
                self.failed_attempts = self.failed_attempts.saturating_add(1);
                if self.failed_attempts >= UNATTENDED_MAX_FAILED_ATTEMPTS {
                    self.locked_until = Some(
                        Instant::now() + Duration::from_secs(UNATTENDED_LOCKOUT_DURATION_SECS),
                    );
                }
                Err(e)
            }
        }
    }

    /// Hash comparison without lockout bookkeeping; the caller owns counting.
    fn check_password(&self, password: &str) -> Result<()> {
        let phc = self
            .password_hash
            .as_deref()
            .ok_or(UnattendedError::NotConfigured)?;
        let parsed = PasswordHash::new(phc).map_err(|_| UnattendedError::CorruptStore)?;
        if Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
        {
            Ok(())
        } else {
            Err(UnattendedError::BadPassword)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::constants::{UNATTENDED_MAX_FAILED_ATTEMPTS, UNATTENDED_TOTP_STEP_SECS};

    /// RFC 6238 Appendix B secret ("12345678901234567890"), 20 bytes.
    const RFC_SECRET: [u8; 20] = *b"12345678901234567890";

    #[test]
    fn no_password_stored_means_everything_is_denied() {
        let mut access = UnattendedAccess::new();
        assert!(!access.enabled());
        assert!(matches!(
            access.verify_full(Some("anything"), None),
            Err(UnattendedError::NotConfigured)
        ));
    }

    #[test]
    fn set_then_verify_roundtrip() {
        let mut access = UnattendedAccess::new();
        access.set_password("correct horse battery staple").unwrap();
        assert!(access.enabled());
        assert_eq!(
            access
                .verify_full(Some("correct horse battery staple"), None)
                .unwrap(),
            ()
        );
    }

    #[test]
    fn wrong_password_is_rejected() {
        let mut access = UnattendedAccess::new();
        access.set_password("right").unwrap();
        assert!(matches!(
            access.verify_full(Some("wrong"), None),
            Err(UnattendedError::BadPassword)
        ));
    }

    #[test]
    fn hash_is_salted_never_plaintext() {
        let mut a = UnattendedAccess::new();
        let mut b = UnattendedAccess::new();
        a.set_password("same").unwrap();
        b.set_password("same").unwrap();
        assert_ne!(a.stored_secret(), b.stored_secret());
        let stored = a.stored_secret().unwrap_or_default();
        assert!(!stored.contains("same"));
    }

    #[test]
    fn restored_hash_verifies_without_rehashing() {
        let mut a = UnattendedAccess::new();
        a.set_password("persist me").unwrap();
        let phc = a.stored_secret().unwrap_or_default().to_owned();

        let mut b = UnattendedAccess::new();
        b.restore_password_hash(&phc);
        assert_eq!(b.verify_full(Some("persist me"), None).unwrap(), ());
    }

    #[test]
    fn lockout_after_max_failed_attempts_even_with_the_right_password() {
        let mut access = UnattendedAccess::new();
        access.set_password("right").unwrap();
        for _ in 0..UNATTENDED_MAX_FAILED_ATTEMPTS {
            let _ = access.verify_full(Some("nope"), None);
        }
        assert!(access.locked_out());
        assert!(matches!(
            access.verify_full(Some("right"), None),
            Err(UnattendedError::LockedOut { .. })
        ));
    }

    #[test]
    fn correct_attempt_resets_the_failure_counter() {
        let mut access = UnattendedAccess::new();
        access.set_password("right").unwrap();
        for _ in 0..(UNATTENDED_MAX_FAILED_ATTEMPTS - 1) {
            let _ = access.verify_full(Some("nope"), None);
        }
        assert_eq!(access.verify_full(Some("right"), None).unwrap(), ());
        // The counter restarted, so this many failures must not lock out yet.
        for _ in 0..(UNATTENDED_MAX_FAILED_ATTEMPTS - 1) {
            let _ = access.verify_full(Some("nope"), None);
        }
        assert!(!access.locked_out());
    }

    #[test]
    fn rfc6238_vectors_truncated_to_six_digits() {
        // The RFC publishes 8-digit SHA1 vectors; our generator emits the
        // first six digits of the same dynamic truncation, i.e. the last six
        // of the published strings:
        //   t=59          -> 94287082 -> "287082"
        //   t=1111111109  -> 07081804 -> "081804"
        //   t=1234567890  -> 89005924 -> "005924"
        for (unix_time, expected) in [
            (59u64, "287082"),
            (1_111_111_109, "081804"),
            (1_234_567_890, "005924"),
        ] {
            let code = Totp::new(&RFC_SECRET).generate(unix_time).unwrap();
            assert_eq!(code, expected.to_owned(), "vector at t={unix_time}");
        }
    }

    #[test]
    fn verify_accepts_current_step_and_rejects_far_neighbors() {
        let totp = Totp::new(&RFC_SECRET);
        let current = totp.generate(30_000).unwrap();
        assert_eq!(totp.verify(&current, 30_000).unwrap(), ());
        let later = totp.generate(30_000 + UNATTENDED_TOTP_STEP_SECS).unwrap();
        assert_ne!(later, current);
        assert!(matches!(
            totp.verify(&current, 30_000 + 4 * UNATTENDED_TOTP_STEP_SECS),
            Err(UnattendedError::BadCode)
        ));
    }

    #[test]
    fn non_numeric_code_is_bad_code_not_a_panic() {
        let totp = Totp::new(&RFC_SECRET);
        assert!(matches!(
            totp.verify("abcdef", 0),
            Err(UnattendedError::BadCode)
        ));
        assert!(matches!(totp.verify("", 0), Err(UnattendedError::BadCode)));
        assert!(matches!(
            totp.verify("1234567", 0),
            Err(UnattendedError::BadCode)
        ));
    }

    #[test]
    fn two_fa_gate_combines_password_and_totp() {
        let mut access = UnattendedAccess::new();
        access.set_password("pw").unwrap();
        access.set_totp_secret([7u8; 20]);
        // The full gate verifies against the real clock, so the code must be
        // minted for the current step.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let totp = access.totp().unwrap_or_else(|| panic!("totp set above"));
        let code = totp.generate(now).unwrap();

        assert!(matches!(
            access.verify_full(Some("pw"), None),
            Err(UnattendedError::MissingCode)
        ));
        assert!(matches!(
            access.verify_full(None, Some(&code)),
            Err(UnattendedError::MissingPassword)
        ));
        assert_eq!(access.verify_full(Some("pw"), Some(&code)).unwrap(), ());
    }
}
