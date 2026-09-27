//! A minimal Nostr client: the push channel of the rendezvous (ADR 0116).
//!
//! The DHT half of the rendezvous (ADR 0113) can only be polled, and a poll
//! is a lookup of several seconds and some 15 KB. Polled often enough for a
//! guest to be answered while it is still dialing, it costs a host tens of
//! megabytes a day; polled rarely, a knock is never answered in time. Public
//! Nostr relays push instead: a host keeps one idle WebSocket per relay with a
//! subscription on it, and a guest's event reaches it a fraction of a second
//! after the guest sent it.
//!
//! Only what that needs is here: connect, subscribe to one topic, publish an
//! event on it, receive the events others publish on it. The events are of an
//! *ephemeral* kind (NIP-01: 20000-29999), which relays forward to live
//! subscribers and never store. What they carry is opaque to this module — the
//! rendezvous seals it with a key derived from the invite, so a relay sees a
//! random topic and random bytes.
//!
//! Every relay gets its own task and its own connection, and a message goes
//! out on all of them: one relay that an ISP freezes or blocks costs nothing
//! while another one answers. A freeze is silent — the flow simply stops, no
//! RST — so each connection pings and is replaced when it has been silent too
//! long ([`SIGNAL_SILENCE_SECS`]).

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use data_encoding::HEXLOWER;
use k256::schnorr::SigningKey;
use lumepeer_core::constants::{
    SIGNAL_CONNECT_TIMEOUT_SECS, SIGNAL_MAX_MESSAGE_BYTES, SIGNAL_OUTBOX_FRESH_SECS,
    SIGNAL_PING_SECS, SIGNAL_RECONNECT_CEILING_SECS, SIGNAL_SILENCE_SECS,
};
use n0_future::{SinkExt as _, StreamExt as _};
use noq::rustls;
use rand::{Rng as _, RngExt as _};
use sha2::{Digest as _, Sha256};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc};
use tokio_websockets::{
    ClientBuilder, Connector, Limits, MaybeTlsStream, Message, WebSocketStream,
};

/// Public relays the rendezvous signals through (ADR 0116).
///
/// Chosen by measurement from the host that needed it (`beta`, Rostelecom,
/// 2026-09-26): each one answered a subscription there, and the first six
/// delivered hundreds of kilobytes on one connection without the freeze that
/// Russian TSPU applies to many foreign hosting ranges. The last two freeze
/// after about 15 KB per connection, which a signalling channel never comes
/// near before its next reconnect, so they still count. They are spread over
/// different operators and networks (Cloudflare, Japan, independent hosts) so
/// that no single block takes the channel down.
pub const SIGNAL_RELAYS: &[&str] = &[
    "wss://relay.primal.net",
    "wss://relay.coinos.io",
    "wss://relay.snort.social",
    "wss://offchain.pub",
    "wss://nostr.bitcoiner.social",
    "wss://relay.nostr.wirednet.jp",
    "wss://relay.damus.io",
    "wss://nos.lol",
];

/// The event kind signalling uses: in NIP-01's ephemeral range, so relays
/// forward it to whoever is subscribed right now and store nothing.
pub const SIGNAL_EVENT_KIND: u32 = 25_116;

/// The subscription id this client uses on every connection. Each connection
/// carries exactly one subscription, so it needs no more than a constant.
const SUBSCRIPTION_ID: &str = "lp";

/// Messages the outbox holds for a relay that is still connecting.
const OUTBOX_CAPACITY: usize = 32;

/// Received events buffered between the relay tasks and the reader.
const INBOX_CAPACITY: usize = 64;

/// Event ids remembered to drop the copies of one event that every relay
/// forwards.
const SEEN_CAPACITY: usize = 256;

/// A connection that lasted at least this long was a working one: the next
/// failure starts the backoff over.
const HEALTHY_SESSION: Duration = Duration::from_mins(1);

/// One message queued for every relay.
#[derive(Debug, Clone)]
struct Outgoing {
    /// When it was queued, so a relay that connects late can tell a message
    /// that is still worth sending from one whose dial is long over.
    at: Instant,
    /// The whole `["EVENT", {...}]` frame.
    frame: Arc<str>,
}

/// One event a relay forwarded.
#[derive(Debug)]
struct Heard {
    id: String,
    content: String,
}

/// A live presence on the signalling relays for one topic.
///
/// Opening it spawns one task per relay; dropping it stops them all. It needs
/// a tokio runtime to be opened in.
pub struct Channel {
    publisher: Publisher,
    inbox: mpsc::Receiver<Heard>,
    seen: VecDeque<String>,
    tasks: Vec<tokio::task::AbortHandle>,
}

