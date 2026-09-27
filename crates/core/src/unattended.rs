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
//! credentials, and every lockout after that without a success in between
//! lasts twice as long, up to [`UNATTENDED_LOCKOUT_MAX_SECS`] (ADR 0122). A
//! success resets both.
//!
//! A one-time code is one-time: a code that has let somebody in cannot let
//! anybody in again, nor can any code from before it (RFC 6238 §5.2;
//! ADR 0122).
//!
//! A guest that can proves it knows the password instead of sending it
//! (ADR 0123): it repeats the Argon2id the host stored the password with and
//! uses the result as the password of a SPAKE2 exchange bound to both
//! endpoint identities and the session. A host that is not the one the
//! password was set on — somebody who handed the guest their own invite code
//! and asked for the password — learns nothing it can test a guess against.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use argon2::Argon2;
use argon2::password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash};
use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};

use crate::NodeId;
use crate::consent::Role;
use crate::constants::{
    UNATTENDED_LOCKOUT_DURATION_SECS, UNATTENDED_LOCKOUT_MAX_SECS, UNATTENDED_MAX_FAILED_ATTEMPTS,
    UNATTENDED_PASSWORD_MAX_BYTES, UNATTENDED_PASSWORD_MIN_BYTES, UNATTENDED_TOTP_STEP_SECS,
};
use crate::protocol::ProofKdf;

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
    /// A host's password-proof challenge is not one this build will answer:
    /// its key derivation is out of bounds or its message does not decode
    /// (ADR 0123). Raised on the guest, never sent anywhere.
    #[error("the host's challenge cannot be answered")]
    BadChallenge,
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
        self.matching_step(code, unix_secs).map(|_| ())
    }

    /// The time step `code` belongs to, among the one containing `unix_secs`
    /// and its two neighbours; the step is what makes a code spent once it has
    /// been used (ADR 0122).
    ///
    /// Every candidate is compared, and each comparison looks at every byte,
    /// so how long this takes says nothing about how close a guess was.
    ///
    /// # Errors
    /// [`UnattendedError::BadCode`] unless one of the accepted steps matches.
    pub fn matching_step(&self, code: &str, unix_secs: u64) -> Result<u64> {
        if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
            return Err(UnattendedError::BadCode);
        }
        let step = i64::try_from(UNATTENDED_TOTP_STEP_SECS).unwrap_or(30);
        let mut matched = None;
        for drift in [0i64, -1, 1] {
            let candidate =
                (i64::try_from(unix_secs).unwrap_or(i64::MAX) + drift * step).clamp(0, i64::MAX);
            let candidate = u64::try_from(candidate).unwrap_or(0);
            if same_code(&self.generate(candidate)?, code) && matched.is_none() {
                matched = Some(candidate / UNATTENDED_TOTP_STEP_SECS);
            }
        }
        matched.ok_or(UnattendedError::BadCode)
    }
}

