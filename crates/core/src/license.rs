//! License token format and validation (design doc §12).
//!
//! The token is a compact custom binary format, not a JWT (§5.1), signed with
//! Ed25519 over every byte preceding the signature and verified with
//! `verify_strict` — no hand-rolled constant-time comparison (§12.1, §20).

use ed25519_dalek::{Signature, VerifyingKey};

use crate::constants::{
    MAX_CONCURRENT_GUESTS_PRO, MAX_CONCURRENT_GUESTS_TEAM, MAX_CONCURRENT_GUESTS_TRIAL,
    OFFLINE_GRACE_PRO_DAYS, OFFLINE_GRACE_TEAM_DAYS,
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
    use super::*;

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
