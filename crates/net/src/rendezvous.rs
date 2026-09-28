//! Serverless rendezvous for the obfuscated transport, over the Mainline DHT
//! (ADR 0113).
//!
//! The obfuscated transport was shipped with one address — the one the invite
//! pinned — and no way for the host to punch back (ADR 0053, ADR 0082). Both
//! fail on a real network: a host that restarts or whose uplink changes is no
//! longer at the pinned address, and a host behind a NAT that filters by the
//! sender's address never lets the guest's first packet in. The iroh relay
//! could carry the coordination, but it is exactly the path a TLS-freezing ISP
//! breaks (ADR 0111), so this uses the public `BitTorrent` DHT instead: UDP, no
//! server of ours, and already in the process for iroh's address lookup.
//!
//! Two records, both sealed with a key derived from the invite id so only a
//! holder of the invite can read them, and both BEP 44 mutable items so the
//! DHT itself checks who wrote them:
//!
//! - **host** — where the host's obfuscated endpoint is reachable now. Signed
//!   by the host's own endpoint key under an invite-derived salt, so nobody
//!   but the host can move it.
//! - **knock** — where a guest is dialing from. Signed by a key every holder
//!   of the invite can derive, because the host cannot know its guests in
//!   advance. What a forged knock can do is make the host send a few small
//!   packets somewhere; it cannot let anybody in (the handshake and consent
//!   still decide that, §2.3).
//!
//! The DHT serves stale copies next to fresh ones: nodes that missed the last
//! `put` keep answering with the previous value. So every read here drains the
//! whole lookup and keeps the highest sequence number, never the first answer
//! (`n0_mainline`'s own `get_mutable_most_recent` keeps the first).
//!
//! The DHT can only be polled, though, and a poll a host can afford is far
//! too slow for a guest that is dialing *now*. So the same two records also
//! travel as pushes over public Nostr relays ([`Signals`], ADR 0116): a host
//! hears a knock a fraction of a second after it was sent, and answers with
//! where it is. The DHT stays as the path that works when no relay does.
//!
//! A relay checks nobody's signature, so a host's answer there carries its
//! own: the host's endpoint key over the record, inside the seal (ADR 0123).
//! Without it anybody holding the invite could say where the host is, and a
//! guest would dial there for the rest of the session. A guest takes a host
//! record off a relay only if that signature verifies; the DHT's copy is
//! already signed by the same key, by BEP 44.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chacha20poly1305::aead::{Aead as _, KeyInit as _, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use data_encoding::HEXLOWER;
use ed25519_dalek::{Signature, Signer as _, SigningKey, Verifier as _, VerifyingKey};
use lumepeer_core::NodeId;
use lumepeer_core::constants::RENDEZVOUS_LOOKUP_TIMEOUT_SECS;
use n0_future::StreamExt as _;
use n0_mainline::{Dht, MutableItem};
use rand::Rng as _;

use crate::error::{NetError, Result};
use crate::nostr;
use crate::ticket::INVITE_ID_BYTES;

/// KDF context for the key both records are sealed with.
const SEAL_CONTEXT: &str = "lumepeer 2026 ADR 0113 rendezvous record key";
/// KDF context for the salt the host record is stored under.
const HOST_SALT_CONTEXT: &str = "lumepeer 2026 ADR 0113 rendezvous host salt";
/// KDF context for the signing key every knock is written with.
const KNOCK_KEY_CONTEXT: &str = "lumepeer 2026 ADR 0113 rendezvous knock signing key";
/// KDF context for the Nostr topic both records of one invite travel on.
const TOPIC_CONTEXT: &str = "lumepeer 2026 ADR 0116 rendezvous signal topic";
/// Bytes of the topic (hex-encoded on the wire): enough that two invites never
/// share one, short enough to cost nothing in a relay's index.
const TOPIC_BYTES: usize = 16;
/// Bytes of the host record's salt (BEP 44 allows up to 64).
const SALT_BYTES: usize = 16;
/// Version of the sealed payload below.
const FORMAT_VERSION: u8 = 1;
/// Bytes of the `XChaCha20-Poly1305` nonce that prefixes a sealed record.
const NONCE_BYTES: usize = 24;
/// Domain separator of a host record's signature (ADR 0123).
const HOST_SIGNATURE_CONTEXT: &[u8] = b"lumepeer 2026 ADR 0123 rendezvous host record";
/// Bytes of an ed25519 signature.
const SIGNATURE_BYTES: usize = 64;
/// Longest a `put` may take before it is given up on. A put walks to the
/// closest nodes and stores on each; one that has not finished by now is
/// stuck on unanswering nodes, and the next publish replaces it anyway.
const PUT_TIMEOUT: Duration = Duration::from_secs(30);