/// Whether two codes are the same, looking at every byte whatever the first
/// difference is.
fn same_code(expected: &str, presented: &str) -> bool {
    expected.len() == presented.len()
        && expected
            .bytes()
            .zip(presented.bytes())
            .fold(0u8, |differs, (a, b)| differs | (a ^ b))
            == 0
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
    /// Lockouts since the last success, which is what each next one's length
    /// doubles on (ADR 0122).
    lockouts: u32,
    /// Until when every verification is refused, regardless of credentials.
    locked_until: Option<Instant>,
    /// The TOTP step of the last code that let somebody in: that code and
    /// every one before it are spent (RFC 6238 §5.2; ADR 0122).
    last_code_step: Option<u64>,
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
            lockouts: 0,
            locked_until: None,
            last_code_step: None,
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
        // Steps spent under the old secret say nothing about the new one.
        self.last_code_step = None;
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
        self.lockouts = 0;
        self.locked_until = None;
        self.last_code_step = None;
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
        self.check_open()?;
        // Take both verdicts first so neither factor leaks information about
        // the other through timing ordering; only then bookkeep.
        let password_ok = match password {
            None => Err(UnattendedError::MissingPassword),
            Some(password) => self.check_password(password),
        };
        self.conclude(password_ok, code)
    }

    /// Refuses before any factor is looked at: nothing configured, or locked
    /// out.
    fn check_open(&self) -> Result<()> {
        if !self.enabled() {
            return Err(UnattendedError::NotConfigured);
        }
        if let Some(remaining) = self.lockout_remaining_secs() {
            return Err(UnattendedError::LockedOut {
                remaining_secs: remaining,
            });
        }
        Ok(())
    }

    /// The second factor and the bookkeeping, for a password verdict already
    /// taken — by comparing a password, or by a proof (ADR 0123).
    fn conclude(&mut self, password_ok: Result<()>, code: Option<&str>) -> Result<()> {
        let code_ok = match (&self.totp_secret, code) {
            (None, _) => Ok(None),
            (Some(_), None) => Err(UnattendedError::MissingCode),
            (Some(secret), Some(code)) => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                match Totp::new(secret).matching_step(code, now) {
                    // A code already used, or one older than it, is refused
                    // like a wrong one: somebody who watched it being typed
                    // must not be able to type it again inside its window.
                    Ok(step) if self.last_code_step.is_some_and(|spent| step <= spent) => {
                        Err(UnattendedError::BadCode)
                    }
                    other => other.map(Some),
                }
            }
        };

        match (password_ok, code_ok) {
            (Ok(()), Ok(step)) => {
                self.failed_attempts = 0;
                self.lockouts = 0;
                self.locked_until = None;
                if step.is_some() {
                    self.last_code_step = step;
                }
                Ok(())
            }
            (Err(e), _) | (_, Err(e)) => {
                self.failed_attempts = self.failed_attempts.saturating_add(1);
                if self.failed_attempts >= UNATTENDED_MAX_FAILED_ATTEMPTS {
                    self.lockouts = self.lockouts.saturating_add(1);
                    self.locked_until =
                        Some(Instant::now() + Duration::from_secs(lockout_secs(self.lockouts)));
                }
                Err(e)
            }
        }
    }

    /// Host side: starts a password proof with `guest`, as `host`, on the
    /// session `session_id` (ADR 0123).
    ///
    /// Needs no password: the host's side of the exchange is the key its own
    /// stored hash already holds. What comes back is the state to keep until
    /// the guest answers, the derivation the guest has to repeat, and the
    /// message to send it.
    ///
    /// # Errors
    /// [`UnattendedError::NotConfigured`] with no password set, and
    /// [`UnattendedError::CorruptStore`] for a stored hash this build cannot
    /// run a proof against.
    pub fn begin_proof(
        &self,
        host: &NodeId,
        guest: &NodeId,
        session_id: [u8; 16],
    ) -> Result<(PendingProof, ProofKdf, Vec<u8>)> {
        let phc = self
            .password_hash
            .as_deref()
            .ok_or(UnattendedError::NotConfigured)?;
        let (kdf, key) = stored_key(phc)?;
        let (spake, message) = Spake2::<Ed25519Group>::start_b(
            &Password::new(&key),
            &Identity::new(guest.as_bytes()),
            &Identity::new(host.as_bytes()),
        );
        Ok((PendingProof { spake, session_id }, kdf, message))
    }

    /// Host side: the admission decision for a guest's proof, with exactly
    /// the lockout and second-factor rules of [`Self::admit`] (ADR 0123).
    ///
    /// A proof that does not confirm is a wrong password, whatever made it
    /// wrong: a different password, a message that does not decode, or an
    /// answer made for another host or another session.
    ///
    /// # Errors
    /// As [`Self::admit`].
    pub fn admit_proof(&mut self, pending: PendingProof, answer: &ProofAnswer) -> Result<Role> {
        self.check_open()?;
        let opened = pending
            .spake
            .finish(&answer.guest_message)
            .ok()
            .filter(|key| {
                confirm_mac(key, &pending.session_id, answer.sealed_code.as_deref())
                    .is_ok_and(|mac| mac.verify_slice(&answer.confirm).is_ok())
            })
            .map(|key| {
                answer
                    .sealed_code
                    .as_deref()
                    .map(|sealed| open_code(&key, &pending.session_id, sealed))
            });
        let (password_ok, code) = match opened {
            Some(code) => (Ok(()), code.flatten()),
            None => (Err(UnattendedError::BadPassword), None),
        };
        self.conclude(password_ok, code.as_deref())?;
        Ok(self.role)
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

/// Host side: one password proof waiting for the guest's answer (ADR 0123).
pub struct PendingProof {
    spake: Spake2<Ed25519Group>,
    session_id: [u8; 16],
}

impl std::fmt::Debug for PendingProof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The exchange's secret scalar never reaches a log line.
        f.debug_struct("PendingProof").finish_non_exhaustive()
    }
}

