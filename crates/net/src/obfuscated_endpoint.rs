//! Binding the persistent obfuscated QUIC endpoint on each side (task 17
//! increment 2, ADR 0053): the host discovers its public address via STUN
//! and keeps its NAT mapping open until a guest dials in; the guest dials
//! that address with TLS pinned to the host's cert fingerprint rather than a
//! CA. Both build on increment 1's `ObfuscatedSocket`/`Obfuscator`
//! (`crate::obfuscate`) unchanged.
//!
//! Increment 3 (gap-tasks/21; ADR 0080) gives the host endpoint the explicit
//! shutdown it lacked, so its lifetime can be tied to the invite it was bound
//! for rather than to the process, teaches both sides the four ALPNs of §4.1,
//! and binds each side's certificate to its own ed25519 endpoint identity so a
//! connection here carries the same `NodeId` the iroh path would have. It is
//! exercised by `examples/obfuscated_wan_probe.rs` and by this module's own
//! tests.
//!
//! gap-tasks/22 (ADR 0082) settles what a hole punch can be here. The host's
//! half stays the keep-alive: a one-way invite has no channel on which to
//! coordinate a simultaneous send, and the one it could borrow — a live iroh
//! connection — is exactly what is missing on the networks that would need
//! the punch. The guest's half is [`punch`]: the dial itself, on a bounded
//! cadence, so its packets are ordinary sealed datagrams and a punch that
//! cannot land fails in seconds instead of minutes.
//!
//! ADR 0113 gives the punch its missing half through `crate::rendezvous`: the
//! host publishes where its endpoint is *now* and polls for knocks, and a
//! guest that knocks gets packets sent towards it, which is what opens a NAT
//! that filters by sender. The guest also stops taking the ticket's address as
//! the only truth: a host that restarted or moved uplinks is somewhere else,
//! and its record says where. The pinned certificate needs no such help — it
//! is the same on every bind (see [`identity_certificate`]).
//!
//! ADR 0116 makes that exchange immediate: the knock and the host's answer
//! also travel over public Nostr relays ([`crate::rendezvous::Signals`]), which
//! push, so a host punches back within a second of the knock instead of at
//! its next DHT poll.

use std::net::{SocketAddr, ToSocketAddrs as _, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ed25519_dalek::SigningKey;
use lumepeer_core::NodeId;
use lumepeer_core::constants::{
    NAT_MAPPING_KEEPALIVE_SECS, OBFUSCATED_CONNECT_ATTEMPTS, OBFUSCATED_CONNECT_RETRY_BACKOFF_MS,
    OBFUSCATED_PUNCH_ATTEMPT_TIMEOUT_MS, OBFUSCATED_SILENCE_REKNOCK_MS,
    RENDEZVOUS_KNOCK_FRESH_SECS, RENDEZVOUS_POLL_SECS, RENDEZVOUS_PUNCH_INTERVAL_MS,
    RENDEZVOUS_PUNCH_PACKETS, RENDEZVOUS_PUNCHES_PER_MINUTE, RENDEZVOUS_REPUBLISH_SECS,
    RENDEZVOUS_REPUNCH_SECS, STUN_QUERY_TIMEOUT_MS,
};
use noq::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use noq::rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime,
};
use noq::rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use noq::rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use noq::{
    AsyncUdpSocket, ClientConfig, Connection, Endpoint, EndpointConfig, ServerConfig, TokioRuntime,
};
use rand::{Rng as _, RngExt as _};

use crate::endpoint::SUPPORTED_ALPNS;
use crate::error::{NetError, Result};
use crate::obfuscate::{
    HostRoute, ObfuscatedSocket, Obfuscator, StunTap, obfuscated_transport_config,
};
use crate::peer_connection::PeerConnection;
use crate::rendezvous::{Rendezvous, Sighting, Signals, Speaker};
use crate::stun;
use crate::ticket::INVITE_ID_BYTES;

/// Public STUN reflectors tried in order, same list `examples/stun_probe.rs`
/// uses (task 17, ADR 0052/0053).
///
/// Public so a probe can measure this machine's NAT through the very
/// reflectors the transport discovers with: an answer from a different list
/// would say nothing about the address this endpoint advertises
/// (gap-tasks/22 task 2).
///
/// The list spreads across independent operators and across ports on purpose
/// (ADR 0111): discovery returns on the first reflector that answers, so a
/// reflector an ISP has frozen only costs the next one a try, and a network
/// that filters STUN's usual UDP/3478 can still be discovered through an
/// operator that answers on 443. Only when *every* reflector is unreachable
/// does the host issue a ticket with no obfuscated address and its guests fall
/// back to iroh — an ordinary outcome, not a failure.
pub const STUN_SERVERS: &[&str] = &[
    "stun.cloudflare.com:3478",
    "stun.l.google.com:19302",
    "stun1.l.google.com:19302",
    "stun.nextcloud.com:443",
    "stun.sipgate.net:3478",
];

/// Subject name both sides put in their certificate and dial by.
///
/// It authenticates nothing and is not meant to: rustls insists on a name for
/// the handshake, and what actually decides who is on the other end is the key
/// inside the certificate (`peer_id_from_certificate`) plus, on the guest's
/// side, the fingerprint the ticket pinned (ADR 0053).
const CERT_SUBJECT: &str = "localhost";

/// DER tag for a constructed SEQUENCE.
const DER_SEQUENCE: u8 = 0x30;
/// DER tag for a BIT STRING.
const DER_BIT_STRING: u8 = 0x03;
/// DER tag of the optional `[0] EXPLICIT Version` that opens a v2/v3
/// `TBSCertificate` (RFC 5280 §4.1).
const DER_CONTEXT_0: u8 = 0xa0;
/// Fields of a `TBSCertificate` between the version and the public key:
/// `serialNumber`, `signature`, `issuer`, `validity`, `subject` (RFC 5280
/// §4.1).
const TBS_FIELDS_BEFORE_PUBLIC_KEY: usize = 5;
/// The DER `AlgorithmIdentifier` contents of an Ed25519 `SubjectPublicKeyInfo`:
/// OID 1.3.101.112 and, as RFC 8410 §3 requires, no parameters at all.
const ED25519_ALGORITHM: &[u8] = &[0x06, 0x03, 0x2b, 0x65, 0x70];
/// Bytes of an ed25519 public or private key.
const ED25519_KEY_BYTES: usize = 32;
/// The fixed PKCS#8 v1 prefix of an Ed25519 private key (RFC 8410 §7): a
/// 48-byte SEQUENCE holding version 0, the Ed25519 algorithm identifier, and
/// the 32-byte seed inside a nested OCTET STRING.
const ED25519_PKCS8_PREFIX: &[u8] = &[
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];

/// One node's certificate for this transport, generated from its own ed25519
/// endpoint identity so the key inside it *is* the node's `NodeId`.
///
/// This is what lets a connection here mean the same thing a connection over
/// iroh means. iroh's own TLS binds a session to the endpoint key; `noq` has
/// no identity of its own, so without this the host would learn nothing about
/// who dialed it and every peer would be a stranger — and the session logic
/// above the transport, which is keyed by `NodeId` throughout, would have had
/// to grow a second notion of a peer (§2.3; ADR 0080).
struct IdentityCertificate {
    /// The certificate, DER-encoded.
    der: CertificateDer<'static>,
    /// Its private key, PKCS#8-encoded.
    key: PrivateKeyDer<'static>,
    /// Blake3 of `der`, which is what an invite ticket pins (ADR 0053).
    fingerprint: [u8; 32],
}

impl std::fmt::Debug for IdentityCertificate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The private key never reaches a log line.
        f.debug_struct("IdentityCertificate")
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

/// Generates a self-signed certificate whose key is `identity`.
///
/// The same certificate on every bind: `rcgen` derives the serial from the
/// key and uses fixed validity dates, and an ed25519 signature is
/// deterministic. So the fingerprint an invite pins still names the
/// certificate a host presents after it restarts, which is what lets a
/// restored invite be served again at all (ADR 0113).
///
/// # Errors
/// [`NetError::Endpoint`] if the key cannot be encoded or the certificate
/// cannot be signed.
fn identity_certificate(identity: &SigningKey) -> Result<IdentityCertificate> {
    let mut pkcs8 = Vec::with_capacity(ED25519_PKCS8_PREFIX.len() + ED25519_KEY_BYTES);
    pkcs8.extend_from_slice(ED25519_PKCS8_PREFIX);
    pkcs8.extend_from_slice(&identity.to_bytes());

    let key_pair = rcgen::KeyPair::from_pkcs8_der_and_sign_algo(
        &PrivatePkcs8KeyDer::from(pkcs8.as_slice()),
        &rcgen::PKCS_ED25519,
    )
    .map_err(|e| NetError::Endpoint(e.to_string()))?;
    let params = rcgen::CertificateParams::new(vec![CERT_SUBJECT.to_owned()])
        .map_err(|e| NetError::Endpoint(e.to_string()))?;
    let certificate = params
        .self_signed(&key_pair)
        .map_err(|e| NetError::Endpoint(e.to_string()))?;

    let der = certificate.der().clone();
    let fingerprint: [u8; 32] = *blake3::hash(&der).as_bytes();
    Ok(IdentityCertificate {
        der,
        key: PrivateKeyDer::Pkcs8(pkcs8.into()),
        fingerprint,
    })
}

/// Splits one DER element off the front of `der`: its tag, its contents, and
/// whatever follows it.
///
/// `None` for anything that is not a well-formed element. Every byte reaching
/// this is a certificate an unauthenticated peer chose, so nothing here
/// indexes unchecked, trusts a declared length, or panics (§2.4).
fn der_element(der: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let tag = *der.first()?;
    let first = *der.get(1)?;
    let (length, header) = if first < 0x80 {
        (usize::from(first), 2)
    } else {
        // Long form: the low seven bits count the length's own bytes. More
        // than four would describe a certificate no buffer here can hold, and
        // zero is the indefinite form, which DER forbids.
        let count = usize::from(first & 0x7f);
        if count == 0 || count > 4 {
            return None;
        }
        let mut length = 0usize;
        for byte in der.get(2..2usize.checked_add(count)?)? {
            length = length.checked_mul(256)?.checked_add(usize::from(*byte))?;
        }
        (length, 2usize.checked_add(count)?)
    };
    let end = header.checked_add(length)?;
    Some((tag, der.get(header..end)?, der.get(end..)?))
}

/// The ed25519 public key a peer's certificate carries, which on this
/// transport is its `NodeId`.
///
/// Walks the certificate to `subjectPublicKeyInfo` rather than searching it
/// for a byte pattern: a pattern search would let a peer put a second, decoy
/// key somewhere earlier in the encoding and be recognised as a node whose
/// private key it does not hold, while TLS validated the real one. The key
/// this returns is the one the handshake proved possession of because it is
/// the only public key the certificate has.
///
/// # Errors
/// [`NetError::Io`] for any certificate that is not a well-formed X.509
/// carrying exactly one ed25519 key — the same opaque answer for a truncated
/// encoding, an RSA key or a key that is not a point on the curve, so a peer
/// learns nothing about which it was.
fn peer_id_from_certificate(cert: &CertificateDer<'_>) -> Result<NodeId> {
    let malformed = || NetError::Io("peer certificate carries no ed25519 identity".to_owned());

    let (tag, certificate, _) = der_element(cert).ok_or_else(malformed)?;
    if tag != DER_SEQUENCE {
        return Err(malformed());
    }
    let (tag, tbs, _) = der_element(certificate).ok_or_else(malformed)?;
    if tag != DER_SEQUENCE {
        return Err(malformed());
    }

    // TBSCertificate ::= SEQUENCE { [0] version OPTIONAL, serialNumber,
    // signature, issuer, validity, subject, subjectPublicKeyInfo, ... }
    let (tag, _, after_version) = der_element(tbs).ok_or_else(malformed)?;
    let mut rest = if tag == DER_CONTEXT_0 {
        after_version
    } else {
        tbs
    };
    for _ in 0..TBS_FIELDS_BEFORE_PUBLIC_KEY {
        let (_, _, after) = der_element(rest).ok_or_else(malformed)?;
        rest = after;
    }

    // SubjectPublicKeyInfo ::= SEQUENCE { algorithm, subjectPublicKey }
    let (tag, spki, _) = der_element(rest).ok_or_else(malformed)?;
    if tag != DER_SEQUENCE {
        return Err(malformed());
    }
    let (tag, algorithm, after_algorithm) = der_element(spki).ok_or_else(malformed)?;
    if tag != DER_SEQUENCE || algorithm != ED25519_ALGORITHM {
        return Err(malformed());
    }
    let (tag, key_bits, _) = der_element(after_algorithm).ok_or_else(malformed)?;
    // A BIT STRING's first content byte counts the unused bits of its last
    // byte; a whole number of key bytes leaves none.
    if tag != DER_BIT_STRING || key_bits.first() != Some(&0) {
        return Err(malformed());
    }
    let key: [u8; ED25519_KEY_BYTES] = key_bits
        .get(1..)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(malformed)?;
    NodeId::from_bytes(&key).map_err(|_| malformed())
}

