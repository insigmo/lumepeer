//! License token format and validation (design doc §12).
//!
//! The token is a compact custom binary format, not a JWT (§5.1), signed with
//! Ed25519 over every byte preceding the signature and verified with
//! `verify_strict` — no hand-rolled constant-time comparison (§12.1, §20).

use ed25519_dalek::{Signature, VerifyingKey};

use crate::constants::{
    CLOCK_ROLLBACK_ACTIVE_SESSION_CUTOFF_SECS, LICENSE_WARN_BEFORE_SECS, MAX_CONCURRENT_GUESTS_PRO,
    MAX_CONCURRENT_GUESTS_TEAM, MAX_CONCURRENT_GUESTS_TRIAL, OFFLINE_GRACE_PRO_DAYS,
    OFFLINE_GRACE_TEAM_DAYS, TRIAL_SESSION_LIMIT_SECS,
};
use crate::error::{CoreError, Result};

/// Length of the signed prefix of a token: everything up to `signature`.
const SIGNED_PREFIX_LEN: usize = 1 + 4 + 16 + 1 + 16 + 8 + 8 + 8 + 8 + 32;
/// Total on-wire length of a license token (§12.1).
pub const TOKEN_LEN: usize = SIGNED_PREFIX_LEN + 64;
/// Only token layout version understood by this build.
pub const TOKEN_VERSION: u8 = 1;

/// Commercial plan carried by a license token (§8.2, §12.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// 30 minutes of cumulative active session time.
    Trial,
    /// Single guest, 7 days of offline grace.
    Pro,
    /// Up to 5 concurrent guests, 3 days of offline grace.
    Team,
}

impl Plan {
    /// Total concurrent guests allowed, controller included (§8.2).
    #[must_use]
    pub const fn max_concurrent_guests(self) -> u8 {
        match self {
            Self::Trial => MAX_CONCURRENT_GUESTS_TRIAL,
            Self::Pro => MAX_CONCURRENT_GUESTS_PRO,
            Self::Team => MAX_CONCURRENT_GUESTS_TEAM,
        }
    }

    /// Days the plan may run without a successful heartbeat (§12.3, §12.4).
    /// The trial plan has no offline grace: it is bounded by cumulative
    /// session time instead.
    #[must_use]
    pub const fn offline_grace_days(self) -> Option<u64> {
        match self {
            Self::Trial => None,
            Self::Pro => Some(OFFLINE_GRACE_PRO_DAYS),
            Self::Team => Some(OFFLINE_GRACE_TEAM_DAYS),
        }
    }

    const fn from_wire(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Trial),
            1 => Some(Self::Pro),
            2 => Some(Self::Team),
            _ => None,
        }
    }

    /// Plan for a wire byte, falling back to the least privileged plan.
    ///
    /// A row that somehow holds an unknown plan byte must not widen what a
    /// device may do (§2.1), so it reads as `Trial`.
    #[must_use]
    pub const fn from_wire_or_trial(value: u8) -> Self {
        match Self::from_wire(value) {
            Some(plan) => plan,
            None => Self::Trial,
        }
    }

    /// Wire encoding of the plan byte (§12.1).
    #[must_use]
    pub const fn to_wire(self) -> u8 {
        match self {
            Self::Trial => 0,
            Self::Pro => 1,
            Self::Team => 2,
        }
    }
}

/// Decoded license token (§12.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LicenseToken {
    /// Layout version.
    pub version: u8,
    /// Broker signing key identifier, enables key rotation (§12.2).
    pub key_id: u32,
    /// License identity.
    pub license_id: [u8; 16],
    /// Plan encoded by the token.
    pub plan: Plan,
    /// Random local device identifier, not a hardware fingerprint (§7).
    pub device_id: [u8; 16],
    /// Unix seconds the token was issued at.
    pub issued_at: u64,
    /// Unix seconds before which the token is not valid.
    pub not_before: u64,
    /// Unix seconds after which the token is not valid.
    pub expires_at: u64,
    /// Feature bitmask.
    pub features: u64,
    /// BLAKE3 of the broker-side payload this token was derived from.
    pub payload_hash: [u8; 32],
    /// Ed25519 signature over the preceding bytes.
    pub signature: [u8; 64],
}

