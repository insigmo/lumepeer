//! One connection with one peer, whichever transport carries it (design doc
//! §4.1; gap-tasks/21 task 2, ADR 0080).
//!
//! Two transports reach a host: the iroh endpoint of `crate::endpoint`, and
//! the obfuscated direct QUIC of `crate::obfuscated_endpoint` (ADR 0052).
//! Everything above them — the handshake, consent, grants, revoke, the lazy
//! `rd/file/1` — is identical, and had to stay identical: a second copy of the
//! session logic under a second transport is a second place for an
//! authorization bug to live, which §2.3 does not allow. So the two are
//! narrowed to this one type here, and nothing above it knows which it holds.
//!
//! It is deliberately thin. Both transports are the same QUIC implementation —
//! iroh's `Connection` and `noq`'s are both `noq`, and iroh re-exports `noq`'s
//! `SendStream`/`RecvStream` unchanged — so every stream operation below is
//! one `match` and a delegation, with no wrapper types of its own and nothing
//! copied on the way through.
//!
//! What is *not* uniform is where the two facts above the transport come from.
//! On iroh, the peer's `NodeId` and the negotiated ALPN are properties of the
//! connection. On the obfuscated transport they are read once out of the TLS
//! handshake by whoever built the connection (`crate::obfuscated_endpoint`)
//! and carried here, because `noq` has no notion of an endpoint identity of
//! its own.

use iroh::endpoint::Connection as IrohConnection;
use lumepeer_core::NodeId;
use noq::{
    AcceptBi, AcceptUni, Connection as ObfuscatedConnection, ConnectionError, OpenBi, OpenUni,
    VarInt,
};

/// Which transport carries a [`PeerConnection`].
#[derive(Debug, Clone)]
enum Transport {
    /// The iroh endpoint: relay-capable, address-lookup-capable, and what
    /// every session used before the obfuscated transport existed.
    Iroh(IrohConnection),
    /// Direct obfuscated QUIC, never relayed (ADR 0052).
    Obfuscated(ObfuscatedConnection),
}

/// An established QUIC connection with one peer on one ALPN.
///
/// Cloning is cheap and shares the underlying connection, exactly as cloning
/// either wrapped type does: the connection lives as long as any handle does.
#[derive(Debug, Clone)]
pub struct PeerConnection {
    transport: Transport,
    peer: NodeId,
    alpn: Vec<u8>,
}

impl PeerConnection {
    /// Wraps a connection made or accepted through the iroh endpoint.
    ///
    /// The peer identity and ALPN are read off the connection here, once, so
    /// that reading them later costs nothing and cannot fail — the same shape
    /// the obfuscated side is forced into.
    #[must_use]
    pub fn from_iroh(connection: IrohConnection) -> Self {
        let peer = connection.remote_id();
        let alpn = connection.alpn().to_vec();
        Self {
            transport: Transport::Iroh(connection),
            peer,
            alpn,
        }
    }

    /// Wraps a connection made or accepted on the obfuscated transport.
    ///
    /// `peer` is the identity its TLS certificate proved and `alpn` the
    /// protocol its handshake negotiated; `crate::obfuscated_endpoint` reads
    /// both before calling this, since neither is something `noq` tracks.
    #[must_use]
    pub fn from_obfuscated(connection: ObfuscatedConnection, peer: NodeId, alpn: Vec<u8>) -> Self {
        Self {
            transport: Transport::Obfuscated(connection),
            peer,
            alpn,
        }
    }

    /// Authenticated identity of the peer at the other end (§7).
    ///
    /// Authenticated on both transports and by the same key: iroh's TLS binds
    /// the connection to the peer's endpoint key, and the obfuscated
    /// transport's certificates carry that same ed25519 identity, which is why
    /// one node has one `NodeId` however it was reached.
    #[must_use]
    pub const fn peer(&self) -> NodeId {
        self.peer
    }

    /// The negotiated ALPN, which decides the channel this connection carries
    /// ([`crate::Channel::from_alpn`], §4.1).
    #[must_use]
    pub fn alpn(&self) -> &[u8] {
        &self.alpn
    }

    /// Whether this connection is on the obfuscated transport rather than the
    /// iroh one.
    ///
    /// Only for reporting what a session is actually using — nothing may take
    /// a *decision* from it, because the transport says nothing about what a
    /// peer is allowed to do (§2.3).
    #[must_use]
    pub const fn is_obfuscated(&self) -> bool {
        matches!(self.transport, Transport::Obfuscated(_))
    }

    /// The underlying iroh connection, or `None` on the obfuscated transport.
    ///
    /// For the handful of diagnostics that are iroh's own — which of its paths
    /// are open, and through which relay. The obfuscated transport has no such
    /// notion: it is one direct path and never a relay (ADR 0052).
    #[must_use]
    pub const fn iroh(&self) -> Option<&IrohConnection> {
        match &self.transport {
            Transport::Iroh(connection) => Some(connection),
            Transport::Obfuscated(_) => None,
        }
    }

    /// Opens a bidirectional stream.
    #[must_use]
    pub fn open_bi(&self) -> OpenBi<'_> {
        match &self.transport {
            Transport::Iroh(connection) => connection.open_bi(),
            Transport::Obfuscated(connection) => connection.open_bi(),
        }
    }

    /// Accepts the next bidirectional stream the peer opens.
    #[must_use]
    pub fn accept_bi(&self) -> AcceptBi<'_> {
        match &self.transport {
            Transport::Iroh(connection) => connection.accept_bi(),
            Transport::Obfuscated(connection) => connection.accept_bi(),
        }
    }

    /// Opens a unidirectional stream.
    #[must_use]
    pub fn open_uni(&self) -> OpenUni<'_> {
        match &self.transport {
            Transport::Iroh(connection) => connection.open_uni(),
            Transport::Obfuscated(connection) => connection.open_uni(),
        }
    }

    /// Accepts the next unidirectional stream the peer opens.
    #[must_use]
    pub fn accept_uni(&self) -> AcceptUni<'_> {
        match &self.transport {
            Transport::Iroh(connection) => connection.accept_uni(),
            Transport::Obfuscated(connection) => connection.accept_uni(),
        }
    }

    /// Closes the connection with an application close code (§18).
    pub fn close(&self, error_code: VarInt, reason: &[u8]) {
        match &self.transport {
            Transport::Iroh(connection) => connection.close(error_code, reason),
            Transport::Obfuscated(connection) => connection.close(error_code, reason),
        }
    }

    /// Waits until the connection is closed, by either side or by the network.
    pub async fn closed(&self) -> ConnectionError {
        match &self.transport {
            Transport::Iroh(connection) => connection.closed().await,
            Transport::Obfuscated(connection) => connection.closed().await,
        }
    }

    /// Why the connection closed, or `None` while it is still live.
    #[must_use]
    pub fn close_reason(&self) -> Option<ConnectionError> {
        match &self.transport {
            Transport::Iroh(connection) => connection.close_reason(),
            Transport::Obfuscated(connection) => connection.close_reason(),
        }
    }
}
