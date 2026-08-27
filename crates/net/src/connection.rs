//! Per-ALPN connection handling and the control handshake (design doc §4.1, §9).
//!
//! The handshake is `Hello` -> `HelloAck` and nothing more: it authenticates
//! and versions the connection. Consent is a separate exchange that only the
//! host's `SessionManager` may resolve (§2.3, §8.1).

use iroh::endpoint::{Connection, RecvStream, SendStream};
use lumepeer_core::NodeId;
use lumepeer_core::consent::Role;
use lumepeer_core::protocol::{
    Direction, MessageEnvelope, MessageKind, PROTOCOL_MAJOR, PROTOCOL_MINOR,
};
use lumepeer_core::{CoreError, session::SessionManager};

use crate::endpoint::{ALPN_CONTROL, ALPN_FILE, ALPN_MEDIA};
use crate::error::{NetError, Result, close_code};
use crate::framing::{FrameReader, FrameWriter};

/// Which of the three ALPNs a connection belongs to (§4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// `rd/control/1`, opened first and kept responsive for revoke.
    Control,
    /// `rd/media/1`, opened after `ConsentGrant(view)`.
    Media,
    /// `rd/file/1`, opened lazily after `FileAccept(true)`.
    File,
}

impl Channel {
    /// Channel an accepted connection belongs to, or `None` for an ALPN this
    /// build does not speak.
    #[must_use]
    pub fn from_alpn(alpn: &[u8]) -> Option<Self> {
        match alpn {
            ALPN_CONTROL => Some(Self::Control),
            ALPN_MEDIA => Some(Self::Media),
            ALPN_FILE => Some(Self::File),
            _ => None,
        }
    }
}

/// What the guest announced in its `Hello` (§9.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelloInfo {
    /// Protocol minor of the guest; unknown optional features are ignored.
    pub minor: u16,
    /// Role the guest asked for. Advisory only: the host decides (§2.3).
    pub role_request: Role,
    /// Feature strings the guest advertised.
    pub features: Vec<String>,
    /// Proof of possession of a valid invite, verified against the ticket
    /// registry before consent is even offered (§7).
    pub invite_proof: Vec<u8>,
}

/// Read half of a [`ControlConnection`].
///
/// Owned by whoever drives the inbound loop. It keeps a clone of the QUIC
/// connection so it can still close it with a §18 code; the connection stays
/// open as long as either half is alive.
#[derive(Debug)]
pub struct ControlReader {
    connection: Connection,
    session_id: [u8; 16],
    reader: FrameReader<RecvStream>,
}

impl ControlReader {
    /// Authenticated peer identity.
    #[must_use]
    pub fn peer(&self) -> NodeId {
        self.connection.remote_id()
    }

    /// Underlying QUIC connection, for the close codes of §18.
    #[must_use]
    pub const fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Closes the control connection with the code that matches `error` (§18).
    pub fn close_with(&self, error: &NetError) {
        let (code, reason) = close_for(error);
        self.connection.close(code.into(), reason.as_bytes());
    }

    /// Session this connection belongs to.
    #[must_use]
    pub const fn session_id(&self) -> [u8; 16] {
        self.session_id
    }

    /// Reads the next control message.
    ///
    /// Framing enforces the size bound, the direction and strict `seq`; this
    /// adds the `session_id` half of the anti-replay tuple, so a frame from
    /// another session cannot be spliced in (§9.1).
    ///
    /// This is **not** cancel-safe: a partially read frame cannot be resumed,
    /// so it must not be dropped inside a `select!` that another branch can
    /// win. That is exactly why the read half is separable from the write
    /// half — a reader loop never has to race a writer.
    ///
    /// # Errors
    /// Propagates framing and I/O errors; the caller closes the stream with the
    /// matching code from [`crate::error::close_code`] (§18).
    pub async fn recv(&mut self) -> Result<MessageEnvelope> {
        let envelope = self.reader.read_frame().await?;
        if envelope.session_id != self.session_id {
            return Err(NetError::Framing(CoreError::Malformed));
        }
        Ok(envelope)
    }