impl LicenseToken {
    /// Parses and cryptographically verifies a token.
    ///
    /// Verification happens before any field is trusted; the caller still has
    /// to check `not_before`/`expires_at` against a clock it trusts (§12.3).
    ///
    /// # Errors
    /// [`CoreError::LicenseDenied`] on wrong length, unknown version, unknown
    /// plan or invalid signature. The reason string never contains token bytes
    /// (§15).
    pub fn parse_and_verify(bytes: &[u8], key: &VerifyingKey) -> Result<Self> {
        if bytes.len() != TOKEN_LEN {
            return Err(CoreError::LicenseDenied {
                reason: "malformed license token".to_owned(),
            });
        }
        let (signed, signature_bytes) = bytes.split_at(SIGNED_PREFIX_LEN);

        let mut signature = [0u8; 64];
        signature.copy_from_slice(signature_bytes);
        key.verify_strict(signed, &Signature::from_bytes(&signature))
            .map_err(|_| CoreError::LicenseDenied {
                reason: "invalid license signature".to_owned(),
            })?;

        let mut cursor = Cursor::new(signed);
        let version = cursor.u8();
        if version != TOKEN_VERSION {
            return Err(CoreError::LicenseDenied {
                reason: "unsupported license token version".to_owned(),
            });
        }
        let key_id = cursor.u32();
        let license_id = cursor.array16();
        let plan = Plan::from_wire(cursor.u8()).ok_or_else(|| CoreError::LicenseDenied {
            reason: "unknown plan in license token".to_owned(),
        })?;

        Ok(Self {
            version,
            key_id,
            license_id,
            plan,
            device_id: cursor.array16(),
            issued_at: cursor.u64(),
            not_before: cursor.u64(),
            expires_at: cursor.u64(),
            features: cursor.u64(),
            payload_hash: cursor.array32(),
            signature,
        })
    }

    /// Whether `now` (Unix seconds) is inside `[not_before, expires_at]`.
    #[must_use]
    pub const fn is_valid_at(&self, now: u64) -> bool {
        now >= self.not_before && now <= self.expires_at
    }

    /// Seconds left before `expires_at`, saturating at zero.
    #[must_use]
    pub const fn seconds_left(&self, now: u64) -> u64 {
        self.expires_at.saturating_sub(now)
    }

    /// Serializes the signed prefix: every byte the signature covers (§12.1).
    #[must_use]
    pub fn signed_prefix(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(SIGNED_PREFIX_LEN);
        out.push(self.version);
        out.extend_from_slice(&self.key_id.to_be_bytes());
        out.extend_from_slice(&self.license_id);
        out.push(self.plan.to_wire());
        out.extend_from_slice(&self.device_id);
        out.extend_from_slice(&self.issued_at.to_be_bytes());
        out.extend_from_slice(&self.not_before.to_be_bytes());
        out.extend_from_slice(&self.expires_at.to_be_bytes());
        out.extend_from_slice(&self.features.to_be_bytes());
        out.extend_from_slice(&self.payload_hash);
        out
    }

    /// Serializes the whole token, signature included.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = self.signed_prefix();
        out.extend_from_slice(&self.signature);
        out
    }

    /// Signs `self` with the broker key, filling in [`Self::signature`].
    ///
    /// Only the broker ever calls this; clients verify (§12.1).
    pub fn sign(&mut self, key: &ed25519_dalek::SigningKey) {
        use ed25519_dalek::Signer as _;
        self.signature = key.sign(&self.signed_prefix()).to_bytes();
    }

    /// Which of the `LICENSE_WARN_BEFORE_SECS` thresholds `now` has just
    /// crossed, given the previous evaluation at `previous` (§9.1, §12).
    ///
    /// Returns the threshold itself, so the caller can put it into
    /// `LicenseWarn { seconds_left }`.
    #[must_use]
    pub fn warning_crossed(&self, previous: u64, now: u64) -> Option<u64> {
        let before = self.seconds_left(previous);
        let after = self.seconds_left(now);
        LICENSE_WARN_BEFORE_SECS
            .into_iter()
            .find(|threshold| before > *threshold && after <= *threshold)
    }
}

