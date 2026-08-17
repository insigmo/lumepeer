//! Invite tickets and their QR/short-link encodings (design doc §7).
//!
//! A ticket is single-use with a TTL of `INVITE_TICKET_TTL_SECS`. The QR code
//! carries the ticket itself; a short link carries only an opaque random id of
//! `SHORT_LINK_ID_BITS`, never endpoint identity.
//!
//! Parsing a ticket authorizes nothing: the host verifies the signature, the
//! TTL and the single-use state, and the guest still has to pass consent (§2.3).

use std::collections::HashMap;

use data_encoding::BASE32_NOPAD;
use ed25519_dalek::{Signer as _, SigningKey, Verifier as _, VerifyingKey};
use lumepeer_core::consent::Role;
use lumepeer_core::constants::{INVITE_ID_BITS, INVITE_TICKET_TTL_SECS, SHORT_LINK_ID_BITS};
use lumepeer_core::protocol::PROTOCOL_MAJOR;
use rand::Rng as _;
use serde::{Deserialize, Serialize};

use crate::error::{NetError, Result};

/// Prefix of the QR payload, so a scanner can tell our tickets apart.
pub const QR_PREFIX: &str = "lumepeer1:";

/// Bytes of the random invite id, from `INVITE_ID_BITS` (§14).
pub const INVITE_ID_BYTES: usize = INVITE_ID_BITS / 8;
/// Bytes of the opaque short-link id, from `SHORT_LINK_ID_BITS` (§14).
pub const SHORT_LINK_ID_BYTES: usize = SHORT_LINK_ID_BITS / 8;

/// One-shot invitation issued by the host (§7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteTicket {
    /// Protocol major the host speaks (§9.1).
    pub protocol_major: u16,
    /// Serialized address of the host (`iroh::EndpointAddr`, the `NodeAddr` of
    /// the design doc).
    pub node_addr: Vec<u8>,
    /// Random identifier, `INVITE_ID_BITS` wide.
    pub invite_id: [u8; INVITE_ID_BYTES],
    /// Unix seconds after which the ticket is dead.
    pub expires_at: u64,
    /// Capability the guest is allowed to ask for; the host still decides (§2.3).
    pub allowed_request: Role,
    /// Ed25519 signature of the host over the preceding fields.
    #[serde(with = "serde_big_array::BigArray")]
    pub signature: [u8; 64],
}

/// The signed part of a ticket: everything except the signature itself.
#[derive(Debug, Serialize)]
struct SignedFields<'a> {
    protocol_major: u16,
    node_addr: &'a [u8],
    invite_id: &'a [u8; INVITE_ID_BYTES],
    expires_at: u64,
    allowed_request: Role,
}

/// Lifecycle of a ticket on the host side; claiming is atomic and reuse is
/// refused (§7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketState {
    /// Issued, not claimed yet.
    Unused,
    /// Claimed by a peer, handshake in progress.
    Claimed,
    /// Successfully used; cannot be reused.
    Consumed,
    /// TTL elapsed before it was claimed.
    Expired,
}

impl InviteTicket {
    /// Issues a ticket for `addr`, valid for `INVITE_TICKET_TTL_SECS` from
    /// `now` (Unix seconds), signed with the host's invite key.
    ///
    /// # Errors
    /// [`NetError::MalformedTicket`] if the address or the signed prefix cannot
    /// be serialized.
    pub fn issue(
        signing_key: &SigningKey,
        addr: &iroh::EndpointAddr,
        allowed_request: Role,
        now: u64,
    ) -> Result<Self> {
        let node_addr = postcard::to_allocvec(addr).map_err(|_| NetError::MalformedTicket)?;
        let mut invite_id = [0u8; INVITE_ID_BYTES];
        rand::rng().fill_bytes(&mut invite_id);
        let expires_at = now.saturating_add(INVITE_TICKET_TTL_SECS);

        let signed = postcard::to_allocvec(&SignedFields {
            protocol_major: PROTOCOL_MAJOR,
            node_addr: &node_addr,
            invite_id: &invite_id,
            expires_at,
            allowed_request,
        })
        .map_err(|_| NetError::MalformedTicket)?;

        Ok(Self {
            protocol_major: PROTOCOL_MAJOR,
            node_addr,
            invite_id,
            expires_at,
            allowed_request,
            signature: signing_key.sign(&signed).to_bytes(),
        })
    }

