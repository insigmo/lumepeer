//! Reconnect window and peer identity checks (design doc §10).
//!
//! Resume is allowed only within `RECONNECT_WINDOW_SECS`, only for the same
//! authenticated `NodeId`, the same `session_id` and unchanged grants.
//! 0-RTT for application data is forbidden.

use std::time::{Duration, Instant};

use lumepeer_core::NodeId;
use lumepeer_core::constants::RECONNECT_WINDOW_SECS;

/// State kept for a session that lost its transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectWindow {
    peer: NodeId,
    session_id: [u8; 16],
    opened_at: Instant,
}

impl ReconnectWindow {
    /// Opens the window at the moment the transport dropped.
    #[must_use]
    pub fn open(peer: NodeId, session_id: [u8; 16]) -> Self {
        Self {
            peer,
            session_id,
            opened_at: Instant::now(),
        }
    }

    /// Whether the window is still open, measured on the monotonic clock so a
    /// wall-clock rollback cannot extend it (§12.3).
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.opened_at.elapsed() <= Duration::from_secs(RECONNECT_WINDOW_SECS)
    }

    /// Whether `peer` and `session_id` match the interrupted session and the
    /// window is still open. Any mismatch ends the session and forces a new
    /// invite plus a new consent (§10, §18).
    #[must_use]
    pub fn accepts(&self, peer: &NodeId, session_id: &[u8; 16]) -> bool {
        self.is_open() && &self.peer == peer && &self.session_id == session_id
    }

    /// Peer allowed to resume.
    #[must_use]
    pub const fn peer(&self) -> NodeId {
        self.peer
    }

    /// Session that may be resumed.
    #[must_use]
    pub const fn session_id(&self) -> [u8; 16] {
        self.session_id
    }
}
