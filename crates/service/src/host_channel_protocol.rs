//! What the client and the session host may say to each other (ADR 0169 §4).
//!
//! The session host is `LocalSystem`; the client is whatever the signed-in
//! person runs, and so is every other program of theirs, all of which may open
//! this channel (`host_channel.rs`). So the rule is about what the channel can
//! say, not who says it: the client **reads** the host's state and **narrows**
//! it — ends a session, turns a grant off, refuses a request, turns hosting
//! off, chats — and nothing on this wire admits a guest, raises a grant, turns
//! `secure_desktop_input` on, trusts a device or changes the device password.
//! Those are not refused here; they cannot be written down (ADR 0043).
//! One read is of the person's own data rather than the host's state: the
//! guest stores still in the admin tree, handed to their client once and then
//! let go (ADR 0169 §3, [`Request::GuestStores`]).
//! [`Request::effect`] is where that is kept true: every request is one of
//! [`Effect`]'s three kinds, and there is no fourth to put a widening one in.
//!
//! The framing follows `agent_protocol.rs`'s rules where its fixed size cannot
//! carry what this one must (a session list, a chat line):
//!
//! - **The length is checked before anything is allocated.** A frame is
//!   `magic:u8 | kind:u8 | len:u32 LE | payload`, and a `len` over the
//!   direction's bound ([`MAX_REQUEST_PAYLOAD`], [`MAX_REPLY_PAYLOAD`]) ends
//!   the connection without a byte of it being read.
//! - **Direction is in the kind byte.** Requests are below `0x80`, replies at
//!   or above it, so neither side can be handed the other's message.
//! - **A kind nobody knows ends the connection.** There is no "skip it": a
//!   peer that says something this side does not understand is not a peer to
//!   keep listening to.
//!
//! Nothing here panics on any input.

use std::io::{Read, Write};

/// First byte of every frame in both directions; garbage from something that
/// connected by accident is told apart from a request by it, nothing more.
pub const HOST_CHANNEL_MAGIC: u8 = b'S';

/// Bytes before a frame's payload: magic, kind and the payload's length.
const HEADER_LEN: usize = 6;

/// The largest request payload, in bytes: one label and one chat line.
pub const MAX_REQUEST_PAYLOAD: usize = 1 + MAX_LABEL_BYTES + MAX_CHAT_BYTES;

/// The largest reply payload, in bytes: one snapshot of the host's sessions
/// or one chat transcript, as JSON.
///
/// A mebibyte is far more than either: a session row is a few hundred bytes
/// and the host admits a handful of guests; a transcript is bounded by the
/// actor's own history. A reply over it is a reply not sent, never one sent
/// truncated.
pub const MAX_REPLY_PAYLOAD: usize = 1024 * 1024;

/// The longest peer label, in bytes. Labels are the actor's own short
/// pseudonyms; anything longer is not one.
pub const MAX_LABEL_BYTES: usize = 64;

/// The longest chat line, in bytes.
///
/// The same bound as `lumepeer_core::constants::CHAT_MAX_BYTES`, kept as a
/// literal for the reason `agent_protocol.rs` keeps its own: this crate does
/// not depend on `lumepeer-core`. The desktop crate, which links both, checks
/// the two agree.
pub const MAX_CHAT_BYTES: usize = 4_096;

/// The longest grant name, in bytes (`secure_desktop_input` is the longest).
const MAX_GRANT_BYTES: usize = 32;

/// Kind byte of [`Request::Snapshot`].
const KIND_SNAPSHOT: u8 = 0x01;
/// Kind byte of [`Request::Transcript`].
const KIND_TRANSCRIPT: u8 = 0x02;
/// Kind byte of [`Request::MarkChatRead`].
const KIND_MARK_CHAT_READ: u8 = 0x03;
/// Kind byte of [`Request::EndSession`].
const KIND_END_SESSION: u8 = 0x10;
/// Kind byte of [`Request::LowerGrant`].
const KIND_LOWER_GRANT: u8 = 0x11;
/// Kind byte of [`Request::RefusePending`].
const KIND_REFUSE_PENDING: u8 = 0x12;
/// Kind byte of [`Request::HostingOff`].
const KIND_HOSTING_OFF: u8 = 0x13;
/// Kind byte of [`Request::SendChat`].
const KIND_SEND_CHAT: u8 = 0x14;
/// Kind byte of [`Request::GuestStores`].
const KIND_GUEST_STORES: u8 = 0x04;
/// Kind byte of [`Request::GuestStoresTaken`].
const KIND_GUEST_STORES_TAKEN: u8 = 0x15;

