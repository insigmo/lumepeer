//! Invite tickets and their QR/short-link encodings (design doc §7).
//!
//! A ticket is single-use with a TTL of `INVITE_TICKET_TTL_SECS`. The QR code
//! carries the ticket itself; a short link carries only an opaque random id of
//! `SHORT_LINK_ID_BITS`, never endpoint identity.

use lumepeer_core::consent::Role;
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// One-shot invitation issued by the host (§7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteTicket {
    /// Protocol major the host speaks (§9.1).
    pub protocol_major: u16,
    /// Serialized address of the host (`iroh::EndpointAddr`, the `NodeAddr` of
    /// the design doc).
    pub node_addr: Vec<u8>,
    /// Random identifier, `INVITE_ID_BITS` wide.
    pub invite_id: [u8; 16],
    /// Unix seconds after which the ticket is dead.
    pub expires_at: u64,
    /// Capability the guest is allowed to ask for; the host still decides (§2.3).
    pub allowed_request: Role,
    /// Ed25519 signature of the host over the preceding fields.
    #[serde(with = "serde_big_array::BigArray")]
    pub signature: [u8; 64],
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
    /// Encodes the ticket into the string embedded in a QR code (§7).
    ///
    /// # Errors
    /// [`crate::error::NetError::MalformedTicket`] if encoding fails.
    pub fn to_qr_string(&self) -> Result<String> {
        todo!("phase 1: postcard + base32 encoding of the invite ticket")
    }

    /// Parses a ticket produced by [`Self::to_qr_string`].
    ///
    /// The signature and TTL are checked by the host before the ticket is
    /// honoured; parsing alone authorizes nothing (§2.3).
    ///
    /// # Errors
    /// [`crate::error::NetError::MalformedTicket`] on decoding failure.
    pub fn from_qr_string(_encoded: &str) -> Result<Self> {
        todo!("phase 1: decode and verify the invite ticket")
    }

    /// Whether `now` (Unix seconds) is past `expires_at`.
    #[must_use]
    pub const fn is_expired_at(&self, now: u64) -> bool {
        now > self.expires_at
    }
}
