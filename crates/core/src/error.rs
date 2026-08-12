//! `CoreError` — the single error type of the core state machine (design doc §6).

use crate::constants::{MAX_CONTROL_FRAME_BYTES, MAX_PENDING_CONSENTS};

/// Errors returned by the session/consent/license state machine.
///
/// Every variant maps to a row of the error matrix (§18) or to an explicit
/// limit of §14; nothing here panics or aborts the process (§2.4).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CoreError {
    /// The consent queue already holds `MAX_PENDING_CONSENTS` requests (§8.1).
    #[error("pending consent queue is full ({MAX_PENDING_CONSENTS} requests)")]
    PendingConsentQueueFull,

    /// The per-`NodeId` `ConsentRequest` rate limit was exceeded (§9.2).
    #[error("consent request rate limit exceeded for peer")]
    ConsentRateLimited,

    /// The plan's concurrent guest limit would be exceeded (§8.2).
    #[error("concurrent guest limit for plan reached: {limit}")]
    ConcurrentGuestLimit {
        /// Limit that applies to the currently active plan.
        limit: u8,
    },

    /// Another peer already holds a controller role (§8.2).
    #[error("another peer already holds the controller role")]
    ControllerAlreadyGranted,

    /// The peer has no pending or active session entry.
    #[error("unknown peer")]
    UnknownPeer,

    /// The requested transition is not allowed by the state machine (§8.1).
    #[error("invalid session state transition: {from} -> {to}")]
    InvalidTransition {
        /// State the session is currently in.
        from: &'static str,
        /// State that was requested.
        to: &'static str,
    },

    /// The action is not covered by any grant held by the peer (§2.1, §2.2).
    #[error("action not permitted by current grants")]
    NotPermitted,

    /// Frame length outside `1..=MAX_CONTROL_FRAME_BYTES` (§9.1).
    #[error("control frame size {size} outside 1..={MAX_CONTROL_FRAME_BYTES}")]
    FrameSize {
        /// Length announced by the frame header.
        size: usize,
    },

    /// Duplicate or skipped `seq` for `(session_id, direction)` (§9.1).
    #[error("replay or out-of-order control message: expected seq {expected}, got {actual}")]
    ReplayOrOrder {
        /// Sequence number the receiver expected.
        expected: u64,
        /// Sequence number that actually arrived.
        actual: u64,
    },

    /// `Hello` advertised a different protocol major (§9.1).
    #[error("incompatible protocol major: local {local}, remote {remote}")]
    IncompatibleVersion {
        /// Major version supported locally.
        local: u16,
        /// Major version announced by the peer.
        remote: u16,
    },

    /// Control message could not be decoded.
    #[error("malformed control message")]
    Malformed,

    /// License is missing, expired or was denied by the broker (§12).
    #[error("license denied: {reason}")]
    LicenseDenied {
        /// Human-readable reason, safe for UI display and free of secrets (§15).
        reason: String,
    },

    /// Wall-clock rollback detected; a new session needs an online check (§12.3).
    #[error("system clock rollback detected, online license check required")]
    ClockRollback,
}

/// Convenience alias for core results.
pub type Result<T> = core::result::Result<T, CoreError>;