/// Kind byte of [`Reply::Snapshot`].
const KIND_REPLY_SNAPSHOT: u8 = 0x81;
/// Kind byte of [`Reply::Transcript`].
const KIND_REPLY_TRANSCRIPT: u8 = 0x82;
/// Kind byte of [`Reply::Done`].
const KIND_REPLY_DONE: u8 = 0x83;
/// Kind byte of [`Reply::Refused`].
const KIND_REPLY_REFUSED: u8 = 0x84;
/// Kind byte of [`Reply::GuestStores`].
const KIND_REPLY_GUEST_STORES: u8 = 0x85;

/// The bit that separates the two directions.
const REPLY_KIND_FLOOR: u8 = 0x80;

/// Everything the client may ask of the session host.
///
/// The whole list. Each is read, narrowing or a chat line — see
/// [`Request::effect`]. Peers are named by the label the host's own snapshot
/// gave them, never by a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// The host's sessions as they stand, as JSON the desktop crate defines.
    Snapshot,
    /// The chat with `peer`, as JSON.
    Transcript {
        /// The session's label.
        peer: String,
    },
    /// The chat with `peer` has been read here.
    MarkChatRead {
        /// The session's label.
        peer: String,
    },
    /// End `peer`'s session.
    EndSession {
        /// The session's label.
        peer: String,
    },
    /// Turn `grant` off for `peer`'s session. There is no "on": the field
    /// that would say so does not exist.
    LowerGrant {
        /// The session's label.
        peer: String,
        /// The grant's name, as `lumepeer_core::consent::IndependentGrant`
        /// spells it; the session host parses it into that closed enum and
        /// refuses a name it does not know.
        grant: String,
    },
    /// Say no to `peer`'s pending request.
    RefusePending {
        /// The request's label.
        peer: String,
    },
    /// End every session, and stop counting this connection as a reason to
    /// listen (ADR 0169 §1).
    HostingOff,
    /// Send `text` to `peer` in the session's chat.
    SendChat {
        /// The session's label.
        peer: String,
        /// The line.
        text: String,
    },
    /// The person's own guest stores still in the admin tree — the hosts
    /// they remembered and the passwords remembered for those hosts — as
    /// JSON, so their client can keep them in the profile (ADR 0169 §3).
    ///
    /// A read of the person's own data about other machines: nothing in it
    /// lets anybody into this one.
    GuestStores,
    /// The client has kept what [`Request::GuestStores`] gave it on this
    /// connection: take it out of the admin tree. The host's own stores —
    /// identity, device password, trusted devices, invite, audit — are not
    /// touched (ADR 0169 §3).
    GuestStoresTaken,
}

/// What a request does to the host — and the whole list of what one may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Reads the host's state, or marks what was read.
    Reads,
    /// Takes something away from a guest, or from the host's reach.
    Narrows,
    /// Puts a line of chat in front of a guest already admitted.
    Says,
}

impl Request {
    /// What this request does to the host.
    ///
    /// Matched without a catch-all on purpose: a new request does not compile
    /// until somebody has said which of the three it is, and none of the three
    /// widens anything.
    #[must_use]
    pub const fn effect(&self) -> Effect {
        match self {
            // The guest stores are the person's own data, read out once
            // (ADR 0169 §3)...
            Self::Snapshot
            | Self::Transcript { .. }
            | Self::MarkChatRead { .. }
            | Self::GuestStores => Effect::Reads,
            // ...then taken out of the host's tree: out of its reach, never
            // into it.
            Self::EndSession { .. }
            | Self::LowerGrant { .. }
            | Self::RefusePending { .. }
            | Self::HostingOff
            | Self::GuestStoresTaken => Effect::Narrows,
            Self::SendChat { .. } => Effect::Says,
        }
    }
}

/// Everything the session host may answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// [`Request::Snapshot`]'s answer.
    Snapshot(Vec<u8>),
    /// [`Request::Transcript`]'s answer.
    Transcript(Vec<u8>),
    /// The request was carried out.
    Done,
    /// The request was understood and not carried out: no such session, a
    /// grant that is not one, an actor that said no.
    Refused,
    /// [`Request::GuestStores`]'s answer.
    GuestStores(Vec<u8>),
}

/// Writes `request` as one frame; whether it went out whole.
pub fn write_request(writer: &mut impl Write, request: &Request) -> bool {
    let (kind, payload) = encode_request(request);
    write_frame(writer, kind, &payload)
}

