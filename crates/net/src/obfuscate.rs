//! Obfuscated datagram codec for a serverless QUIC transport (task 17, Fase 2;
//! ADR 0051).
//!
//! Every UDP datagram of a session is wrapped in an `XChaCha20-Poly1305`
//! envelope keyed from the invite's shared secret, so on the wire it is uniform
//! random noise on a non-standard port: no QUIC Initial, no SNI, no ALPN, no
//! fixed header — nothing for signature DPI to key on. The key material is the
//! `invite_id` the two peers already exchange by hand, and the ticket that
//! carries it is signed by the host (`crate::ticket`), so the material is
//! authenticated before a byte is sealed with it.
//!
//! This module is the *codec* — the wire format and its keys. Wrapping a live
//! QUIC endpoint's socket with it is the endpoint-integration follow-up
//! (ADR 0051, §5): iroh 1.0.2 owns its UDP socket internally and exposes no
//! hook to wrap it, so there is no endpoint in this crate to attach the codec
//! to yet. Keeping the codec free of any endpoint type is what lets that
//! follow-up reuse it unchanged, whether it wires in through an iroh custom
//! transport or a forked IP transport.
//!
//! Obfuscation hides the traffic from an observer; it does not decide who is
//! allowed in. Consent and grants still live in `lumepeer-core`, and nothing
//! here widens a grant (task 17 trap; §2.3).

use chacha20poly1305::aead::{Aead as _, KeyInit as _};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use lumepeer_core::constants::OBFUSCATE_PADDING_MAX_BYTES;
use rand::Rng as _;
use rand::RngExt as _;

use crate::error::{NetError, Result};
use crate::ticket::INVITE_ID_BYTES;

/// Bytes of the `XChaCha20-Poly1305` nonce that prefixes every datagram.
const NONCE_BYTES: usize = 24;
/// Bytes of the Poly1305 authentication tag the AEAD appends.
const TAG_BYTES: usize = 16;
/// Bytes of the little-endian payload-length header sealed inside the envelope.
const LEN_HEADER_BYTES: usize = 2;

/// KDF context for the host-to-guest datagram key. Distinct per direction, so a
/// datagram captured in one direction can never be opened as the other and the
/// two directions fail independently.
const KDF_CONTEXT_HOST_TO_GUEST: &str =
    "lumepeer 2026 task17 obfuscation host-to-guest datagram key";
/// KDF context for the guest-to-host datagram key.
const KDF_CONTEXT_GUEST_TO_HOST: &str =
    "lumepeer 2026 task17 obfuscation guest-to-host datagram key";

/// Which way a datagram travels, naming the two independent keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Sealed by the host, opened by the guest.
    HostToGuest,
    /// Sealed by the guest, opened by the host.
    GuestToHost,
}

impl Direction {
    const fn context(self) -> &'static str {
        match self {
            Self::HostToGuest => KDF_CONTEXT_HOST_TO_GUEST,
            Self::GuestToHost => KDF_CONTEXT_GUEST_TO_HOST,
        }
    }
}

/// Derives the datagram key for `direction` from the invite's shared secret.
///
/// Deterministic in `invite_id` and `direction`, so both peers derive the same
/// key for the same direction, and the two directions never collide. It is one
/// blake3 KDF call, the same primitive the file keystore uses to bind a key to
/// a context string (§11.2).
#[must_use]
pub fn derive_datagram_key(invite_id: &[u8; INVITE_ID_BYTES], direction: Direction) -> Key {
    Key::from(blake3::derive_key(direction.context(), invite_id))
}

/// The pair of keys one peer holds: one to seal what it sends, one to open what
/// it receives. Host and guest hold mirror pairs, so each peer's send key is
/// the other's receive key.
#[derive(Clone)]
pub struct Obfuscator {
    seal_key: Key,
    open_key: Key,
}

impl std::fmt::Debug for Obfuscator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Key bytes never reach a log line.
        f.debug_struct("Obfuscator").finish_non_exhaustive()
    }
}

impl Obfuscator {
    /// Keys for the host: seals host-to-guest, opens guest-to-host.
    #[must_use]
    pub fn for_host(invite_id: &[u8; INVITE_ID_BYTES]) -> Self {
        Self {
            seal_key: derive_datagram_key(invite_id, Direction::HostToGuest),
            open_key: derive_datagram_key(invite_id, Direction::GuestToHost),
        }
    }

    /// Keys for the guest: seals guest-to-host, opens host-to-guest.
    #[must_use]
    pub fn for_guest(invite_id: &[u8; INVITE_ID_BYTES]) -> Self {
        Self {
            seal_key: derive_datagram_key(invite_id, Direction::GuestToHost),
            open_key: derive_datagram_key(invite_id, Direction::HostToGuest),
        }
    }

