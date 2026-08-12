//! Consent model: roles, consent queue, grants and revocation (design doc §8).
//!
//! Deny-by-default: nothing is permitted unless the host explicitly granted it,
//! and neither the UI nor the guest is a source of authorization (§2.1, §2.3).

use serde::{Deserialize, Serialize};

use crate::NodeId;
use crate::constants::MAX_PENDING_CONSENTS;
use crate::error::{CoreError, Result};

/// Role a guest may hold. `FullControl` does not imply clipboard, file
/// transfer or recording — those are independent grants (§2.2, §8.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Role {
    /// Only `view`.
    ViewOnly,
    /// `view` plus allowlisted actions from `config/control_policy.toml` (§8.2).
    ControlLimited,
    /// `view` plus keyboard and mouse.
    FullControl,
}

impl Role {
    /// Whether this role counts against the single-controller rule (§8.2).
    #[must_use]
    pub const fn is_controller(self) -> bool {
        matches!(self, Self::ControlLimited | Self::FullControl)
    }
}

/// The independent grants an active session may hold (§8.1).
///
/// All fields default to `false`: a fresh session grants nothing.
#[allow(
    clippy::struct_excessive_bools,
    reason = "§2.2 requires these six permissions to stay independent flags"
)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Grants {
    /// Receive video and audio.
    pub view: bool,
    /// Inject keyboard and pointer input.
    pub input: bool,
    /// Read the host clipboard.
    pub clipboard_read: bool,
    /// Write the host clipboard.
    pub clipboard_write: bool,
    /// Exchange files over `rd/file/1`.
    pub file_transfer: bool,
    /// Record the session.
    pub recording: bool,
}

impl Grants {
    /// Grants implied by a role at the moment of `ConsentGrant` (§8.2).
    ///
    /// Clipboard, file transfer and recording are never implied.
    #[must_use]
    pub const fn from_role(role: Role) -> Self {
        Self {
            view: true,
            input: matches!(role, Role::FullControl),
            clipboard_read: false,
            clipboard_write: false,
            file_transfer: false,
            recording: false,
        }
    }
}

/// Opaque handle handed to the host when a consent request is queued (§8.3).
///
/// A guest cannot forge it; the host uses it to grant or revoke one specific
/// request without re-authenticating the peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsentTicket {
    /// Peer that asked for consent.
    pub peer: NodeId,
    /// Role the peer asked for; the host may grant a lower one.
    pub requested_role: Role,
    /// Monotonic instant, not wall-clock (§12.3).
    pub requested_at: std::time::Instant,
    /// 0-based position, bounded by `MAX_PENDING_CONSENTS`.
    pub queue_position: u8,
}

/// FIFO queue of unanswered consent requests, bounded by
/// `MAX_PENDING_CONSENTS` across all guests (§8.1).
#[derive(Debug, Default)]
pub struct ConsentQueue {
    pending: Vec<ConsentTicket>,
}

impl ConsentQueue {
    /// Creates an empty queue.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pending: Vec::new(),
        }
    }

    /// Number of unanswered requests.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Whether the queue holds no requests.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Queues a request and returns its ticket.
    ///
    /// A full queue rejects the newcomer instead of evicting an older request
    /// (§8.1, §18); the existing queue is left untouched.
    ///
    /// # Errors
    /// [`CoreError::PendingConsentQueueFull`] when the queue already holds
    /// `MAX_PENDING_CONSENTS` requests.
    pub fn push(&mut self, peer: NodeId, requested_role: Role) -> Result<ConsentTicket> {
        if self.pending.len() >= MAX_PENDING_CONSENTS {
            return Err(CoreError::PendingConsentQueueFull);
        }
        let ticket = ConsentTicket {
            peer,
            requested_role,
            requested_at: std::time::Instant::now(),
            queue_position: u8::try_from(self.pending.len()).unwrap_or(u8::MAX),
        };
        self.pending.push(ticket.clone());
        Ok(ticket)
    }

    /// Removes the request of `peer`, if any, and renumbers the remaining ones.
    pub fn remove(&mut self, peer: &NodeId) -> Option<ConsentTicket> {
        let index = self.pending.iter().position(|t| &t.peer == peer)?;
        let removed = self.pending.remove(index);
        for (position, ticket) in self.pending.iter_mut().enumerate() {
            ticket.queue_position = u8::try_from(position).unwrap_or(u8::MAX);
        }
        Some(removed)
    }

    /// Whether `peer` already has an unanswered request.
    #[must_use]
    pub fn contains(&self, peer: &NodeId) -> bool {
        self.pending.iter().any(|t| &t.peer == peer)
    }

    /// Currently queued requests, oldest first.
    #[must_use]
    pub fn tickets(&self) -> &[ConsentTicket] {
        &self.pending
    }

    /// Drops every pending request, e.g. on screen lock or session end (§18).
    pub fn clear(&mut self) {
        self.pending.clear();
    }
}