impl std::fmt::Debug for Channel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Channel")
            .field("topic", &self.publisher.topic)
            .field("relays", &self.tasks.len())
            .finish_non_exhaustive()
    }
}

impl Channel {
    /// Connects to every relay in `relays` and subscribes to `topic` on each.
    ///
    /// Returns at once: the connections come up in the background, and a
    /// message published before a relay is ready is sent to it as soon as it
    /// is, provided it is still fresh ([`SIGNAL_OUTBOX_FRESH_SECS`]).
    #[must_use]
    pub fn open<S: AsRef<str>>(relays: &[S], topic: &str) -> Self {
        let (outbox, _) = broadcast::channel(OUTBOX_CAPACITY);
        let (inbox_tx, inbox) = mpsc::channel(INBOX_CAPACITY);
        let topic: Arc<str> = Arc::from(topic);
        let tasks = relays
            .iter()
            .map(|relay| {
                tokio::spawn(serve_relay(
                    Arc::from(relay.as_ref()),
                    Arc::clone(&topic),
                    outbox.subscribe(),
                    inbox_tx.clone(),
                ))
                .abort_handle()
            })
            .collect();
        Self {
            publisher: Publisher {
                outbox,
                topic,
                key: Arc::new(random_key()),
            },
            inbox,
            seen: VecDeque::with_capacity(SEEN_CAPACITY),
            tasks,
        }
    }

    /// Publishes `content` on this channel's topic, on every relay.
    pub fn publish(&self, content: &str) {
        self.publisher.publish(content);
    }

    /// A handle that publishes on this channel from somewhere else.
    #[must_use]
    pub fn publisher(&self) -> Publisher {
        self.publisher.clone()
    }

    /// The content of the next event someone published on this topic, each
    /// event once however many relays forwarded it. Includes this channel's
    /// own events when a relay echoes them; the caller tells them apart by
    /// what they say.
    ///
    /// `None` only once every relay task has ended, which does not happen
    /// while the channel is alive.
    pub async fn recv(&mut self) -> Option<String> {
        loop {
            let heard = self.inbox.recv().await?;
            if self.seen.contains(&heard.id) {
                continue;
            }
            if self.seen.len() == SEEN_CAPACITY {
                self.seen.pop_front();
            }
            self.seen.push_back(heard.id);
            return Some(heard.content);
        }
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Publishes on a [`Channel`]'s topic. Cheap to clone; publishing after the
/// channel is gone does nothing.
#[derive(Clone)]
pub struct Publisher {
    outbox: broadcast::Sender<Outgoing>,
    topic: Arc<str>,
    /// This channel's own Nostr key. Random and thrown away with the channel:
    /// it authenticates nothing here (the content is sealed by the caller),
    /// and a fresh one per channel links no two dials together.
    key: Arc<SigningKey>,
}

impl std::fmt::Debug for Publisher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Publisher")
            .field("topic", &self.topic)
            .finish_non_exhaustive()
    }
}

impl Publisher {
    /// Publishes `content` on the topic, on every relay.
    pub fn publish(&self, content: &str) {
        let Some(frame) = event_frame(&self.key, &self.topic, content, unix_now()) else {
            tracing::warn!("could not sign a signalling event");
            return;
        };
        // No receiver means no relay task is left: nothing to do.
        let _ = self.outbox.send(Outgoing {
            at: Instant::now(),
            frame: Arc::from(frame),
        });
    }
}

/// A fresh random secp256k1 key.
fn random_key() -> SigningKey {
    loop {
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        // Fails only for zero or a value above the group order: a chance of
        // about 2^-128 per draw.
        if let Ok(key) = SigningKey::from_bytes(&bytes) {
            return key;
        }
    }
}

/// The `REQ` that subscribes to `topic`: live events only, since the kind is
/// ephemeral and nothing older is stored anyway.
///
/// Built for each connection, not once per channel: a relay that does keep
/// the kind answers a `since` with everything after it, and one fixed when
/// the channel opened would have every reconnect replay every knock since —
/// each one a punch and an answer out of a host that is long past it.
fn subscription(topic: &str) -> String {
    serde_json::json!([
        "REQ",
        SUBSCRIPTION_ID,
        {"kinds": [SIGNAL_EVENT_KIND], "#t": [topic], "since": unix_now().saturating_sub(10)}
    ])
    .to_string()
}

