//! Shell keystrokes and output over `rd/term/1` (design doc §4.1; ADR 0079).
//!
//! The byte pipeline of a remote terminal, and nothing above it. Whether a
//! shell may exist at all is decided in `lumepeer-core` and re-read for every
//! one; what the shell is allowed to *be* is decided in `lumepeer-terminal`,
//! which refuses rather than lending it this process's privileges. This module
//! moves bytes between a QUIC stream and a pseudo-terminal once both of those
//! have been settled (§2.3).
//!
//! Design invariants, and they are deliberately the ones [`crate::tunnel`]
//! states — the two channels carry different things over the same shape:
//!
//! - **One framed stream per shell.** A session may hold several
//!   ([`lumepeer_core::constants::MAX_TERMINALS_PER_SESSION`]), and none of
//!   them should wait on another's output.
//! - **The frame header is the shell id.** Both directions carry
//!   `u32_be session_id || u32_be len || bytes`, so a reader knows which shell
//!   a payload belongs to without a second framing layer, and a length is
//!   checked against
//!   [`lumepeer_core::constants::TERMINAL_OUTPUT_MAX_BYTES`] before anything
//!   allocates (§9.1). `session_id` here names a **shell**, never the sixteen
//!   bytes of `MessageEnvelope::session_id`.
//! - **Either end closing closes both.** A `len` of zero ends that shell's
//!   stream, which is what turns a host-side `exit` into a closed window and a
//!   closed window into a killed process.
//! - **Nothing here panics on data from the network.** Every length, every id
//!   and every read is a `Result`.
//!
//! Transport-agnostic below one seam, like `file_transfer` and `tunnel`: it
//! talks to any [`AsyncRead`]/[`AsyncWrite`] pair, so tests run over in-memory
//! duplexes and production over the QUIC connection of `ALPN_TERMINAL`.

use lumepeer_core::constants::TERMINAL_OUTPUT_MAX_BYTES;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{NetError, Result};

/// Identifies one shell inside a session's terminal channel.
///
/// Chosen by the **host**, unlike [`crate::tunnel::StreamId`]: the shell is
/// the host's process, so the host names it and the guest only ever repeats
/// one back (ADR 0079).
pub type ShellId = u32;

/// Wire form of one terminal frame: `u32_be session_id || u32_be len || bytes`.
pub const TERMINAL_HEADER_BYTES: usize = 4 + 4;

/// Writes one payload frame for `shell`.
///
/// A zero-length payload is the end-of-stream marker; use [`write_close`] for
/// it rather than an empty slice, so the intent is in the call.
///
/// # Errors
/// [`NetError::Io`] on write failure, and [`NetError::Framing`] with
/// `Malformed` for a payload over [`TERMINAL_OUTPUT_MAX_BYTES`] — a caller
/// that read more than one frame's worth has a bug, and sending it would
/// produce a frame the far side must refuse.
pub async fn write_frame<W>(writer: &mut W, shell: ShellId, payload: &[u8]) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    if payload.len() > TERMINAL_OUTPUT_MAX_BYTES {
        return Err(NetError::Framing(lumepeer_core::CoreError::Malformed));
    }
    let len = u32::try_from(payload.len())
        .map_err(|_| NetError::Framing(lumepeer_core::CoreError::Malformed))?;
    let mut header = [0u8; TERMINAL_HEADER_BYTES];
    header[..4].copy_from_slice(&shell.to_be_bytes());
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

/// Writes the end-of-stream marker for `shell`.
///
/// # Errors
/// [`NetError::Io`] on write failure.
pub async fn write_close<W>(writer: &mut W, shell: ShellId) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    write_frame(writer, shell, &[]).await
}

/// One frame read off a terminal connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalFrame {
    /// Which shell this belongs to.
    pub shell: ShellId,
    /// The payload; empty means that shell is over.
    pub payload: Vec<u8>,
}