/// What the license layer allows right now (§12.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LicenseDecision {
    /// Sessions may start and continue.
    Allow {
        /// Plan in force.
        plan: Plan,
        /// Seconds left in the current window, if bounded.
        seconds_left: Option<u64>,
    },
    /// Existing sessions continue on the monotonic timer, but no new session
    /// may start until an online check succeeds. This is the clock-rollback row
    /// of §12.4: the active session is additionally capped by
    /// `CLOCK_ROLLBACK_ACTIVE_SESSION_CUTOFF_SECS`.
    DenyNewSession {
        /// Reason, safe for the UI and free of secrets (§15).
        reason: &'static str,
        /// Hard cap for any session still running.
        cutoff_secs: u64,
    },
    /// Nothing may run: `LicenseDeny` goes out and every session ends (§18).
    Deny {
        /// Reason, safe for the UI and free of secrets (§15).
        reason: &'static str,
    },
}

/// Seconds in a day, for the offline grace of §12.3 expressed in days.
const SECS_PER_DAY: u64 = 24 * 60 * 60;

/// Client-side license state: the single place that implements the offline
/// table of §12.4.
///
/// Wall-clock time is untrusted input here. Rollback is detected by keeping the
/// highest wall clock seen so far, and any duration that has to be trustworthy
/// is measured on [`Instant`] instead (§12.3).
#[derive(Debug)]
pub struct LicenseGuard {
    token: Option<LicenseToken>,
    /// Highest wall clock observed, for rollback detection.
    high_water_wall: u64,
    /// Wall clock of the last successful heartbeat.
    last_heartbeat_wall: u64,
    /// Cumulative trial seconds already spent (§12.3).
    trial_used_secs: u64,
    /// Set once a rollback is seen; cleared by a successful online check.
    rollback_seen: bool,
}

impl LicenseGuard {
    /// Starts from a cached token, as validated at `now` (Unix seconds).
    #[must_use]
    pub fn new(token: Option<LicenseToken>, now: u64) -> Self {
        Self {
            token,
            high_water_wall: now,
            last_heartbeat_wall: now,
            trial_used_secs: 0,
            rollback_seen: false,
        }
    }

    /// Replaces the cached token after a refresh and clears the rollback flag:
    /// a successful online exchange is exactly the check §12.4 asks for.
    pub fn on_online_check(&mut self, token: Option<LicenseToken>, now: u64) {
        if token.is_some() {
            self.token = token;
        }
        self.last_heartbeat_wall = now;
        self.high_water_wall = self.high_water_wall.max(now);
        self.rollback_seen = false;
    }

    /// Records a successful heartbeat (§12.2).
    pub fn on_heartbeat(&mut self, now: u64) {
        self.last_heartbeat_wall = now;
        self.high_water_wall = self.high_water_wall.max(now);
        self.rollback_seen = false;
    }

    /// Adds elapsed trial time, measured on the monotonic clock by the caller.
    pub fn add_trial_seconds(&mut self, elapsed_secs: u64) {
        self.trial_used_secs = self.trial_used_secs.saturating_add(elapsed_secs);
    }

    /// Trial seconds still available (§12.3).
    #[must_use]
    pub const fn trial_remaining_secs(&self) -> u64 {
        TRIAL_SESSION_LIMIT_SECS.saturating_sub(self.trial_used_secs)
    }

    /// Cached token, if any.
    #[must_use]
    pub const fn token(&self) -> Option<&LicenseToken> {
        self.token.as_ref()
    }

    /// Whether a wall-clock rollback is currently held against this install.
    #[must_use]
    pub const fn rollback_seen(&self) -> bool {
        self.rollback_seen
    }