/// A guest's answer to a password-proof challenge (ADR 0123), as it travels
/// in [`crate::protocol::MessageKind::UnattendedProof`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofAnswer {
    /// The guest's SPAKE2 message.
    pub guest_message: Vec<u8>,
    /// The one-time code, sealed under the exchange's key.
    pub sealed_code: Option<Vec<u8>>,
    /// HMAC under the exchange's key over the session and the sealed code.
    pub confirm: [u8; 32],
}

/// Guest side: proves `password` (and hands over `code`) to the host `host`
/// that challenged this guest `guest` on the session `session_id`
/// (ADR 0123).
///
/// The password never leaves this function: what does is a SPAKE2 message
/// made from the Argon2id of it, and a confirmation under the key that
/// exchange agrees on. A host that does not hold that same Argon2id — any
/// host other than the one the password was set on — gets one guess at it
/// per answer and nothing to test further guesses against.
///
/// # Errors
/// [`UnattendedError::BadChallenge`] for a derivation outside the bounds a
/// guest runs, or a host message that does not decode.
pub fn answer_proof(
    password: &str,
    code: Option<&str>,
    kdf: &ProofKdf,
    host_message: &[u8],
    host: &NodeId,
    guest: &NodeId,
    session_id: [u8; 16],
) -> Result<ProofAnswer> {
    let key = derive_key(password.as_bytes(), kdf)?;
    let (spake, guest_message) = Spake2::<Ed25519Group>::start_a(
        &Password::new(&key),
        &Identity::new(guest.as_bytes()),
        &Identity::new(host.as_bytes()),
    );
    let shared = spake
        .finish(host_message)
        .map_err(|_| UnattendedError::BadChallenge)?;
    let sealed_code = code.map(|code| seal_code(&shared, &session_id, code));
    let confirm = confirm_mac(&shared, &session_id, sealed_code.as_deref())
        .map_err(|_| UnattendedError::BadChallenge)?
        .finalize()
        .into_bytes()
        .into();
    Ok(ProofAnswer {
        guest_message,
        sealed_code,
        confirm,
    })
}

/// KDF context of the proof's confirmation (ADR 0123).
const PROOF_CONFIRM_CONTEXT: &[u8] = b"lumepeer 2026 ADR 0123 unattended proof confirm";
/// KDF context of the pad a one-time code is sealed with (ADR 0123).
const PROOF_CODE_CONTEXT: &[u8] = b"lumepeer 2026 ADR 0123 unattended proof code";

/// The derivation and key of a stored PHC string, for the host's side of a
/// proof. Only Argon2id at version 0x13 is one — the only kind this build
/// writes.
fn stored_key(phc: &str) -> Result<(ProofKdf, Vec<u8>)> {
    let parsed = PasswordHash::new(phc).map_err(|_| UnattendedError::CorruptStore)?;
    if parsed.algorithm.as_str() != "argon2id" || parsed.version != Some(0x13) {
        return Err(UnattendedError::CorruptStore);
    }
    let params = argon2::Params::try_from(&parsed).map_err(|_| UnattendedError::CorruptStore)?;
    let salt = parsed.salt.ok_or(UnattendedError::CorruptStore)?;
    let key = parsed
        .hash
        .ok_or(UnattendedError::CorruptStore)?
        .as_bytes()
        .to_vec();
    let kdf = ProofKdf {
        salt: salt.as_ref().to_vec(),
        memory_kib: params.m_cost(),
        iterations: params.t_cost(),
        lanes: params.p_cost(),
        output_len: u32::try_from(key.len()).map_err(|_| UnattendedError::CorruptStore)?,
    };
    if !kdf.within_bounds() {
        return Err(UnattendedError::CorruptStore);
    }
    Ok((kdf, key))
}