/// Reads one request; `None` ends the connection — it closed, sent a reply's
/// kind, a kind nobody knows, a length over the bound or a payload that does
/// not parse.
pub fn read_request(reader: &mut impl Read) -> Option<Request> {
    let (kind, payload) = read_frame(reader, MAX_REQUEST_PAYLOAD)?;
    if kind >= REPLY_KIND_FLOOR {
        return None;
    }
    decode_request(kind, &payload)
}

/// Writes `reply` as one frame; whether it went out whole. A reply over
/// [`MAX_REPLY_PAYLOAD`] is not written at all.
pub fn write_reply(writer: &mut impl Write, reply: &Reply) -> bool {
    let (kind, payload): (u8, &[u8]) = match reply {
        Reply::Snapshot(json) => (KIND_REPLY_SNAPSHOT, json),
        Reply::Transcript(json) => (KIND_REPLY_TRANSCRIPT, json),
        Reply::Done => (KIND_REPLY_DONE, &[]),
        Reply::Refused => (KIND_REPLY_REFUSED, &[]),
        Reply::GuestStores(json) => (KIND_REPLY_GUEST_STORES, json),
    };
    payload.len() <= MAX_REPLY_PAYLOAD && write_frame(writer, kind, payload)
}

/// Reads one reply; `None` as [`read_request`]'s.
pub fn read_reply(reader: &mut impl Read) -> Option<Reply> {
    let (kind, payload) = read_frame(reader, MAX_REPLY_PAYLOAD)?;
    match kind {
        KIND_REPLY_SNAPSHOT => Some(Reply::Snapshot(payload)),
        KIND_REPLY_TRANSCRIPT => Some(Reply::Transcript(payload)),
        KIND_REPLY_DONE if payload.is_empty() => Some(Reply::Done),
        KIND_REPLY_REFUSED if payload.is_empty() => Some(Reply::Refused),
        KIND_REPLY_GUEST_STORES => Some(Reply::GuestStores(payload)),
        _ => None,
    }
}

/// One frame's header and payload, with the length checked against `max`
/// before the payload is allocated.
fn read_frame(reader: &mut impl Read, max: usize) -> Option<(u8, Vec<u8>)> {
    let mut header = [0u8; HEADER_LEN];
    reader.read_exact(&mut header).ok()?;
    let [magic, kind, a, b, c, d] = header;
    if magic != HOST_CHANNEL_MAGIC {
        return None;
    }
    let len = usize::try_from(u32::from_le_bytes([a, b, c, d])).ok()?;
    if len > max {
        return None;
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).ok()?;
    Some((kind, payload))
}

/// Writes one frame and flushes it.
fn write_frame(writer: &mut impl Write, kind: u8, payload: &[u8]) -> bool {
    let Ok(len) = u32::try_from(payload.len()) else {
        return false;
    };
    let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
    frame.push(HOST_CHANNEL_MAGIC);
    frame.push(kind);
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(payload);
    writer.write_all(&frame).is_ok() && writer.flush().is_ok()
}

/// A request's kind byte and payload.
fn encode_request(request: &Request) -> (u8, Vec<u8>) {
    match request {
        Request::Snapshot => (KIND_SNAPSHOT, Vec::new()),
        Request::Transcript { peer } => (KIND_TRANSCRIPT, peer.as_bytes().to_vec()),
        Request::MarkChatRead { peer } => (KIND_MARK_CHAT_READ, peer.as_bytes().to_vec()),
        Request::EndSession { peer } => (KIND_END_SESSION, peer.as_bytes().to_vec()),
        Request::LowerGrant { peer, grant } => (KIND_LOWER_GRANT, with_label(peer, grant)),
        Request::RefusePending { peer } => (KIND_REFUSE_PENDING, peer.as_bytes().to_vec()),
        Request::HostingOff => (KIND_HOSTING_OFF, Vec::new()),
        Request::SendChat { peer, text } => (KIND_SEND_CHAT, with_label(peer, text)),
        Request::GuestStores => (KIND_GUEST_STORES, Vec::new()),
        Request::GuestStoresTaken => (KIND_GUEST_STORES_TAKEN, Vec::new()),
    }
}

/// `label_len:u8 | label | rest`: a label followed by one more string.
fn with_label(peer: &str, rest: &str) -> Vec<u8> {
    let mut payload = Vec::with_capacity(1 + peer.len() + rest.len());
    payload.push(u8::try_from(peer.len()).unwrap_or(u8::MAX));
    payload.extend_from_slice(peer.as_bytes());
    payload.extend_from_slice(rest.as_bytes());
    payload
}

