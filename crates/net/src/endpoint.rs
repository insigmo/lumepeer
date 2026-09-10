//! Wrapper over `iroh::Endpoint` (design doc §4, §4.1).
//!
//! One endpoint serves three ALPNs, each on its own QUIC connection, so that
//! media load or a file transfer can never delay a revoke on the control
//! channel.

use iroh::endpoint::Builder as EndpointBuilder;
use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, RelayMode, RelayUrl};

use crate::error::{NetError, Result};

/// Control channel ALPN: `Hello`/consent/input/clipboard. Opened first (§4.1).
pub const ALPN_CONTROL: &[u8] = b"rd/control/1";
/// Media channel ALPN: video and audio. Opened after `ConsentGrant(view)` (§4.1).
pub const ALPN_MEDIA: &[u8] = b"rd/media/1";
/// File channel ALPN. Opened lazily only after `FileAccept(true)` and closed
/// when the transfer finishes or is cancelled (§4, §4.1).
pub const ALPN_FILE: &[u8] = b"rd/file/1";
/// Tunnel channel ALPN: forwarded TCP payload (§4.1; ADR 0078). Opened
/// lazily, only after the host has allowed a specific address, and torn down
/// with the `tunnel` grant.
///
/// Its own connection rather than a second use of `rd/file/1`, for the reason
/// every ALPN here is separate: a channel that is busy must not be able to
/// delay a revoke on another one, and a tunnel is the busiest thing a guest
/// can hold open.
pub const ALPN_TUNNEL: &[u8] = b"rd/tunnel/1";

/// Every ALPN this build speaks, in the order they may be opened.
pub const SUPPORTED_ALPNS: [&[u8]; 4] = [ALPN_CONTROL, ALPN_MEDIA, ALPN_FILE, ALPN_TUNNEL];

fn alpn_list() -> Vec<Vec<u8>> {
    SUPPORTED_ALPNS.iter().map(|a| (*a).to_vec()).collect()
}

/// Environment variable that takes the direct IP transports away, leaving the
/// relay as the only route. See [`relay_only_enabled`].
pub const RELAY_ONLY_ENV: &str = "LUMEPEER_RELAY_ONLY";