/// Repeats the host's Argon2id over `password` (ADR 0123).
fn derive_key(password: &[u8], kdf: &ProofKdf) -> Result<Vec<u8>> {
    if !kdf.within_bounds() {
        return Err(UnattendedError::BadChallenge);
    }
    let len = usize::try_from(kdf.output_len).map_err(|_| UnattendedError::BadChallenge)?;
    let params = argon2::Params::new(kdf.memory_kib, kdf.iterations, kdf.lanes, Some(len))
        .map_err(|_| UnattendedError::BadChallenge)?;
    let mut key = vec![0u8; len];
    Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
        .hash_password_into(password, &kdf.salt, &mut key)
        .map_err(|_| UnattendedError::BadChallenge)?;
    Ok(key)
}

/// HMAC-SHA256 under the exchange's key over the session and the sealed code.
fn confirm_mac(
    shared: &[u8],
    session_id: &[u8; 16],
    sealed_code: Option<&[u8]>,
) -> core::result::Result<Hmac<Sha256>, hmac::digest::InvalidLength> {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(shared)?;
    mac.update(PROOF_CONFIRM_CONTEXT);
    mac.update(session_id);
    match sealed_code {
        Some(sealed) => {
            mac.update(&[1]);
            mac.update(sealed);
        }
        None => mac.update(&[0]),
    }
    Ok(mac)
}

/// The pad a one-time code is sealed with: fresh per exchange, since the key
/// is, so an XOR is a one-time pad (ADR 0123).
fn code_pad(shared: &[u8], session_id: &[u8; 16]) -> [u8; 32] {
    <Hmac<Sha256> as KeyInit>::new_from_slice(shared).map_or([0u8; 32], |mut mac| {
        mac.update(PROOF_CODE_CONTEXT);
        mac.update(session_id);
        mac.finalize().into_bytes().into()
    })
}

fn seal_code(shared: &[u8], session_id: &[u8; 16], code: &str) -> Vec<u8> {
    let pad = code_pad(shared, session_id);
    code.bytes()
        .zip(pad)
        .map(|(byte, key)| byte ^ key)
        .collect()
}

/// `None` for a sealed code that does not open into text, which the second
/// factor then refuses like any other wrong code.
fn open_code(shared: &[u8], session_id: &[u8; 16], sealed: &[u8]) -> Option<String> {
    let pad = code_pad(shared, session_id);
    String::from_utf8(
        sealed
            .iter()
            .zip(pad)
            .map(|(byte, key)| byte ^ key)
            .collect(),
    )
    .ok()
}

