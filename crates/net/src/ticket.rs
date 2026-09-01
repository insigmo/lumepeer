//! Invite tickets and their text/short-link encodings (design doc §7).
//!
//! A ticket carries a TTL of `INVITE_TICKET_TTL_SECS` and is reusable until
//! it expires or the host issues a replacement (ADR 0016). The invite code
//! carries the ticket itself; a short link carries only an opaque random id of
//! `SHORT_LINK_ID_BITS`, never endpoint identity.
//!
//! Parsing a ticket authorizes nothing: the host verifies the signature, the
//! TTL and the registry state, and the guest still has to pass consent (§2.3).

use std::collections::HashMap;
use std::net::SocketAddr;

use data_encoding::BASE32_NOPAD;
use ed25519_dalek::{Signer as _, SigningKey, Verifier as _, VerifyingKey};
use lumepeer_core::consent::Role;
use lumepeer_core::constants::{INVITE_ID_BITS, INVITE_TICKET_TTL_SECS, SHORT_LINK_ID_BITS};
use lumepeer_core::protocol::PROTOCOL_MAJOR;
use rand::Rng as _;
use serde::{Deserialize, Serialize};

use crate::error::{NetError, Result};

/// Prefix of the invite code, so a reader can tell our tickets apart.
pub const INVITE_CODE_PREFIX: &str = "lumepeer1:";

/// Bytes of the random invite id, from `INVITE_ID_BITS` (§14).
pub const INVITE_ID_BYTES: usize = INVITE_ID_BITS / 8;
/// Bytes of the opaque short-link id, from `SHORT_LINK_ID_BITS` (§14).
pub const SHORT_LINK_ID_BYTES: usize = SHORT_LINK_ID_BITS / 8;

/// Invitation issued by the host (§7). Live until its TTL runs out or the
/// host issues a replacement (ADR 0016).
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
    /// Host's public reflexive address on the obfuscated transport, if STUN
    /// discovery found one (task 17 increment 2, ADR 0053). `None` when no
    /// reflector answered or the mapping is unusable (double NAT), in which
    /// case a guest falls back to `node_addr` over the existing iroh path.
    pub obfuscated_addr: Option<SocketAddr>,
    /// Blake3 fingerprint of the host's self-signed cert for the obfuscated
    /// transport, if `obfuscated_addr` is set (task 17 increment 2, ADR 0053).
    /// A guest pins its TLS verification to exactly this cert rather than
    /// validating against a CA, since there is no CA for an ad-hoc peer cert.
    pub host_cert_fingerprint: Option<[u8; 32]>,
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
    obfuscated_addr: Option<SocketAddr>,
    host_cert_fingerprint: Option<[u8; 32]>,
}

/// Lifecycle of a ticket on the host side (§7, as amended by ADR 0016).
///
/// A live ticket may be claimed more than once: the host's consent decision,
/// not the invite, is what authorizes a session, and a guest that was let in
/// once has to be able to come back without the host reading out a new code.
/// What bounds an invite is its TTL and the host's own ability to retire it by
/// issuing a replacement — never a claim count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketState {
    /// Issued and claimable, however many times, until it expires or the host
    /// retires it.
    Live,
    /// TTL elapsed; no further claim is possible.
    Expired,
    /// Withdrawn by the host, which is what issuing a replacement does.
    Retired,
}

