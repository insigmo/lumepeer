//! Minimal STUN Binding client for serverless address discovery (task 17;
//! ADR 0052).
//!
//! A host behind NAT cannot learn its own public reflexive `ip:port` from its
//! router alone — under a double NAT the router only knows its own private WAN
//! address (Fase 0 of the task; ADR 0051). One stateless STUN Binding
//! request/response reveals it, and that is all this module does: it is a
//! *reflector*, not a relay. It never carries session data — the query runs on
//! the very socket that will later carry the obfuscated QUIC session, so the
//! reflexive address it learns is the mapping that session will use, and the
//! address then travels in the invite the two peers already exchange by hand.
//!
//! This deliberately is **not** iroh's own netcheck: that learns the reflexive
//! address through n0's relay fleet, the exact dependency task 17 removes.
//! Here the caller supplies its own STUN servers, so no n0 (or other fixed)
//! server is in the path.
//!
//! Everything parsed here is untrusted network input, so it never panics, never
//! indexes unchecked, and rejects anything malformed with [`NetError`]
//! (§2.4; task 17 trap).

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::Duration;

use lumepeer_core::constants::STUN_QUERY_TIMEOUT_MS;
use rand::Rng as _;

use crate::error::{NetError, Result};

/// STUN magic cookie (RFC 5389 §6): fixed, and also the XOR key for the mapped
/// address.
const MAGIC_COOKIE: u32 = 0x2112_A442;
/// High 16 bits of [`MAGIC_COOKIE`], the XOR key for the mapped port.
const MAGIC_COOKIE_HI: u16 = 0x2112;
/// STUN message type for a Binding request.
const BINDING_REQUEST: u16 = 0x0001;
/// STUN message type for a Binding success response.
const BINDING_SUCCESS: u16 = 0x0101;
/// Attribute type carrying the reflexive address, obfuscated with the cookie
/// (RFC 5389 §15.2). Preferred over the plain `MAPPED-ADDRESS` because the XOR
/// keeps the address out of the clear, which some middleboxes rewrite.
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
/// Address family byte for IPv4 inside a mapped-address attribute.
const FAMILY_IPV4: u8 = 0x01;
/// Bytes of the fixed STUN header: type, length, cookie, transaction id.
const HEADER_BYTES: usize = 20;
/// Bytes of the random STUN transaction id.
const TXID_BYTES: usize = 12;

/// Asks one STUN server for this socket's public reflexive address.
///
/// Sends a Binding request from `socket` to `server` and waits up to
/// [`STUN_QUERY_TIMEOUT_MS`] for the matching success response. Runs the query
/// on `socket` itself so the mapping it reports is the one a later session on
/// the same socket will use; the caller is expected to try a list of servers in
/// turn until one answers.
///
/// # Errors
/// [`NetError::Io`] if the socket send/recv or its read-timeout setup fails, if
/// the reply comes from another source, or if it is not a well-formed Binding
/// success for this request — so a silent, unreachable or lying server is
/// skipped, never fatal.
pub fn reflexive_addr(socket: &UdpSocket, server: SocketAddr) -> Result<SocketAddr> {
    let mut txid = [0u8; TXID_BYTES];
    rand::rng().fill_bytes(&mut txid);
    let request = binding_request(&txid);

    socket
        .set_read_timeout(Some(Duration::from_millis(STUN_QUERY_TIMEOUT_MS)))
        .map_err(|e| NetError::Io(e.to_string()))?;
    socket
        .send_to(&request, server)
        .map_err(|e| NetError::Io(e.to_string()))?;

    let mut buf = [0u8; 1500];
    let (n, from) = socket
        .recv_from(&mut buf)
        .map_err(|e| NetError::Io(e.to_string()))?;
    if from != server {
        return Err(NetError::Io(
            "stun reply from an unexpected source".to_owned(),
        ));
    }
    let response = buf
        .get(..n)
        .ok_or_else(|| NetError::Io("short stun read".to_owned()))?;
    parse_binding_response(&txid, response)
        .ok_or_else(|| NetError::Io("malformed stun reply".to_owned()))
}

