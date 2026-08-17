//! Length-prefixed control framing: `u32_be length || postcard(MessageEnvelope)`
//! (design doc §9.1).
//!
//! The length is validated **before** any buffer is allocated: this is the
//! allocation-DoS mitigation listed in §3.2.

use lumepeer_core::CoreError;
use lumepeer_core::constants::MAX_CONTROL_FRAME_BYTES;
use lumepeer_core::protocol::{Direction, MessageEnvelope};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{NetError, Result};

/// Bytes of the length prefix.
pub const LENGTH_PREFIX_BYTES: usize = 4;

/// Validates an announced frame length before allocating for it.
///
/// # Errors
/// [`CoreError::FrameSize`] if `length` is 0 or above
/// `MAX_CONTROL_FRAME_BYTES`; the caller closes the stream with `FRAME_SIZE`.
pub const fn check_frame_length(length: usize) -> core::result::Result<(), CoreError> {
    if length == 0 || length > MAX_CONTROL_FRAME_BYTES {
        return Err(CoreError::FrameSize { size: length });
    }
    Ok(())
}

/// Reads length-prefixed control frames from one QUIC stream and enforces the
/// per-direction sequence rule of §9.1.
#[derive(Debug)]
pub struct FrameReader<R> {
    inner: R,
    expected_seq: u64,
    direction: Direction,
}

impl<R: AsyncRead + Unpin + Send> FrameReader<R> {
    /// Creates a reader for frames arriving in `direction`.
    pub const fn new(inner: R, direction: Direction) -> Self {
        Self {
            inner,
            expected_seq: 0,
            direction,
        }
    }

    /// Reads the next envelope.
    ///
    /// Enforces, in this order: length bounds before allocation, decodability,
    /// direction, then strict `seq` monotonicity (§9.1).
    ///
    /// # Errors
    /// [`NetError::Framing`] wrapping [`CoreError::FrameSize`],
    /// [`CoreError::Malformed`] or [`CoreError::ReplayOrOrder`];
    /// [`NetError::Io`] if the stream fails or ends mid-frame.
    pub async fn read_frame(&mut self) -> Result<MessageEnvelope> {
        let mut length_bytes = [0u8; LENGTH_PREFIX_BYTES];
        self.inner
            .read_exact(&mut length_bytes)
            .await
            .map_err(|e| NetError::Io(e.to_string()))?;
        let length = u32::from_be_bytes(length_bytes) as usize;
        check_frame_length(length)?;

        let mut payload = vec![0u8; length];
        self.inner
            .read_exact(&mut payload)
            .await
            .map_err(|e| NetError::Io(e.to_string()))?;

        let envelope = MessageEnvelope::decode(&payload)?;
        if envelope.direction != self.direction {
            return Err(NetError::Framing(CoreError::Malformed));
        }
        if envelope.seq != self.expected_seq {
            return Err(NetError::Framing(CoreError::ReplayOrOrder {
                expected: self.expected_seq,
                actual: envelope.seq,
            }));
        }
        self.expected_seq = self.expected_seq.saturating_add(1);
        Ok(envelope)
    }

    /// Sequence number the next frame must carry.
    #[must_use]
    pub const fn expected_seq(&self) -> u64 {
        self.expected_seq
    }

    /// Resumes counting after a successful `ResumeHello` (§10).
    pub const fn resume_from(&mut self, last_received_seq: u64) {
        self.expected_seq = last_received_seq.saturating_add(1);
    }
}

/// Writes length-prefixed control frames and assigns sequence numbers.
#[derive(Debug)]
pub struct FrameWriter<W> {
    inner: W,
    next_seq: u64,
}

impl<W: AsyncWrite + Unpin + Send> FrameWriter<W> {
    /// Creates a writer starting at `seq` 0.
    pub const fn new(inner: W) -> Self {
        Self { inner, next_seq: 0 }
    }

    /// Assigns the next `seq` to `envelope` and writes it.
    ///
    /// # Errors
    /// [`NetError::Framing`] if the encoded envelope violates the size bound,
    /// [`NetError::Io`] on write failure.
    pub async fn write_frame(&mut self, envelope: &mut MessageEnvelope) -> Result<()> {
        envelope.seq = self.next_seq;
        let payload = envelope.encode()?;
        let length = u32::try_from(payload.len()).map_err(|_| CoreError::FrameSize {
            size: payload.len(),
        })?;

        self.inner
            .write_all(&length.to_be_bytes())
            .await
            .map_err(|e| NetError::Io(e.to_string()))?;
        self.inner
            .write_all(&payload)
            .await
            .map_err(|e| NetError::Io(e.to_string()))?;
        self.inner
            .flush()
            .await
            .map_err(|e| NetError::Io(e.to_string()))?;

        self.next_seq = self.next_seq.saturating_add(1);
        Ok(())
    }

    /// Sequence number the next written frame will carry.
    #[must_use]
    pub const fn next_seq(&self) -> u64 {
        self.next_seq
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use lumepeer_core::protocol::MessageKind;

    use super::*;

    fn envelope(seq: u64) -> MessageEnvelope {
        MessageEnvelope {
            session_id: [1u8; 16],
            direction: Direction::GuestToHost,
            seq,
            kind: MessageKind::ConsentRequest,
            body: Vec::new(),
        }
    }

    #[test]
    fn zero_and_oversized_lengths_are_rejected() {
        assert!(check_frame_length(0).is_err());
        assert!(check_frame_length(MAX_CONTROL_FRAME_BYTES + 1).is_err());
        assert!(check_frame_length(1).is_ok());
        assert!(check_frame_length(MAX_CONTROL_FRAME_BYTES).is_ok());
    }

    #[tokio::test]
    async fn reader_rejects_replayed_seq() {
        let mut buffer = Vec::new();
        {
            let mut writer = FrameWriter::new(&mut buffer);
            let mut first = envelope(0);
            writer.write_frame(&mut first).await.unwrap();
        }
        // Append the very same frame again: a replay.
        let replayed = buffer.clone();
        buffer.extend_from_slice(&replayed);

        let mut reader = FrameReader::new(buffer.as_slice(), Direction::GuestToHost);
        assert!(reader.read_frame().await.is_ok());
        let err = reader.read_frame().await.unwrap_err();
        assert!(matches!(
            err,
            NetError::Framing(CoreError::ReplayOrOrder {
                expected: 1,
                actual: 0
            })
        ));
    }
}
