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
use std::path::{Component, Path, PathBuf};

use lumepeer_core::constants::{
    FILE_CHUNK_MAX_BYTES, FILE_NAME_MAX_BYTES, MAX_CONCURRENT_FILE_TRANSFERS,
};
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
        let id = self.next_id;
        self.begin_with(id, total)?;
        self.next_id = self.next_id.wrapping_add(1);
        Ok(id)
    }

    /// Registers a transfer under an id the *sender* chose and announced in
    /// `FileTransferStart` (§9.2; ADR 0032).
    ///
    /// The receiver cannot pick the id: `FileAbort` and `FileChunkAck` both
    /// name one, and a number each side invented for itself would name two
    /// different transfers. So the sender names it and this side records it.
    ///
    /// # Errors
    /// [`NetError::TooManyTransfers`] past `MAX_CONCURRENT_FILE_TRANSFERS`
    /// live transfers, [`NetError::TransferClosed`] when `id` is already in
    /// use — a peer must not be able to restart a transfer under an id that
    /// already has bytes and a running hash behind it.
    pub fn begin_with(&mut self, id: TransferId, total: u64) -> Result<()> {
        let active = self.states.values().filter(|s| s.ended.is_none()).count();
        if active >= MAX_CONCURRENT_FILE_TRANSFERS {
            return Err(NetError::TooManyTransfers);
        }
        if self.states.contains_key(&id) {
            return Err(NetError::TransferClosed);
        }
        self.states.insert(
            id,
            TransferState {
                received: 0,
                total,
                ended: None,
            },
        );
        self.hashes.insert(id, blake3::Hasher::new());
        Ok(())
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

/// Windows device names, which are reserved as whole path components with or
/// without an extension: `CON.txt` opens the console, not a file.
const WINDOWS_DEVICE_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Reduces an offered file name to something safe to create inside a chosen
/// directory, or refuses it (§9.2, §18).
///
/// `FileOffer` documents its `name` as "basename after normalization, no path
/// separators" — a promise made by the *sender*, which is to say by an
/// attacker. Everything here treats the string as hostile:
///
/// - any name that is not a single ordinary path component is refused, which
///   covers `../../etc/passwd`, `..\\..\\windows`, absolute paths, and a bare
///   `.` or `..`;
/// - a Windows drive-relative name (`C:`, `C:file`) and an NTFS alternate data
///   stream (`name:stream`) are refused, because a colon means something on
///   the target platform whether or not it means anything on the sender's;
/// - the Windows reserved device names are refused;
/// - anything over `FILE_NAME_MAX_BYTES` is refused rather than truncated,
///   since a truncated name can collide with a file that is already there.
///
/// Refused rather than sanitized on purpose. Rewriting a hostile name
/// produces a file the receiving user did not ask for under a name neither
/// side chose; refusing produces a question they can answer.
#[must_use]
pub fn safe_file_name(name: &str) -> Option<String> {
    if name.is_empty() || name.len() > FILE_NAME_MAX_BYTES {
        return None;
    }
    // Control characters would make the name unprintable in the accept
    // dialog, which is the one place a user gets to judge it.
    if name.chars().any(char::is_control) {
        return None;
    }
    // `:` is a separator on Windows in two different ways (drive letters and
    // alternate data streams) and is checked before `Path` sees the string,
    // because on Unix `Path` has no reason to treat it as anything special.
    if name.contains(':') || name.contains('/') || name.contains('\\') {
        return None;
    }
    let path = Path::new(name);
    let mut components = path.components();
    let (Some(Component::Normal(only)), None) = (components.next(), components.next()) else {
        return None;
    };
    let only = only.to_str()?;
    if only != name {
        return None;
    }
    // Trailing dots and spaces are silently stripped by Windows, so a name
    // ending in one is a name that becomes a *different* name once written.
    if only.ends_with('.') || only.ends_with(' ') {
        return None;
    }
    let stem = only.split('.').next().unwrap_or(only).to_ascii_uppercase();
    if WINDOWS_DEVICE_NAMES.contains(&stem.as_str()) {
        return None;
    }
    Some(only.to_owned())
}

/// One transfer's staging file: everything received lands here, and nothing
/// leaves for the destination until the BLAKE3 of the offer matches (§9.2).
///
/// Staging is not a performance detail. A transfer that wrote straight to the
/// destination would leave a truncated file under the real name on every
/// cancel, every disconnect and every hash mismatch — and a half-written file
/// under the name the user was expecting is worse than no file, because
/// nothing about it says it is half-written.
#[derive(Debug)]
pub struct StagedReceive {
    path: PathBuf,
    file: tokio::fs::File,
}

impl StagedReceive {
    /// Creates the staging file for `id` inside `dir`.
    ///
    /// `dir` is normally the directory the receiving user chose, so that
    /// [`Self::export`] is a rename on the same volume rather than a second
    /// pass over up to `FILE_OFFER_MAX_BYTES` of data — and so that a
    /// destination which turns out to be unwritable fails at the first chunk
    /// instead of after the last one.
    ///
    /// # Errors
    /// [`NetError::Io`] when the directory cannot be created or the file
    /// cannot be opened.
    pub async fn create(dir: &Path, id: TransferId) -> Result<Self> {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| NetError::Io(e.to_string()))?;
        // Named by the transfer id alone: the offered name is untrusted, and
        // nothing hostile should be able to decide a path, even inside a
        // directory this side chose.
        let path = dir.join(format!(".lumepeer-{id}.part"));
        let file = tokio::fs::File::create(&path)
            .await
            .map_err(|e| NetError::Io(e.to_string()))?;
        Ok(Self { path, file })
    }

    /// Where the partial file currently lives.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one verified-in-order chunk.
    ///
    /// Sequential only, matching [`ReceiveTracker::apply_chunk`]: the tracker
    /// has already refused any offset that is not the resume point, so there
    /// is nothing to seek to.
    ///
    /// # Errors
    /// [`NetError::Io`] on write failure.
    pub async fn append(&mut self, bytes: &[u8]) -> Result<()> {
        self.file
            .write_all(bytes)
            .await
            .map_err(|e| NetError::Io(e.to_string()))
    }

    /// Moves the completed file to `dest`, after the caller has verified the
    /// hash.
    ///
    /// Only ever called with a `true` from [`ReceiveTracker::finish`]. The
    /// rename is attempted first and a copy is the fallback: staging and the
    /// destination can be on different volumes, which is not an error.
    ///
    /// # Errors
    /// [`NetError::Io`] when the file cannot be flushed or moved.
    pub async fn export(mut self, dest: &Path) -> Result<()> {
        self.file
            .flush()
            .await
            .map_err(|e| NetError::Io(e.to_string()))?;
        self.file
            .sync_all()
            .await
            .map_err(|e| NetError::Io(e.to_string()))?;
        drop(self.file);
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| NetError::Io(e.to_string()))?;
        }
        if tokio::fs::rename(&self.path, dest).await.is_ok() {
            return Ok(());
        }
        tokio::fs::copy(&self.path, dest)
            .await
            .map_err(|e| NetError::Io(e.to_string()))?;
        let _ = tokio::fs::remove_file(&self.path).await;
        Ok(())
    }

    /// Deletes the partial file. Used on cancel, on abort, and on a hash that
    /// did not match — every path that must leave nothing behind (§9.2).
    pub async fn discard(self) {
        drop(self.file);
        if let Err(error) = tokio::fs::remove_file(&self.path).await {
            tracing::debug!(%error, "could not remove a staging file");
        }
    }
}