impl InviteTicket {
    /// Issues a ticket for `addr`, valid for `INVITE_TICKET_TTL_SECS` from
    /// `now` (Unix seconds), signed with the host's invite key.
    ///
    /// `obfuscated_addr`/`host_cert_fingerprint` carry the STUN-discovered
    /// address and pinned cert fingerprint for the obfuscated transport
    /// (task 17 increment 2, ADR 0053); pass `None` for both when that
    /// transport is not bound or STUN found no usable address — the ticket
    /// still works over `addr` via the existing iroh path.
    ///
    /// # Errors
    /// [`NetError::MalformedTicket`] if the address or the signed prefix cannot
    /// be serialized.
    pub fn issue(
        signing_key: &SigningKey,
        addr: &iroh::EndpointAddr,
        allowed_request: Role,
        now: u64,
        obfuscated_addr: Option<SocketAddr>,
        host_cert_fingerprint: Option<[u8; 32]>,
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
            obfuscated_addr,
            host_cert_fingerprint,
        })
        .map_err(|_| NetError::MalformedTicket)?;

        Ok(Self {
            protocol_major: PROTOCOL_MAJOR,
            node_addr,
            invite_id,
            expires_at,
            allowed_request,
            obfuscated_addr,
            host_cert_fingerprint,
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
            obfuscated_addr: self.obfuscated_addr,
            host_cert_fingerprint: self.host_cert_fingerprint,
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

    /// Encodes the ticket into the invite code the host shows and the guest
    /// pastes (§7).
    ///
    /// # Errors
    /// [`NetError::MalformedTicket`] if encoding fails.
    pub fn to_code(&self) -> Result<String> {
        let bytes = postcard::to_allocvec(self).map_err(|_| NetError::MalformedTicket)?;
        Ok(format!(
            "{INVITE_CODE_PREFIX}{}",
            BASE32_NOPAD.encode(&bytes)
        ))
    }

    /// Parses a ticket produced by [`Self::to_code`].
    ///
    /// The signature and TTL are checked by [`Self::verify`] before the ticket
    /// is honoured; parsing alone authorizes nothing (§2.3).
    ///
    /// # Errors
    /// [`NetError::MalformedTicket`] on decoding failure.
    pub fn from_code(encoded: &str) -> Result<Self> {
        let body = encoded
            .strip_prefix(INVITE_CODE_PREFIX)
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

/// Host-side registry of issued tickets. A claim checks that the invite is one
/// this host issued, is still live and has not expired; a repeat claim of a
/// live ticket is allowed and still faces consent (§7, ADR 0016).
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

    /// Records a freshly issued ticket as [`TicketState::Live`].
    pub fn register(&mut self, ticket: &InviteTicket) {
        self.states.insert(ticket.invite_id, TicketState::Live);
    }

    /// State of a ticket, if the host issued it.
    #[must_use]
    pub fn state(&self, invite_id: &[u8; INVITE_ID_BYTES]) -> Option<TicketState> {
        self.states.get(invite_id).copied()
    }

    /// Claims a ticket for a peer that presented it.
    ///
    /// Expires the entry when `now` is past the TTL, and refuses anything this
    /// host did not issue or has already retired. A live ticket claimed again
    /// is accepted: the guest still has to pass consent, and that is where the
    /// host decides (§2.3, ADR 0016).
    ///
    /// # Errors
    /// [`NetError::InvalidTicket`] if the ticket is unknown, expired or
    /// retired.
    pub fn claim(&mut self, ticket: &InviteTicket, now: u64) -> Result<()> {
        let state = self
            .states
            .get_mut(&ticket.invite_id)
            .ok_or(NetError::InvalidTicket)?;
        if ticket.is_expired_at(now) {
            *state = TicketState::Expired;
            return Err(NetError::InvalidTicket);
        }
        if *state != TicketState::Live {
            return Err(NetError::InvalidTicket);
        }
        Ok(())
    }

    /// Retires every ticket this host has issued so far.
    ///
    /// Issuing a replacement invite is what calls this: exactly one invite is
    /// live at a time, so "refresh the code" is also how a host withdraws the
    /// one it handed out earlier (ADR 0016).
    pub fn retire_all(&mut self) {
        for state in self.states.values_mut() {
            if *state == TicketState::Live {
                *state = TicketState::Retired;
            }
        }
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
        InviteTicket::issue(&keypair(), &addr(), Role::ViewOnly, now, None, None).unwrap()
    }

    /// A fixed obfuscated address/fingerprint pair for the tests that need
    /// the `Some` case (task 17 increment 2, ADR 0053).
    fn obfuscated() -> (SocketAddr, [u8; 32]) {
        ("203.0.113.7:41230".parse().unwrap(), [0x42; 32])
    }

    #[test]
    fn code_roundtrip_preserves_the_ticket() {
        let issued = ticket(1_000);
        let decoded = InviteTicket::from_code(&issued.to_code().unwrap()).unwrap();
        assert_eq!(issued, decoded);
        assert_eq!(decoded.endpoint_addr().unwrap(), addr());
        // The common no-STUN-address case round-trips as `None`, not a
        // decode error.
        assert_eq!(decoded.obfuscated_addr, None);
        assert_eq!(decoded.host_cert_fingerprint, None);
    }

    /// task 17 increment 2 (ADR 0053): when STUN found a usable address, both
    /// new fields round-trip through the code exactly, alongside everything
    /// the previous test already covers for the `None` case.
    #[test]
    fn code_roundtrip_preserves_the_obfuscated_address_and_fingerprint() {
        let (obfuscated_addr, fingerprint) = obfuscated();
        let issued = InviteTicket::issue(
            &keypair(),
            &addr(),
            Role::ViewOnly,
            1_000,
            Some(obfuscated_addr),
            Some(fingerprint),
        )
        .unwrap();
        let decoded = InviteTicket::from_code(&issued.to_code().unwrap()).unwrap();
        assert_eq!(decoded.obfuscated_addr, Some(obfuscated_addr));
        assert_eq!(decoded.host_cert_fingerprint, Some(fingerprint));
        decoded.verify(&keypair().verifying_key(), 1_000).unwrap();
    }

    /// task 17 increment 2 (ADR 0053): the two new fields are signed like
    /// every other field — tampering with either is caught the same way
    /// `signature_and_ttl_are_both_enforced` already covers `allowed_request`.
    #[test]
    fn tampering_with_the_obfuscated_address_or_fingerprint_is_caught() {
        let (obfuscated_addr, fingerprint) = obfuscated();
        let issued = InviteTicket::issue(
            &keypair(),
            &addr(),
            Role::ViewOnly,
            1_000,
            Some(obfuscated_addr),
            Some(fingerprint),
        )
        .unwrap();
        let key = keypair().verifying_key();

        let mut tampered_addr = issued.clone();
        tampered_addr.obfuscated_addr = Some("198.51.100.9:1".parse().unwrap());
        assert!(matches!(
            tampered_addr.verify(&key, 1_000),
            Err(NetError::InvalidTicket)
        ));

        let mut tampered_fingerprint = issued;
        tampered_fingerprint.host_cert_fingerprint = Some([0x99; 32]);
        assert!(matches!(
            tampered_fingerprint.verify(&key, 1_000),
            Err(NetError::InvalidTicket)
        ));
    }

    #[test]
    fn a_code_without_the_prefix_is_refused() {
        let encoded = ticket(1_000).to_code().unwrap();
        let stripped = encoded.strip_prefix(INVITE_CODE_PREFIX).unwrap().to_owned();
        assert!(matches!(
            InviteTicket::from_code(&stripped),
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

    /// ADR 0016: an invite is a way back to a host the guest has already been
    /// let in to, so the same code has to keep working. Consent, not the
    /// invite, is what authorizes each of those sessions.
    #[test]
    fn a_live_ticket_can_be_claimed_more_than_once() {
        let issued = ticket(1_000);
        let mut registry = TicketRegistry::new();
        registry.register(&issued);
        registry.claim(&issued, 1_000).unwrap();
        registry.claim(&issued, 1_000).unwrap();
        registry
            .claim(&issued, 1_000 + INVITE_TICKET_TTL_SECS)
            .unwrap();
        assert_eq!(registry.state(&issued.invite_id), Some(TicketState::Live));
    }

    /// The host's own withdrawal path: issuing a replacement invite retires
    /// every code it read out before, and a retired code stops working at
    /// once even though its TTL has not run out.
    #[test]
    fn a_retired_ticket_is_refused_even_before_its_ttl_runs_out() {
        let issued = ticket(1_000);
        let mut registry = TicketRegistry::new();
        registry.register(&issued);
        registry.claim(&issued, 1_000).unwrap();

        registry.retire_all();
        assert_eq!(
            registry.state(&issued.invite_id),
            Some(TicketState::Retired)
        );
        assert!(matches!(
            registry.claim(&issued, 1_000),
            Err(NetError::InvalidTicket)
        ));
    }

    /// A code this host never issued is refused whatever its state table says
    /// — deny-by-default, not "unknown means fine".
    #[test]
    fn an_unregistered_ticket_is_refused() {
        let issued = ticket(1_000);
        let mut registry = TicketRegistry::new();
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