    /// Verifies the host signature and the TTL against `now` (Unix seconds).
    ///
    /// # Errors
    /// [`NetError::InvalidTicket`] if the signature does not verify, the
    /// protocol major differs or the ticket has expired.
    pub fn verify(&self, verifying_key: &VerifyingKey, now: u64) -> Result<()> {
        if self.protocol_major != PROTOCOL_MAJOR || self.is_expired_at(now) {
            return Err(NetError::InvalidTicket);
        }
        let signed = postcard::to_allocvec(&SignedFields {
            protocol_major: self.protocol_major,
            node_addr: &self.node_addr,
            invite_id: &self.invite_id,
            expires_at: self.expires_at,
            allowed_request: self.allowed_request,
        })
        .map_err(|_| NetError::InvalidTicket)?;
        let signature = ed25519_dalek::Signature::from_bytes(&self.signature);
        verifying_key
            .verify(&signed, &signature)
            .map_err(|_| NetError::InvalidTicket)
    }

    /// Host address carried by the ticket.
    ///
    /// # Errors
    /// [`NetError::MalformedTicket`] if the embedded address does not decode.
    pub fn endpoint_addr(&self) -> Result<iroh::EndpointAddr> {
        postcard::from_bytes(&self.node_addr).map_err(|_| NetError::MalformedTicket)
    }

    /// Encodes the ticket into the string embedded in a QR code (§7).
    ///
    /// # Errors
    /// [`NetError::MalformedTicket`] if encoding fails.
    pub fn to_qr_string(&self) -> Result<String> {
        let bytes = postcard::to_allocvec(self).map_err(|_| NetError::MalformedTicket)?;
        Ok(format!("{QR_PREFIX}{}", BASE32_NOPAD.encode(&bytes)))
    }

    /// Parses a ticket produced by [`Self::to_qr_string`].
    ///
    /// The signature and TTL are checked by [`Self::verify`] before the ticket
    /// is honoured; parsing alone authorizes nothing (§2.3).
    ///
    /// # Errors
    /// [`NetError::MalformedTicket`] on decoding failure.
    pub fn from_qr_string(encoded: &str) -> Result<Self> {
        let body = encoded
            .strip_prefix(QR_PREFIX)
            .ok_or(NetError::MalformedTicket)?;
        let bytes = BASE32_NOPAD
            .decode(body.as_bytes())
            .map_err(|_| NetError::MalformedTicket)?;
        postcard::from_bytes(&bytes).map_err(|_| NetError::MalformedTicket)
    }

    /// Whether `now` (Unix seconds) is past `expires_at`.
    #[must_use]
    pub const fn is_expired_at(&self, now: u64) -> bool {
        now > self.expires_at
    }
}

/// Opaque short-link id. It carries no endpoint identity, only randomness (§7).
#[must_use]
pub fn new_short_link_id() -> [u8; SHORT_LINK_ID_BYTES] {
    let mut id = [0u8; SHORT_LINK_ID_BYTES];
    rand::rng().fill_bytes(&mut id);
    id
}

/// Host-side registry of issued tickets. A claim is atomic: the first claimer
/// wins and every later attempt is refused, expired or not (§7).
#[derive(Debug, Default)]
pub struct TicketRegistry {
    states: HashMap<[u8; INVITE_ID_BYTES], TicketState>,
}