    /// Highest sequence number processed so far, for `ResumeHello` (§10).
    #[must_use]
    pub const fn last_received_seq(&self) -> u64 {
        self.reader.expected_seq().saturating_sub(1)
    }
}

/// Write half of a [`ControlConnection`].
#[derive(Debug)]
pub struct ControlWriter {
    connection: Connection,
    session_id: [u8; 16],
    outbound: Direction,
    writer: FrameWriter<SendStream>,
}

impl ControlWriter {
    /// Authenticated peer identity.
    #[must_use]
    pub fn peer(&self) -> NodeId {
        self.connection.remote_id()
    }

    /// Underlying QUIC connection, for the close codes of §18.
    #[must_use]
    pub const fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Closes the control connection with the code that matches `error` (§18).
    pub fn close_with(&self, error: &NetError) {
        let (code, reason) = close_for(error);
        self.connection.close(code.into(), reason.as_bytes());
    }

    /// Session this connection belongs to.
    #[must_use]
    pub const fn session_id(&self) -> [u8; 16] {
        self.session_id
    }

    /// Sends one control message, filling in session, direction and sequence.
    ///
    /// # Errors
    /// Propagates framing and I/O errors.
    pub async fn send(&mut self, kind: MessageKind) -> Result<()> {
        let mut envelope = MessageEnvelope {
            session_id: self.session_id,
            direction: self.outbound,
            seq: 0,
            kind,
            body: Vec::new(),
        };
        self.writer.write_frame(&mut envelope).await
    }
}

/// An authenticated control connection with one peer.
///
/// It owns the QUIC connection: dropping every handle to a `Connection` closes
/// it, so the control channel has to outlive the handshake that produced it.
#[derive(Debug)]
pub struct ControlConnection {
    reader: ControlReader,
    writer: ControlWriter,
    /// `PROTOCOL_MINOR` the far side announced, from `Hello` or `HelloAck`.
    ///
    /// Kept because feature strings only travel one way: a guest advertises
    /// what it understands in `Hello`, and `HelloAck` carries no feature list
    /// at all — so the *guest* has nothing but the minor to tell it what the
    /// host can decode. §9.1 makes that sound: a peer at minor N understands
    /// every optional message added at or below N.
    peer_minor: u16,
}

impl ControlConnection {
    /// Wraps an accepted or dialed bidirectional stream together with the
    /// connection it belongs to.
    #[must_use]
    pub fn new(
        connection: Connection,
        session_id: [u8; 16],
        recv: RecvStream,
        send: SendStream,
        inbound_direction: Direction,
    ) -> Self {
        let outbound = match inbound_direction {
            Direction::HostToGuest => Direction::GuestToHost,
            Direction::GuestToHost => Direction::HostToGuest,
        };
        Self {
            peer_minor: 0,
            reader: ControlReader {
                connection: connection.clone(),
                session_id,
                reader: FrameReader::new(recv, inbound_direction),
            },
            writer: ControlWriter {
                connection,
                session_id,
                outbound,
                writer: FrameWriter::new(send),
            },
        }
    }

    /// Splits into an independently owned read and write half, so an inbound
    /// loop and an outbound sender can run as two tasks without either one
    /// cancelling the other's partially completed frame.
    #[must_use]
    pub fn split(self) -> (ControlReader, ControlWriter) {
        (self.reader, self.writer)
    }

    /// Authenticated peer identity.
    #[must_use]
    pub fn peer(&self) -> NodeId {
        self.reader.peer()
    }

    /// `PROTOCOL_MINOR` the far side announced.
    ///
    /// Zero until a handshake has run. Guest side this is the host's minor
    /// from `HelloAck`, which is the only thing a guest ever learns about
    /// what the host can decode — `HelloAck` carries no feature list (§9.1).
    #[must_use]
    pub const fn peer_minor(&self) -> u16 {
        self.peer_minor
    }

    /// Underlying QUIC connection, for the close codes of §18.
    #[must_use]
    pub const fn connection(&self) -> &Connection {
        self.reader.connection()
    }