/// The peer identity and negotiated ALPN of an established `noq` connection.
///
/// Both come out of the TLS handshake, and both are required: a connection
/// whose peer presented no usable certificate has nobody on the other end that
/// the session logic could name, and one with no ALPN belongs to no channel
/// (§4.1).
///
/// # Errors
/// [`NetError::Io`] if either is missing or unusable.
fn peer_and_alpn(connection: &Connection) -> Result<(NodeId, Vec<u8>)> {
    let certificates = connection
        .peer_identity()
        .and_then(|identity| identity.downcast::<Vec<CertificateDer<'static>>>().ok())
        .ok_or_else(|| NetError::Io("peer presented no certificate".to_owned()))?;
    let end_entity = certificates
        .first()
        .ok_or_else(|| NetError::Io("peer presented an empty certificate chain".to_owned()))?;
    let peer = peer_id_from_certificate(end_entity)?;

    let alpn = connection
        .handshake_data()
        .and_then(|data| data.downcast::<noq::crypto::rustls::HandshakeData>().ok())
        .and_then(|data| data.protocol)
        .ok_or_else(|| NetError::Io("peer negotiated no ALPN".to_owned()))?;
    Ok((peer, alpn))
}

/// Every ALPN of §4.1, as `rustls` wants them.
fn alpn_protocols() -> Vec<Vec<u8>> {
    SUPPORTED_ALPNS
        .iter()
        .map(|alpn| (*alpn).to_vec())
        .collect()
}

/// A bound obfuscated QUIC endpoint on the host side, with whatever public
/// address STUN discovered (task 17 increment 2, ADR 0053).
pub struct HostObfuscatedEndpoint {
    /// The live `noq` endpoint, ready to accept connections.
    pub endpoint: Endpoint,
    /// Public reflexive address, if a STUN server answered. `None` on total
    /// STUN failure (no reflector reachable) or an unusable mapping (double
    /// NAT) — the caller falls back to the existing iroh path for this
    /// invite, same as if this transport did not exist.
    pub public_addr: Option<SocketAddr>,
    /// Blake3 fingerprint of `endpoint`'s self-signed cert. Meaningful to a
    /// caller only alongside a `Some(public_addr)` — a fingerprint for an
    /// endpoint nobody can dial is not worth carrying into a ticket.
    pub cert_fingerprint: [u8; 32],
    /// The NAT-mapping keep-alive of [`spawn_keepalive`], when STUN found a
    /// server to keep hitting. Owned here so the task dies with the endpoint
    /// instead of with the process (gap-tasks/21 task 1; ADR 0080).
    keepalive: Option<tokio::task::JoinHandle<()>>,
    /// The rendezvous of [`serve_rendezvous`] — publishing this endpoint's
    /// address and answering knocks — when the caller gave it a DHT and STUN
    /// found an address to publish (ADR 0113). Dies with the endpoint, like
    /// the keep-alive.
    rendezvous: Option<tokio::task::JoinHandle<()>>,
}

impl std::fmt::Debug for HostObfuscatedEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostObfuscatedEndpoint")
            .field("public_addr", &self.public_addr)
            .finish_non_exhaustive()
    }
}

impl HostObfuscatedEndpoint {
    /// Address this endpoint's socket is actually bound to, which is what a
    /// peer on the same machine dials — unlike [`Self::public_addr`], which is
    /// the reflexive address a peer on the far side of the NAT needs.
    ///
    /// # Errors
    /// [`NetError::Endpoint`] if the socket has no local address, which means
    /// it is already closed.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.endpoint
            .local_addr()
            .map_err(|e| NetError::Endpoint(e.to_string()))
    }

    /// Accepts the next connection on this endpoint, or `None` once it is
    /// closed.
    ///
    /// What comes back is the same [`PeerConnection`] the iroh path produces,
    /// carrying the peer's identity — the ed25519 key its client certificate
    /// proved — and the ALPN its handshake negotiated, so the channel it
    /// belongs to is read with the same [`crate::Channel::from_alpn`] and the
    /// caller never learns which transport it was (§4.1; ADR 0080).
    ///
    /// # Errors
    /// [`NetError::Io`] if the QUIC handshake fails, or the peer presented no
    /// usable identity or no ALPN. The caller keeps accepting afterwards: one
    /// refused connection is not a reason to stop serving the invite.
    pub async fn accept(&self) -> Option<Result<PeerConnection>> {
        let acceptor = self.acceptor();
        acceptor.accept().await
    }

    /// The accept half of this endpoint on its own, so an accept loop can run
    /// off the thread that owns the endpoint.
    ///
    /// The actor owns the endpoint but must never block in a loop (ADR 0027),
    /// and closing the endpoint ends every acceptor taken from it: `accept`
    /// then answers `None` and the loop falls out, which is what ties the loop
    /// to the invite as tightly as the endpoint itself.
    #[must_use]
    pub fn acceptor(&self) -> ObfuscatedAcceptor {
        ObfuscatedAcceptor {
            endpoint: self.endpoint.clone(),
        }
    }

    /// Closes the endpoint: the keep-alive stops and the socket is released.
    ///
    /// Explicit rather than left to the process, because the endpoint's
    /// lifetime is the invite's (ADR 0062, ADR 0080): a replaced invite's
    /// endpoint has to stop holding a NAT mapping open — and stop sending a
    /// STUN request every [`NAT_MAPPING_KEEPALIVE_SECS`] — the moment its
    /// invite is retired, or a host that renews its code a few times is left
    /// with a fan of live sockets nobody can dial.
    pub async fn close(mut self) {
        self.stop_keepalive();
        self.endpoint.close(noq::VarInt::from_u32(0), b"");
        self.endpoint.wait_idle().await;
    }

    /// Aborts the keep-alive and rendezvous tasks, if they are running.
    /// Idempotent.
    fn stop_keepalive(&mut self) {
        for task in [self.keepalive.take(), self.rendezvous.take()]
            .into_iter()
            .flatten()
        {
            task.abort();
        }
    }

    /// Abort handle for the keep-alive task, so a test can watch it stop.
    #[cfg(test)]
    fn keepalive_probe(&self) -> Option<tokio::task::AbortHandle> {
        self.keepalive
            .as_ref()
            .map(tokio::task::JoinHandle::abort_handle)
    }
}

impl Drop for HostObfuscatedEndpoint {
    /// A dropped endpoint must not leave its keep-alive behind. [`Self::close`]
    /// is the orderly path — this is the backstop for every other way the
    /// value can go away, including a panic between bind and close.
    fn drop(&mut self) {
        self.stop_keepalive();
    }
}

/// The accept half of a [`HostObfuscatedEndpoint`], taken with
/// [`HostObfuscatedEndpoint::acceptor`].
///
/// Holds the endpoint open only as an accept loop does: once the endpoint it
/// came from is closed, [`Self::accept`] answers `None` for good.
#[derive(Debug, Clone)]
pub struct ObfuscatedAcceptor {
    endpoint: Endpoint,
}

impl ObfuscatedAcceptor {
    /// Accepts the next connection, or `None` once the endpoint is closed.
    ///
    /// See [`HostObfuscatedEndpoint::accept`], which is this call.
    ///
    /// # Errors
    /// [`NetError::Io`] if the QUIC handshake fails, or the peer presented no
    /// usable identity or no ALPN.
    pub async fn accept(&self) -> Option<Result<PeerConnection>> {
        loop {
            let incoming = self.accept_incoming().await?;
            if let Some(outcome) = Self::finish(incoming).await {
                return Some(outcome);
            }
        }
    }

    /// The next connection attempt, before its handshake has run, or `None`
    /// once the endpoint is closed (ADR 0123).
    ///
    /// Split from [`Self::finish`] so an accept loop can hand each handshake
    /// to a task of its own: awaited inline, one peer that starts a
    /// handshake and never finishes it holds every guest behind it until its
    /// idle timeout.
    pub async fn accept_incoming(&self) -> Option<noq::Incoming> {
        self.endpoint.accept().await
    }

    /// Runs the handshake of an attempt from [`Self::accept_incoming`].
    ///
    /// `None` for an attempt the guest itself abandoned, which is not a
    /// failure: a guest drops the attempt in flight when this host's answer
    /// reaches it (ADR 0116), and a handshake closed by its own application
    /// arrives as exactly this — the guest moving on to the attempt that gets
    /// through.
    ///
    /// # Errors
    /// [`NetError::Io`] if the handshake fails, or the peer presented no
    /// usable identity or no ALPN.
    pub async fn finish(incoming: noq::Incoming) -> Option<Result<PeerConnection>> {
        match incoming.await {
            Ok(connection) => Some(
                peer_and_alpn(&connection)
                    .map(|(peer, alpn)| PeerConnection::from_obfuscated(connection, peer, alpn)),
            ),
            Err(noq::ConnectionError::ConnectionClosed(close))
                if close.error_code == noq::TransportErrorCode::APPLICATION_ERROR =>
            {
                tracing::debug!("a guest abandoned one of its dial attempts");
                None
            }
            Err(error) => Some(Err(NetError::Io(error.to_string()))),
        }
    }
}

/// Binds the host side: a UDP socket, STUN discovery of its public address, a
/// fresh self-signed cert, and a `noq::Endpoint` wrapping it all in the
/// obfuscated transport keyed by `invite_id`.
///
/// If a STUN server answered, spawns a background task that resends a STUN
/// request to the same server every `NAT_MAPPING_KEEPALIVE_SECS` to keep the
/// discovered NAT mapping open (ADR 0053 — this substitutes for a
/// synchronized simultaneous punch, which this app's one-way invite has no
/// channel to coordinate). That task now stops with the endpoint rather than
/// with the process — see [`HostObfuscatedEndpoint::close`].
///
/// With a `rendezvous` it also publishes the address to the DHT, republishes
/// it whenever the keep-alive's STUN answer says it moved, and punches towards
/// every guest that knocks (ADR 0113) — the simultaneous send the keep-alive
/// alone could never be.
///
/// `identity` is this node's ed25519 endpoint key: the certificate is
/// generated from it, so a guest that connects here is talking to the same
/// `NodeId` the iroh path would have given it (ADR 0080).
///
/// # Errors
/// [`NetError::Endpoint`] if the socket cannot be bound, cloned, or the `noq`
/// endpoint cannot be constructed.
pub async fn bind_host(
    invite_id: &[u8; INVITE_ID_BYTES],
    identity: &SigningKey,
    rendezvous: Option<Rendezvous>,
) -> Result<HostObfuscatedEndpoint> {
    bind_host_via(invite_id, identity, STUN_SERVERS, rendezvous).await
}

/// [`bind_host`], with the reflector list named by the caller.
///
/// Split out so a test can point the discovery and its keep-alive at a local
/// reflector instead of the public fleet, which is the only way to prove the
/// keep-alive both runs and stops without depending on the internet.
async fn bind_host_via(
    invite_id: &[u8; INVITE_ID_BYTES],
    identity: &SigningKey,
    servers: &[&str],
    rendezvous: Option<Rendezvous>,
) -> Result<HostObfuscatedEndpoint> {
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| NetError::Endpoint(e.to_string()))?;
    let probe_socket = socket
        .try_clone()
        .map_err(|e| NetError::Endpoint(e.to_string()))?;
    let keepalive_socket = socket
        .try_clone()
        .map_err(|e| NetError::Endpoint(e.to_string()))?;
    let punch_socket = socket
        .try_clone()
        .map_err(|e| NetError::Endpoint(e.to_string()))?;

    let resolved = resolve_servers(servers);
    let (public_addr, stun_server) =
        tokio::task::spawn_blocking(move || discover_public_addr(&probe_socket, &resolved))
            .await
            .map_err(|e| NetError::Endpoint(e.to_string()))?;

    let keepalive = stun_server.map(|server| spawn_keepalive(keepalive_socket, server));

    // Once `noq` owns the socket the keep-alive cannot read its own answers,
    // so they come back through the tap — which is how a host whose uplink
    // changed finds out (ADR 0113).
    let (current_addr, watched_addr) = tokio::sync::watch::channel(public_addr);
    let runtime: Arc<dyn noq::Runtime> = Arc::new(TokioRuntime);
    let wrapped = runtime
        .wrap_udp_socket(socket)
        .map_err(|e| NetError::Endpoint(e.to_string()))?;
    let obfuscated_socket: Box<dyn AsyncUdpSocket> = Box::new(
        ObfuscatedSocket::new(wrapped, Obfuscator::for_host(invite_id)).with_stun_tap(
            StunTap::new(stun_server.into_iter().collect(), current_addr),
        ),
    );
    let rendezvous = match (rendezvous, public_addr) {
        (Some(rendezvous), Some(_)) => Some(tokio::spawn(serve_rendezvous(
            rendezvous,
            identity.clone(),
            *invite_id,
            watched_addr,
            punch_socket,
            stun_server,
        ))),
        _ => None,
    };

    let certificate = identity_certificate(identity)?;
    let cert_fingerprint = certificate.fingerprint;

    // Client authentication is mandatory, and the certificate it asks for is
    // the guest's identity rather than a credential: this is where the host
    // learns which `NodeId` dialed it (ADR 0080). It authorizes nothing on its
    // own — the invite ticket and the host's own consent decision still do all
    // of that, in `lumepeer-core` (§2.3).
    let provider = Arc::new(noq::rustls::crypto::ring::default_provider());
    let mut rustls_config = noq::rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&noq::rustls::version::TLS13])
        .map_err(|e| NetError::Endpoint(e.to_string()))?
        .with_client_cert_verifier(Arc::new(IdentityClientVerifier))
        .with_single_cert(vec![certificate.der], certificate.key)
        .map_err(|e| NetError::Endpoint(e.to_string()))?;
    rustls_config.alpn_protocols = alpn_protocols();
    let quic_server_config = noq::crypto::rustls::QuicServerConfig::try_from(rustls_config)
        .map_err(|e| NetError::Endpoint(e.to_string()))?;
    let mut server_config = ServerConfig::with_crypto(Arc::new(quic_server_config));
    server_config.transport_config(Arc::new(obfuscated_transport_config()));

    let endpoint = Endpoint::new_with_abstract_socket(
        EndpointConfig::default(),
        Some(server_config),
        obfuscated_socket,
        runtime,
    )
    .map_err(|e| NetError::Endpoint(e.to_string()))?;

    Ok(HostObfuscatedEndpoint {
        endpoint,
        public_addr,
        cert_fingerprint,
        keepalive,
        rendezvous,
    })
}

