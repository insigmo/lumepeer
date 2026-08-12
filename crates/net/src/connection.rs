//! Per-ALPN connection handling (design doc §4.1, §9).

use lumepeer_core::NodeId;
use lumepeer_core::protocol::{Direction, MessageEnvelope};

use crate::error::Result;
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

/// An authenticated control connection with one peer.
#[derive(Debug)]
pub struct ControlConnection {
    peer: NodeId,
    session_id: [u8; 16],
    reader: FrameReader<iroh::endpoint::RecvStream>,
    writer: FrameWriter<iroh::endpoint::SendStream>,
}

impl ControlConnection {
    /// Wraps an accepted or dialed bidirectional stream.
    #[must_use]
    pub const fn new(
        peer: NodeId,
        session_id: [u8; 16],
        recv: iroh::endpoint::RecvStream,
        send: iroh::endpoint::SendStream,
        inbound_direction: Direction,
    ) -> Self {
        Self {
            peer,
            session_id,
            reader: FrameReader::new(recv, inbound_direction),
            writer: FrameWriter::new(send),
        }
    }

    /// Authenticated peer identity.
    #[must_use]
    pub const fn peer(&self) -> NodeId {
        self.peer
    }

    /// Session this connection belongs to.
    #[must_use]
    pub const fn session_id(&self) -> [u8; 16] {
        self.session_id
    }

    /// Reads the next control message.
    ///
    /// # Errors
    /// Propagates framing and I/O errors; the caller closes the stream with
    /// the matching code from [`crate::error::close_code`] (§18).
    pub async fn recv(&mut self) -> Result<MessageEnvelope> {
        self.reader.read_frame().await
    }

    /// Sends a control message, assigning its sequence number.
    ///
    /// # Errors
    /// Propagates framing and I/O errors.
    pub async fn send(&mut self, envelope: &mut MessageEnvelope) -> Result<()> {
        self.writer.write_frame(envelope).await
    }

    /// Highest sequence number processed so far, for `ResumeHello` (§10).
    #[must_use]
    pub const fn last_received_seq(&self) -> u64 {
        self.reader.expected_seq().saturating_sub(1)
    }
}
