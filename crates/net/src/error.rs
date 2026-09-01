//! Transport-level errors (design doc §6).

use lumepeer_core::CoreError;

/// Errors of the Iroh endpoint, framing and reconnect layers.
///
/// A protocol violation closes exactly one stream or connection; it never
/// panics and never tears down the process (§2.4).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum NetError {
    /// Endpoint could not be created or bound.
    #[error("endpoint setup failed: {0}")]
    Endpoint(String),

    /// Dialing the peer failed.
    #[error("dial failed: {0}")]
    Dial(String),

    /// Stream I/O failed or the peer closed the connection.
    #[error("stream i/o failed: {0}")]
    Io(String),

    /// Frame violated the wire format (§9.1).
    #[error("framing error: {0}")]
    Framing(#[from] CoreError),

    /// Invite ticket is expired, retired by the host, or its signature is
    /// invalid (§7, ADR 0016).
    #[error("invalid invite ticket")]
    InvalidTicket,

    /// An obfuscated datagram could not be opened: too short, corrupt, padded
    /// wrong, or sealed under a different key (task 17 Fase 2, ADR 0051). The
    /// variant is deliberately opaque and carries no detail — the input is
    /// untrusted, so distinguishing the reasons would hand an observer a
    /// decryption oracle. The caller drops the datagram silently and keeps the
    /// connection, exactly as a QUIC stack drops a packet that fails its own
    /// authentication.
    #[error("obfuscated datagram rejected")]
    Obfuscation,

    /// This node already holds a control connection to the host it was asked
    /// to dial. Guest-side only: a second dial would replace the live
    /// connection and its teardown would end the session that was working.
    #[error("already connected to this host")]
    AlreadyConnected,

    /// Ticket string could not be decoded.
    #[error("malformed invite ticket encoding")]
    MalformedTicket,

    /// Host cannot take another consent decision right now: the pending queue
    /// is full or the peer is rate limited (§8.1, §9.2). Raised on the host
    /// side to close a connection it can no longer make progress on.
    #[error("host cannot accept another consent request")]
    ConsentUnavailable,

    /// Reconnect came from a different peer or for a different session (§10).
    #[error("reconnect rejected")]
    ReconnectRejected,

    /// Keystore is unavailable or refused the operation (§11.2).
    #[error("keystore unavailable: {0}")]
    Keystore(String),

    /// The local endpoint has no address a peer could dial yet — it has not
    /// reached a relay and has no usable direct address either. Raised on the
    /// host side instead of issuing an invite nobody can act on (§7).
    #[error("this device is not reachable yet")]
    Offline,

    /// More than `MAX_CONCURRENT_FILE_TRANSFERS` transfers are active (§9.2).
    #[error("too many concurrent file transfers")]
    TooManyTransfers,

    /// Chunk for a transfer id that was never registered (§9.2).
    #[error("unknown file transfer")]
    UnknownTransfer,

    /// Chunk offset is not the resume point; chunks are strictly sequential.
    #[error("chunk gap: expected offset {expected}, got {got}")]
    ChunkGap {
        /// Offset the receiver had reached.
        expected: u64,
        /// Offset the chunk claimed.
        got: u64,
    },

    /// Chunk would extend the transfer past its announced size (§9.2).
    #[error("chunk overruns the announced file size")]
    ChunkOverrun,

    /// Chunk length outside `1..=FILE_CHUNK_MAX_BYTES` (§9.2, §3.2).
    #[error("file chunk size {0} outside 1..=FILE_CHUNK_MAX_BYTES")]
    ChunkTooLarge(usize),

    /// Transfer already ended; no further chunks are accepted.
    #[error("transfer already closed")]
    TransferClosed,

    /// Chunk accounting overflowed; treated as hostile input.
    #[error("offset arithmetic overflow")]
    Overflow,

    /// Peer stream ended mid-frame; the partial data must be discarded.
    #[error("peer closed the stream mid-{0}")]
    TruncatedStream(&'static str),
}

/// Convenience alias for net results.
pub type Result<T> = core::result::Result<T, NetError>;

/// Application-level close codes carried on QUIC stream/connection close (§18).
pub mod close_code {
    /// Frame length outside `1..=MAX_CONTROL_FRAME_BYTES` (§9.1).
    pub const FRAME_SIZE: &str = "FRAME_SIZE";
    /// Duplicate or skipped `seq` (§9.1).
    pub const REPLAY_OR_ORDER: &str = "REPLAY_OR_ORDER";
    /// Protocol major mismatch (§9.1).
    pub const INCOMPATIBLE_VERSION: &str = "IncompatibleVersion";
    /// Message could not be decoded.
    pub const MALFORMED: &str = "MALFORMED";
    /// Host cannot queue another consent decision (§8.1, §9.2).
    pub const CONSENT_UNAVAILABLE: &str = "CONSENT_UNAVAILABLE";
    /// This node is ending the connection on purpose, not reporting a fault
    /// (docs/bugs/02-connect-form.md task 3, docs/bugs/03-connection-list.md
    /// task 3).
    pub const NORMAL: &str = "NORMAL";
}