    /// Closes the control connection with the code that matches `error` (§18).
    pub fn close_with(&self, error: &NetError) {
        self.reader.close_with(error);
    }

    /// Session this connection belongs to.
    #[must_use]
    pub const fn session_id(&self) -> [u8; 16] {
        self.reader.session_id()
    }

    /// Adopts the session id the host assigned in its `HelloAck`.
    pub const fn set_session_id(&mut self, session_id: [u8; 16]) {
        self.reader.session_id = session_id;
        self.writer.session_id = session_id;
    }

    /// Reads the next control message. See [`ControlReader::recv`].
    ///
    /// # Errors
    /// Propagates framing and I/O errors; the caller closes the stream with the
    /// matching code from [`crate::error::close_code`] (§18).
    pub async fn recv(&mut self) -> Result<MessageEnvelope> {
        self.reader.recv().await
    }

    /// Sends one control message. See [`ControlWriter::send`].
    ///
    /// # Errors
    /// Propagates framing and I/O errors.
    pub async fn send(&mut self, kind: MessageKind) -> Result<()> {
        self.writer.send(kind).await
    }

    /// Highest sequence number processed so far, for `ResumeHello` (§10).
    #[must_use]
    pub const fn last_received_seq(&self) -> u64 {
        self.reader.last_received_seq()
    }
}

/// Guest side of the handshake: opens the control stream, sends `Hello` and
/// waits for `HelloAck`.
///
/// The session id is chosen by the host and arrives on the `HelloAck` envelope;
/// the guest's own `Hello` carries an all-zero id because no session exists yet.
///
/// # Errors
/// - [`NetError::Framing`] wrapping [`CoreError::IncompatibleVersion`] if the
///   host speaks a different protocol major (§9.1).
/// - [`NetError::Framing`] wrapping [`CoreError::Malformed`] if the host
///   answers with anything other than `HelloAck`.
/// - [`NetError::Io`] if the stream cannot be opened.
pub async fn guest_handshake(
    connection: Connection,
    role_request: Role,
    invite_proof: Vec<u8>,
    features: Vec<String>,
) -> Result<ControlConnection> {
    let (send, recv) = connection
        .open_bi()
        .await
        .map_err(|e| NetError::Io(e.to_string()))?;
    let mut control =
        ControlConnection::new(connection, [0u8; 16], recv, send, Direction::HostToGuest);

    control
        .send(MessageKind::Hello {
            major: PROTOCOL_MAJOR,
            minor: PROTOCOL_MINOR,
            role_request,
            features,
            invite_proof,
        })
        .await?;

    let envelope = control.reader.reader.read_frame().await?;
    let MessageKind::HelloAck { major, minor } = envelope.kind else {
        let error = NetError::Framing(CoreError::Malformed);
        control.close_with(&error);
        return Err(error);
    };
    if let Err(e) = lumepeer_core::protocol::check_version(major) {
        let error = NetError::Framing(e);
        control.close_with(&error);
        return Err(error);
    }
    control.peer_minor = minor;
    control.set_session_id(envelope.session_id);
    Ok(control)
}

/// Host side of the handshake: accepts the control stream, reads `Hello`,
/// assigns a CSPRNG session id and answers `HelloAck`.
///
/// A major mismatch closes the connection before consent is ever offered
/// (§9.1, §18).
///
/// # Errors
/// - [`NetError::Framing`] wrapping [`CoreError::IncompatibleVersion`] on a
///   major mismatch, after closing the connection.
/// - [`NetError::Framing`] wrapping [`CoreError::Malformed`] if the first
///   message is not a `Hello`.
/// - [`NetError::Io`] if the stream cannot be accepted.
pub async fn host_handshake(connection: Connection) -> Result<(ControlConnection, HelloInfo)> {
    let (send, recv) = connection
        .accept_bi()
        .await
        .map_err(|e| NetError::Io(e.to_string()))?;
    let mut control =
        ControlConnection::new(connection, [0u8; 16], recv, send, Direction::GuestToHost);

    let envelope = control.reader.reader.read_frame().await?;
    let MessageKind::Hello {
        major,
        minor,
        role_request,
        features,
        invite_proof,
    } = envelope.kind
    else {
        let error = NetError::Framing(CoreError::Malformed);
        control.close_with(&error);
        return Err(error);
    };

    if let Err(e) = lumepeer_core::protocol::check_version(major) {
        let error = NetError::Framing(e);
        control.close_with(&error);
        return Err(error);
    }

    control.set_session_id(SessionManager::new_session_id());
    control
        .send(MessageKind::HelloAck {
            major: PROTOCOL_MAJOR,
            minor: PROTOCOL_MINOR,
        })
        .await?;

    Ok((
        control,
        HelloInfo {
            minor,
            role_request,
            features,
            invite_proof,
        },
    ))
}

