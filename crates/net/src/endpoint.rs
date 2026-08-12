//! Wrapper over `iroh::Endpoint` (design doc §4, §4.1).
//!
//! One endpoint serves three ALPNs, each on its own QUIC connection, so that
//! media load or a file transfer can never delay a revoke on the control
//! channel.

use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr};

use crate::error::{NetError, Result};

/// Control channel ALPN: `Hello`/consent/input/clipboard. Opened first (§4.1).
pub const ALPN_CONTROL: &[u8] = b"rd/control/1";
/// Media channel ALPN: video and audio. Opened after `ConsentGrant(view)` (§4.1).
pub const ALPN_MEDIA: &[u8] = b"rd/media/1";
/// File channel ALPN. Opened lazily only after `FileAccept(true)` and closed
/// when the transfer finishes or is cancelled (§4, §4.1).
pub const ALPN_FILE: &[u8] = b"rd/file/1";

/// Every ALPN this build speaks, in the order they may be opened.
pub const SUPPORTED_ALPNS: [&[u8]; 3] = [ALPN_CONTROL, ALPN_MEDIA, ALPN_FILE];

fn alpn_list() -> Vec<Vec<u8>> {
    SUPPORTED_ALPNS.iter().map(|a| (*a).to_vec()).collect()
}

/// Owner of the Iroh endpoint and its per-ALPN accept loops.
#[derive(Debug, Clone)]
pub struct PeerEndpoint {
    inner: Endpoint,
}

impl PeerEndpoint {
    /// Binds an endpoint using the long-term identity from the OS keystore
    /// (§7, §11.2), with relays and address lookup enabled.
    ///
    /// # Errors
    /// [`NetError::Endpoint`] if binding or discovery setup fails.
    pub async fn bind(secret_key: iroh::SecretKey) -> Result<Self> {
        let inner = Endpoint::builder(presets::N0)
            .secret_key(secret_key)
            .alpns(alpn_list())
            .bind()
            .await
            .map_err(|e| NetError::Endpoint(e.to_string()))?;
        Ok(Self { inner })
    }

    /// Binds an endpoint without relays or address lookup: the peers have to
    /// know each other's direct addresses. Used on a LAN and by the tests.
    ///
    /// # Errors
    /// [`NetError::Endpoint`] if binding fails.
    pub async fn bind_local(secret_key: iroh::SecretKey) -> Result<Self> {
        let inner = Endpoint::builder(presets::Minimal)
            .secret_key(secret_key)
            .alpns(alpn_list())
            .bind()
            .await
            .map_err(|e| NetError::Endpoint(e.to_string()))?;
        Ok(Self { inner })
    }

    /// Local endpoint identity. In iroh 1.0 this is `EndpointId`, which is the
    /// same `PublicKey` the design doc calls `NodeId`.
    #[must_use]
    pub fn node_id(&self) -> lumepeer_core::NodeId {
        self.inner.id()
    }

    /// Address to put into an invite ticket (§7).
    #[must_use]
    pub fn addr(&self) -> EndpointAddr {
        self.inner.addr()
    }

    /// Waits until the endpoint has reached a relay, so that [`Self::addr`] is
    /// dialable from outside the local network.
    pub async fn online(&self) {
        self.inner.online().await;
    }

    /// Dials `addr` on the control ALPN. Media and file connections are opened
    /// separately and later, never as part of this one (§4.1).
    ///
    /// # Errors
    /// [`NetError::Dial`] if no connection can be established.
    pub async fn connect_control(
        &self,
        addr: impl Into<EndpointAddr>,
    ) -> Result<iroh::endpoint::Connection> {
        self.connect(addr, ALPN_CONTROL).await
    }

    /// Dials `addr` on one specific ALPN.
    ///
    /// # Errors
    /// [`NetError::Dial`] if no connection can be established.
    pub async fn connect(
        &self,
        addr: impl Into<EndpointAddr>,
        alpn: &[u8],
    ) -> Result<iroh::endpoint::Connection> {
        self.inner
            .connect(addr, alpn)
            .await
            .map_err(|e| NetError::Dial(e.to_string()))
    }

    /// Accepts the next incoming connection, or `None` once the endpoint is
    /// closed. The ALPN is available on the returned connection and decides
    /// which channel it belongs to (§4.1).
    ///
    /// # Errors
    /// [`NetError::Io`] if the handshake of an incoming connection fails; the
    /// caller keeps accepting afterwards.
    pub async fn accept(&self) -> Option<Result<iroh::endpoint::Connection>> {
        let incoming = self.inner.accept().await?;
        Some(incoming.await.map_err(|e| NetError::Io(e.to_string())))
    }

    /// Borrows the underlying endpoint.
    #[must_use]
    pub const fn inner(&self) -> &Endpoint {
        &self.inner
    }

    /// Closes the endpoint and every connection it owns.
    pub async fn close(self) {
        self.inner.close().await;
    }
}
