//! Unattended access: device password, brute-force lockout and TOTP 2FA
//! (design doc §8; ADR 0023 §1-2, ADR 0033).
//!
//! The module header used to cite an "ADR 0021" that was never written: the
//! decision it meant — Argon2id for the device password, a hand-rolled RFC
//! 6238 second factor — is recorded in ADR 0023 §1 and §2, and the admission
//! path that finally uses this module is ADR 0033.
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
use argon2::password_hash::{phc::PasswordHash, PasswordHasher, PasswordVerifier};
use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;

use crate::consent::Role;
use crate::constants::{
    UNATTENDED_LOCKOUT_DURATION_SECS, UNATTENDED_MAX_FAILED_ATTEMPTS,
    UNATTENDED_PASSWORD_MAX_BYTES, UNATTENDED_PASSWORD_MIN_BYTES, UNATTENDED_TOTP_STEP_SECS,
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
    /// The proposed password is shorter than `UNATTENDED_PASSWORD_MIN_BYTES`
    /// or longer than `UNATTENDED_PASSWORD_MAX_BYTES` (§8).
    ///
    /// Raised only when the host *sets* a password, never when one is
    /// presented: telling a guest that its guess was the wrong length would
    /// narrow the search for it.
    #[error("the password must be between {min} and {max} bytes")]
    PasswordPolicy {
        /// Shortest accepted password.
        min: usize,
        /// Longest accepted password.
        max: usize,
    },
}

/// Convenience alias for unattended results.
pub type Result<T> = core::result::Result<T, UnattendedError>;