/// Which record a payload belongs to, sealed in as associated data so a knock
/// can never be opened as a host record or the other way round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Host = 1,
    Knock = 2,
}

/// One record as read back: where somebody said they can be reached, and when.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sighting {
    /// The BEP 44 sequence number: the writer's clock in microseconds, so a
    /// later write always wins.
    pub seq: i64,
    /// The address the writer published.
    pub addr: SocketAddr,
    /// Unix seconds at which the writer published it.
    pub at: u64,
}

/// A DHT client node for the rendezvous records (ADR 0113), and the Nostr
/// relays the same records are pushed through (ADR 0116).
///
/// One per process: it is a UDP socket and a routing table, and every host
/// endpoint and guest dial of the run shares it.
#[derive(Clone)]
pub struct Rendezvous {
    dht: Dht,
    /// Where [`Signals`] connect. Empty means no push channel at all — the
    /// DHT alone, as before ADR 0116.
    relays: Arc<[String]>,
}

impl std::fmt::Debug for Rendezvous {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rendezvous").finish_non_exhaustive()
    }
}

impl Rendezvous {
    /// Starts a DHT client on the public Mainline network. Must be called
    /// inside a tokio runtime.
    ///
    /// On the bootstrap addresses the endpoint's bind already looked up, when
    /// it has (ADR 0125): handed names, the DHT would resolve them again on
    /// the caller's thread, which on a slow resolver is the whole start.
    ///
    /// # Errors
    /// [`NetError::Endpoint`] if the DHT socket cannot be bound.
    pub fn start() -> Result<Self> {
        let dht = Dht::builder()
            .bootstrap(&crate::dns::mainline_bootstrap())
            .build()
            .map_err(|e| NetError::Endpoint(e.to_string()))?;
        Ok(Self {
            dht,
            relays: nostr::SIGNAL_RELAYS
                .iter()
                .map(|relay| (*relay).to_owned())
                .collect(),
        })
    }

    /// Starts a DHT client on the network `bootstrap` names instead of the
    /// public one — a private test network, for instance — with no signalling
    /// relays until [`Self::with_relays`] names some.
    ///
    /// # Errors
    /// [`NetError::Endpoint`] if the DHT socket cannot be bound.
    pub fn with_bootstrap(bootstrap: &[String]) -> Result<Self> {
        let dht = Dht::builder()
            .bootstrap(bootstrap)
            .build()
            .map_err(|e| NetError::Endpoint(e.to_string()))?;
        Ok(Self {
            dht,
            relays: Arc::from([]),
        })
    }

    /// The same rendezvous, signalling through `relays` instead of
    /// [`nostr::SIGNAL_RELAYS`]; an empty list turns the push channel off.
    #[must_use]
    pub fn with_relays(mut self, relays: &[String]) -> Self {
        self.relays = relays.iter().cloned().collect();
        self
    }

    /// Host: the push channel for `invite_id`, on which guests' knocks
    /// arrive and this host says where it is (ADR 0116), signing what it
    /// says with its endpoint key `identity` (ADR 0123). `None` with no
    /// relays configured. Opens connections: call it inside a tokio runtime.
    #[must_use]
    pub fn host_signals(
        &self,
        invite_id: &[u8; INVITE_ID_BYTES],
        identity: &SigningKey,
    ) -> Option<Signals> {
        self.signals(
            invite_id,
            Kind::Knock,
            Kind::Host,
            Some(identity.clone()),
            None,
        )
    }