/// A signed `["EVENT", {...}]` frame carrying `content` on `topic`
/// (NIP-01), or `None` if signing failed.
fn event_frame(key: &SigningKey, topic: &str, content: &str, created_at: u64) -> Option<String> {
    let pubkey = HEXLOWER.encode(&key.verifying_key().to_bytes());
    let tags = serde_json::json!([["t", topic]]);
    // NIP-01: the id is the SHA-256 of this exact array, serialized with no
    // whitespace. serde_json writes it that way, and nothing in it here (hex
    // and a fixed tag name) needs escaping.
    let commitment =
        serde_json::json!([0, pubkey, created_at, SIGNAL_EVENT_KIND, tags, content]).to_string();
    let id: [u8; 32] = Sha256::digest(commitment.as_bytes()).into();
    let mut aux = [0u8; 32];
    rand::rng().fill_bytes(&mut aux);
    let signature = key.sign_raw(&id, &aux).ok()?;
    Some(
        serde_json::json!([
            "EVENT",
            {
                "id": HEXLOWER.encode(&id),
                "pubkey": pubkey,
                "created_at": created_at,
                "kind": SIGNAL_EVENT_KIND,
                "tags": tags,
                "content": content,
                "sig": HEXLOWER.encode(&signature.to_bytes()),
            }
        ])
        .to_string(),
    )
}

/// What one frame from a relay means to this client.
#[derive(Debug, PartialEq, Eq)]
enum Frame {
    /// An event on this client's subscription and topic.
    Event { id: String, content: String },
    /// The relay ended this client's subscription.
    Closed(String),
    /// Anything else: an `OK`, an `EOSE`, a `NOTICE`, another kind.
    Other,
}

/// Reads one text frame from a relay. Everything a relay sends is untrusted
/// input, so nothing here trusts a shape it has not checked (§2.4).
fn parse_frame(text: &str, topic: &str) -> Frame {
    let Ok(serde_json::Value::Array(parts)) = serde_json::from_str::<serde_json::Value>(text)
    else {
        return Frame::Other;
    };
    match (
        parts.first().and_then(|v| v.as_str()),
        parts.get(1).and_then(|v| v.as_str()),
    ) {
        (Some("EVENT"), Some(SUBSCRIPTION_ID)) => {}
        (Some("CLOSED"), Some(SUBSCRIPTION_ID)) => {
            let reason = parts.get(2).and_then(|v| v.as_str()).unwrap_or_default();
            return Frame::Closed(reason.to_owned());
        }
        _ => return Frame::Other,
    }
    let Some(event) = parts.get(2) else {
        return Frame::Other;
    };
    let kind = event.get("kind").and_then(serde_json::Value::as_u64);
    let on_topic = event
        .get("tags")
        .and_then(|v| v.as_array())
        .is_some_and(|tags| {
            tags.iter().any(|tag| {
                tag.as_array().is_some_and(|tag| {
                    tag.first().and_then(|v| v.as_str()) == Some("t")
                        && tag.get(1).and_then(|v| v.as_str()) == Some(topic)
                })
            })
        });
    let (Some(id), Some(content)) = (
        event.get("id").and_then(|v| v.as_str()),
        event.get("content").and_then(|v| v.as_str()),
    ) else {
        return Frame::Other;
    };
    if kind != Some(u64::from(SIGNAL_EVENT_KIND)) || !on_topic {
        return Frame::Other;
    }
    Frame::Event {
        id: id.to_owned(),
        content: content.to_owned(),
    }
}

/// One relay, for the life of its channel: connect, serve, and when the
/// connection fails, wait and connect again.
async fn serve_relay(
    relay: Arc<str>,
    topic: Arc<str>,
    mut outbox: broadcast::Receiver<Outgoing>,
    inbox: mpsc::Sender<Heard>,
) {
    let mut failures = 0u32;
    loop {
        let started = Instant::now();
        match serve_connection(&relay, &topic, &mut outbox, &inbox).await {
            Ok(()) => return,
            Err(error) => tracing::debug!(%relay, %error, "signalling relay connection ended"),
        }
        if started.elapsed() >= HEALTHY_SESSION {
            failures = 0;
        }
        failures = failures.saturating_add(1);
        let pause = reconnect_pause(failures);
        tokio::select! {
            () = tokio::time::sleep(pause) => {}
            () = inbox.closed() => return,
        }
    }
}

/// Pause before the `failures`-th reconnect to one relay: one second doubling
/// to [`SIGNAL_RECONNECT_CEILING_SECS`], plus up to a second of jitter so the
/// relays of one channel do not all come back in the same instant.
fn reconnect_pause(failures: u32) -> Duration {
    let base = 1_u64
        .checked_shl(failures.saturating_sub(1).min(16))
        .unwrap_or(u64::MAX)
        .min(SIGNAL_RECONNECT_CEILING_SECS);
    Duration::from_secs(base) + Duration::from_millis(rand::rng().random_range(0..1_000))
}