    /// Feeds the current wall clock in and decides what is allowed (§12.4).
    ///
    /// Going backwards at all is treated as a rollback: the client has no
    /// trustworthy way to tell a small backwards step from a large one, and
    /// §24.5 says to prefer the safe reading.
    pub fn evaluate(&mut self, now: u64) -> LicenseDecision {
        if now < self.high_water_wall {
            self.rollback_seen = true;
        } else {
            self.high_water_wall = now;
        }

        let Some(token) = self.token.as_ref() else {
            return LicenseDecision::Deny {
                reason: "no license token; an online check is required",
            };
        };

        // Trial is bounded by cumulative active time, not by offline grace, and
        // being offline never extends it (§12.3).
        if token.plan == Plan::Trial && self.trial_remaining_secs() == 0 {
            return LicenseDecision::Deny {
                reason: "the trial session limit is used up",
            };
        }

        if self.rollback_seen {
            return LicenseDecision::DenyNewSession {
                reason: "system clock rolled back; an online check is required",
                cutoff_secs: CLOCK_ROLLBACK_ACTIVE_SESSION_CUTOFF_SECS,
            };
        }

        if !token.is_valid_at(now) {
            return LicenseDecision::Deny {
                reason: "the license token is outside its validity window",
            };
        }

        if let Some(grace_days) = token.plan.offline_grace_days() {
            let offline_for = now.saturating_sub(self.last_heartbeat_wall);
            if offline_for > grace_days.saturating_mul(SECS_PER_DAY) {
                return LicenseDecision::Deny {
                    reason: "offline grace elapsed without a successful heartbeat",
                };
            }
        }

        let seconds_left = if token.plan == Plan::Trial {
            Some(self.trial_remaining_secs().min(token.seconds_left(now)))
        } else {
            Some(token.seconds_left(now))
        };
        LicenseDecision::Allow {
            plan: token.plan,
            seconds_left,
        }
    }
}