    /// Guest: the push channel for `invite_id`, on which this guest knocks
    /// and the host says where it is (ADR 0116) — heard only when `host`
    /// signed it (ADR 0123). `None` with no relays configured. Opens
    /// connections: call it inside a tokio runtime.
    #[must_use]
    pub fn guest_signals(
        &self,
        invite_id: &[u8; INVITE_ID_BYTES],
        host: NodeId,
    ) -> Option<Signals> {
        self.signals(invite_id, Kind::Host, Kind::Knock, None, Some(host))
    }

    fn signals(
        &self,
        invite_id: &[u8; INVITE_ID_BYTES],
        listen: Kind,
        speak: Kind,
        signer: Option<SigningKey>,
        signed_by: Option<NodeId>,
    ) -> Option<Signals> {
        if self.relays.is_empty() {
            return None;
        }
        let channel = nostr::Channel::open(&self.relays, &topic(invite_id));
        Some(Signals {
            speaker: Speaker {
                publisher: channel.publisher(),
                invite_id: *invite_id,
                kind: speak,
                signer: signer.map(Arc::new),
            },
            channel,
            listen,
            signed_by,
        })
    }

    /// Host: publishes that the obfuscated endpoint for `invite_id` is
    /// reachable at `addr`, signed with the host's own `identity`.
    ///
    /// # Errors
    /// [`NetError::Io`] if the record cannot be sealed or the DHT did not
    /// store it.
    pub async fn publish_host(
        &self,
        identity: &SigningKey,
        invite_id: &[u8; INVITE_ID_BYTES],
        addr: SocketAddr,
    ) -> Result<()> {
        let signer = n0_mainline::SigningKey::from_bytes(&identity.to_bytes());
        let salt = host_salt(invite_id);
        self.put(&signer, Some(&salt), invite_id, Kind::Host, addr)
            .await
    }

    /// Guest: where `host` last said its obfuscated endpoint for `invite_id`
    /// is, or `None` when the DHT has no readable record.
    pub async fn host(&self, host: &NodeId, invite_id: &[u8; INVITE_ID_BYTES]) -> Option<Sighting> {
        let salt = host_salt(invite_id);
        self.newest(host.as_bytes(), Some(&salt), None, invite_id, Kind::Host)
            .await
    }

    /// Guest: asks the host of `invite_id` to punch towards `addr`.
    ///
    /// # Errors
    /// [`NetError::Io`] if the record cannot be sealed or the DHT did not
    /// store it.
    pub async fn knock(&self, invite_id: &[u8; INVITE_ID_BYTES], addr: SocketAddr) -> Result<()> {
        self.put(&knock_key(invite_id), None, invite_id, Kind::Knock, addr)
            .await
    }

    /// Host: the newest knock for `invite_id` written after sequence number
    /// `after`, if there is one.
    ///
    /// Passing the last one seen is what keeps polling cheap: a node with
    /// nothing newer answers "no more recent value" and sends no record.
    pub async fn knock_after(
        &self,
        invite_id: &[u8; INVITE_ID_BYTES],
        after: Option<i64>,
    ) -> Option<Sighting> {
        let key = knock_key(invite_id).verifying_key().to_bytes();
        self.newest(&key, None, after, invite_id, Kind::Knock)
            .await
            .filter(|sighting| after.is_none_or(|after| sighting.seq > after))
    }

    async fn put(
        &self,
        signer: &n0_mainline::SigningKey,
        salt: Option<&[u8]>,
        invite_id: &[u8; INVITE_ID_BYTES],
        kind: Kind,
        addr: SocketAddr,
    ) -> Result<()> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let seq = i64::try_from(now.as_micros()).unwrap_or(i64::MAX);
        let value = seal(invite_id, kind, addr, now.as_secs(), None)?;
        let item = MutableItem::new(signer, &value, seq, salt);
        match tokio::time::timeout(PUT_TIMEOUT, self.dht.put_mutable(item, None)).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(NetError::Io(format!("rendezvous put: {e}"))),
            Err(_) => Err(NetError::Io("rendezvous put timed out".to_owned())),
        }
    }

    async fn newest(
        &self,
        key: &[u8; 32],
        salt: Option<&[u8]>,
        after: Option<i64>,
        invite_id: &[u8; INVITE_ID_BYTES],
        kind: Kind,
    ) -> Option<Sighting> {
        let mut stream = self.dht.get_mutable(key, salt, after).await.ok()?;
        let mut best: Option<Sighting> = None;
        let drain = async {
            while let Some(item) = stream.next().await {
                let Some(sighting) = open(invite_id, kind, item.value(), item.seq()) else {
                    continue;
                };
                if best.is_none_or(|b| sighting.seq > b.seq) {
                    best = Some(sighting);
                }
            }
        };
        // A lookup that runs out of time keeps what it has: the freshest
        // answer so far is still better than none.
        let _ =
            tokio::time::timeout(Duration::from_secs(RENDEZVOUS_LOOKUP_TIMEOUT_SECS), drain).await;
        best
    }
}