/// One connection to one relay. `Ok` when the channel is gone and the task
/// should end; `Err` when the connection failed and should be replaced.
async fn serve_connection(
    relay: &str,
    topic: &str,
    outbox: &mut broadcast::Receiver<Outgoing>,
    inbox: &mpsc::Sender<Heard>,
) -> Result<(), String> {
    let mut socket = tokio::time::timeout(
        Duration::from_secs(SIGNAL_CONNECT_TIMEOUT_SECS),
        connect(relay),
    )
    .await
    .map_err(|_| "connect timed out".to_owned())??;
    socket
        .send(Message::text(subscription(topic)))
        .await
        .map_err(|e| e.to_string())?;
    tracing::debug!(%relay, "signalling relay subscribed");

    let silence = Duration::from_secs(SIGNAL_SILENCE_SECS);
    let mut ping = tokio::time::interval(Duration::from_secs(SIGNAL_PING_SECS));
    ping.reset();
    let mut last_heard = Instant::now();
    loop {
        tokio::select! {
            frame = socket.next() => {
                let frame = frame
                    .ok_or_else(|| "closed by the relay".to_owned())?
                    .map_err(|e| e.to_string())?;
                last_heard = Instant::now();
                let Some(text) = frame.as_text() else {
                    continue;
                };
                match parse_frame(text, topic) {
                    Frame::Event { id, content } => {
                        if inbox.send(Heard { id, content }).await.is_err() {
                            return Ok(());
                        }
                    }
                    Frame::Closed(reason) => {
                        return Err(format!("the relay closed the subscription: {reason}"));
                    }
                    Frame::Other => {
                        tracing::trace!(%relay, frame = %text, "signalling relay said");
                    }
                }
            }
            outgoing = outbox.recv() => match outgoing {
                Ok(outgoing) => {
                    if outgoing.at.elapsed() <= Duration::from_secs(SIGNAL_OUTBOX_FRESH_SECS) {
                        socket
                            .send(Message::text(outgoing.frame.to_string()))
                            .await
                            .map_err(|e| e.to_string())?;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
            _ = ping.tick() => {
                if last_heard.elapsed() >= silence {
                    return Err("the relay went silent".to_owned());
                }
                socket
                    .send(Message::ping(&b"lp"[..]))
                    .await
                    .map_err(|e| e.to_string())?;
            }
            () = inbox.closed() => return Ok(()),
        }
    }
}

/// Opens a WebSocket to `relay`: TCP to its first IPv4 address (a machine
/// with no route to the IPv6 internet is common, ADR 0095), TLS against the
/// web PKI for a `wss://` relay, then the upgrade.
async fn connect(relay: &str) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, String> {
    // Bounded well below the library's default (ADR 0122): a relay is a
    // public server, and what it sends is only ever a small event.
    let builder = ClientBuilder::new()
        .uri(relay)
        .map_err(|e| e.to_string())?
        .limits(Limits::default().max_payload_len(Some(SIGNAL_MAX_MESSAGE_BYTES)));
    let (tls, host, port) =
        relay_target(relay).ok_or_else(|| format!("not a ws:// or wss:// relay: {relay}"))?;
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| e.to_string())?
        .collect();
    let addr = addrs
        .iter()
        .find(|addr| addr.is_ipv4())
        .or_else(|| addrs.first())
        .copied()
        .ok_or_else(|| format!("{host} did not resolve"))?;
    let tcp = TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
    let _ = tcp.set_nodelay(true);
    let stream = if tls {
        tls_connector()
            .wrap(host, tcp)
            .await
            .map_err(|e| e.to_string())?
    } else {
        MaybeTlsStream::Plain(tcp)
    };
    let (socket, _) = builder
        .connect_on(stream)
        .await
        .map_err(|e| e.to_string())?;
    Ok(socket)
}

/// Whether `relay` wants TLS, and the host and port to reach it on:
/// `wss://host[:port][/...]`, or `ws://` for a relay on a private test
/// network. `None` for any other scheme, an empty host, a port that is not
/// one, or an IPv6 literal, which no relay in use is.
fn relay_target(relay: &str) -> Option<(bool, &str, u16)> {
    let (tls, rest) = match relay.strip_prefix("wss://") {
        Some(rest) => (true, rest),
        None => (false, relay.strip_prefix("ws://")?),
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, port.parse().ok()?),
        None => (authority, if tls { 443 } else { 80 }),
    };
    (!host.is_empty() && !host.starts_with('[')).then_some((tls, host, port))
}

/// A TLS connector trusting the web PKI roots, built once per process.
fn tls_connector() -> &'static Connector {
    static CONNECTOR: std::sync::OnceLock<Connector> = std::sync::OnceLock::new();
    CONNECTOR.get_or_init(|| {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        // The safe defaults of a provider this crate already uses for QUIC
        // cannot be refused; if they ever were, the client config falls back
        // to the plain builder, which picks the same.
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_or_else(
                |_| rustls::ClientConfig::builder().with_root_certificates(roots.clone()),
                |builder| builder.with_root_certificates(roots.clone()),
            )
            .with_no_client_auth();
        Connector::Rustls(tokio_rustls::TlsConnector::from(Arc::new(config)))
    })
}

