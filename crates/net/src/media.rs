//! Length-prefixed media framing: `u32_be length || bytes` (design doc §4.1,
//! §11).
//!
//! The control channel's [`crate::framing`] is hard-typed to
//! `MessageEnvelope` — it decodes postcard, checks the direction and enforces
//! the per-direction `seq` of §9.1. None of that applies to a video
//! bitstream, so media gets its own reader/writer rather than a `MessageKind`
//! wrapper that would cap frames at `MAX_CONTROL_FRAME_BYTES` and pay a
//! serialization round trip per picture.
//!
//! What it *does* copy from `framing` is the part that matters for safety: the
//! announced length is validated **before** any buffer is allocated, which is
//! the allocation-DoS mitigation of §3.2. A media stream is opened by the host
//! only towards a peer that already holds a granted control session, but "the
//! peer is authenticated" has never been a reason to trust its length prefix.
//!
//! There is no anti-replay tuple here on purpose: a media stream carries no
//! authorization, so replaying a picture cannot widen anything. QUIC already
//! guarantees the bytes of one stream arrive in order and exactly once, and
//! reordering across frames is [`lumepeer_media::jitter`]'s job upstream of
//! decode, not this layer's.

use iroh::endpoint::{Connection, RecvStream, SendStream};
use lumepeer_core::CoreError;
use lumepeer_core::constants::MAX_MEDIA_FRAME_BYTES;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{NetError, Result};

/// Bytes of the length prefix, matching the control channel's.
pub const MEDIA_LENGTH_PREFIX_BYTES: usize = 4;

/// Validates an announced media frame length before allocating for it.
///
/// # Errors
/// [`CoreError::FrameSize`] if `length` is 0 or above
/// [`MAX_MEDIA_FRAME_BYTES`]; the caller drops the stream rather than
/// allocating what the peer asked for.
pub const fn check_media_frame_length(length: usize) -> core::result::Result<(), CoreError> {
    if length == 0 || length > MAX_MEDIA_FRAME_BYTES {
        return Err(CoreError::FrameSize { size: length });
    }
    Ok(())
}

/// Writes length-prefixed media frames onto one QUIC stream.
#[derive(Debug)]
pub struct MediaFrameWriter<W> {
    inner: W,
}

impl<W: AsyncWrite + Unpin + Send> MediaFrameWriter<W> {
    /// Wraps the write half of a media stream.
    pub const fn new(inner: W) -> Self {
        Self { inner }
    }

    /// Writes one frame.
    ///
    /// # Errors
    /// [`NetError::Framing`] wrapping [`CoreError::FrameSize`] if `payload` is
    /// empty or larger than [`MAX_MEDIA_FRAME_BYTES`] — the sender refuses to
    /// put a frame on the wire that the receiver is required to reject;
    /// [`NetError::Io`] on write failure.
    pub async fn write_frame(&mut self, payload: &[u8]) -> Result<()> {
        check_media_frame_length(payload.len())?;
        let length = u32::try_from(payload.len()).map_err(|_| CoreError::FrameSize {
            size: payload.len(),
        })?;

        self.inner
            .write_all(&length.to_be_bytes())
            .await
            .map_err(|e| NetError::Io(e.to_string()))?;
        self.inner
            .write_all(payload)
            .await
            .map_err(|e| NetError::Io(e.to_string()))?;
        self.inner
            .flush()
            .await
            .map_err(|e| NetError::Io(e.to_string()))?;
        Ok(())
    }
}

/// Reads length-prefixed media frames from one QUIC stream.
#[derive(Debug)]
pub struct MediaFrameReader<R> {
    inner: R,
}

impl<R: AsyncRead + Unpin + Send> MediaFrameReader<R> {
    /// Wraps the read half of a media stream.
    pub const fn new(inner: R) -> Self {
        Self { inner }
    }

