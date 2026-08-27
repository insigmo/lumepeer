//! Host-side audit log (design doc §15, §16.1).
//!
//! Append-only and kept apart from the general `tracing` stream. Peer
//! identities are stored hashed with a rotating install salt: raw `NodeId`,
//! tickets, tokens, IPs, file names and clipboard content must never reach it.

use crate::NodeId;
use crate::consent::{IndependentGrant, Role};

/// Everything worth auditing on the host (§15).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditEvent {
    /// A guest asked for consent.
    ConsentRequested {
        /// Role that was asked for.
        role: Role,
    },
    /// The host granted a role.
    ConsentGranted {
        /// Role that was granted.
        role: Role,
    },
    /// Consent was withdrawn or the session ended.
    ConsentRevoked,
    /// A consent request was refused because the queue is full (§8.1).
    ConsentRejectedQueueFull,
    /// A consent request was refused because of the plan ceiling (§8.2).
    ConsentRejectedGuestLimit {
        /// Ceiling that applies.
        limit: u8,
    },
    /// Input injection was enabled or disabled.
    InputToggled {
        /// New state.
        enabled: bool,
    },
    /// Recording was started or stopped.
    RecordingToggled {
        /// New state.
        enabled: bool,
    },
    /// A file transfer was offered, accepted, rejected or completed.
    FileAction {
        /// Short machine-readable action tag, never a file name (§15).
        action: &'static str,
    },
    /// A protocol violation closed a stream (§18).
    ProtocolViolation {
        /// Error code, e.g. `FRAME_SIZE` or `REPLAY_OR_ORDER`.
        code: &'static str,
    },
    /// The host turned one independent grant of a running session on or off
    /// (§8.2). Which permission moved, never what it was used for: clipboard
    /// content and file names stay out of the log (§15).
    GrantChanged {
        /// Permission that changed.
        grant: IndependentGrant,
        /// New state.
        enabled: bool,
    },
    /// An unattended admission was decided (§8): a guest presented device
    /// credentials to a host with nobody sitting at it.
    ///
    /// The verdict only. Which factor failed is not recorded any more than it
    /// is disclosed on the wire — the log would otherwise become the oracle
    /// `unattended::UnattendedError` refuses to be.
    UnattendedLogin {
        /// Whether the credentials were accepted.
        accepted: bool,
    },
    /// The host marked a device trusted, or withdrew that mark (§8).
    ///
    /// Trust decides who is even allowed to try the unattended password, so
    /// granting it is a widening of the host's own exposure and belongs in the
    /// log next to `GrantChanged`. The device's label and notes stay out: they
    /// are host-identifying free text (§15), and the peer hash already names
    /// the row.
    DeviceTrustChanged {
        /// Whether the device is trusted after the change.
        trusted: bool,
    },
}

/// One audit record: an event plus the pseudonymized peer it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRecord {
    /// BLAKE3 of `install_salt || node_id`, never the raw identity (§15).
    pub peer_hash: [u8; 32],
    /// Unix seconds. Wall-clock is fine here: audit records are evidence, not
    /// an authorization input (§12.3 applies to license timing only).
    pub at_unix_secs: u64,
    /// What happened.
    pub event: AuditEvent,
}

/// Pseudonymizes a peer identity for the audit log and for telemetry (§15).
#[must_use]
pub fn peer_hash(install_salt: &[u8; 32], peer: &NodeId) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(install_salt);
    hasher.update(peer.as_bytes());
    *hasher.finalize().as_bytes()
}

/// Sink for audit records. Phase 4 backs this with an append-only SQLite table
/// with a 30 day retention and UI export/delete (§15).
pub trait AuditSink: Send {
    /// Appends one record. Implementations must not block the consent path.
    fn append(&mut self, record: AuditRecord);
}

/// Sink that drops everything: used in tests and before a real sink is wired.
#[derive(Debug, Default)]
pub struct NullAuditSink;

impl AuditSink for NullAuditSink {
    fn append(&mut self, _record: AuditRecord) {}
}
