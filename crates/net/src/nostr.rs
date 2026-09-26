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
    SIGNAL_CONNECT_TIMEOUT_SECS, SIGNAL_OUTBOX_FRESH_SECS, SIGNAL_PING_SECS,
    SIGNAL_RECONNECT_CEILING_SECS, SIGNAL_SILENCE_SECS,
};
use n0_future::{SinkExt as _, StreamExt as _};
use noq::rustls;
use rand::{Rng as _, RngExt as _};
use sha2::{Digest as _, Sha256};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc};
use tokio_websockets::{ClientBuilder, Connector, MaybeTlsStream, Message, WebSocketStream};

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
        let request: Arc<str> = Arc::from(subscription(&topic));
        let tasks = relays
            .iter()
            .map(|relay| {
                tokio::spawn(serve_relay(
                    Arc::from(relay.as_ref()),
                    Arc::clone(&request),
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
    request: Arc<str>,
    topic: Arc<str>,
    mut outbox: broadcast::Receiver<Outgoing>,
    inbox: mpsc::Sender<Heard>,
) {
    let mut failures = 0u32;
    loop {
        let started = Instant::now();
        match serve_connection(&relay, &request, &topic, &mut outbox, &inbox).await {
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
    request: &str,
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
        .send(Message::text(request.to_owned()))
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

/// Opens a WebSocket to `relay`, a `wss://` URL: TCP to its first IPv4
/// address (a machine with no route to the IPv6 internet is common, ADR
/// 0095), TLS against the web PKI, then the upgrade.
async fn connect(relay: &str) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, String> {
    let builder = ClientBuilder::new().uri(relay).map_err(|e| e.to_string())?;
    let host = relay
        .strip_prefix("wss://")
        .ok_or_else(|| format!("not a wss:// relay: {relay}"))?
        .split(['/', ':'])
        .next()
        .unwrap_or_default()
        .to_owned();
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 443))
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
    let tls = tls_connector()
        .wrap(&host, tcp)
        .await
        .map_err(|e| e.to_string())?;
    let (socket, _) = builder.connect_on(tls).await.map_err(|e| e.to_string())?;
    Ok(socket)
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
}