    /// Reads the next frame, bounding the length before allocating.
    ///
    /// # Errors
    /// [`NetError::Framing`] wrapping [`CoreError::FrameSize`] on an
    /// out-of-bounds length prefix; [`NetError::Io`] if the stream fails or
    /// ends mid-frame.
    pub async fn read_frame(&mut self) -> Result<Vec<u8>> {
        let mut length_bytes = [0u8; MEDIA_LENGTH_PREFIX_BYTES];
        self.inner
            .read_exact(&mut length_bytes)
            .await
            .map_err(|e| NetError::Io(e.to_string()))?;
        let length = u32::from_be_bytes(length_bytes) as usize;
        check_media_frame_length(length)?;

        let mut payload = vec![0u8; length];
        self.inner
            .read_exact(&mut payload)
            .await
            .map_err(|e| NetError::Io(e.to_string()))?;
        Ok(payload)
    }
}

/// Host side: opens the unidirectional stream frames are written on.
///
/// The host is the sender, so the host opens the stream. That is also what
/// keeps the guest from having to prove anything on this channel — by the time
/// this is called the host has already decided the peer holds a live `view`
/// grant, and a guest that was never granted one simply never sees a stream.
///
/// # Errors
/// [`NetError::Io`] if the stream cannot be opened.
pub async fn open_media_stream(connection: &Connection) -> Result<MediaFrameWriter<SendStream>> {
    let send = connection
        .open_uni()
        .await
        .map_err(|e| NetError::Io(e.to_string()))?;
    Ok(MediaFrameWriter::new(send))
}

/// Guest side: accepts the stream the host opened.
///
/// # Errors
/// [`NetError::Io`] if the connection closes before a stream arrives.
pub async fn accept_media_stream(connection: &Connection) -> Result<MediaFrameReader<RecvStream>> {
    let recv = connection
        .accept_uni()
        .await
        .map_err(|e| NetError::Io(e.to_string()))?;
    Ok(MediaFrameReader::new(recv))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn zero_and_oversized_lengths_are_rejected() {
        assert!(check_media_frame_length(0).is_err());
        assert!(check_media_frame_length(MAX_MEDIA_FRAME_BYTES + 1).is_err());
        assert!(check_media_frame_length(1).is_ok());
        assert!(check_media_frame_length(MAX_MEDIA_FRAME_BYTES).is_ok());
    }

    #[tokio::test]
    async fn frames_round_trip_in_order() {
        let mut buffer = Vec::new();
        {
            let mut writer = MediaFrameWriter::new(&mut buffer);
            writer.write_frame(b"first").await.unwrap();
            writer.write_frame(b"second").await.unwrap();
        }

        let mut reader = MediaFrameReader::new(buffer.as_slice());
        assert_eq!(reader.read_frame().await.unwrap(), b"first");
        assert_eq!(reader.read_frame().await.unwrap(), b"second");
        // A clean end of stream is an I/O error, not a silent empty frame.
        assert!(matches!(
            reader.read_frame().await,
            Err(NetError::Io(_) | NetError::Framing(_))
        ));
    }

    #[tokio::test]
    async fn an_empty_frame_is_refused_by_the_writer() {
        let mut buffer = Vec::new();
        let mut writer = MediaFrameWriter::new(&mut buffer);
        assert!(matches!(
            writer.write_frame(&[]).await,
            Err(NetError::Framing(CoreError::FrameSize { size: 0 }))
        ));
        assert!(buffer.is_empty(), "nothing may reach the wire");
    }

    /// The reason this reader exists at all: the length is checked before the
    /// buffer is allocated, so a peer cannot make the receiver reserve
    /// gigabytes by lying about a frame it never sends (§3.2).
    #[tokio::test]
    async fn an_oversized_length_prefix_is_rejected_before_allocating() {
        let mut wire = Vec::new();
        let announced = u32::try_from(MAX_MEDIA_FRAME_BYTES + 1).unwrap();
        wire.extend_from_slice(&announced.to_be_bytes());
        // Deliberately no payload: a reader that allocated first would block or
        // reserve the announced size before noticing.
        let mut reader = MediaFrameReader::new(wire.as_slice());
        assert!(matches!(
            reader.read_frame().await,
            Err(NetError::Framing(CoreError::FrameSize { .. }))
        ));
    }

    #[tokio::test]
    async fn a_truncated_payload_fails_instead_of_returning_a_short_frame() {
        let mut wire = Vec::new();
        wire.extend_from_slice(&8u32.to_be_bytes());
        wire.extend_from_slice(b"abc");
        let mut reader = MediaFrameReader::new(wire.as_slice());
        assert!(matches!(reader.read_frame().await, Err(NetError::Io(_))));
    }
}