/// Builds a 20-byte Binding request with no attributes.
fn binding_request(txid: &[u8; TXID_BYTES]) -> [u8; HEADER_BYTES] {
    let mut msg = [0u8; HEADER_BYTES];
    msg[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    // Message length: zero attributes.
    msg[2..4].copy_from_slice(&0u16.to_be_bytes());
    msg[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    msg[8..HEADER_BYTES].copy_from_slice(txid);
    msg
}

/// Parses a Binding success response, returning the reflexive address, or
/// `None` for anything that is not a well-formed reply to `txid`.
///
/// Every field is bounds-checked before it is read; a truncated, mistyped, or
/// mismatched message returns `None` rather than panicking (untrusted input).
fn parse_binding_response(txid: &[u8; TXID_BYTES], msg: &[u8]) -> Option<SocketAddr> {
    let header = msg.get(..HEADER_BYTES)?;
    let msg_type = u16::from_be_bytes([*header.first()?, *header.get(1)?]);
    let cookie = u32::from_be_bytes([
        *header.get(4)?,
        *header.get(5)?,
        *header.get(6)?,
        *header.get(7)?,
    ]);
    if msg_type != BINDING_SUCCESS || cookie != MAGIC_COOKIE || header.get(8..HEADER_BYTES)? != txid
    {
        return None;
    }

    // Walk the attributes. Each is type(2) len(2) value(len) padded to 4 bytes.
    let mut attrs = msg.get(HEADER_BYTES..)?;
    while !attrs.is_empty() {
        let attr_type = u16::from_be_bytes([*attrs.first()?, *attrs.get(1)?]);
        let attr_len = usize::from(u16::from_be_bytes([*attrs.get(2)?, *attrs.get(3)?]));
        let value = attrs.get(4..4 + attr_len)?;
        if attr_type == ATTR_XOR_MAPPED_ADDRESS {
            return parse_xor_mapped_ipv4(value);
        }
        // Advance past this attribute and its padding to the 4-byte boundary.
        let padded = 4 + attr_len.next_multiple_of(4);
        attrs = attrs.get(padded..)?;
    }
    None
}

/// Decodes an IPv4 `XOR-MAPPED-ADDRESS` attribute value: `0 || family ||
/// x-port || x-address`, each field combined with the magic cookie by XOR
/// (RFC 5389 §15.2). IPv6 (family `0x02`) is not decoded — this pair has no
/// IPv6, and an unrecognised family returns `None` rather than guessing.
fn parse_xor_mapped_ipv4(value: &[u8]) -> Option<SocketAddr> {
    // reserved(1) family(1) x-port(2) x-address(4) = 8 bytes for IPv4.
    if value.len() < 8 || *value.get(1)? != FAMILY_IPV4 {
        return None;
    }
    let x_port = u16::from_be_bytes([*value.get(2)?, *value.get(3)?]);
    let port = x_port ^ MAGIC_COOKIE_HI;
    let x_addr = u32::from_be_bytes([
        *value.get(4)?,
        *value.get(5)?,
        *value.get(6)?,
        *value.get(7)?,
    ]);
    let addr = Ipv4Addr::from(x_addr ^ MAGIC_COOKIE);
    Some(SocketAddr::V4(SocketAddrV4::new(addr, port)))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// RFC 5769 §2.2 transaction id, so the sample response below verifies.
    const RFC_TXID: [u8; TXID_BYTES] = [
        0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae,
    ];

    /// A Binding success response carrying only the RFC 5769 §2.2 IPv4
    /// `XOR-MAPPED-ADDRESS`, which decodes to 192.0.2.1:32853.
    fn rfc_response() -> Vec<u8> {
        let mut msg = Vec::new();
        msg.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
        msg.extend_from_slice(&12u16.to_be_bytes()); // one 12-byte attribute
        msg.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        msg.extend_from_slice(&RFC_TXID);
        msg.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
        msg.extend_from_slice(&8u16.to_be_bytes());
        msg.extend_from_slice(&[0x00, FAMILY_IPV4, 0xa1, 0x47, 0xe1, 0x12, 0xa6, 0x43]);
        msg
    }

    #[test]
    fn parses_the_rfc_5769_sample_response() {
        let addr = parse_binding_response(&RFC_TXID, &rfc_response()).unwrap();
        assert_eq!(addr, "192.0.2.1:32853".parse().unwrap());
    }

    #[test]
    fn a_reply_to_a_different_transaction_is_ignored() {
        let wrong = [0u8; TXID_BYTES];
        assert!(parse_binding_response(&wrong, &rfc_response()).is_none());
    }

    #[test]
    fn a_request_type_is_not_read_as_a_success_response() {
        let mut msg = rfc_response();
        msg[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
        assert!(parse_binding_response(&RFC_TXID, &msg).is_none());
    }

    #[test]
    fn truncated_and_junk_input_never_panics() {
        let full = rfc_response();
        for len in 0..full.len() {
            assert!(parse_binding_response(&RFC_TXID, &full[..len]).is_none());
        }
        for len in 0..40 {
            assert!(parse_binding_response(&RFC_TXID, &vec![0xffu8; len]).is_none());
        }
    }

    #[test]
    fn an_attribute_length_past_the_message_is_rejected() {
        let mut msg = rfc_response();
        // Claim the attribute is far longer than the bytes present.
        msg[22..24].copy_from_slice(&0xffffu16.to_be_bytes());
        assert!(parse_binding_response(&RFC_TXID, &msg).is_none());
    }

    #[test]
    fn a_request_round_trips_its_header_fields() {
        let txid = [0x11u8; TXID_BYTES];
        let req = binding_request(&txid);
        assert_eq!(u16::from_be_bytes([req[0], req[1]]), BINDING_REQUEST);
        assert_eq!(u16::from_be_bytes([req[2], req[3]]), 0);
        assert_eq!(
            u32::from_be_bytes([req[4], req[5], req[6], req[7]]),
            MAGIC_COOKIE
        );
        assert_eq!(&req[8..], &txid);
    }
}