/// Seconds since the Unix epoch, or zero on a clock set before it.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::test_relay::Relay;
    use super::*;
    use k256::schnorr::{Signature, VerifyingKey};

    /// An event this client signs is one any relay accepts: the id is the
    /// NIP-01 hash of its fields and the signature verifies under its pubkey.
    #[test]
    fn an_event_carries_its_nip01_id_and_a_valid_signature() {
        let key = random_key();
        let frame = event_frame(&key, "abcd", "0011", 1_790_000_000).unwrap();
        let value: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(value[0], "EVENT");
        let event = &value[1];

        let commitment = serde_json::json!([
            0,
            event["pubkey"],
            event["created_at"],
            event["kind"],
            event["tags"],
            event["content"]
        ])
        .to_string();
        let id: [u8; 32] = Sha256::digest(commitment.as_bytes()).into();
        assert_eq!(event["id"].as_str().unwrap(), HEXLOWER.encode(&id));
        assert_eq!(event["kind"], SIGNAL_EVENT_KIND);

        let pubkey = HEXLOWER
            .decode(event["pubkey"].as_str().unwrap().as_bytes())
            .unwrap();
        let signature = HEXLOWER
            .decode(event["sig"].as_str().unwrap().as_bytes())
            .unwrap();
        let verifying = VerifyingKey::from_bytes(&pubkey).unwrap();
        let signature = Signature::try_from(signature.as_slice()).unwrap();
        verifying.verify_raw(&id, &signature).unwrap();
    }

    #[test]
    fn only_events_on_this_subscription_kind_and_topic_are_taken() {
        let key = random_key();
        let frame = event_frame(&key, "abcd", "0011", 1).unwrap();
        let value: serde_json::Value = serde_json::from_str(&frame).unwrap();
        let forwarded = serde_json::json!(["EVENT", SUBSCRIPTION_ID, value[1]]).to_string();

        match parse_frame(&forwarded, "abcd") {
            Frame::Event { content, .. } => assert_eq!(content, "0011"),
            other => panic!("expected the event, got {other:?}"),
        }
        assert_eq!(parse_frame(&forwarded, "ffff"), Frame::Other);
        let other_sub = serde_json::json!(["EVENT", "zz", value[1]]).to_string();
        assert_eq!(parse_frame(&other_sub, "abcd"), Frame::Other);
        assert_eq!(
            parse_frame(r#"["CLOSED","lp","rate-limited"]"#, "abcd"),
            Frame::Closed("rate-limited".to_owned())
        );
        for junk in [
            "",
            "{}",
            "[]",
            r#"["EVENT"]"#,
            r#"["EVENT","lp",7]"#,
            "\u{0}",
        ] {
            assert_eq!(parse_frame(junk, "abcd"), Frame::Other, "{junk:?}");
        }
    }

    #[test]
    fn the_reconnect_pause_doubles_to_its_ceiling() {
        let secs = |failures| reconnect_pause(failures).as_secs();
        assert_eq!(secs(1), 1);
        assert_eq!(secs(2), 2);
        assert_eq!(secs(3), 4);
        assert_eq!(secs(40), SIGNAL_RECONNECT_CEILING_SECS);
    }

    /// A relay is dialed on the port its URL names: a relay on a port of its
    /// own used to be dialed on 443 whatever the URL said.
    #[test]
    fn a_relay_url_names_its_scheme_host_and_port() {
        assert_eq!(
            relay_target("wss://relay.damus.io"),
            Some((true, "relay.damus.io", 443))
        );
        assert_eq!(
            relay_target("wss://relay.example:7777/nostr"),
            Some((true, "relay.example", 7777))
        );
        assert_eq!(
            relay_target("ws://127.0.0.1:4000"),
            Some((false, "127.0.0.1", 4000))
        );
        assert_eq!(
            relay_target("ws://relay.local/"),
            Some((false, "relay.local", 80))
        );
        for bad in [
            "https://relay.damus.io",
            "relay.damus.io",
            "wss://",
            "wss://:443",
            "wss://relay.example:port",
            "wss://relay.example:70000",
            "wss://[::1]:443",
        ] {
            assert_eq!(relay_target(bad), None, "{bad}");
        }
    }

    /// How long a test waits for something a loopback relay delivers before
    /// calling it lost.
    const DELIVERY: Duration = Duration::from_secs(10);
    /// How long a test waits to be sure something is *not* delivered.
    const QUIET: Duration = Duration::from_millis(400);

    /// The next event `channel` hears, or a failure naming `what`.
    async fn next(channel: &mut Channel, what: &str) -> String {
        tokio::time::timeout(DELIVERY, channel.recv())
            .await
            .unwrap_or_else(|_| panic!("{what} never arrived"))
            .expect("the channel is alive")
    }

    /// Asserts `channel` hears nothing more for a while.
    async fn silent(channel: &mut Channel, what: &str) {
        if let Ok(heard) = tokio::time::timeout(QUIET, channel.recv()).await {
            panic!("{what}, but heard {heard:?}");
        }
    }

    /// The `["EVENT", "lp", {...}]` a relay forwards for an event this
    /// client signs, with the parts a test wants to vary.
    fn forwarded(subscription: &str, topic: &str, kind: u32, content: &str) -> String {
        let frame = event_frame(&random_key(), topic, content, unix_now()).unwrap();
        let mut frame: serde_json::Value = serde_json::from_str(&frame).unwrap();
        let mut event = frame[1].take();
        event["kind"] = kind.into();
        serde_json::json!(["EVENT", subscription, event]).to_string()
    }

    /// The whole round trip on one relay: what one channel publishes reaches
    /// the other channel on the topic, once, and comes back to its sender
    /// as relays echo it.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_event_reaches_the_other_channel_on_the_topic_once() {
        let relay = Relay::start().await;
        let mut guest = Channel::open(&[&relay.url], "t1");
        let mut host = Channel::open(&[&relay.url], "t1");
        relay
            .wait_for("both subscriptions", |seen| seen.filters.len() == 2)
            .await;

        guest.publish("aa");
        assert_eq!(next(&mut host, "the guest's event").await, "aa");
        assert_eq!(next(&mut guest, "the relay's echo").await, "aa");
        silent(&mut host, "one event is heard once").await;
    }

    /// A message published while the relay cannot be reached yet goes out
    /// the moment it can — the guest knocks before its relays have answered.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_message_published_before_the_relay_is_up_goes_out_when_it_is() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        // Nothing listens on `port` now: the first connect is refused.
        let early = Channel::open(&[format!("ws://{port}")], "t1");
        early.publish("early");
        tokio::time::sleep(Duration::from_millis(300)).await;

        let relay = Relay::start_at(port).await;
        relay
            .wait_for("the message published before it was up", |seen| {
                seen.events.iter().any(|event| event["content"] == "early")
            })
            .await;
        drop(early);
    }

    /// Every relay forwards the same event; the reader hears it once.
    #[tokio::test(flavor = "multi_thread")]
    async fn copies_of_one_event_from_several_relays_are_heard_once() {
        let first = Relay::start().await;
        let second = Relay::start().await;
        let relays = [&first.url, &second.url];
        let guest = Channel::open(&relays, "t1");
        let mut host = Channel::open(&relays, "t1");
        for relay in [&first, &second] {
            relay
                .wait_for("both subscriptions", |seen| seen.filters.len() == 2)
                .await;
        }

        guest.publish("once");
        assert_eq!(next(&mut host, "the event").await, "once");
        silent(&mut host, "the second relay's copy must be dropped").await;
        for relay in [&first, &second] {
            assert_eq!(relay.seen(|seen| seen.events.len()), 1);
        }
    }

    /// What a relay says off this channel's subscription, topic or kind, and
    /// what is not a message at all, never reaches the reader — and does not
    /// cost the connection either.
    #[tokio::test(flavor = "multi_thread")]
    async fn only_this_channels_events_reach_it_whatever_a_relay_sends() {
        let relay = Relay::start().await;
        let mut channel = Channel::open(&[&relay.url], "t1");
        relay
            .wait_for("the subscription", |seen| seen.filters.len() == 1)
            .await;

        for junk in [
            forwarded(SUBSCRIPTION_ID, "t2", SIGNAL_EVENT_KIND, "other topic"),
            forwarded("zz", "t1", SIGNAL_EVENT_KIND, "other subscription"),
            forwarded(SUBSCRIPTION_ID, "t1", 1, "a stored kind"),
            "not json".to_owned(),
            r#"["NOTICE","slow down"]"#.to_owned(),
            r#"["EOSE","lp"]"#.to_owned(),
            r#"["OK","00",false,"blocked"]"#.to_owned(),
        ] {
            relay.say(&junk);
        }
        relay.say(&forwarded(SUBSCRIPTION_ID, "t1", SIGNAL_EVENT_KIND, "mine"));

        assert_eq!(next(&mut channel, "the one real event").await, "mine");
        assert_eq!(
            relay.seen(|seen| seen.accepted),
            1,
            "junk is no reason to reconnect"
        );
    }

    /// A relay that ends the subscription is subscribed to again, and the new
    /// subscription asks for events from *now*: one that asked from when the
    /// channel opened would have a relay that keeps ephemeral events replay
    /// every knock since then, and the host punch towards each of them.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_closed_subscription_is_renewed_from_the_time_it_is_renewed() {
        let relay = Relay::start().await;
        let mut channel = Channel::open(&[&relay.url], "t1");
        relay
            .wait_for("the subscription", |seen| seen.filters.len() == 1)
            .await;

        relay.say(r#"["CLOSED","lp","error: shutting down"]"#);
        relay
            .wait_for("the renewed subscription", |seen| seen.filters.len() == 2)
            .await;
        let (first, renewed) = relay.seen(|seen| {
            (
                seen.filters[0]["since"].as_u64().unwrap(),
                seen.filters[1]["since"].as_u64().unwrap(),
            )
        });
        assert!(
            renewed > first,
            "the renewed subscription asked from {renewed}, the first from {first}"
        );

        let other = Channel::open(&[&relay.url], "t1");
        relay
            .wait_for("the other subscription", |seen| seen.filters.len() == 3)
            .await;
        other.publish("after");
        assert_eq!(
            next(&mut channel, "an event after the renewal").await,
            "after"
        );
    }

    /// A relay that drops the connection is dialed again, and what is
    /// published afterwards arrives.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_dropped_connection_is_replaced() {
        let relay = Relay::start().await;
        let mut channel = Channel::open(&[&relay.url], "t1");
        relay
            .wait_for("the subscription", |seen| seen.filters.len() == 1)
            .await;

        relay.kick();
        relay
            .wait_for("the connection again", |seen| {
                seen.accepted == 2 && seen.filters.len() == 2
            })
            .await;
        let other = Channel::open(&[&relay.url], "t1");
        relay
            .wait_for("the other subscription", |seen| seen.filters.len() == 3)
            .await;
        other.publish("again");
        assert_eq!(next(&mut channel, "an event after the drop").await, "again");
    }

    /// A relay that cannot be reached at all costs nothing while another one
    /// carries the messages both ways.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_dead_relay_costs_nothing_while_another_answers() {
        let dead = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let relay = Relay::start().await;
        let mut both = Channel::open(&[format!("ws://{dead}"), relay.url.clone()], "t1");
        let mut live = Channel::open(std::slice::from_ref(&relay.url), "t1");
        relay
            .wait_for("both subscriptions", |seen| seen.filters.len() == 2)
            .await;

        live.publish("to both");
        assert_eq!(next(&mut both, "the live relay's event").await, "to both");
        both.publish("from both");
        assert_eq!(
            next(&mut live, "an event out through the live relay").await,
            "to both"
        );
        assert_eq!(
            next(&mut live, "an event out through the live relay").await,
            "from both"
        );
    }

    /// Dropping a channel closes every relay connection it holds: an endpoint
    /// that is gone must not keep eight sockets open.
    #[tokio::test(flavor = "multi_thread")]
    async fn dropping_the_channel_closes_its_connections() {
        let first = Relay::start().await;
        let second = Relay::start().await;
        let channel = Channel::open(&[&first.url, &second.url], "t1");
        for relay in [&first, &second] {
            relay
                .wait_for("the connection", |seen| seen.open == 1)
                .await;
        }

        drop(channel);
        for relay in [&first, &second] {
            relay
                .wait_for("the connection closing", |seen| seen.open == 0)
                .await;
        }
    }
}

