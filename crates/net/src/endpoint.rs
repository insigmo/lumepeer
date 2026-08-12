//! Wrapper over `iroh::Endpoint` (design doc §4, §4.1).
//!
//! One endpoint serves a router with three ALPNs, each on its own QUIC
//! connection, so that media load or a file transfer can never delay a revoke
//! on the control channel.

use crate::error::Result;

/// Control channel ALPN: `Hello`/consent/input/clipboard. Opened first (§4.1).
pub const ALPN_CONTROL: &[u8] = b"rd/control/1";
/// Media channel ALPN: video and audio. Opened after `ConsentGrant(view)` (§4.1).
pub const ALPN_MEDIA: &[u8] = b"rd/media/1";
/// File channel ALPN. Opened lazily only after `FileAccept(true)` and closed
/// when the transfer finishes or is cancelled (§4, §4.1).
pub const ALPN_FILE: &[u8] = b"rd/file/1";

/// Every ALPN this build speaks, in the order they may be opened.
pub const SUPPORTED_ALPNS: [&[u8]; 3] = [ALPN_CONTROL, ALPN_MEDIA, ALPN_FILE];

/// Owner of the Iroh endpoint and its per-ALPN accept loops.
#[derive(Debug)]
pub struct PeerEndpoint {
    inner: iroh::Endpoint,
}

impl PeerEndpoint {
    /// Binds an endpoint using the long-term identity from the OS keystore
    /// (§7, §11.2).
    ///
    /// # Errors
    /// [`crate::error::NetError::Endpoint`] if binding or discovery setup fails.
    pub fn bind(_secret_key: iroh::SecretKey) -> Result<Self> {
        todo!("phase 1: bind iroh endpoint with the three ALPNs of §4.1")
    }

    /// Local endpoint identity. In iroh 1.0 this is `EndpointId`, which is the
    /// same `PublicKey` the design doc calls `NodeId`.
    #[must_use]
    pub fn node_id(&self) -> lumepeer_core::NodeId {
        self.inner.id()
    }

    /// Borrows the underlying endpoint.
    #[must_use]
    pub const fn inner(&self) -> &iroh::Endpoint {
        &self.inner
    }

    /// Closes the endpoint and every connection it owns.
    pub async fn close(self) {
        self.inner.close().await;
    }
}