/// The request a kind byte and payload spell, if they spell one.
fn decode_request(kind: u8, payload: &[u8]) -> Option<Request> {
    match kind {
        KIND_SNAPSHOT => payload.is_empty().then_some(Request::Snapshot),
        KIND_TRANSCRIPT => label(payload).map(|peer| Request::Transcript { peer }),
        KIND_MARK_CHAT_READ => label(payload).map(|peer| Request::MarkChatRead { peer }),
        KIND_END_SESSION => label(payload).map(|peer| Request::EndSession { peer }),
        KIND_LOWER_GRANT => {
            let (peer, grant) = label_and_rest(payload)?;
            let grant_ok = !grant.is_empty()
                && grant.len() <= MAX_GRANT_BYTES
                && grant.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
            grant_ok.then_some(Request::LowerGrant { peer, grant })
        }
        KIND_REFUSE_PENDING => label(payload).map(|peer| Request::RefusePending { peer }),
        KIND_HOSTING_OFF => payload.is_empty().then_some(Request::HostingOff),
        KIND_SEND_CHAT => {
            let (peer, text) = label_and_rest(payload)?;
            (!text.is_empty() && text.len() <= MAX_CHAT_BYTES)
                .then_some(Request::SendChat { peer, text })
        }
        KIND_GUEST_STORES => payload.is_empty().then_some(Request::GuestStores),
        KIND_GUEST_STORES_TAKEN => payload.is_empty().then_some(Request::GuestStoresTaken),
        _ => None,
    }
}

/// `payload` as a label: UTF-8, non-empty, at most [`MAX_LABEL_BYTES`], no
/// control characters.
fn label(payload: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(payload).ok()?;
    let ok =
        !text.is_empty() && text.len() <= MAX_LABEL_BYTES && !text.chars().any(char::is_control);
    ok.then(|| text.to_owned())
}