/// One side's live presence on the signalling relays for one invite (ADR
/// 0116): it hears the other side's records and says its own.
///
/// Dropping it closes every relay connection it opened.
#[derive(Debug)]
pub struct Signals {
    channel: nostr::Channel,
    speaker: Speaker,
    /// The record kind the other side speaks.
    listen: Kind,
    /// The key the other side's records must be signed with, when they are
    /// the host's (ADR 0123). `None` on the host, which hears knocks.
    signed_by: Option<NodeId>,
}

impl Signals {
    /// Tells the other side, over every relay, that this side can be reached
    /// at `addr`: a guest's knock, or a host's answer.
    pub fn say(&self, addr: SocketAddr) {
        self.speaker.say(addr);
    }

    /// A handle that says things on this channel from another task.
    #[must_use]
    pub fn speaker(&self) -> Speaker {
        self.speaker.clone()
    }

    /// The next record the other side said. Anything that does not open
    /// under this invite as the other side's kind — this side's own records
    /// echoed back, a stranger's junk on the topic — is skipped.
    ///
    /// `None` only if the channel's relay tasks have all ended, which does
    /// not happen while it is alive.
    pub async fn heard(&mut self) -> Option<Sighting> {
        loop {
            let content = self.channel.recv().await?;
            let Ok(wire) = HEXLOWER.decode(content.as_bytes()) else {
                continue;
            };
            let Some(sighting) = open(&self.speaker.invite_id, self.listen, &wire, 0) else {
                continue;
            };
            if let Some(host) = &self.signed_by
                && !signed_by(&self.speaker.invite_id, &wire, host)
            {
                // Sealed under the invite, so written by somebody holding it
                // — and not signed by the host, so not the host's word on
                // where it is (ADR 0123).
                tracing::debug!("an unsigned host record on the signalling relays; skipped");
                continue;
            }
            return Some(Sighting {
                seq: i64::try_from(sighting.at).unwrap_or(i64::MAX),
                ..sighting
            });
        }
    }
}

/// Says one side's records on a [`Signals`] channel. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Speaker {
    publisher: nostr::Publisher,
    invite_id: [u8; INVITE_ID_BYTES],
    kind: Kind,
    /// The host's endpoint key, which signs what a host says (ADR 0123).
    signer: Option<Arc<SigningKey>>,
}

impl Speaker {
    /// See [`Signals::say`].
    pub fn say(&self, addr: SocketAddr) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        match seal(
            &self.invite_id,
            self.kind,
            addr,
            now.as_secs(),
            self.signer.as_deref(),
        ) {
            Ok(wire) => self.publisher.publish(&HEXLOWER.encode(&wire)),
            Err(error) => tracing::warn!(%error, "could not seal a signalling record"),
        }
    }
}

/// The Nostr topic both records of `invite_id` travel on (ADR 0116). Anyone
/// holding the invite can compute it; to everybody else it is a random tag.
fn topic(invite_id: &[u8; INVITE_ID_BYTES]) -> String {
    let full = blake3::derive_key(TOPIC_CONTEXT, invite_id);
    HEXLOWER.encode(&full[..TOPIC_BYTES])
}

/// The salt the host record for `invite_id` is stored under.
fn host_salt(invite_id: &[u8; INVITE_ID_BYTES]) -> [u8; SALT_BYTES] {
    let full = blake3::derive_key(HOST_SALT_CONTEXT, invite_id);
    let mut salt = [0u8; SALT_BYTES];
    salt.copy_from_slice(&full[..SALT_BYTES]);
    salt
}