    /// Wraps one outgoing datagram as `nonce || AEAD(len || payload || pad)`.
    ///
    /// The nonce is fresh random per call, so two seals of the same payload
    /// differ on the wire. The padding is a fresh random length in
    /// `0..=OBFUSCATE_PADDING_MAX_BYTES`, sealed inside the envelope where it is
    /// indistinguishable from ciphertext, so it only blurs the datagram length
    /// and adds no header of its own.
    ///
    /// # Errors
    /// [`NetError::Obfuscation`] if `payload` is larger than a `u16` length
    /// header can describe, or in the practically-unreachable case that the
    /// AEAD refuses an in-memory buffer. It is returned rather than unwrapped so
    /// this path can never panic.
    pub fn seal(&self, payload: &[u8]) -> Result<Vec<u8>> {
        // A QUIC datagram is far under u16::MAX; refuse anything that would not
        // fit the length header rather than truncating it silently.
        let payload_len = u16::try_from(payload.len()).map_err(|_| NetError::Obfuscation)?;

        let mut rng = rand::rng();
        let mut nonce = [0u8; NONCE_BYTES];
        rng.fill_bytes(&mut nonce);

        let pad_len = rng.random_range(0..=OBFUSCATE_PADDING_MAX_BYTES);
        let mut padding = vec![0u8; pad_len];
        rng.fill_bytes(&mut padding);

        let mut frame = Vec::with_capacity(LEN_HEADER_BYTES + payload.len() + pad_len);
        frame.extend_from_slice(&payload_len.to_le_bytes());
        frame.extend_from_slice(payload);
        frame.extend_from_slice(&padding);

        let nonce_array: XNonce = nonce.into();
        let ciphertext = XChaCha20Poly1305::new(&self.seal_key)
            .encrypt(&nonce_array, frame.as_slice())
            .map_err(|_| NetError::Obfuscation)?;

        let mut wire = Vec::with_capacity(NONCE_BYTES + ciphertext.len());
        wire.extend_from_slice(&nonce);
        wire.extend_from_slice(&ciphertext);
        Ok(wire)
    }

