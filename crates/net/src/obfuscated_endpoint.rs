//! Binding the persistent obfuscated QUIC endpoint on each side (task 17
//! increment 2, ADR 0053): the host discovers its public address via STUN
//! and keeps its NAT mapping open until a guest dials in; the guest dials
//! that address with TLS pinned to the host's cert fingerprint rather than a
//! CA. Both build on increment 1's `ObfuscatedSocket`/`Obfuscator`
//! (`crate::obfuscate`) unchanged.
//!
//! Neither side is wired into the live app yet — that is increment 3
//! (ADR 0052 roadmap item 3). This module is exercised today by
//! `examples/obfuscated_wan_probe.rs`.

use std::net::{SocketAddr, ToSocketAddrs as _, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use lumepeer_core::constants::{
    NAT_MAPPING_KEEPALIVE_SECS, OBFUSCATED_CONNECT_ATTEMPTS, OBFUSCATED_CONNECT_RETRY_BACKOFF_MS,
};
use noq::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use noq::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use noq::rustls::{DigitallySignedStruct, SignatureScheme};
use noq::{
    AsyncUdpSocket, ClientConfig, Connection, Endpoint, EndpointConfig, ServerConfig, TokioRuntime,
};

use crate::error::{NetError, Result};
use crate::obfuscate::{ObfuscatedSocket, Obfuscator, obfuscated_transport_config};
use crate::stun;
use crate::ticket::INVITE_ID_BYTES;

/// Public STUN reflectors tried in order, same list `examples/stun_probe.rs`
/// uses (task 17, ADR 0052/0053).
const STUN_SERVERS: &[&str] = &[
    "stun.cloudflare.com:3478",
    "stun.l.google.com:19302",
    "stun1.l.google.com:19302",
];

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
}

impl std::fmt::Debug for HostObfuscatedEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostObfuscatedEndpoint")
            .field("public_addr", &self.public_addr)
            .finish_non_exhaustive()
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
/// channel to coordinate). The task runs for the process's lifetime; there is
/// no shutdown handle yet (increment 3 wires that into the app's invite
/// lifecycle).
///
/// # Errors
/// [`NetError::Endpoint`] if the socket cannot be bound, cloned, or the `noq`
/// endpoint cannot be constructed.
pub async fn bind_host(invite_id: &[u8; INVITE_ID_BYTES]) -> Result<HostObfuscatedEndpoint> {
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| NetError::Endpoint(e.to_string()))?;
    let probe_socket = socket
        .try_clone()
        .map_err(|e| NetError::Endpoint(e.to_string()))?;
    let keepalive_socket = socket
        .try_clone()
        .map_err(|e| NetError::Endpoint(e.to_string()))?;

    let (public_addr, stun_server) =
        tokio::task::spawn_blocking(move || discover_public_addr(&probe_socket))
            .await
            .map_err(|e| NetError::Endpoint(e.to_string()))?;

    if let Some(server) = stun_server {
        spawn_keepalive(keepalive_socket, server);
    }

    let runtime: Arc<dyn noq::Runtime> = Arc::new(TokioRuntime);
    let wrapped = runtime
        .wrap_udp_socket(socket)
        .map_err(|e| NetError::Endpoint(e.to_string()))?;
    let obfuscated_socket: Box<dyn AsyncUdpSocket> = Box::new(ObfuscatedSocket::new(
        wrapped,
        Obfuscator::for_host(invite_id),
    ));

    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])
        .map_err(|e| NetError::Endpoint(e.to_string()))?;
    let cert_der = cert.cert.der().clone();
    let cert_fingerprint: [u8; 32] = *blake3::hash(&cert_der).as_bytes();
    let key = PrivateKeyDer::Pkcs8(cert.signing_key.serialize_der().into());

    let mut server_config = ServerConfig::with_single_cert(vec![cert_der], key)
        .map_err(|e| NetError::Endpoint(e.to_string()))?;
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
    })
}

/// Tries each of `STUN_SERVERS` in turn on `socket`, returning the first
/// reflexive address found and the server that answered (so the caller can
/// keep hitting the same one to hold the mapping open). `None` if no server
/// answered.
fn discover_public_addr(socket: &UdpSocket) -> (Option<SocketAddr>, Option<SocketAddr>) {
    for server in STUN_SERVERS {
        let Some(resolved) = server.to_socket_addrs().ok().and_then(|mut a| a.next()) else {
            continue;
        };
        if let Ok(reflexive) = stun::reflexive_addr(socket, resolved) {
            return (Some(reflexive), Some(resolved));
        }
    }
    (None, None)
}

/// Resends a STUN request to `server` on `socket` every
/// `NAT_MAPPING_KEEPALIVE_SECS`, forever. The reply is not needed — only the
/// outbound packet, which is what a NAT counts to keep a mapping alive; a
/// failed/timed-out reply just means one keepalive tick, not the mapping,
/// was lost.
fn spawn_keepalive(mut socket: UdpSocket, server: SocketAddr) {
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
    });
}

/// Dials a host's obfuscated endpoint at `target`, pinning TLS verification
/// to `expected_fingerprint` rather than validating a CA (ADR 0053: there is
/// no CA for an ad-hoc host cert, and the real authentication is the
/// `invite_id`-derived AEAD layer beneath this handshake).
///
/// Retries up to `OBFUSCATED_CONNECT_ATTEMPTS` times, `OBFUSCATED_CONNECT_RETRY_BACKOFF_MS`
/// apart, since the host's NAT mapping may not have accepted the very first
/// packet.
///
/// # Errors
/// [`NetError::Endpoint`] if the local socket or endpoint cannot be built;
/// [`NetError::Dial`] if every connect attempt fails.
pub async fn connect_guest(
    invite_id: &[u8; INVITE_ID_BYTES],
    target: SocketAddr,
    expected_fingerprint: [u8; 32],
) -> Result<Connection> {
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

    let provider = Arc::new(noq::rustls::crypto::ring::default_provider());
    let rustls_config = noq::rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&noq::rustls::version::TLS13])
        .map_err(|e| NetError::Endpoint(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCertVerifier {
            expected_fingerprint,
        }))
        .with_no_client_auth();
    let quic_client_config = noq::crypto::rustls::QuicClientConfig::try_from(rustls_config)
        .map_err(|e| NetError::Endpoint(e.to_string()))?;
    let mut client_config = ClientConfig::new(Arc::new(quic_client_config));
    client_config.transport_config(Arc::new(obfuscated_transport_config()));
    endpoint.set_default_client_config(client_config);

    let mut last = NetError::Dial("no attempt was made".to_owned());
    for attempt in 1..=OBFUSCATED_CONNECT_ATTEMPTS {
        let result = async {
            endpoint
                .connect(target, "localhost")
                .map_err(|e| NetError::Dial(e.to_string()))?
                .await
                .map_err(|e| NetError::Dial(e.to_string()))
        }
        .await;
        match result {
            Ok(connection) => return Ok(connection),
            Err(error) => last = error,
        }
        if attempt < OBFUSCATED_CONNECT_ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(OBFUSCATED_CONNECT_RETRY_BACKOFF_MS)).await;
        }
    }
    Err(last)
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