/// A [`with_label`] payload taken apart.
fn label_and_rest(payload: &[u8]) -> Option<(String, String)> {
    let (&len, rest) = payload.split_first()?;
    let len = usize::from(len);
    let peer = label(rest.get(..len)?)?;
    let tail = std::str::from_utf8(rest.get(len..)?).ok()?.to_owned();
    Some((peer, tail))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn every_request() -> Vec<Request> {
        let peer = || "peer-1a2b".to_owned();
        vec![
            Request::Snapshot,
            Request::Transcript { peer: peer() },
            Request::MarkChatRead { peer: peer() },
            Request::EndSession { peer: peer() },
            Request::LowerGrant {
                peer: peer(),
                grant: "clipboard_read".to_owned(),
            },
            Request::RefusePending { peer: peer() },
            Request::HostingOff,
            Request::SendChat {
                peer: peer(),
                text: "привет".to_owned(),
            },
            Request::GuestStores,
            Request::GuestStoresTaken,
        ]
    }

    /// Every request survives the wire unchanged.
    #[test]
    fn every_request_round_trips() {
        for request in every_request() {
            let mut wire = Vec::new();
            assert!(write_request(&mut wire, &request));
            assert_eq!(read_request(&mut wire.as_slice()), Some(request));
        }
    }

    /// ADR 0169 §4: no request widens anything. `effect` has no catch-all, so
    /// a new request cannot be added without being classified, and the three
    /// classes it can be put in are the only ones there are.
    #[test]
    fn no_request_widens_anything() {
        for request in every_request() {
            let effect = request.effect();
            assert!(
                matches!(effect, Effect::Reads | Effect::Narrows | Effect::Says),
                "{request:?} is {effect:?}"
            );
        }
        // And the one request that names a grant can only turn it off: there
        // is no field on the wire that would say "on".
        let mut wire = Vec::new();
        assert!(write_request(
            &mut wire,
            &Request::LowerGrant {
                peer: "p".to_owned(),
                grant: "secure_desktop_input".to_owned()
            }
        ));
        assert_eq!(wire[1], KIND_LOWER_GRANT);
    }

    /// Every reply survives the wire unchanged.
    #[test]
    fn every_reply_round_trips() {
        for reply in [
            Reply::Snapshot(br#"{"sessions":[]}"#.to_vec()),
            Reply::Transcript(b"[]".to_vec()),
            Reply::Done,
            Reply::Refused,
            Reply::GuestStores(br#"{"hosts":[],"passwords":[]}"#.to_vec()),
        ] {
            let mut wire = Vec::new();
            assert!(write_reply(&mut wire, &reply));
            assert_eq!(read_reply(&mut wire.as_slice()), Some(reply));
        }
    }

    /// A length over the bound ends the read before anything is allocated
    /// for it, and the payload is never looked at.
    #[test]
    fn a_length_over_the_bound_is_refused_before_it_is_read() {
        let too_long = u32::try_from(MAX_REQUEST_PAYLOAD + 1).unwrap();
        let mut wire = vec![HOST_CHANNEL_MAGIC, KIND_SEND_CHAT];
        wire.extend_from_slice(&too_long.to_le_bytes());
        assert_eq!(read_request(&mut wire.as_slice()), None);

        let mut wire = vec![HOST_CHANNEL_MAGIC, KIND_REPLY_SNAPSHOT];
        wire.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(read_reply(&mut wire.as_slice()), None);
    }

    /// A kind nobody knows, a reply sent as a request, a request sent as a
    /// reply, the wrong magic, a truncated frame: each ends the connection.
    #[test]
    fn anything_not_understood_ends_the_connection() {
        let frame = |magic: u8, kind: u8, payload: &[u8]| {
            let mut wire = vec![magic, kind];
            wire.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_le_bytes());
            wire.extend_from_slice(payload);
            wire
        };
        assert_eq!(read_request(&mut frame(b'S', 0x7f, &[]).as_slice()), None);
        assert_eq!(
            read_request(&mut frame(b'S', KIND_REPLY_DONE, &[]).as_slice()),
            None
        );
        assert_eq!(
            read_reply(&mut frame(b'S', KIND_SNAPSHOT, &[]).as_slice()),
            None
        );
        assert_eq!(
            read_request(&mut frame(b'A', KIND_SNAPSHOT, &[]).as_slice()),
            None
        );
        assert_eq!(
            read_request(&mut [HOST_CHANNEL_MAGIC, KIND_SNAPSHOT, 0].as_slice()),
            None
        );
        assert_eq!(
            read_request(&mut frame(b'S', KIND_SNAPSHOT, b"x").as_slice()),
            None,
            "a request with no payload carries none"
        );
    }

    /// Labels, grant names and chat lines are bounded and plain.
    #[test]
    fn fields_are_bounded_and_plain() {
        let long = "p".repeat(MAX_LABEL_BYTES + 1);
        assert_eq!(decode_request(KIND_END_SESSION, long.as_bytes()), None);
        assert_eq!(decode_request(KIND_END_SESSION, b""), None);
        assert_eq!(decode_request(KIND_END_SESSION, b"peer\n"), None);
        assert_eq!(decode_request(KIND_END_SESSION, &[0xff, 0xfe]), None);
        assert_eq!(
            decode_request(KIND_LOWER_GRANT, &with_label("p", "Clipboard-Read")),
            None,
            "a grant name is a snake_case word"
        );
        assert_eq!(decode_request(KIND_LOWER_GRANT, &with_label("p", "")), None);
        assert_eq!(decode_request(KIND_SEND_CHAT, &with_label("p", "")), None);
        let chat = "x".repeat(MAX_CHAT_BYTES + 1);
        assert_eq!(
            decode_request(KIND_SEND_CHAT, &with_label("p", &chat)),
            None
        );
        assert_eq!(
            decode_request(KIND_SEND_CHAT, &[9, b'p']),
            None,
            "a label longer than the payload"
        );
    }

    /// The largest legitimate request fits under the request bound.
    #[test]
    fn the_largest_chat_line_fits() {
        let request = Request::SendChat {
            peer: "p".repeat(MAX_LABEL_BYTES),
            text: "x".repeat(MAX_CHAT_BYTES),
        };
        let mut wire = Vec::new();
        assert!(write_request(&mut wire, &request));
        assert_eq!(read_request(&mut wire.as_slice()), Some(request));
    }

    /// Pack 07: the guest-store requests carry nothing a caller could steer
    /// them with — no host, no entry, no path — and are a read and a
    /// letting-go, never anything that widens.
    #[test]
    fn the_guest_store_requests_carry_nothing() {
        assert_eq!(decode_request(KIND_GUEST_STORES, b"x"), None);
        assert_eq!(decode_request(KIND_GUEST_STORES_TAKEN, b"x"), None);
        assert_eq!(Request::GuestStores.effect(), Effect::Reads);
        assert_eq!(Request::GuestStoresTaken.effect(), Effect::Narrows);
    }
}
