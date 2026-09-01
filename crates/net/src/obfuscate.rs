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

use std::io::{self, IoSliceMut};
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use chacha20poly1305::aead::{Aead as _, KeyInit as _};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use lumepeer_core::constants::{
    OBFUSCATE_PADDING_MAX_BYTES, QUIC_KEEPALIVE_SECS, QUIC_MAX_IDLE_TIMEOUT_SECS,
};
use noq::udp::{RecvMeta, Transmit};
use noq::{AsyncUdpSocket, IdleTimeout, TransportConfig, UdpSender, VarInt};
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

/// Byte capacity of the scratch buffer [`ObfuscatedSocket::poll_recv`] reads
/// one still-sealed wire datagram into before opening it (task 17
/// increment 1, ADR 0052).
///
/// Headroom above a typical path MTU (~1500 bytes) plus the codec's own
/// envelope overhead — nonce, AEAD tag, length header and up to
/// `OBFUSCATE_PADDING_MAX_BYTES` of padding — so an ordinary QUIC datagram
/// always arrives whole; `stun.rs` sizes its own read buffer the same way
/// (1500, with no obfuscation envelope to add).
const OBFUSCATED_DATAGRAM_MAX_BYTES: usize = 2048;

/// Splits one received buffer into its individual datagrams.
///
/// A single [`RecvMeta`] can describe several datagrams coalesced by GRO,
/// laid out back to back in `stride`-sized slices with the last one possibly
/// shorter (§5.3 of the task). `len` and `stride` are untrusted metadata from
/// the inner socket, so both are bounds-checked rather than trusted: an
/// out-of-range `len` or a zero `stride` yields no datagrams instead of
/// panicking.
fn split_by_stride(raw: &[u8], len: usize, stride: usize) -> Vec<&[u8]> {
    let Some(bounded) = raw.get(..len) else {
        return Vec::new();
    };
    if stride == 0 {
        return Vec::new();
    }
    bounded.chunks(stride).collect()
}

/// A `noq::AsyncUdpSocket` that seals every outgoing datagram and opens every
/// incoming one with an [`Obfuscator`] (task 17 increment 1, ADR 0052).
///
/// Wraps whatever socket the caller's `noq::Runtime` produced — real UDP in
/// production, whatever a test substitutes — so a `noq::Endpoint` built on
/// this type sees ordinary QUIC datagrams while the wire underneath carries
/// only the codec's uniform ciphertext. GSO/GRO are turned off on both this
/// socket and the senders it creates (`max_receive_segments`/
/// `max_transmit_segments` both return `1`; §10 of the task): obfuscation's
/// random padding makes every sealed datagram a different length, which
/// segmented sends/receives assume never happens.
pub struct ObfuscatedSocket {
    inner: Box<dyn AsyncUdpSocket>,
    obfuscator: Obfuscator,
}

impl ObfuscatedSocket {
    /// Wraps `inner`, sealing what it sends and opening what it receives with
    /// `obfuscator`.
    #[must_use]
    pub fn new(inner: Box<dyn AsyncUdpSocket>, obfuscator: Obfuscator) -> Self {
        Self { inner, obfuscator }
    }
}

impl std::fmt::Debug for ObfuscatedSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Key bytes never reach a log line (as `Obfuscator`'s own `Debug`).
        f.debug_struct("ObfuscatedSocket").finish_non_exhaustive()
    }
}

impl AsyncUdpSocket for ObfuscatedSocket {
    fn create_sender(&self) -> Pin<Box<dyn UdpSender>> {
        Box::pin(ObfuscatedSender {
            inner: self.inner.create_sender(),
            obfuscator: self.obfuscator.clone(),
        })
    }

    fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        loop {
            // Scratch buffers for the still-sealed wire datagrams: same count
            // as the caller's `bufs`, so one poll can never accept more raw
            // datagrams than there is room for once opened.
            let mut wire: Vec<Vec<u8>> = (0..bufs.len())
                .map(|_| vec![0u8; OBFUSCATED_DATAGRAM_MAX_BYTES])
                .collect();
            let mut wire_bufs: Vec<IoSliceMut<'_>> =
                wire.iter_mut().map(|b| IoSliceMut::new(b)).collect();
            let mut wire_meta = vec![RecvMeta::default(); bufs.len()];

            let received = match self.inner.poll_recv(cx, &mut wire_bufs, &mut wire_meta) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Ready(Ok(n)) => n,
            };
            drop(wire_bufs);

            let mut out = 0usize;
            for (raw, raw_meta) in wire.iter().zip(wire_meta.iter()).take(received) {
                for datagram in split_by_stride(raw, raw_meta.len, raw_meta.stride) {
                    let Ok(plain) = self.obfuscator.open(datagram) else {
                        // Forged, tampered, or sealed under a different key:
                        // untrusted input, dropped silently and the
                        // connection stays up (task 17 trap; §2.4).
                        continue;
                    };
                    let Some(dest_meta) = meta.get_mut(out) else {
                        // The caller's buffers are full; leave the rest for
                        // the next poll rather than overrunning them.
                        return Poll::Ready(Ok(out));
                    };
                    let Some(dest_buf) = bufs.get_mut(out).and_then(|b| b.get_mut(..plain.len()))
                    else {
                        // Would not fit the caller's buffer; drop rather than
                        // truncate it silently.
                        continue;
                    };
                    dest_buf.copy_from_slice(&plain);
                    *dest_meta = RecvMeta::default();
                    dest_meta.addr = raw_meta.addr;
                    dest_meta.len = plain.len();
                    dest_meta.stride = plain.len();
                    dest_meta.dst_ip = raw_meta.dst_ip;
                    out += 1;
                }
            }

            if out > 0 {
                return Poll::Ready(Ok(out));
            }
            // Everything this poll produced was noise (or the batch held no
            // datagrams at all): poll the inner socket again rather than
            // reporting a false `Ready(0)`.
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn max_receive_segments(&self) -> NonZeroUsize {
        NonZeroUsize::MIN
    }

    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
}

/// The [`UdpSender`] half of [`ObfuscatedSocket`], sealing what it sends.
struct ObfuscatedSender {
    inner: Pin<Box<dyn UdpSender>>,
    obfuscator: Obfuscator,
}

impl std::fmt::Debug for ObfuscatedSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Key bytes never reach a log line (as `Obfuscator`'s own `Debug`).
        f.debug_struct("ObfuscatedSender").finish_non_exhaustive()
    }
}

impl UdpSender for ObfuscatedSender {
    fn poll_send(
        self: Pin<&mut Self>,
        transmit: &Transmit<'_>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        // Neither field needs structural pinning: the inner sender is
        // already independently pinned in its own `Box`, and `Obfuscator` has
        // nothing self-referential either, so `Self` is `Unpin`.
        let this = self.get_mut();
        let sealed = match this.obfuscator.seal(transmit.contents) {
            Ok(sealed) => sealed,
            Err(e) => return Poll::Ready(Err(io::Error::new(io::ErrorKind::InvalidInput, e))),
        };
        // GSO is off (`max_transmit_segments` below), so `transmit` is always
        // exactly one datagram; the padding inside `sealed` changes its
        // length, which is exactly why segmenting is not safe here (§10).
        let out = Transmit {
            destination: transmit.destination,
            ecn: None,
            contents: &sealed,
            segment_size: None,
            src_ip: transmit.src_ip,
        };
        this.inner.as_mut().poll_send(&out, cx)
    }

    fn max_transmit_segments(&self) -> NonZeroUsize {
        NonZeroUsize::MIN
    }
}

