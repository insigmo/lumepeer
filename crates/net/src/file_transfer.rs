//! File transfer over `rd/file/1`: chunked, resumable, cancellable (§9.2).
//!
//! The control channel carries the offer/accept dance (`FileOffer`,
//! `FileAccept`) and the abort/ack signals; this module owns the byte
//! pipeline on the dedicated file connection. Design invariants:
//!
//! - Every chunk is length-checked before allocation, exactly like media
//!   frames (§3.2): a malicious peer cannot make us allocate its announced
//!   size.
//! - The receiver writes chunks into a staging area keyed by
//!   [`TransferId`] and only verifies the BLAKE3 of [`FileOffer`] once all
//!   bytes arrived; nothing leaves staging before the hash matches (§9.2).
//! - Progress is reported as contiguous bytes received, which is also the
//!   resume point: a reconnecting sender restarts from the last acked
//!   offset, never from zero.
//!
//! The module is transport-agnostic below one seam: it talks to any
//! [`AsyncRead`]/[`AsyncWrite`] pair, so tests run over in-memory duplexes
//! and production over the QUIC connection of `ALPN_FILE`.

use std::collections::BTreeMap;
use std::io;

use lumepeer_core::constants::{FILE_CHUNK_MAX_BYTES, MAX_CONCURRENT_FILE_TRANSFERS};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{NetError, Result};

/// Identifies one transfer within a session.
pub type TransferId = u64;

/// Wire form of one chunk: `u64_be transfer_id || u64_be offset || u32_be
/// len || bytes`. The explicit header keeps the stream self-describing so a
/// resumed transfer can interleave with acks on one connection without a
/// second framing layer.
pub const CHUNK_HEADER_BYTES: usize = 8 + 8 + 4;

/// Why a transfer stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferEnd {
    /// All bytes transferred and the hash verified.
    Completed,
    /// Either side cancelled; nothing is exported from staging.
    Cancelled,
}

/// State of one tracked transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferState {
    /// Bytes durably received so far (the resume point).
    pub received: u64,
    /// Total size announced by the offer.
    pub total: u64,
    /// How the transfer ended, if it ended.
    pub ended: Option<TransferEnd>,
}

impl TransferState {
    /// Fraction transferred, 0.0..=1.0, for progress UIs.
    ///
    /// The `as` casts are deliberate: a progress bar needs the nearest f64,
    /// not an exact rational, and both operands are bounded by
    /// `FILE_OFFER_MAX_BYTES` in practice.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn progress(&self) -> f64 {
        if self.total == 0 {
            return 1.0;
        }
        #[allow(clippy::cast_precision_loss)]
        let received = self.received as f64;
        #[allow(clippy::cast_precision_loss)]
        let total = self.total as f64;
        (received / total).clamp(0.0, 1.0)
    }
}

/// Receiver-side bookkeeping for the transfers of one session (§9.2).
///
/// Chunks arrive out of band of the control channel; this struct tracks how
/// much of each transfer is on disk in staging, enforces the concurrent-
/// transfer limit and computes the running BLAKE3 so completion can be
/// proven against the offer hash.
#[derive(Debug, Default)]
pub struct ReceiveTracker {
    states: BTreeMap<TransferId, TransferState>,
    hashes: BTreeMap<TransferId, blake3::Hasher>,
    next_id: TransferId,
}