/// A stand-in for a public relay on loopback, for the tests of this module
/// and of the rendezvous built on it (ADR 0116).
#[cfg(test)]
pub(crate) mod test_relay {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use n0_future::{SinkExt as _, StreamExt as _};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::broadcast;
    use tokio_websockets::{Message, ServerBuilder};

    /// What a test makes every open connection of a relay do.
    #[derive(Debug, Clone)]
    enum Control {
        /// Send this text frame as it is.
        Raw(String),
        /// Drop the connection without a close frame, as a relay that
        /// restarts does.
        Kick,
    }

    /// What a relay has seen, for a test to assert on.
    #[derive(Debug, Default)]
    pub(crate) struct Seen {
        /// The filter of every `REQ`, in the order they came.
        pub(crate) filters: Vec<serde_json::Value>,
        /// Every event published to it, in order.
        pub(crate) events: Vec<serde_json::Value>,
        /// Connections accepted so far.
        pub(crate) accepted: usize,
        /// Connections open right now.
        pub(crate) open: usize,
    }

    /// Speaks just enough NIP-01: remembers each connection's subscription,
    /// answers every `EVENT` with `OK`, and forwards it to every connection
    /// subscribed to its topic — the sender's own included, as public relays
    /// do.
    pub(crate) struct Relay {
        /// Where to reach it: `ws://127.0.0.1:<port>`.
        pub(crate) url: String,
        seen: Arc<Mutex<Seen>>,
        control: broadcast::Sender<Control>,
        listener: tokio::task::JoinHandle<()>,
    }

