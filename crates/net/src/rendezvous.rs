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

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chacha20poly1305::aead::{Aead as _, KeyInit as _, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use ed25519_dalek::SigningKey;
use lumepeer_core::NodeId;
use lumepeer_core::constants::RENDEZVOUS_LOOKUP_TIMEOUT_SECS;
use n0_future::StreamExt as _;
use n0_mainline::{Dht, MutableItem};
use rand::Rng as _;

use crate::error::{NetError, Result};
use crate::ticket::INVITE_ID_BYTES;

/// KDF context for the key both records are sealed with.
const SEAL_CONTEXT: &str = "lumepeer 2026 ADR 0113 rendezvous record key";
/// KDF context for the salt the host record is stored under.
const HOST_SALT_CONTEXT: &str = "lumepeer 2026 ADR 0113 rendezvous host salt";
/// KDF context for the signing key every knock is written with.
const KNOCK_KEY_CONTEXT: &str = "lumepeer 2026 ADR 0113 rendezvous knock signing key";
/// Bytes of the host record's salt (BEP 44 allows up to 64).
const SALT_BYTES: usize = 16;
/// Version of the sealed payload below.
const FORMAT_VERSION: u8 = 1;
/// Bytes of the `XChaCha20-Poly1305` nonce that prefixes a sealed record.
const NONCE_BYTES: usize = 24;
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

/// A DHT client node for the rendezvous records (ADR 0113).
///
/// One per process: it is a UDP socket and a routing table, and every host
/// endpoint and guest dial of the run shares it.
#[derive(Clone)]
pub struct Rendezvous {
    dht: Dht,
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
    /// # Errors
    /// [`NetError::Endpoint`] if the DHT socket cannot be bound.
    pub fn start() -> Result<Self> {
        let dht = Dht::client().map_err(|e| NetError::Endpoint(e.to_string()))?;
        Ok(Self { dht })
    }

    /// Starts a DHT client on the network `bootstrap` names instead of the
    /// public one — a private test network, for instance.
    ///
    /// # Errors
    /// [`NetError::Endpoint`] if the DHT socket cannot be bound.
    pub fn with_bootstrap(bootstrap: &[String]) -> Result<Self> {
        let dht = Dht::builder()
            .bootstrap(bootstrap)
            .build()
            .map_err(|e| NetError::Endpoint(e.to_string()))?;
        Ok(Self { dht })
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
        let value = seal(invite_id, kind, addr, now.as_secs())?;
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

/// `nonce || AEAD(version || family || ip || port || at)`, with `kind` as the
/// associated data.
fn seal(
    invite_id: &[u8; INVITE_ID_BYTES],
    kind: Kind,
    addr: SocketAddr,
    at: u64,
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

/// Reverses [`seal`]. `None` for anything this invite did not seal as `kind`:
/// every record is untrusted input from the DHT, so nothing here panics or
/// trusts a length (§2.4).
fn open(invite_id: &[u8; INVITE_ID_BYTES], kind: Kind, wire: &[u8], seq: i64) -> Option<Sighting> {
    let nonce: [u8; NONCE_BYTES] = wire.get(..NONCE_BYTES)?.try_into().ok()?;
    let plain = cipher(invite_id)
        .decrypt(
            &XNonce::from(nonce),
            Payload {
                msg: wire.get(NONCE_BYTES..)?,
                aad: &[kind as u8],
            },
        )
        .ok()?;
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

    #[test]
    fn a_record_opens_under_its_own_invite_and_kind_only() {
        let addr: SocketAddr = "85.173.133.255:4115".parse().unwrap();
        let wire = seal(&INVITE, Kind::Host, addr, 1_790_000_000).unwrap();

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
        let wire = seal(&INVITE, Kind::Knock, addr, 1).unwrap();
        assert_eq!(open(&INVITE, Kind::Knock, &wire, 1).unwrap().addr, addr);
    }

    #[test]
    fn tampered_and_truncated_records_never_open_or_panic() {
        let wire = seal(&INVITE, Kind::Host, "1.2.3.4:5".parse().unwrap(), 1).unwrap();
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