/// Little helper over the fixed-width token layout; every read is bounded by
/// the length check in [`LicenseToken::parse_and_verify`].
struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take<const N: usize>(&mut self) -> [u8; N] {
        let mut out = [0u8; N];
        let end = self.offset + N;
        out.copy_from_slice(&self.bytes[self.offset..end]);
        self.offset = end;
        out
    }

    fn u8(&mut self) -> u8 {
        self.take::<1>()[0]
    }

    fn u32(&mut self) -> u32 {
        u32::from_be_bytes(self.take::<4>())
    }

    fn u64(&mut self) -> u64 {
        u64::from_be_bytes(self.take::<8>())
    }

    fn array16(&mut self) -> [u8; 16] {
        self.take::<16>()
    }

    fn array32(&mut self) -> [u8; 32] {
        self.take::<32>()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use ed25519_dalek::SigningKey;

    use super::*;

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[5u8; 32])
    }

    fn token(plan: Plan, not_before: u64, expires_at: u64) -> LicenseToken {
        let mut token = LicenseToken {
            version: TOKEN_VERSION,
            key_id: 1,
            license_id: [7u8; 16],
            plan,
            device_id: [9u8; 16],
            issued_at: not_before,
            not_before,
            expires_at,
            features: 0,
            payload_hash: [0u8; 32],
            signature: [0u8; 64],
        };
        token.sign(&signing_key());
        token
    }

    #[test]
    fn a_signed_token_round_trips_and_verifies() {
        let issued = token(Plan::Pro, 1_000, 2_000);
        let bytes = issued.encode();
        assert_eq!(bytes.len(), TOKEN_LEN);
        let parsed =
            LicenseToken::parse_and_verify(&bytes, &signing_key().verifying_key()).unwrap();
        assert_eq!(parsed, issued);
    }

    #[test]
    fn a_tampered_field_fails_verification() {
        let mut bytes = token(Plan::Pro, 1_000, 2_000).encode();
        // Flip the plan byte: Pro becomes Team without a matching signature.
        bytes[21] = Plan::Team.to_wire();
        assert!(LicenseToken::parse_and_verify(&bytes, &signing_key().verifying_key()).is_err());
    }

    #[test]
    fn another_key_does_not_verify() {
        let bytes = token(Plan::Team, 0, 10).encode();
        let other = SigningKey::from_bytes(&[6u8; 32]).verifying_key();
        assert!(LicenseToken::parse_and_verify(&bytes, &other).is_err());
    }

    #[test]
    fn warnings_fire_once_per_threshold() {
        let issued = token(Plan::Pro, 0, 1_000);
        // Crossing 300 seconds left.
        assert_eq!(issued.warning_crossed(600, 700), Some(300));
        // Staying inside the same band does not warn again.
        assert_eq!(issued.warning_crossed(700, 800), None);
        // Crossing 60 seconds left.
        assert_eq!(issued.warning_crossed(900, 950), Some(60));
    }

    #[test]
    fn a_valid_token_allows_and_reports_the_time_left() {
        let mut guard = LicenseGuard::new(Some(token(Plan::Pro, 0, 1_000)), 0);
        assert_eq!(
            guard.evaluate(400),
            LicenseDecision::Allow {
                plan: Plan::Pro,
                seconds_left: Some(600)
            }
        );
    }

    #[test]
    fn no_token_denies() {
        let mut guard = LicenseGuard::new(None, 0);
        assert!(matches!(guard.evaluate(0), LicenseDecision::Deny { .. }));
    }

    #[test]
    fn offline_grace_is_per_plan_and_ends_in_a_deny() {
        for (plan, days) in [
            (Plan::Pro, OFFLINE_GRACE_PRO_DAYS),
            (Plan::Team, OFFLINE_GRACE_TEAM_DAYS),
        ] {
            let mut guard = LicenseGuard::new(Some(token(plan, 0, u64::MAX)), 0);
            let inside = days * SECS_PER_DAY;
            assert!(matches!(
                guard.evaluate(inside),
                LicenseDecision::Allow { .. }
            ));
            assert!(matches!(
                guard.evaluate(inside + 1),
                LicenseDecision::Deny { .. }
            ));
            // A heartbeat resets the window.
            guard.on_heartbeat(inside + 1);
            assert!(matches!(
                guard.evaluate(inside + 2),
                LicenseDecision::Allow { .. }
            ));
        }
    }

    #[test]
    fn a_clock_rollback_blocks_new_sessions_and_caps_the_active_one() {
        let mut guard = LicenseGuard::new(Some(token(Plan::Team, 0, u64::MAX)), 0);
        assert!(matches!(
            guard.evaluate(10_000),
            LicenseDecision::Allow { .. }
        ));

        let decision = guard.evaluate(9_000);
        assert_eq!(
            decision,
            LicenseDecision::DenyNewSession {
                reason: "system clock rolled back; an online check is required",
                cutoff_secs: CLOCK_ROLLBACK_ACTIVE_SESSION_CUTOFF_SECS,
            }
        );
        assert!(guard.rollback_seen());

        // Only an online check clears it; time moving forward again does not.
        assert!(matches!(
            guard.evaluate(10_001),
            LicenseDecision::DenyNewSession { .. }
        ));
        guard.on_online_check(None, 10_002);
        assert!(matches!(
            guard.evaluate(10_003),
            LicenseDecision::Allow { .. }
        ));
    }

    #[test]
    fn the_trial_is_bounded_by_cumulative_time_and_offline_does_not_extend_it() {
        let mut guard = LicenseGuard::new(Some(token(Plan::Trial, 0, u64::MAX)), 0);
        assert_eq!(guard.trial_remaining_secs(), TRIAL_SESSION_LIMIT_SECS);
        assert!(matches!(
            guard.evaluate(0),
            LicenseDecision::Allow {
                plan: Plan::Trial,
                ..
            }
        ));

        guard.add_trial_seconds(TRIAL_SESSION_LIMIT_SECS);
        assert_eq!(guard.trial_remaining_secs(), 0);
        // Trial has no offline grace, so this is a plain deny at any wall clock.
        assert!(matches!(guard.evaluate(0), LicenseDecision::Deny { .. }));
        assert!(matches!(
            guard.evaluate(10 * SECS_PER_DAY),
            LicenseDecision::Deny { .. }
        ));
    }

    #[test]
    fn plan_limits_follow_constants() {
        assert_eq!(Plan::Trial.max_concurrent_guests(), 1);
        assert_eq!(Plan::Pro.max_concurrent_guests(), 1);
        assert_eq!(Plan::Team.max_concurrent_guests(), 5);
        assert_eq!(Plan::Trial.offline_grace_days(), None);
    }

    #[test]
    fn short_token_is_rejected() {
        let key = VerifyingKey::from_bytes(&[0u8; 32]);
        if let Ok(key) = key {
            assert!(LicenseToken::parse_and_verify(&[0u8; 8], &key).is_err());
        }
    }
}