/// Builds the `noq` transport configuration every obfuscated QUIC connection
/// on this transport must use (task 17 increment 1, ADR 0052).
///
/// Sets the mandatory keep-alive and its matching idle timeout from
/// [`QUIC_KEEPALIVE_SECS`]/[`QUIC_MAX_IDLE_TIMEOUT_SECS`]. Omitting the
/// keep-alive is the "false ~30 s disconnect" trap (§10): QUIC's own idle
/// timeout silently drops an otherwise healthy path, which was once mistaken
/// for a DPI drop (`project-lumepeer-quic-vs-relay-transport`).
#[must_use]
pub fn obfuscated_transport_config() -> TransportConfig {
    let mut config = TransportConfig::default();
    config.keep_alive_interval(Some(Duration::from_secs(QUIC_KEEPALIVE_SECS)));
    // `IdleTimeout::try_from(Duration)` is fallible only because a `VarInt`
    // tops out at 2^62-1 ms; `QUIC_MAX_IDLE_TIMEOUT_SECS` is nowhere close, so
    // building it from a millisecond `VarInt` directly (rather than
    // `unwrap`/`expect` on that impossible-in-practice error) keeps this
    // function panic-free without asserting something that cannot happen.
    let idle_timeout_ms =
        u32::try_from(QUIC_MAX_IDLE_TIMEOUT_SECS.saturating_mul(1000)).unwrap_or(u32::MAX);
    config.max_idle_timeout(Some(IdleTimeout::from(VarInt::from_u32(idle_timeout_ms))));
    config
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::future::poll_fn;
    use std::net::UdpSocket;
    use std::sync::Arc;
    use std::time::Duration;

    use noq::rustls::RootCertStore;
    use noq::rustls::pki_types::PrivateKeyDer;
    use noq::{ClientConfig, Endpoint, EndpointConfig, Runtime, ServerConfig, TokioRuntime};

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

    async fn send_datagram(
        socket: &ObfuscatedSocket,
        destination: SocketAddr,
        payload: &[u8],
    ) -> io::Result<()> {
        let mut sender = socket.create_sender();
        let transmit = Transmit {
            destination,
            ecn: None,
            contents: payload,
            segment_size: None,
            src_ip: None,
        };
        poll_fn(|cx| sender.as_mut().poll_send(&transmit, cx)).await
    }

    async fn recv_datagram(socket: &mut ObfuscatedSocket) -> io::Result<(SocketAddr, Vec<u8>)> {
        let mut storage = [0u8; 2048];
        let mut bufs = [IoSliceMut::new(&mut storage)];
        let mut meta = [RecvMeta::default()];
        poll_fn(|cx| socket.poll_recv(cx, &mut bufs, &mut meta)).await?;
        Ok((meta[0].addr, bufs[0][..meta[0].len].to_vec()))
    }

    /// Fase 4 (task 17 increment 1, §7.1, mandatory): `ObfuscatedSocket`
    /// exercised through the real `noq::AsyncUdpSocket`/`UdpSender` trait
    /// methods (not the codec directly, unlike
    /// `obfuscated_datagrams_cross_a_real_udp_pair` above) — a bare socket
    /// standing in for an observer sees ciphertext, not the plaintext, and
    /// two wrapped sockets recover the exact payload in both directions.
    #[tokio::test]
    async fn obfuscated_socket_hides_the_wire_and_round_trips_both_ways() {
        let runtime: Arc<dyn Runtime> = Arc::new(TokioRuntime);

        let mut host = ObfuscatedSocket::new(
            runtime
                .wrap_udp_socket(UdpSocket::bind("127.0.0.1:0").unwrap())
                .unwrap(),
            Obfuscator::for_host(&INVITE),
        );
        let mut guest = ObfuscatedSocket::new(
            runtime
                .wrap_udp_socket(UdpSocket::bind("127.0.0.1:0").unwrap())
                .unwrap(),
            Obfuscator::for_guest(&INVITE),
        );
        let host_addr = host.local_addr().unwrap();
        let guest_addr = guest.local_addr().unwrap();

        // A bare socket standing where `guest` would be sees the wire bytes,
        // not the plaintext: this is obfuscation, not just relaying.
        let sniffer = UdpSocket::bind("127.0.0.1:0").unwrap();
        sniffer
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let sniffer_addr = sniffer.local_addr().unwrap();
        send_datagram(&host, sniffer_addr, b"sniff me")
            .await
            .unwrap();
        let mut wire_buf = [0u8; 2048];
        let (n, _) = sniffer.recv_from(&mut wire_buf).unwrap();
        assert_ne!(&wire_buf[..n], b"sniff me");
        assert!(n > b"sniff me".len(), "envelope must add nonce/tag/padding");

        // host -> guest
        send_datagram(&host, guest_addr, b"ping").await.unwrap();
        let (from, plain) = recv_datagram(&mut guest).await.unwrap();
        assert_eq!(from, host_addr);
        assert_eq!(plain, b"ping");

        // guest -> host
        send_datagram(&guest, host_addr, b"pong").await.unwrap();
        let (from, plain) = recv_datagram(&mut host).await.unwrap();
        assert_eq!(from, guest_addr);
        assert_eq!(plain, b"pong");
    }

    /// Fase 4 (task 17 increment 1, §7.2; the readiness criterion of §3
    /// requires this one specifically): two real `noq::Endpoint`s, each built
    /// on an `ObfuscatedSocket` via `new_with_abstract_socket`, complete a
    /// QUIC handshake and exchange a message on a bidirectional stream in
    /// both directions — proving actual QUIC works end to end over the
    /// obfuscated transport, not just the socket wrapper alone.
    #[tokio::test]
    async fn quic_over_the_obfuscated_socket_exchanges_a_message_both_ways() {
        let runtime: Arc<dyn Runtime> = Arc::new(TokioRuntime);

        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let key = PrivateKeyDer::Pkcs8(cert.signing_key.serialize_der().into());
        let mut server_config =
            ServerConfig::with_single_cert(vec![cert.cert.der().clone()], key).unwrap();
        server_config.transport_config(Arc::new(obfuscated_transport_config()));

        let mut roots = RootCertStore::empty();
        roots.add(cert.cert.der().clone()).unwrap();
        let mut client_config = ClientConfig::with_root_certificates(Arc::new(roots)).unwrap();
        client_config.transport_config(Arc::new(obfuscated_transport_config()));

        let host_socket: Box<dyn AsyncUdpSocket> = Box::new(ObfuscatedSocket::new(
            runtime
                .wrap_udp_socket(UdpSocket::bind("127.0.0.1:0").unwrap())
                .unwrap(),
            Obfuscator::for_host(&INVITE),
        ));
        let guest_socket: Box<dyn AsyncUdpSocket> = Box::new(ObfuscatedSocket::new(
            runtime
                .wrap_udp_socket(UdpSocket::bind("127.0.0.1:0").unwrap())
                .unwrap(),
            Obfuscator::for_guest(&INVITE),
        ));

        let host = Endpoint::new_with_abstract_socket(
            EndpointConfig::default(),
            Some(server_config),
            host_socket,
            runtime.clone(),
        )
        .unwrap();
        let guest = Endpoint::new_with_abstract_socket(
            EndpointConfig::default(),
            None,
            guest_socket,
            runtime,
        )
        .unwrap();
        guest.set_default_client_config(client_config);

        let host_addr = host.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let connection = host.accept().await.unwrap().await.unwrap();
            let (mut send, mut recv) = connection.accept_bi().await.unwrap();
            let request = recv.read_to_end(1024).await.unwrap();
            assert_eq!(request, b"ping from guest");
            send.write_all(b"pong from host").await.unwrap();
            send.finish().unwrap();
            // Keep the connection alive until the client is done with it and
            // closes it; dropping the handle here would trigger an implicit
            // close(0, "") that could race the client's read of the reply
            // just written.
            connection.closed().await;
        });

        let connection = guest
            .connect(host_addr, "localhost")
            .unwrap()
            .await
            .unwrap();
        let (mut send, mut recv) = connection.open_bi().await.unwrap();
        send.write_all(b"ping from guest").await.unwrap();
        send.finish().unwrap();
        let reply = recv.read_to_end(1024).await.unwrap();
        assert_eq!(reply, b"pong from host");
        connection.close(0u32.into(), b"done");

        server.await.unwrap();
    }
}
