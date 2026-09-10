//! TCP forwarding over `rd/tunnel/1` (design doc §4.1; ADR 0078).
//!
//! The byte pipeline of a port forward, and nothing above it. Which addresses
//! may be reached, and by whom, is decided in `lumepeer-core` and re-read for
//! every connection; this module moves bytes between a QUIC stream and a TCP
//! socket once that decision has been made (§2.3).
//!
//! Design invariants:
//!
//! - **One QUIC stream per TCP connection.** A tunnel is many short
//!   connections more often than one long one, and multiplexing them into a
//!   single ordered stream would make the slowest of them the speed of all —
//!   the same reason a file transfer opens a stream per file.
//! - **The frame header is the stream id.** Both directions carry
//!   `u32_be stream_id || u32_be len || bytes`, so a reader knows which
//!   connection a payload belongs to without a second framing layer, and a
//!   length is checked against [`TUNNEL_BUFFER_BYTES`] before anything
//!   allocates (§9.1).
//! - **Either end closing closes both.** A `len` of zero is the end of the
//!   stream, which is what turns a remote `FIN` into a local one.
//! - **Nothing here panics on data from the network.** Every length, every
//!   id and every read is a `Result`.
//!
//! Transport-agnostic below one seam, like `file_transfer`: it talks to any
//! [`AsyncRead`]/[`AsyncWrite`] pair, so tests run over in-memory duplexes
//! and production over the QUIC connection of `ALPN_TUNNEL`.

use lumepeer_core::constants::TUNNEL_BUFFER_BYTES;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{NetError, Result};

/// Identifies one forwarded TCP connection inside a session's tunnel.
///
/// Chosen by the guest, which is the side with a local socket waiting on the
/// answer; the host only ever repeats one back.
pub type StreamId = u32;

/// Wire form of one tunnel frame: `u32_be stream_id || u32_be len || bytes`.
///
/// The explicit header is what lets many connections share one QUIC
/// connection without a second framing layer, exactly as the chunk header
/// does for file transfers.
pub const TUNNEL_HEADER_BYTES: usize = 4 + 4;

/// Writes one payload frame for `stream`.
///
/// A zero-length payload is the end-of-stream marker; use [`write_close`] for
/// it rather than an empty slice, so the intent is in the call.
///
/// # Errors
/// [`NetError::Io`] on write failure, and [`NetError::Framing`] with
/// `Malformed` for a payload over [`TUNNEL_BUFFER_BYTES`] — a caller that
/// read more than a buffer's worth has a bug, and sending it would produce a
/// frame the far side must refuse.
pub async fn write_frame<W>(writer: &mut W, stream: StreamId, payload: &[u8]) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    if payload.len() > TUNNEL_BUFFER_BYTES {
        return Err(NetError::Framing(lumepeer_core::CoreError::Malformed));
    }
    let len = u32::try_from(payload.len())
        .map_err(|_| NetError::Framing(lumepeer_core::CoreError::Malformed))?;
    let mut header = [0u8; TUNNEL_HEADER_BYTES];
    header[..4].copy_from_slice(&stream.to_be_bytes());
    header[4..].copy_from_slice(&len.to_be_bytes());
    writer
        .write_all(&header)
        .await
        .map_err(|e| NetError::Io(e.to_string()))?;
    if !payload.is_empty() {
        writer
            .write_all(payload)
            .await
            .map_err(|e| NetError::Io(e.to_string()))?;
    }
    Ok(())
}

/// Writes the end-of-stream marker for `stream`.
///
/// # Errors
/// [`NetError::Io`] on write failure.
pub async fn write_close<W>(writer: &mut W, stream: StreamId) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    write_frame(writer, stream, &[]).await
}

/// One frame read off a tunnel connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelFrame {
    /// Which forwarded connection this belongs to.
    pub stream: StreamId,
    /// The payload; empty means the far side closed that connection.
    pub payload: Vec<u8>,
}