/// RFC 6238 TOTP over HMAC-SHA1 with 6-digit codes.
///
/// SHA1 appears here only because RFC 6238 and every mainstream authenticator
/// app pin it for TOTP; nothing else in the workspace uses it (ADR 0023 §2).
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
        let mac = <Hmac<Sha1> as KeyInit>::new_from_slice(&self.secret)
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

    /// The shared secret in RFC 4648 base32, the form authenticator apps take.
    ///
    /// This is the one moment the secret leaves the host: provisioning an app
    /// is impossible without showing it. Callers must treat the result as the
    /// key it is — show it once, never log it, never persist it outside the
    /// keystore.
    #[must_use]
    pub fn secret_base32(&self) -> String {
        data_encoding::BASE32_NOPAD.encode(&self.secret)
    }

    /// The `otpauth://` provisioning URI for `account`, as authenticator apps
    /// and their QR codes expect it.
    ///
    /// `account` is what the app shows in its list. It is a caller-chosen
    /// display string and must not be a hostname or a user name: this URI is
    /// meant to be shown on screen and photographed, and §15 keeps
    /// host-identifying detail out of anything that travels.
    #[must_use]
    pub fn provisioning_uri(&self, account: &str) -> String {
        format!(
            "otpauth://totp/Lumepeer:{account}?secret={}&issuer=Lumepeer&algorithm=SHA1&digits=6&period={UNATTENDED_TOTP_STEP_SECS}",
            self.secret_base32(),
        )
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

/// Unattended-access credentials of this host (§8; ADR 0023 §1-2, ADR 0033).
///
/// The password never survives in the clear: only an Argon2id PHC string is
/// kept, meant to live in the OS keystore next to the node identity
/// (`crates/net::keystore`), not in a config file. The failure counter and
/// lockout are in-memory; a restart clears them, which is acceptable because
/// the attacker still faces the password itself and each guess costs one full
/// Argon2id evaluation.
#[derive(Debug)]
pub struct UnattendedAccess {
    /// Argon2id PHC string, `None` while unattended access is off.
    password_hash: Option<String>,
    /// Optional second factor secret.
    totp_secret: Option<[u8; 20]>,
    /// Role a successful admission is granted (§8.2). Host-configured, and
    /// `ViewOnly` until the host says otherwise: an unattended session that
    /// nobody watched being set up starts from the least it can do.
    role: Role,
    /// Failed [`Self::verify_full`] calls since the last success.
    failed_attempts: u32,
    /// Until when every verification is refused, regardless of credentials.
    locked_until: Option<Instant>,
}

impl Default for UnattendedAccess {
    fn default() -> Self {
        Self::new()
    }
}

impl UnattendedAccess {
    /// Creates an access gate with nothing configured: everything is denied.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            password_hash: None,
            totp_secret: None,
            role: Role::ViewOnly,
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
    /// [`UnattendedError::PasswordPolicy`] if `password` is outside
    /// `UNATTENDED_PASSWORD_MIN_BYTES..=UNATTENDED_PASSWORD_MAX_BYTES`, and
    /// [`UnattendedError::SaltGeneration`] when the platform CSPRNG fails. In
    /// both cases the previous hash stays in place: a rejected change never
    /// leaves the host with no password at all.
    pub fn set_password(&mut self, password: &str) -> Result<()> {
        use rand::RngExt as _;

        if password.len() < UNATTENDED_PASSWORD_MIN_BYTES
            || password.len() > UNATTENDED_PASSWORD_MAX_BYTES
        {
            return Err(UnattendedError::PasswordPolicy {
                min: UNATTENDED_PASSWORD_MIN_BYTES,
                max: UNATTENDED_PASSWORD_MAX_BYTES,
            });
        }
        // The salt comes from the workspace CSPRNG the same way session ids
        // do (`session.rs`): 16 random bytes, base64'd into a PHC salt. This
        // dodges the two-`rand_core`-versions conflict that feeding a `rand`
        // RNG straight into `SaltString::generate` would hit (ADR 0023 §1).
        let mut bytes = [0u8; 16];
        rand::rng().fill(&mut bytes);
        let hash = Argon2::default()
            .hash_password(password.as_bytes())
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

    /// The stored second-factor secret, for persistence into the keystore.
    #[must_use]
    pub const fn stored_totp_secret(&self) -> Option<&[u8; 20]> {
        self.totp_secret.as_ref()
    }

    /// Whether a one-time code is part of the gate, i.e. what the host tells
    /// a guest in `MessageKind::UnattendedChallenge`.
    #[must_use]
    pub const fn code_required(&self) -> bool {
        self.totp_secret.is_some()
    }

    /// Turns the second factor off, dropping the stored secret.
    pub const fn clear_totp_secret(&mut self) {
        self.totp_secret = None;
    }

    /// Turns unattended access off: no password, no second factor, and every
    /// later `verify_full`/`admit` refuses with `NotConfigured`.
    pub fn disable(&mut self) {
        self.password_hash = None;
        self.totp_secret = None;
        self.failed_attempts = 0;
        self.locked_until = None;
    }

    /// Role a successful admission is granted (§8.2).
    #[must_use]
    pub const fn role(&self) -> Role {
        self.role
    }

    /// Sets the role a successful admission is granted (§8.2).
    ///
    /// Takes effect on the *next* admission only: a session already running
    /// keeps the snapshot it was granted under, the same rule
    /// `SessionManager::set_control_policy` follows.
    pub const fn set_role(&mut self, role: Role) {
        self.role = role;
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

    /// The whole unattended admission decision, in one call (§8, §2.1).
    ///
    /// Verifies both factors and, only on success, hands back the role the
    /// host configured. It exists so that no caller outside this crate ever
    /// has to hold "the credentials were fine" as a value of its own and pair
    /// it with a role: the only way to obtain a [`Role`] here is to have
    /// passed the gate, and a caller that mishandles the `Err` gets no role at
    /// all rather than a default one (§2.1, §2.3).
    ///
    /// # Errors
    /// Exactly what [`Self::verify_full`] returns, with no extra detail: the
    /// coarseness of [`UnattendedError`] is the point.
    pub fn admit(&mut self, password: Option<&str>, code: Option<&str>) -> Result<Role> {
        self.verify_full(password, code)?;
        Ok(self.role)
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
    use crate::constants::{
        UNATTENDED_MAX_FAILED_ATTEMPTS, UNATTENDED_PASSWORD_MAX_BYTES, UNATTENDED_TOTP_STEP_SECS,
    };

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
        access.set_password("right enough").unwrap();
        assert!(matches!(
            access.verify_full(Some("wrong"), None),
            Err(UnattendedError::BadPassword)
        ));
    }

    #[test]
    fn hash_is_salted_never_plaintext() {
        let mut a = UnattendedAccess::new();
        let mut b = UnattendedAccess::new();
        a.set_password("same secret").unwrap();
        b.set_password("same secret").unwrap();
        assert_ne!(a.stored_secret(), b.stored_secret());
        let stored = a.stored_secret().unwrap_or_default();
        assert!(!stored.contains("same secret"));
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
        access.set_password("right enough").unwrap();
        for _ in 0..UNATTENDED_MAX_FAILED_ATTEMPTS {
            let _ = access.verify_full(Some("nope"), None);
        }
        assert!(access.locked_out());
        assert!(matches!(
            access.verify_full(Some("right enough"), None),
            Err(UnattendedError::LockedOut { .. })
        ));
    }

    #[test]
    fn correct_attempt_resets_the_failure_counter() {
        let mut access = UnattendedAccess::new();
        access.set_password("right enough").unwrap();
        for _ in 0..(UNATTENDED_MAX_FAILED_ATTEMPTS - 1) {
            let _ = access.verify_full(Some("nope"), None);
        }
        assert_eq!(access.verify_full(Some("right enough"), None).unwrap(), ());
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
    fn a_password_below_the_policy_floor_is_refused_and_changes_nothing() {
        let mut access = UnattendedAccess::new();
        assert!(matches!(
            access.set_password("short"),
            Err(UnattendedError::PasswordPolicy { .. })
        ));
        assert!(
            !access.enabled(),
            "a refused password must not enable the gate"
        );

        access.set_password("long enough to pass").unwrap();
        let before = access.stored_secret().unwrap_or_default().to_owned();
        // A refused *change* leaves the working password in place, rather than
        // leaving the host with none.
        assert!(access.set_password("tiny").is_err());
        assert_eq!(access.stored_secret().unwrap_or_default(), before);

        let too_long = "x".repeat(UNATTENDED_PASSWORD_MAX_BYTES + 1);
        assert!(matches!(
            access.set_password(&too_long),
            Err(UnattendedError::PasswordPolicy { .. })
        ));
    }

    #[test]
    fn the_provisioning_uri_carries_the_secret_and_this_builds_parameters() {
        let totp = Totp::new(&RFC_SECRET);
        // RFC 4648 base32 of "12345678901234567890".
        assert_eq!(totp.secret_base32(), "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ");

        let uri = totp.provisioning_uri("device");
        assert!(uri.starts_with("otpauth://totp/Lumepeer:device?"));
        assert!(uri.contains("secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"));
        assert!(uri.contains("digits=6"));
        assert!(uri.contains(&format!("period={UNATTENDED_TOTP_STEP_SECS}")));
    }

    #[test]
    fn admit_hands_back_the_configured_role_only_on_success() {
        let mut access = UnattendedAccess::new();
        access.set_password("right enough").unwrap();
        // Deny-by-default: nobody set a role, so the least one applies.
        assert_eq!(access.role(), Role::ViewOnly);
        assert_eq!(
            access.admit(Some("right enough"), None).unwrap(),
            Role::ViewOnly
        );

        access.set_role(Role::FullControl);
        assert_eq!(
            access.admit(Some("right enough"), None).unwrap(),
            Role::FullControl
        );
        // A refusal yields no role at all, not a lesser one.
        assert!(matches!(
            access.admit(Some("wrong"), None),
            Err(UnattendedError::BadPassword)
        ));
    }

    #[test]
    fn a_disabled_gate_forgets_both_factors_and_refuses() {
        let mut access = UnattendedAccess::new();
        access.set_password("right enough").unwrap();
        access.set_totp_secret(RFC_SECRET);
        assert!(access.code_required());

        access.disable();
        assert!(!access.enabled());
        assert!(!access.code_required());
        assert!(access.stored_secret().is_none());
        assert!(access.stored_totp_secret().is_none());
        assert!(matches!(
            access.admit(Some("right enough"), None),
            Err(UnattendedError::NotConfigured)
        ));
    }

    #[test]
    fn the_second_factor_can_be_turned_off_without_losing_the_password() {
        let mut access = UnattendedAccess::new();
        access.set_password("passphrase").unwrap();
        access.set_totp_secret(RFC_SECRET);
        assert_eq!(access.stored_totp_secret(), Some(&RFC_SECRET));

        access.clear_totp_secret();
        assert!(!access.code_required());
        assert!(access.enabled());
        assert_eq!(
            access.admit(Some("passphrase"), None).unwrap(),
            Role::ViewOnly
        );
    }

    #[test]
    fn two_fa_gate_combines_password_and_totp() {
        let mut access = UnattendedAccess::new();
        access.set_password("passphrase").unwrap();
        access.set_totp_secret([7u8; 20]);
        // The full gate verifies against the real clock, so the code must be
        // minted for the current step.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let totp = access.totp().unwrap_or_else(|| panic!("totp set above"));
        let code = totp.generate(now).unwrap();

        assert!(matches!(
            access.verify_full(Some("passphrase"), None),
            Err(UnattendedError::MissingCode)
        ));
        assert!(matches!(
            access.verify_full(None, Some(&code)),
            Err(UnattendedError::MissingPassword)
        ));
        assert_eq!(
            access.verify_full(Some("passphrase"), Some(&code)).unwrap(),
            ()
        );
    }
}