/// Resolves each reflector name to its first address, skipping the ones that
/// do not resolve. Blocking: DNS.
fn resolve_servers<S: AsRef<str>>(servers: &[S]) -> Vec<SocketAddr> {
    servers
        .iter()
        .filter_map(|server| {
            server
                .as_ref()
                .to_socket_addrs()
                .ok()
                .and_then(|mut a| a.next())
        })
        .collect()
}

/// Tries each of `servers` in turn on `socket`, returning the first reflexive
/// address found and the server that answered (so the caller can keep hitting
/// the same one to hold the mapping open). `None` if no server answered.
fn discover_public_addr(
    socket: &UdpSocket,
    servers: &[SocketAddr],
) -> (Option<SocketAddr>, Option<SocketAddr>) {
    for server in servers {
        if let Ok(reflexive) = stun::reflexive_addr(socket, *server) {
            return (Some(reflexive), Some(*server));
        }
    }
    (None, None)
}

/// Resends a STUN request to `server` on `socket` every
/// `NAT_MAPPING_KEEPALIVE_SECS`, forever. The outbound packet is what a NAT
/// counts to keep a mapping alive; the answer is read by the socket's STUN tap
/// rather than here, because `noq` owns the socket's reads (ADR 0113). A lost
/// answer costs one tick of address tracking, never the mapping.
///
/// The handle is returned rather than dropped: the endpoint owns it and aborts
/// it on close, so a retired invite stops holding its mapping open
/// (gap-tasks/21 task 1; ADR 0080).
fn spawn_keepalive(socket: UdpSocket, server: SocketAddr) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(NAT_MAPPING_KEEPALIVE_SECS));
        // The STUN probe during bind already sent one packet; skip the
        // immediate first tick `interval` fires so ticks land on the
        // intended cadence from that point on.
        interval.tick().await;
        loop {
            interval.tick().await;
            let _ = stun::send_binding_request(&socket, server);
        }
    })
}

/// The host's half of the rendezvous (ADR 0113), for the life of one endpoint.
///
/// Publishes where the endpoint is reachable — at once, whenever the STUN tap
/// reports that the mapping moved, and every [`RENDEZVOUS_REPUBLISH_SECS`] so
/// DHT nodes keep it — and polls the DHT for a guest's knock
/// ([`poll_knocks`]). Each new knock is answered with a train of packets
/// towards the guest's address from this endpoint's own socket: the send that
/// makes a NAT which filters by sender let the guest's next packet in.
///
/// Knocks also arrive pushed, over the signalling relays (ADR 0116). Those
/// are answered at once, with the punch and with this endpoint's current
/// address said back on the same relays, so a guest that dialed a stale
/// address is redirected within the same attempt. Each one also asks
/// `stun_server` again: a host whose uplink changed since the last keep-alive
/// learns it now, and the moved address goes out on the relays a round trip
/// later instead of at the next keep-alive.
async fn serve_rendezvous(
    rendezvous: Rendezvous,
    identity: SigningKey,
    invite_id: [u8; INVITE_ID_BYTES],
    mut addr: tokio::sync::watch::Receiver<Option<SocketAddr>>,
    punch_socket: UdpSocket,
    stun_server: Option<SocketAddr>,
) {
    let punch_socket = Arc::new(punch_socket);
    let mut republish = tokio::time::interval(Duration::from_secs(RENDEZVOUS_REPUBLISH_SECS));
    // Owned here so an endpoint that closes takes its punches, publishes and
    // DHT poll with it.
    let mut work = tokio::task::JoinSet::new();
    // One budget for the knocks both paths hear (ADR 0123).
    let budget = Arc::new(Mutex::new(PunchBudget::default()));
    work.spawn(poll_knocks(
        rendezvous.clone(),
        invite_id,
        Arc::clone(&punch_socket),
        Arc::clone(&budget),
    ));
    let mut signals = rendezvous.host_signals(&invite_id, &identity);
    loop {
        tokio::select! {
            _ = republish.tick() => {}
            changed = addr.changed() => {
                if changed.is_err() {
                    return;
                }
                let moved = *addr.borrow();
                tracing::info!(addr = ?moved, "the obfuscated endpoint's public address moved");
                if let (Some(signals), Some(moved)) = (&signals, moved) {
                    signals.say(moved);
                }
                republish.reset();
            }
            Some(knock) = heard(&mut signals) => {
                if !allowed(&budget, knock.addr) {
                    continue;
                }
                tracing::info!(guest = %knock.addr, "a guest knocked through a relay: punching towards it");
                work.spawn(punch_towards(Arc::clone(&punch_socket), knock.addr));
                if let Some(server) = stun_server {
                    let _ = stun::send_binding_request(&punch_socket, server);
                }
                if let (Some(signals), Some(current)) = (&signals, *addr.borrow()) {
                    signals.say(current);
                }
                continue;
            }
            Some(_) = work.join_next(), if !work.is_empty() => continue,
        }
        // Reached from the two publish arms only.
        let Some(current) = *addr.borrow() else {
            continue;
        };
        let rendezvous = rendezvous.clone();
        let identity = identity.clone();
        work.spawn(async move {
            match rendezvous
                .publish_host(&identity, &invite_id, current)
                .await
            {
                Ok(()) => {
                    tracing::info!(addr = %current, "published the obfuscated address to the DHT");
                }
                Err(error) => {
                    tracing::warn!(%error, "could not publish the obfuscated address to the DHT");
                }
            }
        });
    }
}

/// The DHT half of hearing knocks (ADR 0113): polls for a guest's knock,
/// [`RENDEZVOUS_POLL_SECS`] after the previous poll finished, and punches
/// towards each new one.
///
/// A knock that is already there when the endpoint starts is only answered if
/// it is recent by its own clock; every knock after that is new by
/// construction (its sequence number is higher than the last one seen), so no
/// clock between the two machines has to agree.
///
/// A task of its own rather than an arm of [`serve_rendezvous`]: a lookup
/// takes seconds — 7-10 on the public DHT — and the first one starts with the
/// endpoint, which is just when a guest resuming after a restart knocks. A
/// knock pushed through a relay meanwhile must not wait for it (ADR 0116).
async fn poll_knocks(
    rendezvous: Rendezvous,
    invite_id: [u8; INVITE_ID_BYTES],
    punch_socket: Arc<UdpSocket>,
    budget: Arc<Mutex<PunchBudget>>,
) {
    let mut poll = tokio::time::interval(Duration::from_secs(RENDEZVOUS_POLL_SECS));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut punches = tokio::task::JoinSet::new();
    let mut last_knock: Option<i64> = None;
    let mut first_poll = true;
    loop {
        tokio::select! {
            _ = poll.tick() => {}
            Some(_) = punches.join_next(), if !punches.is_empty() => continue,
        }
        let knock = rendezvous.knock_after(&invite_id, last_knock).await;
        // The pause counts from the end of a poll: a lookup takes seconds,
        // and counting from its start would poll back to back.
        poll.reset();
        let Some(knock) = knock else {
            first_poll = false;
            continue;
        };
        last_knock = Some(knock.seq);
        let fresh =
            !first_poll || unix_now().saturating_sub(knock.at) <= RENDEZVOUS_KNOCK_FRESH_SECS;
        first_poll = false;
        if fresh && allowed(&budget, knock.addr) {
            tracing::info!(guest = %knock.addr, "a guest knocked: punching towards it");
            punches.spawn(punch_towards(Arc::clone(&punch_socket), knock.addr));
        }
    }
}

/// How many trains of punch packets a host sends, and where (ADR 0123).
///
/// A knock is sealed under the invite and signed by nobody in particular —
/// the host cannot know its guests in advance — so anybody holding the invite
/// can knock with any address in it, and each knock used to be a train of
/// packets from this host to that address. Bounded here: one train per
/// address per [`RENDEZVOUS_REPUNCH_SECS`], which is what a guest's own dial
/// knocks at, and no more than [`RENDEZVOUS_PUNCHES_PER_MINUTE`] in all.
#[derive(Debug, Default)]
struct PunchBudget {
    recent: std::collections::VecDeque<(std::time::Instant, SocketAddr)>,
}

impl PunchBudget {
    fn take(&mut self, to: SocketAddr, now: std::time::Instant) -> bool {
        let minute = Duration::from_mins(1);
        while self
            .recent
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) >= minute)
        {
            self.recent.pop_front();
        }
        let repunch = Duration::from_secs(RENDEZVOUS_REPUNCH_SECS);
        let again_too_soon = self
            .recent
            .iter()
            .any(|(at, addr)| *addr == to && now.saturating_duration_since(*at) < repunch);
        let full = self.recent.len()
            >= usize::try_from(RENDEZVOUS_PUNCHES_PER_MINUTE).unwrap_or(usize::MAX);
        if again_too_soon || full {
            return false;
        }
        self.recent.push_back((now, to));
        true
    }
}

/// Whether `budget` lets this host punch towards `to` now.
fn allowed(budget: &Mutex<PunchBudget>, to: SocketAddr) -> bool {
    let granted = budget
        .lock()
        .is_ok_and(|mut budget| budget.take(to, std::time::Instant::now()));
    if !granted {
        tracing::debug!(guest = %to, "a knock over the punch budget; not answered");
    }
    granted
}

/// The next record heard on `signals`, or never when there is no push
/// channel — so a `select!` arm on it simply never fires.
async fn heard(signals: &mut Option<Signals>) -> Option<Sighting> {
    match signals {
        Some(signals) => signals.heard().await,
        None => std::future::pending().await,
    }
}

/// Sends [`RENDEZVOUS_PUNCH_PACKETS`] datagrams of random bytes to `to`, one
/// every [`RENDEZVOUS_PUNCH_INTERVAL_MS`].
///
/// Random bytes are what every sealed datagram of this transport already looks
/// like, so a punch adds no shape of its own to the wire (ADR 0082), and the
/// guest's socket drops them as undecryptable noise. Only the sending matters:
/// it is what leaves this side's NAT expecting packets from the guest.
async fn punch_towards(socket: Arc<UdpSocket>, to: SocketAddr) {
    let mut interval = tokio::time::interval(Duration::from_millis(RENDEZVOUS_PUNCH_INTERVAL_MS));
    for _ in 0..RENDEZVOUS_PUNCH_PACKETS {
        interval.tick().await;
        let mut noise = vec![0u8; rand::rng().random_range(48..=96)];
        rand::rng().fill_bytes(&mut noise);
        let _ = socket.send_to(&noise, to);
    }
}

/// Seconds since the Unix epoch, or zero on a clock set before it.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// A guest's obfuscated endpoint, bound once for one host and reused for every
/// channel of the session with it (gap-tasks/21 task 2; ADR 0080).
///
/// One endpoint rather than one per channel, for the same reason the iroh path
/// has one: the datagram keys, the NAT mapping and the socket all belong to
/// the invite, not to `rd/media/1`. Each channel is its own QUIC connection on
/// it, exactly as §4.1 requires, so a busy media or file channel still cannot
/// delay a revoke on the control one.
pub struct GuestObfuscatedEndpoint {
    endpoint: Endpoint,
    /// Where the host's endpoint is: the ticket's address (ADR 0053) until
    /// the host's rendezvous record names a newer one (ADR 0113). Kept for
    /// the order of those records; the socket sends to [`Self::route`].
    target: Arc<Mutex<Target>>,
    /// Where the socket sends the host's datagrams now: moved by every
    /// record `target` follows, and by the host's own packets arriving from
    /// somewhere else (ADR 0134).
    route: HostRoute,
    /// The host's endpoint key, which its rendezvous record is signed with
    /// (ADR 0113).
    host: NodeId,
    /// Blake3 of the host certificate the ticket pinned.
    expected_fingerprint: [u8; 32],
    /// This guest's own certificate, presented on every channel so the host
    /// learns which `NodeId` is dialing it.
    certificate: Arc<IdentityCertificate>,
    /// The invite this endpoint dials under, which names its rendezvous
    /// records.
    invite_id: [u8; INVITE_ID_BYTES],
    /// The rendezvous, when the caller gave this endpoint a DHT (ADR 0113).
    rendezvous: Option<GuestRendezvous>,
    /// The push channel of the rendezvous (ADR 0116), opened by the first
    /// control attempt and kept for the endpoint's life: it is how this
    /// guest knocks, and how the host's answer — where it is now — reaches
    /// the dial while it is still running.
    signals: Mutex<Option<GuestSignals>>,
    /// Fired whenever the host answers on the push channel (ADR 0116). The
    /// host punches before it answers, so from this moment a fresh dial goes
    /// straight through, while the attempt in flight would sit out QUIC's
    /// retransmission timer first: [`punch`] starts over at once instead.
    answered: Arc<tokio::sync::Notify>,
}

/// Where a guest dials the host's obfuscated endpoint, and when the host said
/// so by its own clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Target {
    addr: SocketAddr,
    /// The `at` of the record that named `addr`; 0 for the ticket's address,
    /// which every record the host has written is newer than.
    at: u64,
}