/// QUIC application close code for a protocol major mismatch (§18).
pub const CLOSE_INCOMPATIBLE_VERSION: u32 = 1;
/// QUIC application close code for an undecodable message (§18).
pub const CLOSE_MALFORMED: u32 = 2;
/// QUIC application close code for a frame outside the size bounds (§18).
pub const CLOSE_FRAME_SIZE: u32 = 3;
/// QUIC application close code for a duplicate or skipped `seq` (§18).
pub const CLOSE_REPLAY_OR_ORDER: u32 = 4;
/// QUIC application close code for a host that cannot take another consent
/// decision right now: queue full or per-peer rate limit (§8.1, §9.2).
pub const CLOSE_CONSENT_UNAVAILABLE: u32 = 5;

/// Close code and reason string that a framing error must close the stream
/// with (§9.1, §18).
#[must_use]
pub fn close_for(error: &NetError) -> (u32, &'static str) {
    match error {
        NetError::Framing(CoreError::FrameSize { .. }) => {
            (CLOSE_FRAME_SIZE, close_code::FRAME_SIZE)
        }
        NetError::Framing(CoreError::ReplayOrOrder { .. }) => {
            (CLOSE_REPLAY_OR_ORDER, close_code::REPLAY_OR_ORDER)
        }
        NetError::Framing(CoreError::IncompatibleVersion { .. }) => {
            (CLOSE_INCOMPATIBLE_VERSION, close_code::INCOMPATIBLE_VERSION)
        }
        NetError::ConsentUnavailable => {
            (CLOSE_CONSENT_UNAVAILABLE, close_code::CONSENT_UNAVAILABLE)
        }
        _ => (CLOSE_MALFORMED, close_code::MALFORMED),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_supported_alpn_maps_to_a_channel() {
        assert_eq!(Channel::from_alpn(ALPN_CONTROL), Some(Channel::Control));
        assert_eq!(Channel::from_alpn(ALPN_MEDIA), Some(Channel::Media));
        assert_eq!(Channel::from_alpn(ALPN_FILE), Some(Channel::File));
        assert_eq!(Channel::from_alpn(b"rd/control/2"), None);
    }

    #[test]
    fn framing_errors_map_to_their_close_codes() {
        assert_eq!(
            close_for(&NetError::Framing(CoreError::FrameSize { size: 0 })).1,
            close_code::FRAME_SIZE
        );
        assert_eq!(
            close_for(&NetError::Framing(CoreError::ReplayOrOrder {
                expected: 1,
                actual: 0
            }))
            .1,
            close_code::REPLAY_OR_ORDER
        );
        assert_eq!(
            close_for(&NetError::Framing(CoreError::IncompatibleVersion {
                local: 1,
                remote: 2
            }))
            .1,
            close_code::INCOMPATIBLE_VERSION
        );
        assert_eq!(
            close_for(&NetError::Io("gone".to_owned())).1,
            close_code::MALFORMED
        );
    }

    #[test]
    fn a_host_that_cannot_queue_consent_says_so_in_its_close_code() {
        assert_eq!(
            close_for(&NetError::ConsentUnavailable),
            (CLOSE_CONSENT_UNAVAILABLE, close_code::CONSENT_UNAVAILABLE)
        );
    }
}
