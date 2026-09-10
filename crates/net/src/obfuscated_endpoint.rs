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

use std::net::{SocketAddr, ToSocketAddrs as _, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use lumepeer_core::NodeId;
use lumepeer_core::constants::{
    NAT_MAPPING_KEEPALIVE_SECS, OBFUSCATED_CONNECT_ATTEMPTS, OBFUSCATED_CONNECT_RETRY_BACKOFF_MS,
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

use crate::endpoint::SUPPORTED_ALPNS;
use crate::error::{NetError, Result};
use crate::obfuscate::{ObfuscatedSocket, Obfuscator, obfuscated_transport_config};
use crate::peer_connection::PeerConnection;
use crate::stun;
use crate::ticket::INVITE_ID_BYTES;

/// Public STUN reflectors tried in order, same list `examples/stun_probe.rs`
/// uses (task 17, ADR 0052/0053).
const STUN_SERVERS: &[&str] = &[
    "stun.cloudflare.com:3478",
    "stun.l.google.com:19302",
    "stun1.l.google.com:19302",
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
/// A fresh certificate every bind, over a key that never changes: the serial
/// and validity differ per bind, so the fingerprint an invite pins is the one
/// this endpoint presents and no other, while the identity inside it stays the
/// node's own.
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

    /// Aborts the keep-alive task, if one is running. Idempotent.
    fn stop_keepalive(&mut self) {
        if let Some(task) = self.keepalive.take() {
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
        let incoming = self.endpoint.accept().await?;
        Some(
            match incoming.await.map_err(|e| NetError::Io(e.to_string())) {
                Ok(connection) => peer_and_alpn(&connection)
                    .map(|(peer, alpn)| PeerConnection::from_obfuscated(connection, peer, alpn)),
                Err(error) => Err(error),
            },
        )
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
) -> Result<HostObfuscatedEndpoint> {
    bind_host_via(invite_id, identity, STUN_SERVERS).await
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
) -> Result<HostObfuscatedEndpoint> {
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| NetError::Endpoint(e.to_string()))?;
    let probe_socket = socket
        .try_clone()
        .map_err(|e| NetError::Endpoint(e.to_string()))?;
    let keepalive_socket = socket
        .try_clone()
        .map_err(|e| NetError::Endpoint(e.to_string()))?;

    let resolved: Vec<SocketAddr> = servers
        .iter()
        .filter_map(|server| server.to_socket_addrs().ok().and_then(|mut a| a.next()))
        .collect();
    let (public_addr, stun_server) =
        tokio::task::spawn_blocking(move || discover_public_addr(&probe_socket, &resolved))
            .await
            .map_err(|e| NetError::Endpoint(e.to_string()))?;

    let keepalive = stun_server.map(|server| spawn_keepalive(keepalive_socket, server));

    let runtime: Arc<dyn noq::Runtime> = Arc::new(TokioRuntime);
    let wrapped = runtime
        .wrap_udp_socket(socket)
        .map_err(|e| NetError::Endpoint(e.to_string()))?;
    let obfuscated_socket: Box<dyn AsyncUdpSocket> = Box::new(ObfuscatedSocket::new(
        wrapped,
        Obfuscator::for_host(invite_id),
    ));

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
    })
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
/// `NAT_MAPPING_KEEPALIVE_SECS`, forever. The reply is not needed — only the
/// outbound packet, which is what a NAT counts to keep a mapping alive; a
/// failed/timed-out reply just means one keepalive tick, not the mapping,
/// was lost.
///
/// The handle is returned rather than dropped: the endpoint owns it and aborts
/// it on close, so a retired invite stops holding its mapping open
/// (gap-tasks/21 task 1; ADR 0080).
fn spawn_keepalive(mut socket: UdpSocket, server: SocketAddr) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(NAT_MAPPING_KEEPALIVE_SECS));
        // The STUN probe during bind already sent one packet; skip the
        // immediate first tick `interval` fires so ticks land on the
        // intended cadence from that point on.
        interval.tick().await;
        loop {
            interval.tick().await;
            let sent = tokio::task::spawn_blocking(move || {
                let _ = stun::reflexive_addr(&socket, server);
                socket
            })
            .await;
            match sent {
                Ok(returned) => socket = returned,
                // The blocking task panicked or was cancelled; the socket is
                // gone, so there is nothing left to keep the mapping with.
                Err(_) => return,
            }
        }
    })
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
    /// The host's address from the invite ticket (ADR 0053).
    target: SocketAddr,
    /// Blake3 of the host certificate the ticket pinned.
    expected_fingerprint: [u8; 32],
    /// This guest's own certificate, presented on every channel so the host
    /// learns which `NodeId` is dialing it.
    certificate: Arc<IdentityCertificate>,
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
    /// Nothing is dialed here — [`Self::connect`] opens each channel — so a
    /// bound endpoint costs one UDP socket and no traffic at all.
    ///
    /// # Errors
    /// [`NetError::Endpoint`] if the socket, the certificate or the `noq`
    /// endpoint cannot be built.
    pub fn bind(
        invite_id: &[u8; INVITE_ID_BYTES],
        identity: &SigningKey,
        target: SocketAddr,
        expected_fingerprint: [u8; 32],
    ) -> Result<Self> {
        let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| NetError::Endpoint(e.to_string()))?;
        let runtime: Arc<dyn noq::Runtime> = Arc::new(TokioRuntime);
        let wrapped = runtime
            .wrap_udp_socket(socket)
            .map_err(|e| NetError::Endpoint(e.to_string()))?;
        let obfuscated_socket: Box<dyn AsyncUdpSocket> = Box::new(ObfuscatedSocket::new(
            wrapped,
            Obfuscator::for_guest(invite_id),
        ));
        let endpoint = Endpoint::new_with_abstract_socket(
            EndpointConfig::default(),
            None,
            obfuscated_socket,
            runtime,
        )
        .map_err(|e| NetError::Endpoint(e.to_string()))?;

        Ok(Self {
            endpoint,
            target,
            expected_fingerprint,
            certificate: Arc::new(identity_certificate(identity)?),
        })
    }

    /// Opens one channel to the host, on `alpn`.
    ///
    /// TLS is pinned to the fingerprint the ticket carried rather than
    /// validated against a CA (ADR 0053: there is no CA for an ad-hoc host
    /// certificate, and the real authentication is the `invite_id`-derived
    /// AEAD layer beneath this handshake). Retries up to
    /// [`OBFUSCATED_CONNECT_ATTEMPTS`] times,
    /// [`OBFUSCATED_CONNECT_RETRY_BACKOFF_MS`] apart, since the host's NAT
    /// mapping may not accept the very first packet.
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

        let mut last = NetError::Dial("no attempt was made".to_owned());
        for attempt in 1..=OBFUSCATED_CONNECT_ATTEMPTS {
            let result = async {
                self.endpoint
                    .connect_with(client_config.clone(), self.target, CERT_SUBJECT)
                    .map_err(|e| NetError::Dial(e.to_string()))?
                    .await
                    .map_err(|e| NetError::Dial(e.to_string()))
            }
            .await;
            match result {
                Ok(connection) => {
                    let (peer, negotiated) = peer_and_alpn(&connection)?;
                    return Ok(PeerConnection::from_obfuscated(
                        connection, peer, negotiated,
                    ));
                }
                Err(error) => last = error,
            }
            if attempt < OBFUSCATED_CONNECT_ATTEMPTS {
                tokio::time::sleep(Duration::from_millis(OBFUSCATED_CONNECT_RETRY_BACKOFF_MS))
                    .await;
            }
        }
        Err(last)
    }

    /// Closes the endpoint and every channel it carries.
    pub async fn close(&self) {
        self.endpoint.close(noq::VarInt::from_u32(0), b"");
        self.endpoint.wait_idle().await;
    }
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
        let host = bind_host_via(&INVITE, &identity(1), &[&reflector.to_string()])
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

        let host = bind_host_via(&INVITE, &identity(1), &[&reflector.to_string()])
            .await
            .unwrap();
        assert!(host.public_addr.is_none());
        assert!(
            host.keepalive_probe().is_none(),
            "there is no mapping worth holding open without an address"
        );
        host.close().await;
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
        let host = bind_host_via(&INVITE, &host_identity, &[]).await.unwrap();
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

        let guest =
            GuestObfuscatedEndpoint::bind(&INVITE, &guest_identity, target, fingerprint).unwrap();
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
}
