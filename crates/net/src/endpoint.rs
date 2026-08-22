//! Wrapper over `iroh::Endpoint` (design doc §4, §4.1).
//!
//! One endpoint serves three ALPNs, each on its own QUIC connection, so that
//! media load or a file transfer can never delay a revoke on the control
//! channel.

use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, RelayMode};
use iroh_base::RelayUrl;

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

/// Environment variable that puts the direct IP transports — the LAN path —
/// back on. See [`lan_direct_enabled`].
pub const LAN_DIRECT_ENV: &str = "LUMEPEER_LAN_DIRECT";

/// Whether this process may use direct IP paths (which is what makes two peers
/// on the same LAN talk without ever leaving it).
///
/// **Temporarily off by default.** WAN connectivity is being verified, and as
/// long as a direct path exists two machines on one network reach each other
/// without the relay, so a passing test proves nothing about the internet
/// path. With the direct transports cleared the only route left is the relay,
/// i.e. out to the internet and back — which is exactly what has to be proven
/// to work.
///
/// Nothing is deleted: set `LUMEPEER_LAN_DIRECT=1` (or `true`/`yes`/`on`) and
/// [`PeerEndpoint::bind`] is the full relay-plus-direct endpoint again.
#[must_use]
pub fn lan_direct_enabled() -> bool {
    match std::env::var(LAN_DIRECT_ENV) {
        Ok(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
}

/// Owner of the Iroh endpoint and its per-ALPN accept loops.
#[derive(Debug, Clone)]
pub struct PeerEndpoint {
    inner: Endpoint,
}

/// Environment variable overriding the relay URL clients reach for when hole
/// punching fails (docs/relay-deployment.md). Unset means iroh's default
/// public relay fleet.
const RELAY_URL_ENV: &str = "LUMEPEER_RELAY_URL";

impl PeerEndpoint {
    /// Binds an endpoint using the long-term identity from the OS keystore
    /// (§7, §11.2), with relays and address lookup enabled.
    ///
    /// Which transports it gets is decided by [`lan_direct_enabled`]: today
    /// that means [`Self::bind_relay_only`] unless `LUMEPEER_LAN_DIRECT` is
    /// set, in which case it is [`Self::bind_with_lan`].
    ///
    /// # Errors
    /// [`NetError::Endpoint`] if binding or discovery setup fails.
    pub async fn bind(secret_key: iroh::SecretKey) -> Result<Self> {
        if lan_direct_enabled() {
            Self::bind_with_lan(secret_key).await
        } else {
            Self::bind_relay_only(secret_key).await
        }
    }

    /// Binds the full endpoint: relays, address lookup **and** direct IP
    /// paths, so two peers on one network can hole-punch onto a LAN path and
    /// never touch the relay. This is the shipping behaviour; it is reached
    /// through [`Self::bind`] only while `LUMEPEER_LAN_DIRECT` is set.
    ///
    /// # Errors
    /// [`NetError::Endpoint`] if binding or discovery setup fails.
    pub async fn bind_with_lan(secret_key: iroh::SecretKey) -> Result<Self> {
        let mut builder = Endpoint::builder(presets::N0)
            .secret_key(secret_key)
            .alpns(alpn_list());
        builder = with_relay_override(builder);
        let inner = builder
            .bind()
            .await
            .map_err(|e| NetError::Endpoint(e.to_string()))?;
        Ok(Self { inner })
    }

    /// Binds with relays and address lookup but **no** IP transports at all.
    ///
    /// Without an IP transport there is no direct path to hole-punch onto and
    /// no LAN shortcut to fall into: every packet of every session goes over
    /// the relay, out to the internet and back. That makes a session between
    /// two machines that happen to share a network an honest test of the WAN
    /// path, and it makes the address in an invite ticket relay-only, since
    /// `EndpointAddr` can then hold nothing else.
    ///
    /// # Errors
    /// [`NetError::Endpoint`] if binding or discovery setup fails.
    pub async fn bind_relay_only(secret_key: iroh::SecretKey) -> Result<Self> {
        let mut builder = Endpoint::builder(presets::N0)
            .clear_ip_transports()
            .secret_key(secret_key)
            .alpns(alpn_list());
        builder = with_relay_override(builder);
        let inner = builder
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
        let incoming = self.accept_incoming().await?;
        Some(Self::finish_accept(incoming).await)
    }

    /// Accepts the next incoming connection *without* awaiting its QUIC
    /// handshake, or `None` once the endpoint is closed.
    ///
    /// Unlike [`Self::accept`] this is a single await, so it is safe to drop
    /// inside a `tokio::select!`: no connection can be lost between the two
    /// stages. The caller finishes the handshake with [`Self::finish_accept`],
    /// normally on its own task so that a slow peer cannot stall the loop.
    pub async fn accept_incoming(&self) -> Option<iroh::endpoint::Incoming> {
        self.inner.accept().await
    }

    /// Completes the QUIC handshake of a connection taken from
    /// [`Self::accept_incoming`].
    ///
    /// # Errors
    /// [`NetError::Io`] if the handshake fails.
    pub async fn finish_accept(
        incoming: iroh::endpoint::Incoming,
    ) -> Result<iroh::endpoint::Connection> {
        incoming.await.map_err(|e| NetError::Io(e.to_string()))
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