impl TunnelFrame {
    /// Whether this frame is the end of its stream.
    #[must_use]
    pub fn is_close(&self) -> bool {
        self.payload.is_empty()
    }
}

/// Reads one frame, refusing an announced length before allocating it.
///
/// # Errors
/// [`NetError::Io`] when the stream ends or fails, and [`NetError::Framing`]
/// with `Malformed` for a length over [`TUNNEL_BUFFER_BYTES`] — the
/// allocation bound of §9.1 applied to a peer's own number.
pub async fn read_frame<R>(reader: &mut R) -> Result<TunnelFrame>
where
    R: AsyncRead + Unpin,
{
    let mut header = [0u8; TUNNEL_HEADER_BYTES];
    reader
        .read_exact(&mut header)
        .await
        .map_err(|e| NetError::Io(e.to_string()))?;
    let stream = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    let len = u32::from_be_bytes([header[4], header[5], header[6], header[7]]) as usize;
    // Checked before the allocation, never after: a peer must not be able to
    // make this side reserve its announced size (§9.1, §3.2).
    if len > TUNNEL_BUFFER_BYTES {
        return Err(NetError::Framing(lumepeer_core::CoreError::Malformed));
    }
    let mut payload = vec![0u8; len];
    if len > 0 {
        reader
            .read_exact(&mut payload)
            .await
            .map_err(|e| NetError::Io(e.to_string()))?;
    }
    Ok(TunnelFrame { stream, payload })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// The header is self-describing in both directions: what goes in comes
    /// out under the same stream id, in order.
    #[tokio::test]
    async fn frames_round_trip_under_their_own_stream_id() {
        let mut wire = Vec::new();
        write_frame(&mut wire, 7, b"GET / HTTP/1.1\r\n")
            .await
            .unwrap();
        write_frame(&mut wire, 9, b"other connection")
            .await
            .unwrap();
        write_close(&mut wire, 7).await.unwrap();

        let mut cursor = std::io::Cursor::new(wire);
        let first = read_frame(&mut cursor).await.unwrap();
        assert_eq!(first.stream, 7);
        assert_eq!(first.payload, b"GET / HTTP/1.1\r\n");
        assert!(!first.is_close());
        let second = read_frame(&mut cursor).await.unwrap();
        assert_eq!(second.stream, 9);
        let third = read_frame(&mut cursor).await.unwrap();
        assert_eq!(third.stream, 7);
        assert!(third.is_close(), "an empty payload is the end of a stream");
    }

    /// §9.1: an announced length past the buffer bound is refused before
    /// anything allocates it, and refusing is an error rather than a panic.
    #[tokio::test]
    async fn an_overlong_frame_is_refused_before_it_is_allocated() {
        let mut wire = Vec::new();
        wire.extend_from_slice(&3u32.to_be_bytes());
        wire.extend_from_slice(&u32::MAX.to_be_bytes());
        let mut cursor = std::io::Cursor::new(wire);
        assert!(matches!(
            read_frame(&mut cursor).await,
            Err(NetError::Framing(_))
        ));

        // And this side never writes one either.
        let mut out = Vec::new();
        let payload = vec![0u8; TUNNEL_BUFFER_BYTES + 1];
        assert!(matches!(
            write_frame(&mut out, 1, &payload).await,
            Err(NetError::Framing(_))
        ));
        assert!(out.is_empty(), "a refused frame still wrote a header");
    }

    /// A truncated frame is an error, not a hang and not a partial payload.
    #[tokio::test]
    async fn a_truncated_frame_is_an_error() {
        let mut wire = Vec::new();
        write_frame(&mut wire, 1, b"twelve bytes").await.unwrap();
        wire.truncate(TUNNEL_HEADER_BYTES + 4);
        let mut cursor = std::io::Cursor::new(wire);
        assert!(matches!(
            read_frame(&mut cursor).await,
            Err(NetError::Io(_))
        ));
    }
}