/// The key every knock for `invite_id` is signed with.
fn knock_key(invite_id: &[u8; INVITE_ID_BYTES]) -> n0_mainline::SigningKey {
    n0_mainline::SigningKey::from_bytes(&blake3::derive_key(KNOCK_KEY_CONTEXT, invite_id))
}

fn cipher(invite_id: &[u8; INVITE_ID_BYTES]) -> XChaCha20Poly1305 {
    XChaCha20Poly1305::new(&Key::from(blake3::derive_key(SEAL_CONTEXT, invite_id)))
}

/// `nonce || AEAD(version || family || ip || port || at [|| signature])`,
/// with `kind` as the associated data.
///
/// The signature, when there is a `signer`, is its ed25519 signature over
/// [`HOST_SIGNATURE_CONTEXT`], the invite id and everything before it in the
/// plaintext (ADR 0123). It goes last, so a reader built before it — which
/// takes the fields it knows and ignores the rest — still reads the record.
fn seal(
    invite_id: &[u8; INVITE_ID_BYTES],
    kind: Kind,
    addr: SocketAddr,
    at: u64,
    signer: Option<&SigningKey>,
) -> Result<Vec<u8>> {
    let mut plain = vec![FORMAT_VERSION];
    match addr.ip() {
        IpAddr::V4(ip) => {
            plain.push(4);
            plain.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            plain.push(6);
            plain.extend_from_slice(&ip.octets());
        }
    }
    plain.extend_from_slice(&addr.port().to_be_bytes());
    plain.extend_from_slice(&at.to_be_bytes());
    if let Some(signer) = signer {
        let signature = signer.sign(&signed_message(invite_id, &plain));
        plain.extend_from_slice(&signature.to_bytes());
    }

    let mut nonce = [0u8; NONCE_BYTES];
    rand::rng().fill_bytes(&mut nonce);
    let sealed = cipher(invite_id)
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: &plain,
                aad: &[kind as u8],
            },
        )
        .map_err(|_| NetError::Obfuscation)?;
    let mut wire = nonce.to_vec();
    wire.extend_from_slice(&sealed);
    Ok(wire)
}

/// What a host record's signature covers (ADR 0123).
fn signed_message(invite_id: &[u8; INVITE_ID_BYTES], fields: &[u8]) -> Vec<u8> {
    let mut message = HOST_SIGNATURE_CONTEXT.to_vec();
    message.extend_from_slice(invite_id);
    message.extend_from_slice(fields);
    message
}

/// Whether the host record `wire` is signed by `host` (ADR 0123).
fn signed_by(invite_id: &[u8; INVITE_ID_BYTES], wire: &[u8], host: &NodeId) -> bool {
    let Some(plain) = unseal(invite_id, Kind::Host, wire) else {
        return false;
    };
    let Some(fields) = fields_len(&plain) else {
        return false;
    };
    let (Some(signed), Some(signature)) = (plain.get(..fields), plain.get(fields..)) else {
        return false;
    };
    let Ok(signature) = <[u8; SIGNATURE_BYTES]>::try_from(signature) else {
        return false;
    };
    VerifyingKey::from_bytes(host.as_bytes()).is_ok_and(|key| {
        key.verify(
            &signed_message(invite_id, signed),
            &Signature::from_bytes(&signature),
        )
        .is_ok()
    })
}

/// Bytes of a record's fields before any signature: version, family, the
/// address, port and time.
fn fields_len(plain: &[u8]) -> Option<usize> {
    match *plain.get(1)? {
        4 => Some(2 + 4 + 2 + 8),
        6 => Some(2 + 16 + 2 + 8),
        _ => None,
    }
}

/// Removes the seal of a record this invite wrote as `kind`.
fn unseal(invite_id: &[u8; INVITE_ID_BYTES], kind: Kind, wire: &[u8]) -> Option<Vec<u8>> {
    let nonce: [u8; NONCE_BYTES] = wire.get(..NONCE_BYTES)?.try_into().ok()?;
    cipher(invite_id)
        .decrypt(
            &XNonce::from(nonce),
            Payload {
                msg: wire.get(NONCE_BYTES..)?,
                aad: &[kind as u8],
            },
        )
        .ok()
}