impl ReceiveTracker {
    /// Registers an accepted offer and returns its id.
    ///
    /// # Errors
    /// [`NetError::TooManyTransfers`] when more than
    /// `MAX_CONCURRENT_FILE_TRANSFERS` are already active — mirrors
    /// `MAX_PENDING_FILE_OFFERS` on the control channel (§9.2).
    pub fn begin(&mut self, total: u64) -> Result<TransferId> {
        let active = self.states.values().filter(|s| s.ended.is_none()).count();
        if active >= MAX_CONCURRENT_FILE_TRANSFERS {
            return Err(NetError::TooManyTransfers);
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.states.insert(
            id,
            TransferState {
                received: 0,
                total,
                ended: None,
            },
        );
        self.hashes.insert(id, blake3::Hasher::new());
        Ok(id)
    }

    /// Applies one chunk of `len` bytes at `offset`.
    ///
    /// # Errors
    /// [`NetError::UnknownTransfer`] for an unregistered id,
    /// [`NetError::ChunkGap`] when the offset is not exactly the resume
    /// point (chunks are strictly sequential: simpler receiver, no
    /// sparse-file staging), [`NetError::TransferClosed`] once the transfer
    /// ended.
    pub fn apply_chunk(&mut self, id: TransferId, offset: u64, len: usize) -> Result<()> {
        let state = self.states.get_mut(&id).ok_or(NetError::UnknownTransfer)?;
        if state.ended.is_some() {
            return Err(NetError::TransferClosed);
        }
        if offset != state.received {
            return Err(NetError::ChunkGap {
                expected: state.received,
                got: offset,
            });
        }
        let new_received = state
            .received
            .checked_add(len as u64)
            .ok_or(NetError::Overflow)?;
        if new_received > state.total {
            return Err(NetError::ChunkOverrun);
        }
        state.received = new_received;
        // Feed the running hash through the same accounting: callers hand
        // the bytes to `hash_chunk` right after this returns Ok.
        Ok(())
    }

    /// Feeds chunk bytes into the running hash after a successful
    /// [`Self::apply_chunk`].
    pub fn hash_chunk(&mut self, id: TransferId, bytes: &[u8]) {
        if let Some(hasher) = self.hashes.get_mut(&id) {
            hasher.update(bytes);
        }
    }

    /// Marks a transfer complete iff the running hash equals `expected`;
    /// otherwise the transfer ends cancelled and must never be exported
    /// from staging (§9.2).
    #[must_use]
    pub fn finish(&mut self, id: TransferId, expected: [u8; 32]) -> bool {
        let Some(hasher) = self.hashes.get(&id) else {
            return false;
        };
        let computed: [u8; 32] = *hasher.finalize().as_bytes();
        let ok =
            computed == expected && self.states.get(&id).is_some_and(|s| s.received == s.total);
        self.end(
            id,
            if ok {
                TransferEnd::Completed
            } else {
                TransferEnd::Cancelled
            },
        );
        ok
    }

    /// Cancels a transfer: staging content must be dropped by the caller.
    pub fn cancel(&mut self, id: TransferId) {
        self.end(id, TransferEnd::Cancelled);
    }

    fn end(&mut self, id: TransferId, how: TransferEnd) {
        if let Some(state) = self.states.get_mut(&id) {
            state.ended = Some(how);
        }
        self.hashes.remove(&id);
    }

    /// Snapshot of one transfer's state.
    #[must_use]
    pub fn state(&self, id: TransferId) -> Option<&TransferState> {
        self.states.get(&id)
    }
}

/// Writes one chunk onto a file-connection stream (host or guest side).
///
/// # Errors
/// [`NetError::ChunkTooLarge`] if `bytes` exceeds `FILE_CHUNK_MAX_BYTES`,
/// [`NetError::Io`] on write failure.
pub async fn write_chunk<W: AsyncWrite + Unpin + Send>(
    writer: &mut W,
    id: TransferId,
    offset: u64,
    bytes: &[u8],
) -> Result<()> {
    // The bound check doubles as the truncation proof for the `u32` length
    // on the wire: anything above `FILE_CHUNK_MAX_BYTES` is refused before it.
    if bytes.is_empty() || bytes.len() > FILE_CHUNK_MAX_BYTES {
        return Err(NetError::ChunkTooLarge(bytes.len()));
    }
    #[allow(clippy::cast_possible_truncation)]
    let len = bytes.len() as u32;
    writer
        .write_all(&id.to_be_bytes())
        .await
        .map_err(|e| NetError::Io(e.to_string()))?;
    writer
        .write_all(&offset.to_be_bytes())
        .await
        .map_err(|e| NetError::Io(e.to_string()))?;
    writer
        .write_all(&len.to_be_bytes())
        .await
        .map_err(|e| NetError::Io(e.to_string()))?;
    writer
        .write_all(bytes)
        .await
        .map_err(|e| NetError::Io(e.to_string()))?;
    Ok(())
}

/// Reads one chunk from a file-connection stream, bounding the payload
/// before allocating (§3.2).
///
/// # Errors
/// [`NetError::ChunkTooLarge`] when the announced length exceeds
/// `FILE_CHUNK_MAX_BYTES` — the stream is poisoned afterwards and callers
/// must drop it; [`NetError::Io`] on read failure or truncation.
pub async fn read_chunk<R: AsyncRead + Unpin + Send>(
    reader: &mut R,
) -> Result<(TransferId, u64, Vec<u8>)> {
    let mut header = [0u8; CHUNK_HEADER_BYTES];
    reader
        .read_exact(&mut header)
        .await
        .map_err(map_io_eof("chunk header"))?;
    let id = u64::from_be_bytes(header[0..8].try_into().unwrap_or([0; 8]));
    let offset = u64::from_be_bytes(header[8..16].try_into().unwrap_or([0; 8]));
    let len = u32::from_be_bytes(header[16..20].try_into().unwrap_or([0; 4])) as usize;
    if len == 0 || len > FILE_CHUNK_MAX_BYTES {
        return Err(NetError::ChunkTooLarge(len));
    }
    let mut bytes = vec![0u8; len];
    reader
        .read_exact(&mut bytes)
        .await
        .map_err(map_io_eof("chunk payload"))?;
    Ok((id, offset, bytes))
}

fn map_io_eof(what: &'static str) -> impl Fn(io::Error) -> NetError {
    move |e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            NetError::TruncatedStream(what)
        } else {
            NetError::Io(e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn tracker_with(total: u64) -> ReceiveTracker {
        let mut t = ReceiveTracker::default();
        t.begin(total).unwrap();
        t
    }

    #[test]
    fn chunks_must_arrive_sequentially_from_zero() {
        let mut tracker = tracker_with(100);
        assert!(tracker.apply_chunk(0, 0, 50).is_ok());
        // A gap (skipping bytes) is refused, not silently accepted.
        assert!(matches!(
            tracker.apply_chunk(0, 60, 10),
            Err(NetError::ChunkGap {
                expected: 50,
                got: 60
            })
        ));
        // Exactly the resume point continues fine.
        assert!(tracker.apply_chunk(0, 50, 50).is_ok());
    }

    #[test]
    fn overrun_refuses_more_bytes_than_the_offer() {
        let mut tracker = tracker_with(10);
        assert!(matches!(
            tracker.apply_chunk(0, 0, 11),
            Err(NetError::ChunkOverrun)
        ));
    }

    #[test]
    fn unknown_transfer_is_refused() {
        let mut tracker = ReceiveTracker::default();
        assert!(matches!(
            tracker.apply_chunk(99, 0, 1),
            Err(NetError::UnknownTransfer)
        ));
    }

    #[test]
    fn concurrency_limit_matches_the_constant() {
        let mut tracker = ReceiveTracker::default();
        for _ in 0..MAX_CONCURRENT_FILE_TRANSFERS {
            assert!(tracker.begin(10).is_ok());
        }
        assert!(matches!(tracker.begin(10), Err(NetError::TooManyTransfers)));
    }

    #[test]
    fn finish_requires_matching_hash_and_full_size() {
        let data = [7u8; 64];
        let expected: [u8; 32] = blake3::hash(&data).into();

        let mut tracker = tracker_with(64);
        tracker.apply_chunk(0, 0, data.len()).unwrap();
        tracker.hash_chunk(0, &data);
        assert!(tracker.finish(0, expected));
        assert_eq!(
            tracker.state(0).unwrap().ended,
            Some(TransferEnd::Completed)
        );

        // Wrong hash cancels: staging must not be exported.
        let mut tracker = tracker_with(64);
        tracker.apply_chunk(0, 0, data.len()).unwrap();
        tracker.hash_chunk(0, &[8u8; 64]);
        assert!(!tracker.finish(0, expected));
        assert_eq!(
            tracker.state(0).unwrap().ended,
            Some(TransferEnd::Cancelled)
        );

        // Short transfer (missing tail) fails even with a matching prefix.
        let mut tracker = tracker_with(65);
        tracker.apply_chunk(0, 0, data.len()).unwrap();
        tracker.hash_chunk(0, &data);
        assert!(!tracker.finish(0, expected));
    }

    #[test]
    fn ended_transfers_reject_further_chunks() {
        let mut tracker = tracker_with(10);
        tracker.cancel(0);
        assert!(matches!(
            tracker.apply_chunk(0, 0, 5),
            Err(NetError::TransferClosed)
        ));
    }

    #[tokio::test]
    async fn chunk_roundtrip_over_a_duplex() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        let payload = vec![0xABu8; FILE_CHUNK_MAX_BYTES.min(4096)];

        write_chunk(&mut client, 42, 0, &payload).await.unwrap();
        let (id, offset, bytes) = read_chunk(&mut server).await.unwrap();
        assert_eq!(id, 42);
        assert_eq!(offset, 0);
        assert_eq!(bytes, payload);

        // An oversized announcement is rejected before allocation.
        let mut evil = Vec::new();
        evil.extend_from_slice(&1u64.to_be_bytes());
        evil.extend_from_slice(&0u64.to_be_bytes());
        evil.extend_from_slice(&(u32::MAX).to_be_bytes());
        client.write_all(&evil).await.unwrap();
        drop(payload);
        assert!(matches!(
            read_chunk(&mut server).await,
            Err(NetError::ChunkTooLarge(_))
        ));
    }

    #[test]
    fn progress_is_clamped_to_unit_interval() {
        let empty = TransferState {
            received: 0,
            total: 0,
            ended: None,
        };
        assert!((empty.progress() - 1.0).abs() < f64::EPSILON);
        let half = TransferState {
            received: 50,
            total: 100,
            ended: None,
        };
        assert!((half.progress() - 0.5).abs() < f64::EPSILON);
    }
}