    impl Relay {
        pub(crate) async fn start() -> Self {
            Self::start_at("127.0.0.1:0".parse().unwrap()).await
        }

        pub(crate) async fn start_at(addr: SocketAddr) -> Self {
            let listener = TcpListener::bind(addr).await.unwrap();
            let url = format!("ws://{}", listener.local_addr().unwrap());
            let seen = Arc::new(Mutex::new(Seen::default()));
            let (control, _) = broadcast::channel(64);
            let (events, _) = broadcast::channel(64);
            let accepting = tokio::spawn({
                let seen = Arc::clone(&seen);
                let control = control.clone();
                async move {
                    while let Ok((stream, _)) = listener.accept().await {
                        tokio::spawn(serve(
                            stream,
                            Arc::clone(&seen),
                            events.clone(),
                            control.subscribe(),
                        ));
                    }
                }
            });
            Self {
                url,
                seen,
                control,
                listener: accepting,
            }
        }

        /// Sends `text` to every open connection.
        pub(crate) fn say(&self, text: &str) {
            let _ = self.control.send(Control::Raw(text.to_owned()));
        }

        /// Drops every open connection.
        pub(crate) fn kick(&self) {
            let _ = self.control.send(Control::Kick);
        }

        /// Reads what it has seen so far.
        pub(crate) fn seen<T>(&self, read: impl FnOnce(&Seen) -> T) -> T {
            read(&self.seen.lock().unwrap())
        }