/// Reverses [`seal`]. `None` for anything this invite did not seal as `kind`:
/// every record is untrusted input from the DHT, so nothing here panics or
/// trusts a length (§2.4).
fn open(invite_id: &[u8; INVITE_ID_BYTES], kind: Kind, wire: &[u8], seq: i64) -> Option<Sighting> {
    let plain = unseal(invite_id, kind, wire)?;
    if *plain.first()? != FORMAT_VERSION {
        return None;
    }
    let (ip, rest): (IpAddr, &[u8]) = match *plain.get(1)? {
        4 => {
            let octets: [u8; 4] = plain.get(2..6)?.try_into().ok()?;
            (Ipv4Addr::from(octets).into(), plain.get(6..)?)
        }
        6 => {
            let octets: [u8; 16] = plain.get(2..18)?.try_into().ok()?;
            (Ipv6Addr::from(octets).into(), plain.get(18..)?)
        }
        _ => return None,
    };
    let port = u16::from_be_bytes(rest.get(..2)?.try_into().ok()?);
    let at = u64::from_be_bytes(rest.get(2..10)?.try_into().ok()?);
    Some(Sighting {
        seq,
        addr: SocketAddr::new(ip, port),
        at,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const INVITE: [u8; INVITE_ID_BYTES] = [0x42; INVITE_ID_BYTES];
    fn host_key() -> SigningKey {
        SigningKey::from_bytes(&[0x31; 32])
    }

    fn host_id() -> NodeId {
        NodeId::from_bytes(&host_key().verifying_key().to_bytes()).unwrap()
    }

    /// ADR 0123: a host record heard on a relay is taken only when the host
    /// signed it. One sealed under the invite by anybody else — which is
    /// everybody holding the invite — is skipped, and the host's own record
    /// right behind it still arrives.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_guest_hears_only_the_host_record_the_host_signed() {
        let relay = crate::nostr::test_relay::Relay::start().await;
        let dht = Rendezvous::with_bootstrap(&[])
            .unwrap()
            .with_relays(std::slice::from_ref(&relay.url));
        let mut guest = dht.guest_signals(&INVITE, host_id()).unwrap();
        let forger = crate::nostr::Channel::open(&[&relay.url], &topic(&INVITE));
        let impostor = dht
            .host_signals(&INVITE, &SigningKey::from_bytes(&[0x77; 32]))
            .unwrap();
        let host = dht.host_signals(&INVITE, &host_key()).unwrap();
        relay
            .wait_for("every subscription", |seen| seen.filters.len() == 4)
            .await;

        let elsewhere: SocketAddr = "192.0.2.66:6666".parse().unwrap();
        forger.publish(
            &HEXLOWER.encode(&seal(&INVITE, Kind::Host, elsewhere, u64::MAX / 2, None).unwrap()),
        );
        impostor.say(elsewhere);
        let host_at: SocketAddr = "203.0.113.7:4115".parse().unwrap();
        host.say(host_at);

        let heard = next(&mut guest, "the host's own record").await;
        assert_eq!(
            heard.addr, host_at,
            "a record the host never signed was taken"
        );
    }

    /// ADR 0123: the signature goes after the fields a reader built before
    /// it knows, so that reader still takes the record.
    #[test]
    fn a_signed_record_still_opens_for_a_reader_that_ignores_the_signature() {
        let addr: SocketAddr = "203.0.113.7:4115".parse().unwrap();
        let wire = seal(&INVITE, Kind::Host, addr, 7, Some(&host_key())).unwrap();
        assert_eq!(open(&INVITE, Kind::Host, &wire, 1).unwrap().addr, addr);
        assert!(signed_by(&INVITE, &wire, &host_id()));
        let other =
            NodeId::from_bytes(&SigningKey::from_bytes(&[9; 32]).verifying_key().to_bytes())
                .unwrap();
        assert!(!signed_by(&INVITE, &wire, &other));
        assert!(!signed_by(
            &INVITE,
            &seal(&INVITE, Kind::Host, addr, 7, None).unwrap(),
            &host_id()
        ));
    }

    #[test]
    fn a_record_opens_under_its_own_invite_and_kind_only() {
        let addr: SocketAddr = "85.173.133.255:4115".parse().unwrap();
        let wire = seal(&INVITE, Kind::Host, addr, 1_790_000_000, None).unwrap();

        let sighting = open(&INVITE, Kind::Host, &wire, 7).unwrap();
        assert_eq!(sighting.addr, addr);
        assert_eq!(sighting.at, 1_790_000_000);
        assert_eq!(sighting.seq, 7);

        assert!(open(&[0x43; INVITE_ID_BYTES], Kind::Host, &wire, 7).is_none());
        assert!(
            open(&INVITE, Kind::Knock, &wire, 7).is_none(),
            "a host record must never be read as a knock"
        );
    }

    #[test]
    fn a_v6_address_round_trips() {
        let addr: SocketAddr = "[2001:db8::7]:443".parse().unwrap();
        let wire = seal(&INVITE, Kind::Knock, addr, 1, None).unwrap();
        assert_eq!(open(&INVITE, Kind::Knock, &wire, 1).unwrap().addr, addr);
    }

    #[test]
    fn tampered_and_truncated_records_never_open_or_panic() {
        let wire = seal(&INVITE, Kind::Host, "1.2.3.4:5".parse().unwrap(), 1, None).unwrap();
        for len in 0..wire.len() {
            assert!(open(&INVITE, Kind::Host, &wire[..len], 1).is_none());
        }
        for index in 0..wire.len() {
            let mut bad = wire.clone();
            bad[index] ^= 0x01;
            assert!(open(&INVITE, Kind::Host, &bad, 1).is_none());
        }
    }

    #[test]
    fn the_two_records_live_under_different_keys() {
        let host = SigningKey::from_bytes(&[9; 32]);
        assert_ne!(
            knock_key(&INVITE).verifying_key().to_bytes(),
            host.verifying_key().to_bytes()
        );
        assert_ne!(host_salt(&INVITE), host_salt(&[0x43; INVITE_ID_BYTES]));
        assert_ne!(
            knock_key(&INVITE).verifying_key().to_bytes(),
            knock_key(&[0x43; INVITE_ID_BYTES])
                .verifying_key()
                .to_bytes()
        );
    }

    /// Each invite signals on a topic of its own that anyone holding the
    /// invite derives alike, and that shows a relay nothing of the invite.
    #[test]
    fn every_invite_signals_on_a_topic_of_its_own() {
        let own = topic(&INVITE);
        assert_eq!(own, topic(&INVITE), "both sides derive one topic");
        assert_eq!(own.len(), TOPIC_BYTES * 2);
        assert!(own.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(own, topic(&[0x43; INVITE_ID_BYTES]));
        assert!(!own.contains(&HEXLOWER.encode(&INVITE[..4])));
    }

    /// No relays configured is no push channel at all: the DHT alone, as
    /// before ADR 0116.
    #[tokio::test]
    async fn without_relays_there_is_no_push_channel() {
        let dht = Rendezvous::with_bootstrap(&[]).unwrap();
        assert!(dht.host_signals(&INVITE, &host_key()).is_none());
        assert!(dht.guest_signals(&INVITE, host_id()).is_none());
        let off = dht.with_relays(&[]);
        assert!(off.host_signals(&INVITE, &host_key()).is_none());
        assert!(off.guest_signals(&INVITE, host_id()).is_none());
    }

    /// The next record `signals` hears, or a failure naming `what`.
    async fn next(signals: &mut Signals, what: &str) -> Sighting {
        tokio::time::timeout(Duration::from_secs(10), signals.heard())
            .await
            .unwrap_or_else(|_| panic!("{what} never arrived"))
            .expect("the channel is alive")
    }

    /// ADR 0116: the knock reaches the host and the host's answer reaches
    /// the guest over a relay, each side hearing only the other one — the
    /// relay echoes a side's own record back to it, and that is skipped.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_knock_and_the_answer_cross_the_relays() {
        let relay = crate::nostr::test_relay::Relay::start().await;
        let dht = Rendezvous::with_bootstrap(&[])
            .unwrap()
            .with_relays(std::slice::from_ref(&relay.url));
        let mut host = dht.host_signals(&INVITE, &host_key()).unwrap();
        let mut guest = dht.guest_signals(&INVITE, host_id()).unwrap();
        relay
            .wait_for("both subscriptions", |seen| seen.filters.len() == 2)
            .await;

        let guest_at: SocketAddr = "198.51.100.9:51515".parse().unwrap();
        guest.say(guest_at);
        assert_eq!(next(&mut host, "the knock").await.addr, guest_at);

        let host_at: SocketAddr = "203.0.113.7:4115".parse().unwrap();
        host.speaker().say(host_at);
        let answer = next(&mut guest, "the answer").await;
        assert_eq!(
            answer.addr, host_at,
            "the guest's own knock, echoed first, must not be taken for the answer"
        );
        assert!(answer.at > 0 && i64::try_from(answer.at).unwrap() == answer.seq);
    }

    /// Anything on the topic that is not the other side's record under this
    /// invite is skipped, and the record behind it still arrives.
    #[tokio::test(flavor = "multi_thread")]
    async fn junk_on_the_topic_is_skipped() {
        let relay = crate::nostr::test_relay::Relay::start().await;
        let dht = Rendezvous::with_bootstrap(&[])
            .unwrap()
            .with_relays(std::slice::from_ref(&relay.url));
        let mut host = dht.host_signals(&INVITE, &host_key()).unwrap();
        let stranger = crate::nostr::Channel::open(&[&relay.url], &topic(&INVITE));
        relay
            .wait_for("both subscriptions", |seen| seen.filters.len() == 2)
            .await;

        let somewhere: SocketAddr = "192.0.2.1:1".parse().unwrap();
        let other_invite = seal(&[0x43; INVITE_ID_BYTES], Kind::Knock, somewhere, 1, None).unwrap();
        let host_kind = seal(&INVITE, Kind::Host, somewhere, 1, None).unwrap();
        for junk in [
            "not hex at all".to_owned(),
            "00ff".to_owned(),
            HEXLOWER.encode(&other_invite),
            HEXLOWER.encode(&host_kind),
        ] {
            stranger.publish(&junk);
        }
        let knock: SocketAddr = "198.51.100.9:51515".parse().unwrap();
        stranger.publish(&HEXLOWER.encode(&seal(&INVITE, Kind::Knock, knock, 2, None).unwrap()));

        assert_eq!(next(&mut host, "the real knock").await.addr, knock);
    }

    /// Both records cross a private DHT: the host's address reaches the
    /// guest, the guest's knock reaches the host, and a later publish wins
    /// over an earlier one.
    #[tokio::test(flavor = "multi_thread")]
    async fn host_record_and_knock_cross_a_private_dht() {
        let testnet = n0_mainline::Testnet::new(5).await.unwrap();
        let host_side = Rendezvous::with_bootstrap(&testnet.bootstrap).unwrap();
        let guest_side = Rendezvous::with_bootstrap(&testnet.bootstrap).unwrap();
        let identity = SigningKey::from_bytes(&[7; 32]);
        let host_id = NodeId::from_bytes(&identity.verifying_key().to_bytes()).unwrap();

        let first: SocketAddr = "203.0.113.1:1000".parse().unwrap();
        let moved: SocketAddr = "203.0.113.2:2000".parse().unwrap();
        host_side
            .publish_host(&identity, &INVITE, first)
            .await
            .unwrap();
        host_side
            .publish_host(&identity, &INVITE, moved)
            .await
            .unwrap();
        let seen = guest_side.host(&host_id, &INVITE).await.unwrap();
        assert_eq!(seen.addr, moved, "the later publish must win");

        assert!(host_side.knock_after(&INVITE, None).await.is_none());
        let guest_addr: SocketAddr = "198.51.100.9:51515".parse().unwrap();
        guest_side.knock(&INVITE, guest_addr).await.unwrap();
        let knock = host_side.knock_after(&INVITE, None).await.unwrap();
        assert_eq!(knock.addr, guest_addr);
        assert!(
            host_side
                .knock_after(&INVITE, Some(knock.seq))
                .await
                .is_none(),
            "a knock already seen is not seen again"
        );
    }
}