/// Whether this process must ignore direct IP paths and carry every session
/// over a relay.
///
/// **Off by default**, which is the shipping behaviour: direct paths are what
/// make a session fast, and clearing them makes every client depend on a relay
/// link staying healthy — a dependency that cost one release its connectivity
/// (ADR 0026).
///
/// Set `LUMEPEER_RELAY_ONLY=1` (or `true`/`yes`/`on`) to get the relay-only
/// endpoint back for a deliberate WAN test: with no IP transport there is no
/// direct path to hole-punch onto, so two machines that share a network still
/// have to talk through the internet (ADR 0020).
#[must_use]
pub fn relay_only_enabled() -> bool {
    match std::env::var(RELAY_ONLY_ENV) {
        Ok(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
}

/// Adds Mainline-DHT address lookup, so a peer can be found by its
/// `EndpointId` alone after its address has changed (ADR 0062).
///
/// This is what makes a saved device permanently dialable. Everything else in
/// this file locates a peer from addresses somebody wrote down earlier — the
/// ones baked into an invite ticket — and those stop being true the moment the
/// far machine reboots onto a new public IP. The DHT inverts that: the host
/// publishes a record signed by its own endpoint key under its own public key,
/// and a guest that knows only the key looks up wherever it is *now*.
///
/// No server of ours is involved, which is the point (ADR 0052's serverless
/// line): the Mainline DHT is the public `BitTorrent` one. `presets::N0` already
/// adds n0's pkarr relay and DNS lookup; this sits beside it, so a network
/// that blocks one can still be reached through the other.
///
/// `AddrFilter::unfiltered` on purpose. The default publishes relay addresses
/// only, which is exactly nothing for a host that cannot hold a relay — the
/// case this whole change is about. The cost is stated plainly: anyone holding
/// a host's `EndpointId` (that is, anyone holding its invite code) can read its
/// current IP addresses out of the DHT. That is the trade a permanent invite
/// code makes, and it is the same trade the code itself already made by
/// carrying an address in the first place.
///
/// A DHT node that cannot be built is a warning, never a failed bind: it is an
/// additional way to be found, and losing it must not cost the endpoint the
/// ways it already had (§18).
fn with_dht_lookup(builder: EndpointBuilder, secret_key: &iroh::SecretKey) -> EndpointBuilder {
    use iroh::address_lookup::AddrFilter;
    use iroh_mainline_address_lookup::DhtAddressLookup;

    match DhtAddressLookup::builder()
        .secret_key(secret_key.clone())
        .addr_filter(AddrFilter::unfiltered())
        .build()
    {
        Ok(lookup) => {
            tracing::info!("mainline DHT address lookup is on");
            builder.address_lookup(lookup)
        }
        Err(error) => {
            tracing::warn!(%error, "no mainline DHT address lookup: falling back to DNS only");
            builder
        }
    }
}

/// Owner of the Iroh endpoint and its per-ALPN accept loops.
#[derive(Debug, Clone)]
pub struct PeerEndpoint {
    inner: Endpoint,
}

/// Environment variable overriding the relay URL clients reach for when hole
/// punching fails (docs/relay-deployment.md). It wins over the configured
/// relay; with neither set, iroh's default public relay fleet is used.
pub const RELAY_URL_ENV: &str = "LUMEPEER_RELAY_URL";

/// Points a builder at a specific relay: `LUMEPEER_RELAY_URL` first, then
/// whatever `[network].relay_url` of the config carried (§7 fallback path,
/// docs/relay-deployment.md). Neither means iroh's public fleet.
///
/// A malformed URL is ignored with a warning rather than failing the bind:
/// binding must keep working offline, and the endpoint logs which relay it
/// actually reached either way.
fn with_relay(builder: EndpointBuilder, configured: Option<&str>) -> EndpointBuilder {
    let (source, url) = match std::env::var(RELAY_URL_ENV) {
        Ok(from_env) => (RELAY_URL_ENV, from_env),
        Err(_) => match configured {
            Some(from_config) => ("[network].relay_url", from_config.to_owned()),
            None => return builder,
        },
    };
    match url.parse::<RelayUrl>() {
        Ok(relay) => {
            tracing::info!(%url, %source, "using a configured relay");
            builder.relay_mode(RelayMode::custom([relay]))
        }
        Err(error) => {
            tracing::warn!(%error, %url, %source, "ignoring the relay: not a valid relay URL");
            builder
        }
    }
}

impl PeerEndpoint {
    /// Binds an endpoint using the long-term identity from the OS keystore
    /// (§7, §11.2), with relays, address lookup and direct IP paths enabled.
    ///
    /// `relay_url` is the relay from the configuration file, if the caller
    /// found one; `LUMEPEER_RELAY_URL` still wins over it.
    ///
    /// Which transports it gets is decided by [`relay_only_enabled`]: the
    /// default is [`Self::bind_with_lan`], and only an explicit
    /// `LUMEPEER_RELAY_ONLY` selects [`Self::bind_relay_only`].
    ///
    /// # Errors
    /// [`NetError::Endpoint`] if binding or discovery setup fails.
    pub async fn bind(secret_key: iroh::SecretKey, relay_url: Option<&str>) -> Result<Self> {
        if relay_only_enabled() {
            Self::bind_relay_only(secret_key, relay_url).await
        } else {
            Self::bind_with_lan(secret_key, relay_url).await
        }
    }

    /// Binds the full endpoint: relays, address lookup **and** direct IP
    /// paths, so two peers can hole-punch onto a direct path and only fall
    /// back to a relay when that fails (§5, `prefer_direct = true`). This is
    /// what [`Self::bind`] does unless `LUMEPEER_RELAY_ONLY` says otherwise.
    ///
    /// # Errors
    /// [`NetError::Endpoint`] if binding or discovery setup fails.
    pub async fn bind_with_lan(
        secret_key: iroh::SecretKey,
        relay_url: Option<&str>,
    ) -> Result<Self> {
        let mut builder = Endpoint::builder(presets::N0)
            .secret_key(secret_key.clone())
            .alpns(alpn_list());
        builder = with_relay(builder, relay_url);
        builder = with_dht_lookup(builder, &secret_key);
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
    /// Opt-in only (`LUMEPEER_RELAY_ONLY`): as the shipping default it made
    /// every session hostage to one relay link, which is how v0.0.14 reached
    /// users unable to connect at all (ADR 0026).
    ///
    /// # Errors
    /// [`NetError::Endpoint`] if binding or discovery setup fails.
    pub async fn bind_relay_only(
        secret_key: iroh::SecretKey,
        relay_url: Option<&str>,
    ) -> Result<Self> {
        let mut builder = Endpoint::builder(presets::N0)
            .clear_ip_transports()
            .secret_key(secret_key.clone())
            .alpns(alpn_list());
        builder = with_relay(builder, relay_url);
        builder = with_dht_lookup(builder, &secret_key);
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
