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

    /// Invite ticket is expired, already consumed, or its signature is invalid (§7).
    #[error("invalid invite ticket")]
    InvalidTicket,

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
}