    /// Recovers the payload from one incoming datagram, or rejects it.
    ///
    /// Every failure — too short, bad tag (wrong key or tampered), or a length
    /// header that does not fit its own frame — returns the same opaque
    /// [`NetError::Obfuscation`]. The input is untrusted: this never panics,
    /// never indexes unchecked, and gives an observer no way to tell the
    /// failures apart. The caller drops a rejected datagram and keeps the
    /// connection (task 17 trap; §2.4).
    ///
    /// # Errors
    /// [`NetError::Obfuscation`] on any malformed or unauthenticated datagram.
    pub fn open(&self, wire: &[u8]) -> Result<Vec<u8>> {
        if wire.len() < NONCE_BYTES + TAG_BYTES {
            return Err(NetError::Obfuscation);
        }
        let (nonce, ciphertext) = wire.split_at(NONCE_BYTES);
        let nonce = XNonce::try_from(nonce).map_err(|_| NetError::Obfuscation)?;
        let frame = XChaCha20Poly1305::new(&self.open_key)
            .decrypt(&nonce, ciphertext)
            .map_err(|_| NetError::Obfuscation)?;

        let len_bytes: [u8; LEN_HEADER_BYTES] = frame
            .get(..LEN_HEADER_BYTES)
            .and_then(|s| s.try_into().ok())
            .ok_or(NetError::Obfuscation)?;
        let payload_len = usize::from(u16::from_le_bytes(len_bytes));
        frame
            .get(LEN_HEADER_BYTES..LEN_HEADER_BYTES + payload_len)
            .map(<[u8]>::to_vec)
            .ok_or(NetError::Obfuscation)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::net::UdpSocket;
    use std::time::Duration;

    use super::*;

    const INVITE: [u8; INVITE_ID_BYTES] = [0x5a; INVITE_ID_BYTES];

    /// Fase 4: key derivation is deterministic in the invite id and differs by
    /// direction.
    #[test]
    fn key_derivation_is_deterministic_and_direction_specific() {
        let h2g = derive_datagram_key(&INVITE, Direction::HostToGuest);
        let g2h = derive_datagram_key(&INVITE, Direction::GuestToHost);
        // Same inputs, same key.
        assert_eq!(h2g, derive_datagram_key(&INVITE, Direction::HostToGuest));
        // Different direction, different key.
        assert_ne!(h2g, g2h);
        // Different invite, different key.
        let other = [0xa5; INVITE_ID_BYTES];
        assert_ne!(h2g, derive_datagram_key(&other, Direction::HostToGuest));
    }

    /// Fase 4: seal -> open recovers the exact bytes across a mirror key pair,
    /// in both directions.
    #[test]
    fn round_trip_recovers_the_payload_both_ways() {
        let host = Obfuscator::for_host(&INVITE);
        let guest = Obfuscator::for_guest(&INVITE);

        let up = b"host to guest control frame";
        assert_eq!(guest.open(&host.seal(up).unwrap()).unwrap(), up);

        let down = b"guest to host reply";
        assert_eq!(host.open(&guest.seal(down).unwrap()).unwrap(), down);

        // Empty and MTU-sized payloads survive too.
        assert_eq!(guest.open(&host.seal(b"").unwrap()).unwrap(), b"");
        let big = vec![0x42u8; 1200];
        assert_eq!(guest.open(&host.seal(&big).unwrap()).unwrap(), big);
    }

    /// Fase 4: the nonce works — two seals of one payload differ on the wire.
    #[test]
    fn two_seals_of_one_payload_differ_on_the_wire() {
        let host = Obfuscator::for_host(&INVITE);
        let first = host.seal(b"same bytes").unwrap();
        let second = host.seal(b"same bytes").unwrap();
        assert_ne!(first, second);
    }

    /// Fase 4 (negative): a datagram under the wrong key is rejected, silently
    /// and without panic. A peer holding a different invite is the wrong key.
    #[test]
    fn wrong_key_is_rejected_without_panic() {
        let host = Obfuscator::for_host(&INVITE);
        let stranger = Obfuscator::for_host(&[0x11; INVITE_ID_BYTES]);
        let sealed = host.seal(b"secret").unwrap();
        assert!(matches!(stranger.open(&sealed), Err(NetError::Obfuscation)));
    }

    /// A peer cannot open a datagram it sealed itself: the send and receive keys
    /// are the two different directional keys, so its own traffic is not in its
    /// open direction.
    #[test]
    fn a_peer_does_not_open_its_own_direction() {
        let host = Obfuscator::for_host(&INVITE);
        let sealed = host.seal(b"upstream").unwrap();
        assert!(matches!(host.open(&sealed), Err(NetError::Obfuscation)));
    }

    /// Tampering with any byte fails authentication.
    #[test]
    fn a_tampered_datagram_is_rejected() {
        let host = Obfuscator::for_host(&INVITE);
        let guest = Obfuscator::for_guest(&INVITE);
        let mut sealed = host.seal(b"authentic").unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0x01;
        assert!(matches!(guest.open(&sealed), Err(NetError::Obfuscation)));
    }

    /// Untrusted junk of any length is rejected, never panics — including
    /// inputs shorter than the nonce-plus-tag floor.
    #[test]
    fn short_and_random_input_never_panics() {
        let guest = Obfuscator::for_guest(&INVITE);
        for len in 0..64 {
            let junk = vec![0x7fu8; len];
            assert!(matches!(guest.open(&junk), Err(NetError::Obfuscation)));
        }
    }

    /// Fase 4 (integration): two peers exchange a sealed message in both
    /// directions over a real UDP socket pair, proving the wire format survives
    /// an actual send/recv with the mirror key setup. (The QUIC endpoint that
    /// would sit above this socket is the endpoint-integration follow-up,
    /// ADR 0051 §5; this exercises the datagram layer end to end without it.)
    #[test]
    fn obfuscated_datagrams_cross_a_real_udp_pair() {
        let host_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let guest_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        host_sock
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        guest_sock
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let host_addr = host_sock.local_addr().unwrap();
        let guest_addr = guest_sock.local_addr().unwrap();

        let host = Obfuscator::for_host(&INVITE);
        let guest = Obfuscator::for_guest(&INVITE);

        // host -> guest
        host_sock
            .send_to(&host.seal(b"ping").unwrap(), guest_addr)
            .unwrap();
        let mut buf = [0u8; 2048];
        let (n, from) = guest_sock.recv_from(&mut buf).unwrap();
        assert_eq!(from, host_addr);
        assert_eq!(guest.open(&buf[..n]).unwrap(), b"ping");

        // guest -> host
        guest_sock
            .send_to(&guest.seal(b"pong").unwrap(), host_addr)
            .unwrap();
        let (n, _) = host_sock.recv_from(&mut buf).unwrap();
        assert_eq!(host.open(&buf[..n]).unwrap(), b"pong");
    }
}