impl TicketRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            states: HashMap::new(),
        }
    }

    /// Records a freshly issued ticket as `Unused`.
    pub fn register(&mut self, ticket: &InviteTicket) {
        self.states.insert(ticket.invite_id, TicketState::Unused);
    }

    /// State of a ticket, if the host issued it.
    #[must_use]
    pub fn state(&self, invite_id: &[u8; INVITE_ID_BYTES]) -> Option<TicketState> {
        self.states.get(invite_id).copied()
    }

    /// Claims a ticket for a peer that presented it.
    ///
    /// Expires the entry when `now` is past the TTL, and refuses anything that
    /// is not exactly one `Unused` ticket.
    ///
    /// # Errors
    /// [`NetError::InvalidTicket`] if the ticket is unknown, expired, already
    /// claimed or already consumed.
    pub fn claim(&mut self, ticket: &InviteTicket, now: u64) -> Result<()> {
        let state = self
            .states
            .get_mut(&ticket.invite_id)
            .ok_or(NetError::InvalidTicket)?;
        if ticket.is_expired_at(now) {
            *state = TicketState::Expired;
            return Err(NetError::InvalidTicket);
        }
        if *state != TicketState::Unused {
            return Err(NetError::InvalidTicket);
        }
        *state = TicketState::Claimed;
        Ok(())
    }

    /// Marks a claimed ticket as spent once the handshake succeeded.
    ///
    /// # Errors
    /// [`NetError::InvalidTicket`] if the ticket was not in `Claimed`.
    pub fn consume(&mut self, invite_id: &[u8; INVITE_ID_BYTES]) -> Result<()> {
        let state = self
            .states
            .get_mut(invite_id)
            .ok_or(NetError::InvalidTicket)?;
        if *state != TicketState::Claimed {
            return Err(NetError::InvalidTicket);
        }
        *state = TicketState::Consumed;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use iroh::EndpointAddr;

    use super::*;

    fn keypair() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn addr() -> EndpointAddr {
        EndpointAddr::from(iroh::SecretKey::from_bytes(&[3u8; 32]).public())
    }

    fn ticket(now: u64) -> InviteTicket {
        InviteTicket::issue(&keypair(), &addr(), Role::ViewOnly, now).unwrap()
    }

    #[test]
    fn qr_roundtrip_preserves_the_ticket() {
        let issued = ticket(1_000);
        let decoded = InviteTicket::from_qr_string(&issued.to_qr_string().unwrap()).unwrap();
        assert_eq!(issued, decoded);
        assert_eq!(decoded.endpoint_addr().unwrap(), addr());
    }

    #[test]
    fn qr_string_without_the_prefix_is_refused() {
        let encoded = ticket(1_000).to_qr_string().unwrap();
        let stripped = encoded.strip_prefix(QR_PREFIX).unwrap().to_owned();
        assert!(matches!(
            InviteTicket::from_qr_string(&stripped),
            Err(NetError::MalformedTicket)
        ));
    }

    #[test]
    fn signature_and_ttl_are_both_enforced() {
        let issued = ticket(1_000);
        let key = keypair().verifying_key();
        issued.verify(&key, 1_000).unwrap();
        // One second past the TTL.
        assert!(matches!(
            issued.verify(&key, 1_000 + INVITE_TICKET_TTL_SECS + 1),
            Err(NetError::InvalidTicket)
        ));
        // A different host key does not verify.
        let other = SigningKey::from_bytes(&[9u8; 32]).verifying_key();
        assert!(matches!(
            issued.verify(&other, 1_000),
            Err(NetError::InvalidTicket)
        ));
        // Neither does a tampered field.
        let mut tampered = issued;
        tampered.allowed_request = Role::FullControl;
        assert!(matches!(
            tampered.verify(&key, 1_000),
            Err(NetError::InvalidTicket)
        ));
    }

    #[test]
    fn a_ticket_can_be_claimed_exactly_once() {
        let issued = ticket(1_000);
        let mut registry = TicketRegistry::new();
        registry.register(&issued);
        registry.claim(&issued, 1_000).unwrap();
        assert!(matches!(
            registry.claim(&issued, 1_000),
            Err(NetError::InvalidTicket)
        ));
        registry.consume(&issued.invite_id).unwrap();
        assert_eq!(
            registry.state(&issued.invite_id),
            Some(TicketState::Consumed)
        );
        assert!(matches!(
            registry.claim(&issued, 1_000),
            Err(NetError::InvalidTicket)
        ));
    }

    #[test]
    fn an_expired_ticket_cannot_be_claimed() {
        let issued = ticket(1_000);
        let mut registry = TicketRegistry::new();
        registry.register(&issued);
        let too_late = 1_000 + INVITE_TICKET_TTL_SECS + 1;
        assert!(matches!(
            registry.claim(&issued, too_late),
            Err(NetError::InvalidTicket)
        ));
        assert_eq!(
            registry.state(&issued.invite_id),
            Some(TicketState::Expired)
        );
    }

    #[test]
    fn short_link_ids_are_random_and_the_right_width() {
        let first = new_short_link_id();
        assert_eq!(first.len(), SHORT_LINK_ID_BYTES);
        assert_ne!(first, new_short_link_id());
    }
}