        /// Waits until what it has seen satisfies `ready`, or fails naming
        /// `what` after ten seconds.
        pub(crate) async fn wait_for(&self, what: &str, ready: impl Fn(&Seen) -> bool) {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while !self.seen(&ready) {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the relay never saw {what}: {:?}",
                    self.seen.lock().unwrap()
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }

    impl Drop for Relay {
        fn drop(&mut self) {
            self.listener.abort();
            self.kick();
        }
    }

    async fn serve(
        stream: TcpStream,
        seen: Arc<Mutex<Seen>>,
        events: broadcast::Sender<serde_json::Value>,
        mut control: broadcast::Receiver<Control>,
    ) {
        let Ok((_, mut socket)) = ServerBuilder::new().accept(stream).await else {
            return;
        };
        {
            let mut seen = seen.lock().unwrap();
            seen.accepted += 1;
            seen.open += 1;
        }
        let mut forwarded = events.subscribe();
        // This connection's subscription: its id and its topic.
        let mut subscription: Option<(String, String)> = None;
        loop {
            tokio::select! {
                frame = socket.next() => {
                    let Some(Ok(frame)) = frame else { break };
                    let Some(text) = frame.as_text() else { continue };
                    let Ok(message) = serde_json::from_str::<serde_json::Value>(text) else {
                        continue;
                    };
                    match message[0].as_str() {
                        Some("REQ") => {
                            seen.lock().unwrap().filters.push(message[2].clone());
                            subscription = Some((
                                message[1].as_str().unwrap_or_default().to_owned(),
                                message[2]["#t"][0].as_str().unwrap_or_default().to_owned(),
                            ));
                        }
                        Some("EVENT") => {
                            let event = message[1].clone();
                            seen.lock().unwrap().events.push(event.clone());
                            let ok = serde_json::json!(["OK", event["id"], true, ""]).to_string();
                            if socket.send(Message::text(ok)).await.is_err() {
                                break;
                            }
                            let _ = events.send(event);
                        }
                        _ => {}
                    }
                }
                event = forwarded.recv() => {
                    let Ok(event) = event else { continue };
                    let Some((id, topic)) = &subscription else { continue };
                    if event["tags"][0][1].as_str() != Some(topic.as_str()) {
                        continue;
                    }
                    let frame = serde_json::json!(["EVENT", id, event]).to_string();
                    if socket.send(Message::text(frame)).await.is_err() {
                        break;
                    }
                }
                control = control.recv() => match control {
                    Ok(Control::Raw(text)) => {
                        if socket.send(Message::text(text)).await.is_err() {
                            break;
                        }
                    }
                    Ok(Control::Kick) | Err(_) => break,
                },
            }
        }
        seen.lock().unwrap().open -= 1;
    }
}