impl Target {
    /// Moves to where `seen` says the host is, unless the host has already
    /// said something newer; `false` when `seen` was older and changed
    /// nothing (ADR 0116).
    ///
    /// The relays answer within a second and a DHT lookup takes seconds, so
    /// a lookup that lands after the relays' answer mostly carries the older
    /// of the two — a record the DHT had not caught up on yet — and taking it
    /// sent every later channel of the session to where the host no longer
    /// was. Both records are stamped by the host's clock, so comparing them
    /// needs no two machines to agree. A tie moves: a host says where it is
    /// and, a STUN round trip later, where it moved, within one second.
    fn follow(&mut self, seen: &Sighting) -> bool {
        if seen.at < self.at {
            return false;
        }
        *self = Self {
            addr: seen.addr,
            at: seen.at,
        };
        true
    }
}

/// A guest endpoint's open push channel (ADR 0116).
struct GuestSignals {
    speaker: Speaker,
    /// The task that owns the channel and moves the dial target to wherever
    /// the host says it is. Aborting it closes the channel.
    listener: tokio::task::AbortHandle,
    /// The task that knocks again whenever a live session goes silent
    /// ([`knock_on_silence`]; ADR 0134).
    watchdog: tokio::task::AbortHandle,
}

/// What a guest endpoint needs to look up its host and knock (ADR 0113).
struct GuestRendezvous {
    dht: Rendezvous,
    /// A clone of the endpoint's socket, to send STUN queries from — the
    /// knock has to name the mapping the dial itself goes out through.
    stun_socket: Arc<UdpSocket>,
    /// Reflectors whose answers the socket hands over, and the answer.
    tap: StunTap,
    reflexive: tokio::sync::watch::Receiver<Option<SocketAddr>>,
    /// Names of the reflectors to ask, resolved inside the dial.
    servers: &'static [&'static str],
}

impl std::fmt::Debug for GuestObfuscatedEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuestObfuscatedEndpoint")
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

impl GuestObfuscatedEndpoint {
    /// Binds a socket for one host: obfuscated with `invite_id`'s keys, and
    /// carrying a certificate generated from this node's `identity`.
    ///
    /// `host` is the host's endpoint key and `target`/`expected_fingerprint`
    /// what its ticket said. With a `rendezvous`, [`Self::connect_control`]
    /// also asks the DHT where the host is now and knocks (ADR 0113).
    ///
    /// Nothing is dialed here — [`Self::connect`] opens each channel — so a
    /// bound endpoint costs one UDP socket and no traffic at all.
    ///
    /// # Errors
    /// [`NetError::Endpoint`] if the socket, the certificate or the `noq`
    /// endpoint cannot be built.
    pub fn bind(
        invite_id: &[u8; INVITE_ID_BYTES],
        identity: &SigningKey,
        host: NodeId,
        target: SocketAddr,
        expected_fingerprint: [u8; 32],
        rendezvous: Option<Rendezvous>,
    ) -> Result<Self> {
        Self::bind_via(
            invite_id,
            identity,
            host,
            target,
            expected_fingerprint,
            rendezvous,
            STUN_SERVERS,
        )
    }

    /// [`Self::bind`], with the reflector list named by the caller, so a test
    /// can learn this socket's address from a local reflector.
    fn bind_via(
        invite_id: &[u8; INVITE_ID_BYTES],
        identity: &SigningKey,
        host: NodeId,
        target: SocketAddr,
        expected_fingerprint: [u8; 32],
        rendezvous: Option<Rendezvous>,
        servers: &'static [&'static str],
    ) -> Result<Self> {
        let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| NetError::Endpoint(e.to_string()))?;
        let stun_socket = socket
            .try_clone()
            .map_err(|e| NetError::Endpoint(e.to_string()))?;
        let (reflexive_tx, reflexive) = tokio::sync::watch::channel(None);
        // Empty until the dial resolves the reflectors: that is DNS, and this
        // runs on the actor's thread.
        let tap = StunTap::new(Vec::new(), reflexive_tx);
        let route = HostRoute::new(target);
        let runtime: Arc<dyn noq::Runtime> = Arc::new(TokioRuntime);
        let wrapped = runtime
            .wrap_udp_socket(socket)
            .map_err(|e| NetError::Endpoint(e.to_string()))?;
        let obfuscated_socket: Box<dyn AsyncUdpSocket> = Box::new(
            ObfuscatedSocket::new(wrapped, Obfuscator::for_guest(invite_id))
                .with_stun_tap(tap.clone())
                .with_host_route(route.clone()),
        );
        let endpoint = Endpoint::new_with_abstract_socket(
            EndpointConfig::default(),
            None,
            obfuscated_socket,
            runtime,
        )
        .map_err(|e| NetError::Endpoint(e.to_string()))?;

        Ok(Self {
            endpoint,
            target: Arc::new(Mutex::new(Target {
                addr: target,
                at: 0,
            })),
            route,
            host,
            expected_fingerprint,
            certificate: Arc::new(identity_certificate(identity)?),
            invite_id: *invite_id,
            rendezvous: rendezvous.map(|dht| GuestRendezvous {
                dht,
                stun_socket: Arc::new(stun_socket),
                tap,
                reflexive,
                servers,
            }),
            signals: Mutex::new(None),
            answered: Arc::new(tokio::sync::Notify::new()),
        })
    }

    /// Opens the control channel: one attempt of a dial (ADR 0083).
    ///
    /// With a rendezvous, the attempt also looks the host up and knocks, both
    /// in the background so the punch starts at once (ADR 0113): a lookup that
    /// finds the host somewhere else moves the target under the punch's later
    /// packets, and the knock is what brings the host's own packets towards
    /// this socket. Neither is waited for — a DHT that answers slowly or not
    /// at all leaves this exactly the dial it was before the rendezvous.
    ///
    /// # Errors
    /// As [`Self::connect`].
    pub async fn connect_control(&self) -> Result<PeerConnection> {
        if let Some(rendezvous) = &self.rendezvous {
            self.start_rendezvous(rendezvous);
        }
        self.connect(crate::endpoint::ALPN_CONTROL).await
    }

    /// This endpoint's push channel, opened on first use (ADR 0116). `None`
    /// when the rendezvous has no relays.
    fn signal_speaker(&self, rendezvous: &GuestRendezvous) -> Option<Speaker> {
        let mut slot = self.signals.lock().ok()?;
        if let Some(open) = slot.as_ref() {
            return Some(open.speaker.clone());
        }
        let mut signals = rendezvous.dht.guest_signals(&self.invite_id, self.host)?;
        let speaker = signals.speaker();
        let target = Arc::clone(&self.target);
        let route = self.route.clone();
        let answered = Arc::clone(&self.answered);
        let listener = tokio::spawn(async move {
            while let Some(seen) = signals.heard().await {
                let from = {
                    let Ok(mut current) = target.lock() else {
                        return;
                    };
                    let from = current.addr;
                    if !current.follow(&seen) {
                        tracing::debug!(addr = %seen.addr, "an answer older than the last one came through a relay");
                        continue;
                    }
                    route.point_at(seen.addr);
                    from
                };
                if from == seen.addr {
                    tracing::debug!(addr = %seen.addr, "the host answered through a relay");
                } else {
                    tracing::info!(
                        %from,
                        to = %seen.addr,
                        "the host answered through a relay from somewhere else: dialing it there"
                    );
                }
                answered.notify_waiters();
            }
        })
        .abort_handle();
        let watchdog = tokio::spawn(knock_on_silence(
            self.endpoint.clone(),
            self.route.clone(),
            speaker.clone(),
            Arc::clone(&rendezvous.stun_socket),
            rendezvous.tap.clone(),
            rendezvous.reflexive.clone(),
            rendezvous.servers,
        ))
        .abort_handle();
        *slot = Some(GuestSignals {
            speaker: speaker.clone(),
            listener,
            watchdog,
        });
        Some(speaker)
    }

    /// Spawns this attempt's lookup and knock (see [`Self::connect_control`]).
    fn start_rendezvous(&self, rendezvous: &GuestRendezvous) {
        let speaker = self.signal_speaker(rendezvous);
        let dht = rendezvous.dht.clone();
        let host = self.host;
        let invite_id = self.invite_id;
        let target = Arc::clone(&self.target);
        let route = self.route.clone();
        tokio::spawn(async move {
            let Some(seen) = dht.host(&host, &invite_id).await else {
                return;
            };
            let Ok(mut current) = target.lock() else {
                return;
            };
            let from = current.addr;
            if !current.follow(&seen) {
                return;
            }
            route.point_at(seen.addr);
            if from != seen.addr {
                tracing::info!(
                    %from,
                    to = %seen.addr,
                    "the host's obfuscated endpoint has moved: dialing where it says it is now"
                );
            }
        });

        let dht = rendezvous.dht.clone();
        let socket = Arc::clone(&rendezvous.stun_socket);
        let tap = rendezvous.tap.clone();
        let reflexive = rendezvous.reflexive.clone();
        let servers = rendezvous.servers;
        tokio::spawn(async move {
            let Some(own) = own_reflexive_addr(&socket, &tap, reflexive, servers).await else {
                tracing::warn!("no reflector answered: cannot knock, the host will not punch back");
                return;
            };
            // The push first: it reaches a listening host in well under a
            // second, while the DHT put below takes seconds (ADR 0116).
            if let Some(speaker) = speaker {
                speaker.say(own);
                tracing::info!(addr = %own, "knocked through the signalling relays");
            }
            match dht.knock(&invite_id, own).await {
                Ok(()) => {
                    tracing::info!(addr = %own, "knocked: asked the host to punch towards this guest");
                }
                Err(error) => tracing::warn!(%error, "could not knock"),
            }
        });
    }

    /// Opens one channel to the host, on `alpn`, punching towards it as it
    /// goes.
    ///
    /// TLS is pinned to the fingerprint the ticket carried rather than
    /// validated against a CA (ADR 0053: there is no CA for an ad-hoc host
    /// certificate, and the real authentication is the `invite_id`-derived
    /// AEAD layer beneath this handshake). The dial itself is the punch — see
    /// [`punch`] for the cadence and why the packets are the dial's own rather
    /// than a shape of their own (gap-tasks/22 task 3; ADR 0082).
    ///
    /// # Errors
    /// [`NetError::Endpoint`] if the client configuration cannot be built;
    /// [`NetError::Dial`] if every attempt fails or the host that answered
    /// presented no usable identity.
    pub async fn connect(&self, alpn: &[u8]) -> Result<PeerConnection> {
        let provider = Arc::new(noq::rustls::crypto::ring::default_provider());
        let mut rustls_config = noq::rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&noq::rustls::version::TLS13])
            .map_err(|e| NetError::Endpoint(e.to_string()))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedCertVerifier {
                expected_fingerprint: self.expected_fingerprint,
            }))
            .with_client_auth_cert(
                vec![self.certificate.der.clone()],
                self.certificate.key.clone_key(),
            )
            .map_err(|e| NetError::Endpoint(e.to_string()))?;
        // One ALPN per connection, which is what makes the host's
        // `Channel::from_alpn` answer this connection's channel and no other
        // (§4.1).
        rustls_config.alpn_protocols = vec![alpn.to_vec()];
        let quic_client_config = noq::crypto::rustls::QuicClientConfig::try_from(rustls_config)
            .map_err(|e| NetError::Endpoint(e.to_string()))?;
        let mut client_config = ClientConfig::new(Arc::new(quic_client_config));
        client_config.transport_config(Arc::new(obfuscated_transport_config()));

        let connection = punch_until(
            || async {
                // Always the alias: the socket sends it to wherever the route
                // says the host is, which the rendezvous may move under an
                // attempt (ADR 0113) and the host's own packets may move
                // under a live connection (ADR 0134).
                self.endpoint
                    .connect_with(client_config.clone(), HostRoute::ALIAS, CERT_SUBJECT)
                    .map_err(|e| NetError::Dial(e.to_string()))?
                    .await
                    .map_err(|e| NetError::Dial(e.to_string()))
            },
            &self.answered,
        )
        .await?;
        // Outside the punch on purpose: a host that answered but named itself
        // with a certificate this cannot read will answer the same way every
        // time, so it is a failed connection rather than a failed attempt.
        let (peer, negotiated) = peer_and_alpn(&connection)?;
        // The pinned certificate has to carry the key the ticket names
        // (ADR 0122). A fingerprint only says "this certificate"; this is what
        // says "this certificate belongs to the host being dialed", so a
        // ticket that paired one host's identity with another's certificate
        // connects to nobody.
        if peer != self.host {
            connection.close(noq::VarInt::from_u32(0), b"");
            return Err(NetError::InvalidTicket);
        }
        Ok(PeerConnection::from_obfuscated(
            connection, peer, negotiated,
        ))
    }

    /// Closes the endpoint and every channel it carries.
    pub async fn close(&self) {
        self.stop_signals();
        self.endpoint.close(noq::VarInt::from_u32(0), b"");
        self.endpoint.wait_idle().await;
    }

    /// Closes the push channel, if one was opened. Idempotent.
    fn stop_signals(&self) {
        if let Ok(mut slot) = self.signals.lock()
            && let Some(open) = slot.take()
        {
            open.listener.abort();
            open.watchdog.abort();
        }
    }
}

impl Drop for GuestObfuscatedEndpoint {
    /// The push channel holds relay connections open; they go with the
    /// endpoint however it goes away.
    fn drop(&mut self) {
        self.stop_signals();
    }
}