/// How long the `nth` lockout since the last success lasts: the first one
/// [`UNATTENDED_LOCKOUT_DURATION_SECS`], each after it twice the one before,
/// never more than [`UNATTENDED_LOCKOUT_MAX_SECS`] (ADR 0122).
fn lockout_secs(nth: u32) -> u64 {
    let doublings = nth.saturating_sub(1).min(u64::BITS - 1);
    UNATTENDED_LOCKOUT_DURATION_SECS
        .saturating_mul(1u64 << doublings)
        .min(UNATTENDED_LOCKOUT_MAX_SECS)
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

    /// ADR 0122: every lockout without a success in between is twice the one
    /// before, up to a day, and a success starts the scale over.
    #[test]
    fn each_lockout_is_longer_until_a_success() {
        let mut access = UnattendedAccess::new();
        access.set_password("right enough").unwrap();
        let mut seen = Vec::new();
        for _ in 0..12 {
            // Fail until locked, then let the lock lapse as time would.
            while !access.locked_out() {
                let _ = access.verify_full(Some("nope"), None);
            }
            seen.push(access.lockout_remaining_secs().unwrap_or(0));
            access.locked_until = Some(Instant::now());
        }
        assert!(
            seen[0] <= UNATTENDED_LOCKOUT_DURATION_SECS
                && seen[0] >= UNATTENDED_LOCKOUT_DURATION_SECS - 1
        );
        assert!(
            seen[1] >= 2 * UNATTENDED_LOCKOUT_DURATION_SECS - 1,
            "the second lockout must be longer: {seen:?}"
        );
        assert!(seen.windows(2).all(|pair| pair[1] >= pair[0]));
        assert!(
            seen.iter()
                .all(|&secs| secs <= crate::constants::UNATTENDED_LOCKOUT_MAX_SECS)
        );
        assert!(
            *seen.last().unwrap() >= crate::constants::UNATTENDED_LOCKOUT_MAX_SECS - 1,
            "the scale tops out at the ceiling: {seen:?}"
        );

        assert_eq!(access.verify_full(Some("right enough"), None).unwrap(), ());
        while !access.locked_out() {
            let _ = access.verify_full(Some("nope"), None);
        }
        assert!(
            access.lockout_remaining_secs().unwrap_or(0) <= UNATTENDED_LOCKOUT_DURATION_SECS,
            "a success must start the scale over"
        );
    }

    /// ADR 0122 (RFC 6238 §5.2): a code that let somebody in is spent, and so
    /// is every code before it; the next step's code still works.
    #[test]
    fn a_code_that_was_used_cannot_be_used_again() {
        let mut access = UnattendedAccess::new();
        access.set_password("passphrase").unwrap();
        access.set_totp_secret([7u8; 20]);
        let totp = access.totp().unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let code = totp.generate(now).unwrap();

        assert_eq!(
            access.verify_full(Some("passphrase"), Some(&code)).unwrap(),
            ()
        );
        assert!(matches!(
            access.verify_full(Some("passphrase"), Some(&code)),
            Err(UnattendedError::BadCode)
        ));
        let earlier = totp
            .generate(now.saturating_sub(UNATTENDED_TOTP_STEP_SECS))
            .unwrap();
        if earlier != code {
            assert!(matches!(
                access.verify_full(Some("passphrase"), Some(&earlier)),
                Err(UnattendedError::BadCode)
            ));
        }
        let next = totp.generate(now + UNATTENDED_TOTP_STEP_SECS).unwrap();
        if next != code {
            assert_eq!(
                access.verify_full(Some("passphrase"), Some(&next)).unwrap(),
                (),
                "the code of a later step is still good"
            );
        }
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

    fn node(seed: u8) -> NodeId {
        iroh_base::SecretKey::from_bytes(&[seed; 32]).public()
    }

    const SESSION: [u8; 16] = [0x5a; 16];

    /// Runs one whole proof exchange: the host challenges, the guest answers
    /// with `password` believing it is talking to `believed_host`, the host
    /// decides.
    fn prove(
        access: &mut UnattendedAccess,
        password: &str,
        code: Option<&str>,
        believed_host: &NodeId,
    ) -> Result<Role> {
        let (host, guest) = (node(1), node(2));
        let (pending, kdf, host_message) = access.begin_proof(&host, &guest, SESSION).unwrap();
        let answer = answer_proof(
            password,
            code,
            &kdf,
            &host_message,
            believed_host,
            &guest,
            SESSION,
        )?;
        access.admit_proof(pending, &answer)
    }

    /// ADR 0123: the right password proves itself and gets the configured
    /// role, a wrong one is a wrong password and counts towards the lockout,
    /// and nothing the guest sends contains the password.
    #[test]
    fn a_password_proof_admits_the_right_password_and_nothing_else() {
        let mut access = UnattendedAccess::new();
        access.set_password("right enough").unwrap();
        access.set_role(Role::FullControl);

        assert_eq!(
            prove(&mut access, "right enough", None, &node(1)).unwrap(),
            Role::FullControl
        );
        assert!(matches!(
            prove(&mut access, "wrong enough", None, &node(1)),
            Err(UnattendedError::BadPassword)
        ));
        assert_eq!(access.failed_attempts, 1);

        let (pending, kdf, host_message) = access.begin_proof(&node(1), &node(2), SESSION).unwrap();
        let answer = answer_proof(
            "right enough",
            None,
            &kdf,
            &host_message,
            &node(1),
            &node(2),
            SESSION,
        )
        .unwrap();
        for bytes in [&answer.guest_message[..], &answer.confirm[..]] {
            assert!(
                !bytes
                    .windows("right enough".len())
                    .any(|window| window == b"right enough"),
                "the password crossed the wire"
            );
        }
        drop(pending);
    }

    /// ADR 0123: a guest that proves its password to a host other than the one
    /// the password was set on — it was handed that host's code — does not
    /// get in there either, and the host it meant never sees an answer it
    /// could use: the proof is bound to the identity the guest believed.
    #[test]
    fn a_proof_made_for_another_host_does_not_admit() {
        let mut access = UnattendedAccess::new();
        access.set_password("right enough").unwrap();
        assert!(matches!(
            prove(&mut access, "right enough", None, &node(9)),
            Err(UnattendedError::BadPassword)
        ));
    }

    /// ADR 0123: an answer made for one session is refused on another, even
    /// with the right password behind it.
    #[test]
    fn a_proof_answer_is_bound_to_its_session() {
        let mut access = UnattendedAccess::new();
        access.set_password("right enough").unwrap();
        let (host, guest) = (node(1), node(2));
        let (pending, kdf, host_message) = access.begin_proof(&host, &guest, SESSION).unwrap();
        let answer = answer_proof(
            "right enough",
            None,
            &kdf,
            &host_message,
            &host,
            &guest,
            [0x11; 16],
        )
        .unwrap();
        assert!(matches!(
            access.admit_proof(pending, &answer),
            Err(UnattendedError::BadPassword)
        ));
    }

    /// ADR 0123: the one-time code travels sealed and is checked like every
    /// other code — missing, wrong and right all mean what they mean for
    /// `verify_full`.
    #[test]
    fn a_proof_carries_the_one_time_code_sealed() {
        let mut access = UnattendedAccess::new();
        access.set_password("passphrase").unwrap();
        access.set_totp_secret([7u8; 20]);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let code = access.totp().unwrap().generate(now).unwrap();

        assert!(matches!(
            prove(&mut access, "passphrase", None, &node(1)),
            Err(UnattendedError::MissingCode)
        ));
        let wrong = if code == "000000" { "111111" } else { "000000" };
        assert!(matches!(
            prove(&mut access, "passphrase", Some(wrong), &node(1)),
            Err(UnattendedError::BadCode)
        ));
        assert_eq!(
            prove(&mut access, "passphrase", Some(&code), &node(1)).unwrap(),
            Role::ViewOnly
        );
    }

    /// ADR 0123: the derivation a guest repeats is the one the host stored
    /// the password with — the same key `verify_full` checks against.
    #[test]
    fn the_guest_derives_exactly_the_stored_key() {
        let mut access = UnattendedAccess::new();
        access.set_password("right enough").unwrap();
        let (kdf, stored) = stored_key(access.stored_secret().unwrap()).unwrap();
        assert_eq!(derive_key(b"right enough", &kdf).unwrap(), stored);
        assert_ne!(derive_key(b"wrong enough", &kdf).unwrap(), stored);
    }

    /// ADR 0123: a host cannot make a guest run an unbounded derivation.
    #[test]
    fn an_unbounded_derivation_is_refused_before_it_runs() {
        let mut access = UnattendedAccess::new();
        access.set_password("right enough").unwrap();
        let (_, kdf, host_message) = access.begin_proof(&node(1), &node(2), SESSION).unwrap();
        for greedy in [
            ProofKdf {
                memory_kib: 4 * 1024 * 1024,
                ..kdf.clone()
            },
            ProofKdf {
                iterations: 1_000,
                ..kdf.clone()
            },
            ProofKdf {
                salt: vec![1; 3],
                ..kdf.clone()
            },
        ] {
            assert!(matches!(
                answer_proof(
                    "right enough",
                    None,
                    &greedy,
                    &host_message,
                    &node(1),
                    &node(2),
                    SESSION
                ),
                Err(UnattendedError::BadChallenge)
            ));
        }
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