impl TerminalFrame {
    /// Whether this frame is the end of its shell's stream.
    #[must_use]
    pub fn is_close(&self) -> bool {
        self.payload.is_empty()
    }
}

/// Reads one frame, refusing an announced length before allocating it.
///
/// # Errors
/// [`NetError::Io`] when the stream ends or fails, and [`NetError::Framing`]
/// with `Malformed` for a length over [`TERMINAL_OUTPUT_MAX_BYTES`] — the
/// allocation bound of §9.1 applied to a peer's own number.
pub async fn read_frame<R>(reader: &mut R) -> Result<TerminalFrame>
where
    R: AsyncRead + Unpin,
{
    let mut header = [0u8; TERMINAL_HEADER_BYTES];
    reader
        .read_exact(&mut header)
        .await
        .map_err(|e| NetError::Io(e.to_string()))?;
    let shell = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    let len = u32::from_be_bytes([header[4], header[5], header[6], header[7]]) as usize;
    // Checked before the allocation, never after: a peer must not be able to
    // make this side reserve its announced size (§9.1, §3.2).
    if len > TERMINAL_OUTPUT_MAX_BYTES {
        return Err(NetError::Framing(lumepeer_core::CoreError::Malformed));
    }
    let mut payload = vec![0u8; len];
    if len > 0 {
        reader
            .read_exact(&mut payload)
            .await
            .map_err(|e| NetError::Io(e.to_string()))?;
    }
    Ok(TerminalFrame { shell, payload })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// The header is self-describing in both directions: what goes in comes
    /// out under the same shell id, in order — which is what lets two shells
    /// share one connection without either waiting on the other.
    #[tokio::test]
    async fn frames_round_trip_under_their_own_shell_id() {
        let mut wire = Vec::new();
        write_frame(&mut wire, 1, b"ls -la\r").await.unwrap();
        write_frame(&mut wire, 2, b"echo other shell\r")
            .await
            .unwrap();
        write_close(&mut wire, 1).await.unwrap();

        let mut cursor = std::io::Cursor::new(wire);
        let first = read_frame(&mut cursor).await.unwrap();
        assert_eq!(first.shell, 1);
        assert_eq!(first.payload, b"ls -la\r");
        assert!(!first.is_close());
        let second = read_frame(&mut cursor).await.unwrap();
        assert_eq!(second.shell, 2);
        let third = read_frame(&mut cursor).await.unwrap();
        assert_eq!(third.shell, 1);
        assert!(third.is_close(), "an empty payload ends a shell's stream");
    }

    /// §9.1: an announced length past the output bound is refused before
    /// anything allocates it, and refusing is an error rather than a panic.
    #[tokio::test]
    async fn an_overlong_frame_is_refused_before_it_is_allocated() {
        let mut wire = Vec::new();
        wire.extend_from_slice(&5u32.to_be_bytes());
        wire.extend_from_slice(&u32::MAX.to_be_bytes());
        let mut cursor = std::io::Cursor::new(wire);
        assert!(matches!(
            read_frame(&mut cursor).await,
            Err(NetError::Framing(_))
        ));

        // And this side never writes one either.
        let mut out = Vec::new();
        let payload = vec![0u8; TERMINAL_OUTPUT_MAX_BYTES + 1];
        assert!(matches!(
            write_frame(&mut out, 1, &payload).await,
            Err(NetError::Framing(_))
        ));
        assert!(out.is_empty(), "a refused frame still wrote a header");
    }

    /// A truncated frame is an error, not a hang and not a partial payload
    /// handed to a shell as if it were input.
    #[tokio::test]
    async fn a_truncated_frame_is_an_error() {
        let mut wire = Vec::new();
        write_frame(&mut wire, 1, b"twelve bytes").await.unwrap();
        wire.truncate(TERMINAL_HEADER_BYTES + 4);
        let mut cursor = std::io::Cursor::new(wire);
        assert!(matches!(
            read_frame(&mut cursor).await,
            Err(NetError::Io(_))
        ));
    }
}