/// The public address of a guest endpoint's socket, for its knock (ADR 0113).
///
/// Known after the first attempt of a dial — the socket and its mapping stay
/// the same for the whole dial — and otherwise asked of each reflector in turn
/// through `socket`, with the answer arriving by the socket's STUN tap because
/// `noq` owns the reads.
async fn own_reflexive_addr(
    socket: &UdpSocket,
    tap: &StunTap,
    mut reflexive: tokio::sync::watch::Receiver<Option<SocketAddr>>,
    servers: &'static [&'static str],
) -> Option<SocketAddr> {
    if let Some(known) = *reflexive.borrow() {
        return Some(known);
    }
    let resolved = tokio::task::spawn_blocking(move || resolve_servers(servers))
        .await
        .ok()?;
    tap.set_servers(resolved.clone());
    for server in resolved {
        if stun::send_binding_request(socket, server).is_err() {
            continue;
        }
        let answered = tokio::time::timeout(
            Duration::from_millis(STUN_QUERY_TIMEOUT_MS),
            reflexive.wait_for(Option::is_some),
        )
        .await;
        if let Ok(Ok(addr)) = answered {
            return *addr;
        }
    }
    None
}

/// How often [`knock_on_silence`] looks at how long the host has been quiet,
/// milliseconds.
const SILENCE_CHECK_MS: u64 = 250;

/// Knocks again whenever a guest endpoint with a live connection has heard
/// nothing from its host for [`OBFUSCATED_SILENCE_REKNOCK_MS`] (ADR 0134).
///
/// A path that dies under a live session dies because a NAT moved. When it
/// was the host's, the host's packets arriving from its new address usually
/// repair it on their own ([`HostRoute`]). When it was this guest's, or one
/// side's NAT stopped letting the other side in, only the host can repair it:
/// a knock makes it punch towards where this guest is now and say where it
/// is now, which is what the dial does for a new session. Without the knock
/// the session waits out QUIC's idle timeout and is dialed again from scratch,
/// and that wait is the frozen picture.
///
/// The knock goes out at once with the address this socket was last seen at,
/// and once more if the reflectors say that address has moved. Never more
/// often than the host would punch towards one address anyway
/// ([`RENDEZVOUS_REPUNCH_SECS`]), and never while the endpoint has no
/// connection open.
///
/// A dial that has heard nothing at all yet is silent too, counted from its
/// own knock (ADR 0148). That knock is lost when the host is not listening
/// yet — a host restarting into an update, say — and the relays keep no
/// knock for later, so without this one the host that comes up mid-dial
/// would never hear this guest until the dial's next attempt, many seconds
/// on.
async fn knock_on_silence(
    endpoint: Endpoint,
    route: HostRoute,
    speaker: Speaker,
    socket: Arc<UdpSocket>,
    tap: StunTap,
    mut reflexive: tokio::sync::watch::Receiver<Option<SocketAddr>>,
    servers: &'static [&'static str],
) {
    let quiet = Duration::from_millis(OBFUSCATED_SILENCE_REKNOCK_MS);
    let again = Duration::from_secs(RENDEZVOUS_REPUNCH_SECS);
    let mut check = tokio::time::interval(Duration::from_millis(SILENCE_CHECK_MS));
    check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Started by the dial that knocks as it starts this (`start_rendezvous`).
    let started = tokio::time::Instant::now();
    let mut knocked = Some(started);
    loop {
        check.tick().await;
        let silent = route.silent_for().unwrap_or_else(|| started.elapsed());
        if endpoint.open_connections() == 0
            || silent < quiet
            || knocked.is_some_and(|at| at.elapsed() < again)
        {
            continue;
        }
        knocked = Some(tokio::time::Instant::now());

        let known = *reflexive.borrow_and_update();
        let own = match known {
            Some(own) => Some(own),
            None => own_reflexive_addr(&socket, &tap, reflexive.clone(), servers).await,
        };
        let Some(own) = own else {
            tracing::warn!("the host went quiet and no reflector answered: cannot knock");
            continue;
        };
        speaker.say(own);
        tracing::info!(
            addr = %own,
            route = %route.addr(),
            heard_before = route.silent_for().is_some(),
            "the host went quiet: knocked again through the signalling relays"
        );

        // Whether this socket's own mapping is still where the knock said.
        for server in tap.servers() {
            let _ = stun::send_binding_request(&socket, server);
        }
        let answer = tokio::time::timeout(
            Duration::from_millis(STUN_QUERY_TIMEOUT_MS),
            reflexive.changed(),
        )
        .await;
        if let Ok(Ok(())) = answer
            && let Some(moved) = *reflexive.borrow_and_update()
            && moved != own
        {
            speaker.say(moved);
            tracing::info!(
                from = %own,
                to = %moved,
                "this guest's own address moved: knocked again from where it is now"
            );
        }
    }
}

/// The guest's half of the hole punch: `dial` up to
/// [`OBFUSCATED_CONNECT_ATTEMPTS`] times, each attempt bounded by
/// [`OBFUSCATED_PUNCH_ATTEMPT_TIMEOUT_MS`] and the next one
/// [`OBFUSCATED_CONNECT_RETRY_BACKOFF_MS`] behind it, stopping at the first
/// attempt that connects (gap-tasks/22 task 3; ADR 0082).
///
/// **The punch packets are the dial's own.** Each attempt's QUIC Initial goes
/// out through the same `ObfuscatedSocket` as every other datagram, sealed
/// with the same invite-derived key, so a punch and a session are the same
/// thing on the wire — a bespoke punch packet would be a second shape for an
/// observer to learn, which is the opposite of what this transport is for.
/// Success is therefore an established connection and nothing weaker: a sent
/// packet proves nothing about a mapping it never reached.
///
/// The bound on each attempt is what makes this a cadence rather than a wait.
/// An unbounded QUIC dial to a mapping that is not open sits there until the
/// idle timeout, so the attempts stop being punches and the caller stops being
/// able to give up in time to try anything else. The host's half of the punch
/// is the NAT-mapping keep-alive of ADR 0053, which is unchanged and stays the
/// only thing a one-way invite can coordinate: nothing here tells the host
/// when to send, because there is no channel on which to tell it (ADR 0082).
///
/// # Errors
/// Whatever the last attempt failed with, or [`NetError::Dial`] if it went
/// unanswered for its whole budget.
#[cfg(test)]
async fn punch<T, F, Fut>(dial: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    punch_until(dial, &tokio::sync::Notify::new()).await
}

/// [`punch`], cutting an attempt short the moment `answered` fires (ADR 0116).
///
/// An answer from the host means its own packets are already on their way, so
/// its NAT now lets this side in. The attempt in flight would only get through
/// on its next retransmission, a second or more away; a fresh attempt gets
/// through on its first packet. So the attempt is dropped, and the next one
/// starts without the backoff. It still counts as an attempt, so a stream of
/// answers cannot keep the punch going for ever.
///
/// The last attempt is never cut short: no attempt comes after it to take
/// its place, and the answer is what lets its next retransmission through.
async fn punch_until<T, F, Fut>(mut dial: F, answered: &tokio::sync::Notify) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let attempt_timeout = Duration::from_millis(OBFUSCATED_PUNCH_ATTEMPT_TIMEOUT_MS);
    let mut last = NetError::Dial("no attempt was made".to_owned());
    for attempt in 1..=OBFUSCATED_CONNECT_ATTEMPTS {
        let interruptible = attempt < OBFUSCATED_CONNECT_ATTEMPTS;
        let interrupted = answered.notified();
        let attempt_outcome = tokio::time::timeout(attempt_timeout, async {
            tokio::select! {
                outcome = dial() => Some(outcome),
                () = interrupted, if interruptible => None,
            }
        })
        .await;
        match attempt_outcome {
            Ok(Some(Ok(connected))) => return Ok(connected),
            Ok(Some(Err(error))) => last = error,
            Ok(None) => {
                last = NetError::Dial("the host answered: dialing again".to_owned());
                continue;
            }
            Err(_) => {
                last = NetError::Dial("the punch went unanswered for its attempt".to_owned());
            }
        }
        if attempt < OBFUSCATED_CONNECT_ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(OBFUSCATED_CONNECT_RETRY_BACKOFF_MS)).await;
        }
    }
    Err(last)
}

/// Accepts any certificate that carries a usable ed25519 identity, and nothing
/// else (ADR 0080).
///
/// Not a weakened check: on this transport a client certificate is not a
/// credential a certificate authority vouches for, it is the peer *naming
/// itself*, and TLS proves it holds the matching private key. Which of the
/// peers that name are allowed to do anything at all is decided afterwards,
/// from the invite ticket and the host's own consent, in `lumepeer-core`
/// (§2.3) — exactly as on the iroh path, where an endpoint key is likewise
/// self-asserted and authenticated but never authorizing.
#[derive(Debug)]
struct IdentityClientVerifier;