/// BLAKE3 of a whole file, for the `hash` an offer carries.
///
/// Reads in `FILE_CHUNK_MAX_BYTES` bites rather than slurping: an offer may
/// be up to `FILE_OFFER_MAX_BYTES`, and §15 budgets the process.
///
/// # Errors
/// [`NetError::Io`] when the file cannot be read.
pub async fn hash_file(path: &Path) -> Result<[u8; 32]> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| NetError::Io(e.to_string()))?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; FILE_CHUNK_MAX_BYTES];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|e| NetError::Io(e.to_string()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(*hasher.finalize().as_bytes())
}

/// Writes `path` onto `writer` as chunks, starting at `from`.
///
/// `from` is the resume point: after a reconnect inside the window of §10 the
/// sender continues from the last `FileChunkAck` it saw rather than from
/// zero, which is the whole reason acks name an offset.
///
/// `progress` is called with the running total after each chunk, so a UI can
/// follow a transfer that is not going through the actor loop.
///
/// # Errors
/// [`NetError::Io`] on a read or write failure; [`NetError::ChunkTooLarge`]
/// cannot happen here, since the read buffer is the bound itself.
pub async fn send_file<W: AsyncWrite + Unpin + Send>(
    writer: &mut W,
    id: TransferId,
    path: &Path,
    from: u64,
    mut progress: impl FnMut(u64) + Send,
) -> Result<()> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| NetError::Io(e.to_string()))?;
    if from > 0 {
        use tokio::io::AsyncSeekExt as _;
        file.seek(io::SeekFrom::Start(from))
            .await
            .map_err(|e| NetError::Io(e.to_string()))?;
    }
    let mut offset = from;
    let mut buffer = vec![0u8; FILE_CHUNK_MAX_BYTES];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|e| NetError::Io(e.to_string()))?;
        if read == 0 {
            break;
        }
        write_chunk(writer, id, offset, &buffer[..read]).await?;
        offset = offset.checked_add(read as u64).ok_or(NetError::Overflow)?;
        progress(offset);
    }
    writer
        .flush()
        .await
        .map_err(|e| NetError::Io(e.to_string()))?;
    Ok(())
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
        // The duplex buffer is deliberately small: the reader side must run
        // concurrently, or a chunk bigger than the buffer deadlocks the
        // writer against the capacity limit — exactly the backpressure QUIC
        // would apply on a real connection.
        let (mut client, mut server) = tokio::io::duplex(1024);
        let payload = vec![0xABu8; FILE_CHUNK_MAX_BYTES.min(4096)];

        let (write_result, read_result) = tokio::join!(
            write_chunk(&mut client, 42, 0, &payload),
            read_chunk(&mut server)
        );
        let () = write_result.unwrap();
        let (id, offset, bytes) = read_result.unwrap();
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

    /// §18: an offered name is attacker-controlled and ends up as a path.
    /// Every shape that could escape the chosen directory, or mean something
    /// other than a file on the target platform, is refused outright.
    #[test]
    fn a_hostile_offer_name_is_refused_rather_than_repaired() {
        for hostile in [
            "../../etc/passwd",
            "..\\..\\windows\\system32\\config\\sam",
            "/etc/passwd",
            "C:\\Windows\\notepad.exe",
            // A drive-relative name: `C:report.pdf` is "report.pdf on C:'s
            // current directory", not a file called `C:report.pdf`.
            "C:report.pdf",
            "C:",
            // NTFS alternate data stream: writes hidden content beside a file
            // whose visible size never changes.
            "notes.txt:secret",
            "..",
            ".",
            "",
            "sub/dir",
            "sub\\dir",
            // Windows strips the trailing dot, so this creates `report.pdf`.
            "report.pdf.",
            "report.pdf ",
            "CON",
            "con.txt",
            "LPT9.log",
            "nul",
            "bell\x07.txt",
        ] {
            assert_eq!(
                safe_file_name(hostile),
                None,
                "{hostile:?} was accepted as a file name"
            );
        }

        // Over the bound is refused, not truncated: a truncated name can
        // collide with a file that is already there.
        assert_eq!(safe_file_name(&"n".repeat(FILE_NAME_MAX_BYTES + 1)), None);
        assert!(safe_file_name(&"n".repeat(FILE_NAME_MAX_BYTES)).is_some());
    }

    /// Ordinary names, including ones with dots and non-ASCII, pass through
    /// unchanged. A normalizer that mangled these would make the feature
    /// useless for everyone in order to stop nobody.
    #[test]
    fn an_ordinary_offer_name_passes_through_unchanged() {
        for ordinary in [
            "report.pdf",
            "annual report 2026.xlsx",
            ".gitignore",
            "отчёт.pdf",
            "console.log",
            "CONFIG.toml",
        ] {
            assert_eq!(safe_file_name(ordinary), Some(ordinary.to_owned()));
        }
    }

    /// The sender names the transfer, and an id that already has bytes behind
    /// it cannot be re-registered — a peer must not be able to reset a
    /// running hash by repeating a `FileTransferStart` (§9.2; ADR 0032).
    #[test]
    fn a_transfer_id_is_taken_once() {
        let mut tracker = ReceiveTracker::default();
        assert!(tracker.begin_with(7, 100).is_ok());
        assert!(matches!(
            tracker.begin_with(7, 100),
            Err(NetError::TransferClosed)
        ));
        // A different id is still fine, up to the concurrency limit.
        assert!(tracker.begin_with(8, 100).is_ok());
    }

    /// §9.2: nothing leaves staging until the hash of the offer matches, and
    /// a cancel leaves nothing behind at all.
    #[tokio::test]
    async fn staging_exports_on_a_matching_hash_and_leaves_nothing_on_a_cancel() {
        let dir = std::env::temp_dir().join(format!("lumepeer-staging-{}", std::process::id()));
        let payload = b"the whole file, in one chunk".to_vec();
        let expected: [u8; 32] = blake3::hash(&payload).into();

        // Happy path: staged, hashed, exported.
        let mut tracker = ReceiveTracker::default();
        tracker.begin_with(1, payload.len() as u64).unwrap();
        let mut staged = StagedReceive::create(&dir, 1).await.unwrap();
        let staging_path = staged.path().to_path_buf();
        tracker.apply_chunk(1, 0, payload.len()).unwrap();
        staged.append(&payload).await.unwrap();
        tracker.hash_chunk(1, &payload);
        assert!(tracker.finish(1, expected));
        let dest = dir.join("exported.bin");
        staged.export(&dest).await.unwrap();
        assert_eq!(tokio::fs::read(&dest).await.unwrap(), payload);
        assert!(
            !staging_path.exists(),
            "staging survived a successful export"
        );

        // Cancelled midway: the partial bytes are removed and nothing was
        // ever written under the destination name.
        let mut tracker = ReceiveTracker::default();
        tracker.begin_with(2, 1000).unwrap();
        let mut staged = StagedReceive::create(&dir, 2).await.unwrap();
        let staging_path = staged.path().to_path_buf();
        tracker.apply_chunk(2, 0, payload.len()).unwrap();
        staged.append(&payload).await.unwrap();
        tracker.cancel(2);
        staged.discard().await;
        assert!(
            !staging_path.exists(),
            "a cancelled transfer left bytes on disk"
        );
        assert!(!dir.join("never-written.bin").exists());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// A file that arrives whole but wrong is not exported. The hash is the
    /// only thing that decides, not the byte count.
    #[tokio::test]
    async fn a_full_but_corrupted_transfer_is_not_exported() {
        let dir = std::env::temp_dir().join(format!("lumepeer-corrupt-{}", std::process::id()));
        let honest = b"what was offered".to_vec();
        let expected: [u8; 32] = blake3::hash(&honest).into();
        let delivered = b"what was WRITTEN".to_vec();
        assert_eq!(honest.len(), delivered.len());

        let mut tracker = ReceiveTracker::default();
        tracker.begin_with(1, honest.len() as u64).unwrap();
        let mut staged = StagedReceive::create(&dir, 1).await.unwrap();
        let staging_path = staged.path().to_path_buf();
        tracker.apply_chunk(1, 0, delivered.len()).unwrap();
        staged.append(&delivered).await.unwrap();
        tracker.hash_chunk(1, &delivered);

        assert!(!tracker.finish(1, expected));
        staged.discard().await;
        assert!(!staging_path.exists());
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// The resume point of §10: a sender picking up from the last acked
    /// offset writes the same bytes a single pass would have.
    #[tokio::test]
    async fn a_resumed_send_continues_from_the_acked_offset() {
        let dir = std::env::temp_dir().join(format!("lumepeer-resume-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let source = dir.join("source.bin");
        // Two and a bit chunks, so the resume point lands mid-file.
        let payload: Vec<u8> = (0..FILE_CHUNK_MAX_BYTES * 2 + 17)
            .map(|i| u8::try_from(i % 251).unwrap_or(0))
            .collect();
        tokio::fs::write(&source, &payload).await.unwrap();

        let mut first = Vec::new();
        let mut reached = 0u64;
        // A truncated first pass: stop after one chunk, as a dropped
        // connection would.
        {
            let mut limited = Vec::new();
            send_file(&mut limited, 3, &source, 0, |at| reached = at)
                .await
                .unwrap();
            first.extend_from_slice(&limited[..CHUNK_HEADER_BYTES + FILE_CHUNK_MAX_BYTES]);
        }
        let acked = u64::try_from(FILE_CHUNK_MAX_BYTES).unwrap();

        let mut second = Vec::new();
        send_file(&mut second, 3, &source, acked, |_| {})
            .await
            .unwrap();

        // Read both halves back as chunks and reassemble.
        let mut stream = first;
        stream.extend_from_slice(&second);
        let mut cursor = std::io::Cursor::new(stream);
        let mut rebuilt = Vec::new();
        while let Ok((id, offset, bytes)) = read_chunk(&mut cursor).await {
            assert_eq!(id, 3);
            assert_eq!(offset, rebuilt.len() as u64, "a resumed send left a gap");
            rebuilt.extend_from_slice(&bytes);
        }
        assert_eq!(rebuilt, payload);
        assert_eq!(reached, payload.len() as u64);

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// The hash an offer carries is computed over the whole file, in bites,
    /// and matches what a one-shot hash of the same bytes gives.
    #[tokio::test]
    async fn a_file_hash_matches_the_bytes_it_covers() {
        let dir = std::env::temp_dir().join(format!("lumepeer-hash-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("payload.bin");
        let payload: Vec<u8> = (0..FILE_CHUNK_MAX_BYTES + 5)
            .map(|i| u8::try_from(i % 253).unwrap_or(0))
            .collect();
        tokio::fs::write(&path, &payload).await.unwrap();

        let expected: [u8; 32] = blake3::hash(&payload).into();
        assert_eq!(hash_file(&path).await.unwrap(), expected);

        // An empty file still has a hash, and it is the empty-input one.
        let empty = dir.join("empty.bin");
        tokio::fs::write(&empty, b"").await.unwrap();
        let empty_hash: [u8; 32] = blake3::hash(b"").into();
        assert_eq!(hash_file(&empty).await.unwrap(), empty_hash);

        let _ = tokio::fs::remove_dir_all(&dir).await;
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