impl ClientCertVerifier for IdentityClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> std::result::Result<ClientCertVerified, noq::rustls::Error> {
        match peer_id_from_certificate(end_entity) {
            Ok(_) => Ok(ClientCertVerified::assertion()),
            Err(_) => Err(noq::rustls::Error::General(
                "client cert carries no ed25519 identity".to_owned(),
            )),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, noq::rustls::Error> {
        noq::rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &noq::rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, noq::rustls::Error> {
        noq::rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &noq::rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        noq::rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Accepts exactly the cert whose blake3 fingerprint matches
/// `expected_fingerprint`, everything else rejected. Stands in for CA
/// validation, which does not apply to an ad-hoc self-signed peer cert
/// (ADR 0053).
#[derive(Debug)]
struct PinnedCertVerifier {
    expected_fingerprint: [u8; 32],
}

impl ServerCertVerifier for PinnedCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, noq::rustls::Error> {
        let fingerprint: [u8; 32] = *blake3::hash(end_entity).as_bytes();
        if fingerprint == self.expected_fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(noq::rustls::Error::General(
                "server cert does not match the invite's pinned fingerprint".to_owned(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, noq::rustls::Error> {
        noq::rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &noq::rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, noq::rustls::Error> {
        noq::rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &noq::rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        noq::rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::net::{Ipv4Addr, SocketAddr};

    use lumepeer_core::constants::QUIC_MAX_IDLE_TIMEOUT_SECS;

    use super::*;

    const INVITE: [u8; INVITE_ID_BYTES] = [0x3c; INVITE_ID_BYTES];

    /// STUN magic cookie (RFC 5389 §6), mirrored from `crate::stun` so this
    /// reflector speaks the format that module parses.
    const MAGIC_COOKIE: u32 = 0x2112_A442;
    /// STUN message type for a Binding success response.
    const BINDING_SUCCESS: u16 = 0x0101;
    /// Attribute type carrying the XOR-obfuscated reflexive address.
    const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
    /// Bytes of the fixed STUN header.
    const HEADER_BYTES: usize = 20;
    /// Bytes of the `XOR-MAPPED-ADDRESS` attribute value for IPv4.
    const XOR_MAPPED_VALUE_BYTES: u16 = 8;

    /// A throwaway endpoint identity. Fixed bytes rather than random so a
    /// failure is the same failure twice.
    fn identity(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; ED25519_KEY_BYTES])
    }

    /// A local stand-in for a public STUN reflector: answers every Binding
    /// request with the source address it saw.
    ///
    /// The point is not to test `crate::stun` — that has its own RFC 5769
    /// vectors — but to give `bind_host_via` a reflector that answers on
    /// loopback, so the keep-alive it spawns exists and can be watched.
    async fn spawn_reflector() -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let socket = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = socket.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            loop {
                let Ok((n, from)) = socket.recv_from(&mut buf).await else {
                    return;
                };
                let Some(request) = buf.get(..n) else { return };
                let Some(reply) = binding_success(request, from) else {
                    continue;
                };
                let _ = socket.send_to(&reply, from).await;
            }
        });
        (addr, task)
    }

    /// Builds the Binding success answer to `request`, echoing `from` as the
    /// XOR-mapped address. `None` for anything too short to be a request or
    /// for an IPv6 peer, which this reflector does not answer.
    fn binding_success(request: &[u8], from: SocketAddr) -> Option<Vec<u8>> {
        let txid = request.get(8..HEADER_BYTES)?;
        let SocketAddr::V4(from) = from else {
            return None;
        };
        let mut reply = Vec::with_capacity(HEADER_BYTES + 12);
        reply.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
        reply.extend_from_slice(&(XOR_MAPPED_VALUE_BYTES + 4).to_be_bytes());
        reply.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        reply.extend_from_slice(txid);
        reply.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
        reply.extend_from_slice(&XOR_MAPPED_VALUE_BYTES.to_be_bytes());
        reply.push(0);
        reply.push(1);
        let cookie_hi = u16::try_from(MAGIC_COOKIE >> 16).ok()?;
        reply.extend_from_slice(&(from.port() ^ cookie_hi).to_be_bytes());
        reply.extend_from_slice(&(u32::from(*from.ip()) ^ MAGIC_COOKIE).to_be_bytes());
        Some(reply)
    }

    /// gap-tasks/21 task 1, and one half of its definition of done: closing
    /// the endpoint stops the keep-alive task.
    ///
    /// Without this the task outlived every invite it was bound for, so a host
    /// that renewed its code kept sending a STUN request every
    /// `NAT_MAPPING_KEEPALIVE_SECS` from every socket it had ever bound.
    #[tokio::test]
    async fn closing_the_endpoint_stops_the_keepalive_task() {
        let (reflector, reflector_task) = spawn_reflector().await;
        let host = bind_host_via(&INVITE, &identity(1), &[&reflector.to_string()], None)
            .await
            .unwrap();

        assert!(
            host.public_addr.is_some(),
            "the local reflector answered, so discovery must have an address"
        );
        let keepalive = host
            .keepalive_probe()
            .expect("a reflector answered, so a keep-alive holds its mapping open");
        assert!(!keepalive.is_finished(), "the keep-alive must be running");

        host.close().await;

        // `abort` is delivered by the runtime, not synchronously by the
        // caller, so the task is finished on one of the next few polls rather
        // than the instant `close` returns.
        for _ in 0..1_000 {
            if keepalive.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            keepalive.is_finished(),
            "closing the endpoint must stop its keep-alive task"
        );

        reflector_task.abort();
    }

    /// Records when each punch attempt was made, relative to the first, so a
    /// test can assert the cadence rather than the wall clock.
    ///
    /// Every timing test below runs on tokio's paused clock: no socket is
    /// bound, no packet is sent, and time only moves when the runtime has
    /// nothing left to poll — so the instants are the schedule itself and not
    /// a machine's load (gap-tasks/22 definition of done).
    async fn punch_attempts<T>(outcome: impl Fn(usize) -> Result<T>) -> (Result<T>, Vec<Duration>) {
        let started = tokio::time::Instant::now();
        let made = std::cell::RefCell::new(Vec::new());
        let result = punch(|| async {
            let attempt = {
                let mut made = made.borrow_mut();
                made.push(started.elapsed());
                made.len()
            };
            outcome(attempt)
        })
        .await;
        (result, made.into_inner())
    }

    /// ADR 0116: the host's answer ends the attempt in flight and the next
    /// one goes out at that instant — no retransmission timer, no backoff.
    #[tokio::test(start_paused = true)]
    async fn an_answer_from_the_host_starts_the_next_attempt_at_once() {
        let answered = Arc::new(tokio::sync::Notify::new());
        let ringer = Arc::clone(&answered);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(700)).await;
            ringer.notify_waiters();
        });
        let started = tokio::time::Instant::now();
        let made = std::cell::RefCell::new(Vec::new());
        let result: Result<()> = punch_until(
            || async {
                let attempt = {
                    let mut made = made.borrow_mut();
                    made.push(started.elapsed());
                    made.len()
                };
                if attempt == 1 {
                    // The first attempt's packets are the ones the host's NAT
                    // dropped: it would only have got through on a resend.
                    std::future::pending::<()>().await;
                }
                Ok(())
            },
            &answered,
        )
        .await;

        assert!(result.is_ok(), "the attempt after the answer connects");
        assert_eq!(
            made.into_inner(),
            vec![Duration::ZERO, Duration::from_millis(700)],
            "the second attempt must start when the answer came, not after a timeout"
        );
    }

    /// ADR 0116: an answer that comes during the *last* attempt does not
    /// abandon it. There is no attempt after it to start in its place, and
    /// the answer is exactly what lets its next retransmission through — cut
    /// short, the dial failed at the moment it was about to connect.
    #[tokio::test(start_paused = true)]
    async fn an_answer_during_the_last_attempt_does_not_abandon_it() {
        let answered = Arc::new(tokio::sync::Notify::new());
        let ringer = Arc::clone(&answered);
        let last_starts = OBFUSCATED_CONNECT_RETRY_BACKOFF_MS
            * u64::from(OBFUSCATED_CONNECT_ATTEMPTS.saturating_sub(1));
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(last_starts + 300)).await;
            ringer.notify_waiters();
        });
        let made = std::cell::Cell::new(0u32);
        let result = punch_until(
            || async {
                made.set(made.get() + 1);
                if made.get() < OBFUSCATED_CONNECT_ATTEMPTS {
                    return Err(NetError::Dial("refused".to_owned()));
                }
                // In flight when the answer comes; its resend gets through.
                tokio::time::sleep(Duration::from_secs(1)).await;
                Ok(made.get())
            },
            &answered,
        )
        .await;

        assert_eq!(
            result.unwrap(),
            OBFUSCATED_CONNECT_ATTEMPTS,
            "the last attempt must run to its end"
        );
    }

    /// ADR 0123: an address gets one train of punches per re-punch window,
    /// and the host sends only so many trains a minute whoever asks.
    #[test]
    fn knocks_cannot_make_the_host_punch_without_limit() {
        let mut budget = PunchBudget::default();
        let start = std::time::Instant::now();
        let guest: SocketAddr = "198.51.100.9:51515".parse().unwrap();
        assert!(budget.take(guest, start));
        assert!(
            !budget.take(guest, start + Duration::from_millis(500)),
            "the same address was punched again at once"
        );
        assert!(budget.take(guest, start + Duration::from_secs(RENDEZVOUS_REPUNCH_SECS)));

        let mut budget = PunchBudget::default();
        let per_minute = usize::try_from(RENDEZVOUS_PUNCHES_PER_MINUTE).unwrap();
        let taken = (0..per_minute * 3)
            .filter(|n| {
                let port = u16::try_from(*n).unwrap() + 1;
                budget.take(SocketAddr::from(([192, 0, 2, 1], port)), start)
            })
            .count();
        assert_eq!(
            taken, per_minute,
            "a flood of knocks punched past the budget"
        );
        assert!(
            budget.take(
                "192.0.2.200:1".parse().unwrap(),
                start + Duration::from_secs(61)
            ),
            "the budget comes back a minute later"
        );
    }

    /// ADR 0116: the dial target only moves forward in the host's own time.
    /// The relays' answer lands first; the DHT lookup lands seconds later
    /// with a record from before the host moved, and must not send the
    /// session's later channels back there.
    #[test]
    fn the_dial_target_never_moves_back_to_an_older_record() {
        let seen = |addr: &str, at| Sighting {
            seq: 0,
            addr: addr.parse().unwrap(),
            at,
        };
        let ticket: SocketAddr = "203.0.113.1:1000".parse().unwrap();
        let mut target = Target {
            addr: ticket,
            at: 0,
        };

        assert!(target.follow(&seen("203.0.113.3:3000", 1_790_000_100)));
        assert!(!target.follow(&seen("203.0.113.2:2000", 1_790_000_000)));
        assert_eq!(target.addr, "203.0.113.3:3000".parse().unwrap());
        // The same second: the host answered, then its STUN refresh said
        // where it had moved.
        assert!(target.follow(&seen("203.0.113.4:4000", 1_790_000_100)));
        assert_eq!(target.addr, "203.0.113.4:4000".parse().unwrap());

        // Any record the host wrote beats what the ticket said.
        let mut from_ticket = Target {
            addr: ticket,
            at: 0,
        };
        assert!(from_ticket.follow(&seen("203.0.113.2:2000", 1)));
    }

    /// gap-tasks/22 task 3: the punch keeps to its attempt count and its
    /// backoff when every attempt is refused outright.
    ///
    /// A refusal costs no time, so the packets are exactly
    /// `OBFUSCATED_CONNECT_RETRY_BACKOFF_MS` apart and there are exactly
    /// `OBFUSCATED_CONNECT_ATTEMPTS` of them — no more, so a failed punch
    /// cannot become an unbounded retry loop, and no fewer, so a lost first
    /// packet is not the end of it.
    #[tokio::test(start_paused = true)]
    async fn a_refused_punch_keeps_to_its_count_and_backoff() {
        let (result, made): (Result<()>, _) =
            punch_attempts(|_| Err(NetError::Dial("refused".to_owned()))).await;

        assert!(result.is_err(), "every attempt was refused");
        assert_eq!(
            u32::try_from(made.len()).unwrap(),
            OBFUSCATED_CONNECT_ATTEMPTS
        );
        for (index, at) in made.iter().enumerate() {
            let expected = Duration::from_millis(
                OBFUSCATED_CONNECT_RETRY_BACKOFF_MS * u64::try_from(index).unwrap(),
            );
            assert_eq!(*at, expected, "attempt {index} landed off its cadence");
        }
    }

    /// gap-tasks/22 task 3: an attempt that goes unanswered is abandoned after
    /// `OBFUSCATED_PUNCH_ATTEMPT_TIMEOUT_MS` so the next packet can go out.
    ///
    /// This is the case that matters — a punch into a mapping that is not open
    /// answers nothing at all — and without the bound the dial would sit
    /// there until QUIC's idle timeout, turning a train of packets into
    /// minutes of silence and a caller that cannot give up in time to try
    /// anything else.
    #[tokio::test(start_paused = true)]
    async fn an_unanswered_punch_is_abandoned_on_its_own_cadence() {
        let started = tokio::time::Instant::now();
        let made = std::cell::RefCell::new(Vec::new());
        let result: Result<()> = punch(|| async {
            made.borrow_mut().push(started.elapsed());
            std::future::pending().await
        })
        .await;
        let made = made.into_inner();

        assert!(result.is_err(), "nothing answered");
        assert_eq!(
            u32::try_from(made.len()).unwrap(),
            OBFUSCATED_CONNECT_ATTEMPTS
        );
        let step = OBFUSCATED_PUNCH_ATTEMPT_TIMEOUT_MS + OBFUSCATED_CONNECT_RETRY_BACKOFF_MS;
        for (index, at) in made.iter().enumerate() {
            let expected = Duration::from_millis(step * u64::try_from(index).unwrap());
            assert_eq!(*at, expected, "attempt {index} landed off its cadence");
        }
    }

    /// gap-tasks/22 task 3: the punch stops at the first attempt that
    /// connects, because success is an established connection and not a sent
    /// packet.
    #[tokio::test(start_paused = true)]
    async fn the_punch_stops_at_the_attempt_that_connects() {
        let (result, made) = punch_attempts(|attempt| {
            if attempt == 3 {
                Ok(())
            } else {
                Err(NetError::Dial("refused".to_owned()))
            }
        })
        .await;

        assert!(result.is_ok());
        assert_eq!(made.len(), 3, "the punch must not outlive its own success");
    }

    /// gap-tasks/21 task 1 item 3: no reflector answering is an ordinary
    /// outcome, not an error — the endpoint still binds, with nothing to put
    /// in a ticket and no mapping to hold open.
    #[tokio::test]
    async fn no_reflector_leaves_the_public_address_unknown() {
        // A reflector that is bound and then immediately dropped: the address
        // is real and nothing is listening on it, which is what "no reflector
        // answered" looks like from here.
        let (reflector, reflector_task) = spawn_reflector().await;
        reflector_task.abort();

        let host = bind_host_via(&INVITE, &identity(1), &[&reflector.to_string()], None)
            .await
            .unwrap();
        assert!(host.public_addr.is_none());
        assert!(
            host.keepalive_probe().is_none(),
            "there is no mapping worth holding open without an address"
        );
        host.close().await;
    }

    /// ADR 0122: the certificate a guest pinned must also carry the key of
    /// the host it meant to dial. A guest told to expect one host and handed
    /// another host's fingerprint reaches that other host's endpoint — and
    /// refuses the connection there, as a verdict, before a byte of the
    /// session is spoken.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_pinned_certificate_of_another_host_is_refused() {
        let actual = bind_host_via(&INVITE, &identity(1), &[], None)
            .await
            .unwrap();
        let target = SocketAddr::new(
            Ipv4Addr::LOCALHOST.into(),
            actual.local_addr().unwrap().port(),
        );
        let serving = tokio::spawn(async move {
            let _ = actual.accept().await;
            actual
        });

        let expected = NodeId::from_bytes(&identity(3).verifying_key().to_bytes()).unwrap();
        let guest = GuestObfuscatedEndpoint::bind(
            &INVITE,
            &identity(2),
            expected,
            target,
            *blake3::hash(identity_certificate(&identity(1)).unwrap().der.as_ref()).as_bytes(),
            None,
        )
        .unwrap();
        let outcome = guest.connect(crate::endpoint::ALPN_CONTROL).await;
        assert!(
            matches!(outcome, Err(NetError::InvalidTicket)),
            "a certificate of a host other than the named one must be refused"
        );
        guest.close().await;
        serving.abort();
    }
    /// gap-tasks/21 task 2, and the other half of its definition of done: the
    /// control handshake and a full consent exchange run over the obfuscated
    /// transport between two local endpoints.
    ///
    /// The point is the *wire*, not the socket. `obfuscate.rs` already has its
    /// own codec tests and `stun.rs` its own vectors; what had never been shown
    /// is that everything above the transport — `Channel::from_alpn` off a
    /// negotiated ALPN, `host_handshake`/`guest_handshake`, and the consent
    /// exchange `SessionManager` resolves — works unchanged when the bytes go
    /// through this endpoint instead of iroh's. Nothing in this test knows
    /// which transport it is on, which is the property being asserted
    /// (ADR 0080).
    #[tokio::test(flavor = "multi_thread")]
    async fn the_handshake_and_consent_run_over_the_obfuscated_transport() {
        use lumepeer_core::consent::Role;
        use lumepeer_core::protocol::{Direction, MessageKind};
        use lumepeer_core::session::{SessionManager, SessionState};

        use crate::connection::{Channel, guest_handshake, host_handshake};

        let host_identity = identity(1);
        let guest_identity = identity(2);
        let guest_id = NodeId::from_bytes(&guest_identity.verifying_key().to_bytes()).unwrap();

        // No reflector: this pair is on one machine, so the address a public
        // STUN server would report is neither available nor wanted. The
        // endpoint binds and accepts all the same — `public_addr: None` is an
        // ordinary outcome, not a failure (task 1 item 3).
        let host = bind_host_via(&INVITE, &host_identity, &[], None)
            .await
            .unwrap();
        assert!(host.public_addr.is_none());
        let target = SocketAddr::new(
            Ipv4Addr::LOCALHOST.into(),
            host.local_addr().unwrap().port(),
        );
        let fingerprint = host.cert_fingerprint;

        let host_side = tokio::spawn(async move {
            let connection = host.accept().await.unwrap().unwrap();
            // The ALPN came out of a `noq` handshake and is read by the same
            // function that reads iroh's (§4.1).
            assert_eq!(
                Channel::from_alpn(connection.alpn()),
                Some(Channel::Control)
            );
            assert!(connection.is_obfuscated());
            let (mut control, hello) = host_handshake(connection).await.unwrap();
            let peer = control.peer();

            let mut sessions = SessionManager::new();
            let request = control.recv().await.unwrap();
            assert_eq!(request.kind, MessageKind::ConsentRequest);
            assert_eq!(request.direction, Direction::GuestToHost);
            sessions
                .request_consent_as(peer, hello.role_request)
                .unwrap();
            sessions.grant(peer, Role::ViewOnly).unwrap();
            assert_eq!(sessions.state(&peer), SessionState::Active);
            control
                .send(MessageKind::ConsentGrant(Role::ViewOnly))
                .await
                .unwrap();
            // Held until the guest has read the grant: dropping the control
            // connection closes the QUIC connection under it.
            (peer, control)
        });

        let host_id = NodeId::from_bytes(&host_identity.verifying_key().to_bytes()).unwrap();
        let guest = GuestObfuscatedEndpoint::bind(
            &INVITE,
            &guest_identity,
            host_id,
            target,
            fingerprint,
            None,
        )
        .unwrap();
        let connection = guest.connect(crate::endpoint::ALPN_CONTROL).await.unwrap();
        let mut control = guest_handshake(connection, Role::FullControl, Vec::new(), Vec::new())
            .await
            .unwrap();
        // The host assigned a session id in its `HelloAck` (§9.1).
        let session_id = control.session_id();
        assert_ne!(session_id, [0u8; 16]);

        control.send(MessageKind::ConsentRequest).await.unwrap();
        let grant = control.recv().await.unwrap();
        // Asked for FullControl, granted ViewOnly: the host decides, here as
        // everywhere (§2.3).
        assert_eq!(grant.kind, MessageKind::ConsentGrant(Role::ViewOnly));
        assert_eq!(grant.direction, Direction::HostToGuest);
        assert_eq!(grant.session_id, session_id);

        let (peer, _host_control) = host_side.await.unwrap();
        assert_eq!(
            peer, guest_id,
            "the host must see the guest's own endpoint identity, exactly as on the iroh path"
        );

        guest.close().await;
    }

    /// A NAT in front of a host whose public address moves (ADR 0134): the
    /// guest reaches the host at one of two public addresses, and only the
    /// active one passes anything in either direction — the old mapping is
    /// simply gone, as it is when an uplink changes or a NAT rebinds.
    struct MovingNat {
        public: [SocketAddr; 2],
        active: Arc<std::sync::atomic::AtomicUsize>,
        tasks: tokio::task::JoinSet<()>,
    }

    impl MovingNat {
        async fn spawn(host: SocketAddr) -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};

            let bind = || tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0));
            let inside = Arc::new(bind().await.unwrap());
            let fronts = [
                Arc::new(bind().await.unwrap()),
                Arc::new(bind().await.unwrap()),
            ];
            let public = [
                fronts[0].local_addr().unwrap(),
                fronts[1].local_addr().unwrap(),
            ];
            let active = Arc::new(AtomicUsize::new(0));
            let guest: Arc<Mutex<Option<SocketAddr>>> = Arc::new(Mutex::new(None));
            let mut tasks = tokio::task::JoinSet::new();

            for (index, front) in fronts.iter().enumerate() {
                let (front, inside, active, guest) = (
                    Arc::clone(front),
                    Arc::clone(&inside),
                    Arc::clone(&active),
                    Arc::clone(&guest),
                );
                tasks.spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    while let Ok((n, from)) = front.recv_from(&mut buf).await {
                        if active.load(Ordering::SeqCst) != index {
                            continue;
                        }
                        *guest.lock().unwrap() = Some(from);
                        let _ = inside.send_to(&buf[..n], host).await;
                    }
                });
            }
            {
                let (active, guest) = (Arc::clone(&active), Arc::clone(&guest));
                tasks.spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    while let Ok((n, _)) = inside.recv_from(&mut buf).await {
                        let Some(to) = *guest.lock().unwrap() else {
                            continue;
                        };
                        let Some(front) = fronts.get(active.load(Ordering::SeqCst)) else {
                            continue;
                        };
                        let _ = front.send_to(&buf[..n], to).await;
                    }
                });
            }
            Self {
                public,
                active,
                tasks,
            }
        }

        fn move_to(&self, index: usize) {
            self.active
                .store(index, std::sync::atomic::Ordering::SeqCst);
        }

        /// Passes nothing either way until the next [`Self::move_to`].
        fn black_out(&self) {
            self.move_to(usize::MAX);
        }
    }

    /// The mirror of [`MovingNat`]: a NAT in front of the guest that starts
    /// sending the guest's packets out from a new public address, and drops
    /// whatever still arrives at the old one.
    struct RebindingGuestNat {
        entry: SocketAddr,
        active: Arc<std::sync::atomic::AtomicUsize>,
        tasks: tokio::task::JoinSet<()>,
    }

    impl RebindingGuestNat {
        async fn spawn(host: SocketAddr) -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};

            let bind = || tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0));
            let entry = Arc::new(bind().await.unwrap());
            let outs = [
                Arc::new(bind().await.unwrap()),
                Arc::new(bind().await.unwrap()),
            ];
            let active = Arc::new(AtomicUsize::new(0));
            let guest: Arc<Mutex<Option<SocketAddr>>> = Arc::new(Mutex::new(None));
            let mut tasks = tokio::task::JoinSet::new();

            for (index, out) in outs.iter().enumerate() {
                let (out, entry, active, guest) = (
                    Arc::clone(out),
                    Arc::clone(&entry),
                    Arc::clone(&active),
                    Arc::clone(&guest),
                );
                tasks.spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    while let Ok((n, _)) = out.recv_from(&mut buf).await {
                        let to = *guest.lock().unwrap();
                        if active.load(Ordering::SeqCst) != index {
                            continue;
                        }
                        if let Some(to) = to {
                            let _ = entry.send_to(&buf[..n], to).await;
                        }
                    }
                });
            }
            let entry_addr = entry.local_addr().unwrap();
            {
                let (active, guest) = (Arc::clone(&active), Arc::clone(&guest));
                tasks.spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    while let Ok((n, from)) = entry.recv_from(&mut buf).await {
                        *guest.lock().unwrap() = Some(from);
                        let out = &outs[active.load(Ordering::SeqCst)];
                        let _ = out.send_to(&buf[..n], host).await;
                    }
                });
            }
            Self {
                entry: entry_addr,
                active,
                tasks,
            }
        }

        fn rebind(&self) {
            self.active.store(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// One byte to the host's echo and back, or `false` if it did not come
    /// back well inside the idle timeout.
    async fn echoed(send: &mut noq::SendStream, recv: &mut noq::RecvStream, byte: u8) -> bool {
        let round_trip = async {
            send.write_all(&[byte]).await.ok()?;
            let mut back = [0u8; 1];
            recv.read_exact(&mut back).await.ok()?;
            Some(back[0])
        };
        let limit = Duration::from_secs(QUIC_MAX_IDLE_TIMEOUT_SECS - 1);
        matches!(tokio::time::timeout(limit, round_trip).await, Ok(Some(b)) if b == byte)
    }

    /// ADR 0134: a host whose public address moves under a live session keeps
    /// it. Its packets arrive from the new address; before this, the guest's
    /// QUIC endpoint threw every one of them away as coming from a stranger,
    /// and the session died of silence with both machines online.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_session_follows_a_host_whose_public_address_moved() {
        let host = bind_host_via(&INVITE, &identity(1), &[], None)
            .await
            .unwrap();
        let host_addr = SocketAddr::new(
            Ipv4Addr::LOCALHOST.into(),
            host.local_addr().unwrap().port(),
        );
        let fingerprint = host.cert_fingerprint;
        let mut nat = MovingNat::spawn(host_addr).await;

        let host_side = tokio::spawn(async move {
            let connection = host.accept().await.unwrap().unwrap();
            let (mut send, mut recv) = connection.accept_bi().await.unwrap();
            let mut byte = [0u8; 1];
            while recv.read_exact(&mut byte).await.is_ok() {
                if send.write_all(&byte).await.is_err() {
                    break;
                }
            }
            drop(connection);
            host
        });

        let host_id = NodeId::from_bytes(&identity(1).verifying_key().to_bytes()).unwrap();
        let guest = GuestObfuscatedEndpoint::bind(
            &INVITE,
            &identity(2),
            host_id,
            nat.public[0],
            fingerprint,
            None,
        )
        .unwrap();
        let connection = guest.connect(crate::endpoint::ALPN_CONTROL).await.unwrap();
        let (mut send, mut recv) = connection.open_bi().await.unwrap();
        assert!(
            echoed(&mut send, &mut recv, 1).await,
            "the path works before the move"
        );

        nat.move_to(1);
        assert!(
            echoed(&mut send, &mut recv, 2).await,
            "the session must survive the host's public address moving"
        );
        assert_eq!(guest.route.addr(), nat.public[1]);
        assert!(echoed(&mut send, &mut recv, 3).await);

        connection.close(noq::VarInt::from_u32(0), b"");
        guest.close().await;
        host_side.abort();
        nat.tasks.abort_all();
    }

    /// ADR 0134, the other direction: a guest whose own public address moves
    /// keeps its session with no help from this module — the host is the
    /// QUIC server, and a server follows a client that migrates.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_session_survives_the_guests_own_address_moving() {
        let host = bind_host_via(&INVITE, &identity(1), &[], None)
            .await
            .unwrap();
        let host_addr = SocketAddr::new(
            Ipv4Addr::LOCALHOST.into(),
            host.local_addr().unwrap().port(),
        );
        let fingerprint = host.cert_fingerprint;
        let mut nat = RebindingGuestNat::spawn(host_addr).await;

        let host_side = tokio::spawn(async move {
            let connection = host.accept().await.unwrap().unwrap();
            let (mut send, mut recv) = connection.accept_bi().await.unwrap();
            let mut byte = [0u8; 1];
            while recv.read_exact(&mut byte).await.is_ok() {
                if send.write_all(&byte).await.is_err() {
                    break;
                }
            }
            drop(connection);
            host
        });

        let host_id = NodeId::from_bytes(&identity(1).verifying_key().to_bytes()).unwrap();
        let guest = GuestObfuscatedEndpoint::bind(
            &INVITE,
            &identity(2),
            host_id,
            nat.entry,
            fingerprint,
            None,
        )
        .unwrap();
        let connection = guest.connect(crate::endpoint::ALPN_CONTROL).await.unwrap();
        let (mut send, mut recv) = connection.open_bi().await.unwrap();
        assert!(
            echoed(&mut send, &mut recv, 1).await,
            "the path works before the move"
        );

        nat.rebind();
        assert!(
            echoed(&mut send, &mut recv, 2).await,
            "the session must survive the guest's own public address moving"
        );
        assert!(echoed(&mut send, &mut recv, 3).await);

        connection.close(noq::VarInt::from_u32(0), b"");
        guest.close().await;
        host_side.abort();
        nat.tasks.abort_all();
    }

    /// ADR 0113 (B): a host that restarts presents the very certificate the
    /// invite pinned, so a restored invite can be served again — and a node
    /// with a different identity presents a different one.
    #[tokio::test]
    async fn a_restarted_host_presents_the_certificate_the_invite_pinned() {
        let before = bind_host_via(&INVITE, &identity(1), &[], None)
            .await
            .unwrap();
        let pinned = before.cert_fingerprint;
        before.close().await;

        let after = bind_host_via(&INVITE, &identity(1), &[], None)
            .await
            .unwrap();
        assert_eq!(after.cert_fingerprint, pinned);
        after.close().await;

        let stranger = bind_host_via(&INVITE, &identity(3), &[], None)
            .await
            .unwrap();
        assert_ne!(stranger.cert_fingerprint, pinned);
        stranger.close().await;
    }

    /// ADR 0113: the guest holds a ticket whose address is dead, the host's
    /// rendezvous record says where it is now, and the dial gets there.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_rendezvous_leads_a_guest_to_where_the_host_is_now() {
        let testnet = n0_mainline::Testnet::new(5).await.unwrap();
        let (reflector, reflector_task) = spawn_reflector().await;
        let host_identity = identity(1);
        let host_id = NodeId::from_bytes(&host_identity.verifying_key().to_bytes()).unwrap();

        let host = bind_host_via(
            &INVITE,
            &host_identity,
            &[&reflector.to_string()],
            Some(Rendezvous::with_bootstrap(&testnet.bootstrap).unwrap()),
        )
        .await
        .unwrap();
        let now_at = host.public_addr.expect("the local reflector answered");
        let pinned = host.cert_fingerprint;
        let serving = tokio::spawn(async move {
            let connection = host.accept().await.unwrap().unwrap();
            let peer = connection.peer();
            connection.closed().await;
            (peer, host)
        });

        let guest_dht = Rendezvous::with_bootstrap(&testnet.bootstrap).unwrap();
        for _ in 0..100 {
            if guest_dht.host(&host_id, &INVITE).await.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let published = guest_dht.host(&host_id, &INVITE).await.unwrap();
        assert_eq!(published.addr, now_at);

        // What the ticket said: a socket that is bound and never answers.
        let stale = UdpSocket::bind("127.0.0.1:0").unwrap();
        let reflectors: &'static [&'static str] =
            Box::leak(vec![&*Box::leak(reflector.to_string().into_boxed_str())].into_boxed_slice());
        let guest = GuestObfuscatedEndpoint::bind_via(
            &INVITE,
            &identity(2),
            host_id,
            stale.local_addr().unwrap(),
            pinned,
            Some(guest_dht),
            reflectors,
        )
        .unwrap();
        let connection = guest.connect_control().await.unwrap();
        assert_eq!(connection.peer(), host_id);
        connection.close(0u32.into(), b"done");

        let (peer, host) = serving.await.unwrap();
        assert_eq!(
            peer,
            NodeId::from_bytes(&identity(2).verifying_key().to_bytes()).unwrap()
        );
        guest.close().await;
        host.close().await;
        reflector_task.abort();
    }

    use crate::nostr::test_relay::Relay;

    /// A rendezvous whose DHT has nobody to ask and whose push channel is
    /// the loopback `relay` (ADR 0116).
    fn signalling_through(relay: &Relay) -> Rendezvous {
        Rendezvous::with_bootstrap(&[])
            .unwrap()
            .with_relays(std::slice::from_ref(&relay.url))
    }

    /// `name` leaked for as long as the test process lives, which is what
    /// [`GuestObfuscatedEndpoint::bind_via`] asks of its reflector list.
    fn leaked_reflectors(name: String) -> &'static [&'static str] {
        Box::leak(vec![&*Box::leak(name.into_boxed_str())].into_boxed_slice())
    }

    /// ADR 0116: a knock pushed through a relay is answered at once, even
    /// while a DHT poll is running. The poll is a lookup of seconds — on the
    /// public DHT, 7-10 — and the first one starts the moment the endpoint is
    /// bound, which is exactly when a guest resuming after the host restarted
    /// knocks; a knock that waits for it waits out the guest's dial.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_relay_knock_is_answered_while_a_dht_poll_runs() {
        let relay = Relay::start().await;
        // A bootstrap node that is there and never answers: every lookup
        // waits on it for as long as the DHT gives a request.
        let silent = UdpSocket::bind("127.0.0.1:0").unwrap();
        let rendezvous = Rendezvous::with_bootstrap(&[silent.local_addr().unwrap().to_string()])
            .unwrap()
            .with_relays(std::slice::from_ref(&relay.url));
        let host_at: SocketAddr = "203.0.113.7:4115".parse().unwrap();
        let (_moves, addr) = tokio::sync::watch::channel(Some(host_at));
        let host = tokio::spawn(serve_rendezvous(
            rendezvous.clone(),
            identity(1),
            INVITE,
            addr,
            UdpSocket::bind("127.0.0.1:0").unwrap(),
            None,
        ));
        let mut guest = rendezvous
            .guest_signals(
                &INVITE,
                NodeId::from_bytes(&identity(1).verifying_key().to_bytes()).unwrap(),
            )
            .unwrap();
        relay
            .wait_for("both subscriptions", |seen| seen.filters.len() == 2)
            .await;

        let guest_socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let knocked = tokio::time::Instant::now();
        guest.say(guest_socket.local_addr().unwrap());
        let mut buf = [0u8; 256];
        tokio::time::timeout(Duration::from_secs(10), guest_socket.recv_from(&mut buf))
            .await
            .expect("the host never punched")
            .unwrap();
        let punched = knocked.elapsed();
        let answer = tokio::time::timeout(Duration::from_secs(10), guest.heard())
            .await
            .expect("the host never answered")
            .unwrap();

        assert_eq!(answer.addr, host_at);
        assert!(
            punched < Duration::from_millis(400),
            "the punch came {punched:?} after the knock: it waited for the DHT poll"
        );
        host.abort();
    }

    /// ADR 0116: a host whose public address moves says so on the relays at
    /// once, so a guest dialing the old one is redirected mid-dial.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_host_whose_address_moves_says_so_on_the_relays() {
        let relay = Relay::start().await;
        let rendezvous = signalling_through(&relay);
        let (tap, watched) = tokio::sync::watch::channel(Some("203.0.113.7:4115".parse().unwrap()));
        let host = tokio::spawn(serve_rendezvous(
            rendezvous.clone(),
            identity(1),
            INVITE,
            watched,
            UdpSocket::bind("127.0.0.1:0").unwrap(),
            None,
        ));
        let mut guest = rendezvous
            .guest_signals(
                &INVITE,
                NodeId::from_bytes(&identity(1).verifying_key().to_bytes()).unwrap(),
            )
            .unwrap();
        relay
            .wait_for("both subscriptions", |seen| seen.filters.len() == 2)
            .await;

        let moved: SocketAddr = "203.0.113.8:5226".parse().unwrap();
        // What the STUN tap does when a keep-alive answer names a new address.
        tap.send(Some(moved)).unwrap();
        let heard = tokio::time::timeout(Duration::from_secs(10), guest.heard())
            .await
            .expect("the move was never said")
            .unwrap();
        assert_eq!(heard.addr, moved);
        host.abort();
    }

    /// ADR 0116 end to end: the ticket's address is dead and the DHT knows
    /// nothing, so the only way to the host is the relays — the knock goes
    /// out on them, the host answers with where it is, and the dial gets
    /// there within its first attempt's budget, because the answer starts a
    /// fresh attempt instead of waiting out the one aimed at the dead
    /// address.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_guest_reaches_a_moved_host_through_the_relays_alone() {
        let relay = Relay::start().await;
        let (reflector, reflector_task) = spawn_reflector().await;
        let host_identity = identity(1);
        let host_id = NodeId::from_bytes(&host_identity.verifying_key().to_bytes()).unwrap();
        let host = bind_host_via(
            &INVITE,
            &host_identity,
            &[&reflector.to_string()],
            Some(signalling_through(&relay)),
        )
        .await
        .unwrap();
        let pinned = host.cert_fingerprint;
        let serving = tokio::spawn(async move {
            let connection = host.accept().await.unwrap().unwrap();
            let peer = connection.peer();
            (peer, connection, host)
        });
        relay
            .wait_for("the host's subscription", |seen| seen.filters.len() == 1)
            .await;

        let stale = UdpSocket::bind("127.0.0.1:0").unwrap();
        let guest = GuestObfuscatedEndpoint::bind_via(
            &INVITE,
            &identity(2),
            host_id,
            stale.local_addr().unwrap(),
            pinned,
            Some(signalling_through(&relay)),
            leaked_reflectors(reflector.to_string()),
        )
        .unwrap();
        let dialing = tokio::time::Instant::now();
        let connection = guest.connect_control().await.unwrap();
        let took = dialing.elapsed();

        assert_eq!(connection.peer(), host_id);
        assert!(
            took < Duration::from_millis(OBFUSCATED_PUNCH_ATTEMPT_TIMEOUT_MS),
            "the dial took {took:?}: the answer did not cut the attempt at the dead address short"
        );
        let (peer, _host_side, host) = serving.await.unwrap();
        assert_eq!(
            peer,
            NodeId::from_bytes(&identity(2).verifying_key().to_bytes()).unwrap()
        );
        connection.close(0u32.into(), b"done");
        guest.close().await;
        host.close().await;
        reflector_task.abort();
    }

    /// ADR 0134: a live session that goes silent knocks again, so the host
    /// punches towards wherever this guest is now — and one that is only
    /// exchanging keep-alives never does.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_live_session_that_goes_silent_knocks_again() {
        let relay = Relay::start().await;
        let (reflector, reflector_task) = spawn_reflector().await;
        // No rendezvous on the host: its answers would point the guest past
        // the NAT this test needs the path to go through.
        let host = bind_host_via(&INVITE, &identity(1), &[], None)
            .await
            .unwrap();
        let host_addr = SocketAddr::new(
            Ipv4Addr::LOCALHOST.into(),
            host.local_addr().unwrap().port(),
        );
        let fingerprint = host.cert_fingerprint;
        let mut nat = MovingNat::spawn(host_addr).await;
        let host_side = tokio::spawn(async move {
            let connection = host.accept().await.unwrap().unwrap();
            let (mut send, mut recv) = connection.accept_bi().await.unwrap();
            let mut byte = [0u8; 1];
            while recv.read_exact(&mut byte).await.is_ok() {
                if send.write_all(&byte).await.is_err() {
                    break;
                }
            }
            drop(connection);
            host
        });

        let guest = GuestObfuscatedEndpoint::bind_via(
            &INVITE,
            &identity(2),
            NodeId::from_bytes(&identity(1).verifying_key().to_bytes()).unwrap(),
            nat.public[0],
            fingerprint,
            Some(signalling_through(&relay)),
            leaked_reflectors(reflector.to_string()),
        )
        .unwrap();
        let connection = guest.connect_control().await.unwrap();
        let (mut send, mut recv) = connection.open_bi().await.unwrap();
        assert!(echoed(&mut send, &mut recv, 1).await);
        relay
            .wait_for("the dial's own knock", |seen| !seen.events.is_empty())
            .await;
        let knocks = || relay.seen(|seen| seen.events.len());

        let idle = knocks();
        tokio::time::sleep(Duration::from_millis(OBFUSCATED_SILENCE_REKNOCK_MS + 1_000)).await;
        assert_eq!(knocks(), idle, "a session exchanging keep-alives knocked");

        nat.black_out();
        let silent = tokio::time::Instant::now();
        relay
            .wait_for("a knock after the host went quiet", |seen| {
                seen.events.len() > idle
            })
            .await;
        // The silence began with the host's last keep-alive, which may be
        // before the black-out: it is what the knock waits on.
        let quiet = guest.route.silent_for().unwrap();
        assert!(
            quiet >= Duration::from_millis(OBFUSCATED_SILENCE_REKNOCK_MS),
            "knocked {quiet:?} into the silence: before the threshold"
        );
        let took = silent.elapsed();
        assert!(
            took < Duration::from_millis(OBFUSCATED_SILENCE_REKNOCK_MS + 1_000),
            "knocked only {took:?} after the host went quiet"
        );

        // The path comes back inside the idle timeout: the session is the
        // same one it was.
        nat.move_to(0);
        assert!(echoed(&mut send, &mut recv, 2).await);

        connection.close(noq::VarInt::from_u32(0), b"");
        guest.close().await;
        host_side.abort();
        nat.tasks.abort_all();
        reflector_task.abort();
    }

    /// ADR 0148: a dial whose host is not listening yet — one restarting into
    /// an update — knocks again while it hears nothing. The relays keep no
    /// knock, so the dial's first one is lost on such a host, and a host that
    /// comes up mid-dial would otherwise not hear this guest until the dial's
    /// next attempt.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_dial_that_hears_nothing_knocks_again() {
        let relay = Relay::start().await;
        let (reflector, reflector_task) = spawn_reflector().await;
        // Bound and never read: where the host was, and will be again.
        let nobody = UdpSocket::bind("127.0.0.1:0").unwrap();
        let guest = GuestObfuscatedEndpoint::bind_via(
            &INVITE,
            &identity(2),
            NodeId::from_bytes(&identity(1).verifying_key().to_bytes()).unwrap(),
            nobody.local_addr().unwrap(),
            [0u8; 32],
            Some(signalling_through(&relay)),
            leaked_reflectors(reflector.to_string()),
        )
        .unwrap();
        let dialing = tokio::spawn(async move {
            let dialed = guest.connect_control().await;
            (dialed.is_ok(), guest)
        });
        relay
            .wait_for("the dial's own knock", |seen| !seen.events.is_empty())
            .await;
        let first = tokio::time::Instant::now();
        relay
            .wait_for("a knock while the dial heard nothing", |seen| {
                seen.events.len() >= 2
            })
            .await;
        let took = first.elapsed();

        let again = Duration::from_secs(RENDEZVOUS_REPUNCH_SECS);
        assert!(
            took + Duration::from_millis(SILENCE_CHECK_MS) >= again,
            "knocked again {took:?} after the first: more often than the host punches"
        );
        assert!(
            took < again + Duration::from_secs(1),
            "knocked again only {took:?} after the first"
        );
        let (connected, guest) = dialing.await.unwrap();
        assert!(!connected, "nothing was there to connect to");
        guest.close().await;
        drop(nobody);
        reflector_task.abort();
    }

    /// ADR 0116: an endpoint that goes away — closed or just dropped — takes
    /// its relay connections with it.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_endpoint_that_goes_away_leaves_no_relay_connection_open() {
        let relay = Relay::start().await;
        let (reflector, reflector_task) = spawn_reflector().await;
        let host = bind_host_via(
            &INVITE,
            &identity(1),
            &[&reflector.to_string()],
            Some(signalling_through(&relay)),
        )
        .await
        .unwrap();
        relay
            .wait_for("the host's connection", |seen| seen.open == 1)
            .await;
        host.close().await;
        relay
            .wait_for("the host's connection closing", |seen| seen.open == 0)
            .await;

        let guest = GuestObfuscatedEndpoint::bind_via(
            &INVITE,
            &identity(2),
            NodeId::from_bytes(&identity(1).verifying_key().to_bytes()).unwrap(),
            "127.0.0.1:9".parse().unwrap(),
            [0; 32],
            Some(signalling_through(&relay)),
            leaked_reflectors(reflector.to_string()),
        )
        .unwrap();
        let rendezvous = guest.rendezvous.as_ref().unwrap();
        assert!(guest.signal_speaker(rendezvous).is_some());
        relay
            .wait_for("the guest's connection", |seen| seen.open == 1)
            .await;
        drop(guest);
        relay
            .wait_for("the guest's connection closing", |seen| seen.open == 0)
            .await;
        reflector_task.abort();
    }
}
