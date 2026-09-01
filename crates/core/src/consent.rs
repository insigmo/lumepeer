//! Consent model: roles, consent queue, grants and revocation (design doc §8).
//!
//! Deny-by-default: nothing is permitted unless the host explicitly granted it,
//! and neither the UI nor the guest is a source of authorization (§2.1, §2.3).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::NodeId;
use crate::constants::{CONSENT_RATE_PER_MINUTE, MAX_PENDING_CONSENTS};
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
    reason = "§2.2 requires these seven permissions to stay independent flags"
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
    /// See the host's secure desktop (UAC prompt, lock screen, fast user
    /// switch) instead of the honest "can't see this" message
    /// (`docs/bugs/15-secure-desktop-capture.md`, ADR 0049).
    ///
    /// Independent of `view` and of every role, `FullControl` included: the
    /// same "`FullControl` does not imply recording or files" ground rule
    /// applies here, because a guest that can move the mouse is not thereby
    /// a guest who should watch an administrator authenticate.
    pub secure_desktop: bool,
}

/// One of the five grants a host may toggle on a session that is already
/// running, without changing its role (§8.2).
///
/// `view` and `input` are deliberately absent: they follow from [`Role`] and
/// change only through a grant or a revoke, so no caller outside this crate
/// can hand a guest input without the host picking a controller role for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndependentGrant {
    /// Read the host clipboard.
    ClipboardRead,
    /// Write the host clipboard.
    ClipboardWrite,
    /// Exchange files over `rd/file/1`.
    FileTransfer,
    /// Record the session.
    Recording,
    /// See the host's secure desktop (ADR 0049).
    SecureDesktop,
}

impl Grants {
    /// Grants implied by a role at the moment of `ConsentGrant` (§8.2).
    ///
    /// Clipboard, file transfer, recording and the secure desktop are never
    /// implied.
    #[must_use]
    pub const fn from_role(role: Role) -> Self {
        Self {
            view: true,
            input: matches!(role, Role::FullControl),
            clipboard_read: false,
            clipboard_write: false,
            file_transfer: false,
            recording: false,
            secure_desktop: false,
        }
    }

    /// Whether `which` is currently held.
    #[must_use]
    pub const fn get(self, which: IndependentGrant) -> bool {
        // Exhaustive on purpose, with no `_` arm: an eighth permission must
        // not be able to appear and silently read as denied here (§2.2).
        match which {
            IndependentGrant::ClipboardRead => self.clipboard_read,
            IndependentGrant::ClipboardWrite => self.clipboard_write,
            IndependentGrant::FileTransfer => self.file_transfer,
            IndependentGrant::Recording => self.recording,
            IndependentGrant::SecureDesktop => self.secure_desktop,
        }
    }

    /// Sets `which` to `allowed`, leaving `view` and `input` untouched.
    pub fn set(&mut self, which: IndependentGrant, allowed: bool) {
        match which {
            IndependentGrant::ClipboardRead => self.clipboard_read = allowed,
            IndependentGrant::ClipboardWrite => self.clipboard_write = allowed,
            IndependentGrant::FileTransfer => self.file_transfer = allowed,
            IndependentGrant::Recording => self.recording = allowed,
            IndependentGrant::SecureDesktop => self.secure_desktop = allowed,
        }
    }
}

/// One allowlistable action of the `ControlLimited` role (§8.2).
///
/// The set is deliberately coarse: a host granting "pointer only" should not
/// have to enumerate keys, and a policy file that needs a scancode table would
/// not be reviewable by the person it protects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlAction {
    /// Move the pointer.
    PointerMove,
    /// Press or release a pointer button.
    PointerClick,
    /// Scroll the wheel.
    Scroll,
    /// Press or release a key.
    KeyPress,
}

impl ControlAction {
    /// Action an input event asks for (§9.1, §11).
    #[must_use]
    pub const fn of(event: &crate::protocol::InputEventPayload) -> Self {
        use crate::protocol::InputDetail;
        match event.detail {
            InputDetail::PointerMove { .. } => Self::PointerMove,
            InputDetail::Wheel { .. } => Self::Scroll,
            InputDetail::Press | InputDetail::Release => {
                if event.logical >= crate::protocol::POINTER_BUTTON_LOGICAL_BASE {
                    Self::PointerClick
                } else {
                    Self::KeyPress
                }
            }
        }
    }
}

/// Host policy for the `ControlLimited` role (§8.2, §5.1).
///
/// Deny by default: an action is permitted only if a rule names it. The host
/// edits this through its own UI; neither a guest nor the broker can reach it,
/// and a change applies to future `ConsentGrant` only — [`super::session::
/// SessionManager`] snapshots the allowlist when it grants, so an edit never
/// widens a session that is already running.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ControlPolicy {
    #[serde(default)]
    defaults: PolicyDefaults,
    #[serde(default)]
    peers: Vec<PolicyPeer>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct PolicyDefaults {
    #[serde(default)]
    allow: Vec<ControlAction>,
}

#[derive(Debug, Clone, Deserialize)]
struct PolicyPeer {
    /// Hex endpoint id the rule applies to.
    peer: String,
    #[serde(default)]
    allow: Vec<ControlAction>,
}

impl ControlPolicy {
    /// A policy that permits nothing.
    #[must_use]
    pub fn deny_all() -> Self {
        Self::default()
    }

    /// Parses `config/control_policy.toml`.
    ///
    /// # Errors
    /// [`CoreError::Malformed`] if the file is not valid TOML for this shape.
    /// A policy that fails to parse is not silently replaced by a permissive
    /// one: the caller keeps denying (§2.1).
    pub fn from_toml(text: &str) -> Result<Self> {
        toml::from_str(text).map_err(|_| CoreError::Malformed)
    }

    /// Actions `peer` may perform under `ControlLimited`.
    #[must_use]
    pub fn allowed_for(&self, peer: &NodeId) -> Vec<ControlAction> {
        let hex = peer.to_string();
        let mut allowed = self.defaults.allow.clone();
        for rule in &self.peers {
            if rule.peer.eq_ignore_ascii_case(&hex) {
                allowed.extend_from_slice(&rule.allow);
            }
        }
        allowed.sort_unstable();
        allowed.dedup();
        allowed
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

/// Sliding-window rate limit of `ConsentRequest` per authenticated peer (§9.2).
///
/// Independent of [`ConsentQueue`]: this bounds how often one `NodeId` may ask,
/// the queue bounds how many unanswered requests exist across all guests (§8.1).
/// Measured on the monotonic clock, so a wall-clock rollback cannot widen the
/// window (§12.3).
#[derive(Debug, Default)]
pub struct ConsentRateLimiter {
    hits: HashMap<NodeId, Vec<Instant>>,
}

impl ConsentRateLimiter {
    /// Creates an empty limiter.
    #[must_use]
    pub fn new() -> Self {
        Self {
            hits: HashMap::new(),
        }
    }

    /// Records one request from `peer`.
    ///
    /// # Errors
    /// [`CoreError::ConsentRateLimited`] once `peer` has already made
    /// `CONSENT_RATE_PER_MINUTE` requests inside the last minute. The excess
    /// request is not recorded, so it cannot push the window forward.
    pub fn check(&mut self, peer: NodeId) -> Result<()> {
        self.check_at(peer, Instant::now())
    }

    /// [`Self::check`] against an explicit instant, for tests.
    ///
    /// # Errors
    /// Same as [`Self::check`].
    pub fn check_at(&mut self, peer: NodeId, now: Instant) -> Result<()> {
        let window = Duration::from_mins(1);
        let hits = self.hits.entry(peer).or_default();
        hits.retain(|at| now.duration_since(*at) < window);
        if hits.len() >= CONSENT_RATE_PER_MINUTE as usize {
            return Err(CoreError::ConsentRateLimited);
        }
        hits.push(now);
        Ok(())
    }

    /// Forgets the history of `peer`, e.g. after the session ended.
    pub fn forget(&mut self, peer: &NodeId) {
        self.hits.remove(peer);
    }

    /// Forgets every peer.
    pub fn clear(&mut self) {
        self.hits.clear();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// Deterministic test peer.
    pub(crate) fn peer(n: u8) -> NodeId {
        iroh_base::SecretKey::from_bytes(&[n; 32]).public()
    }

    #[test]
    fn queue_overflow_rejects_newcomer_without_evicting() {
        let mut queue = ConsentQueue::new();
        for n in 0..u8::try_from(MAX_PENDING_CONSENTS).unwrap() {
            queue.push(peer(n + 1), Role::ViewOnly).unwrap();
        }
        let overflow = queue.push(peer(200), Role::ViewOnly);
        assert!(matches!(overflow, Err(CoreError::PendingConsentQueueFull)));
        // §8.1: the existing queue is untouched, nothing is evicted.
        assert_eq!(queue.len(), MAX_PENDING_CONSENTS);
        assert!(queue.contains(&peer(1)));
        assert!(!queue.contains(&peer(200)));
    }

    #[test]
    fn removing_a_ticket_renumbers_the_rest() {
        let mut queue = ConsentQueue::new();
        queue.push(peer(1), Role::ViewOnly).unwrap();
        queue.push(peer(2), Role::ViewOnly).unwrap();
        queue.push(peer(3), Role::FullControl).unwrap();
        queue.remove(&peer(1)).unwrap();
        let positions: Vec<u8> = queue.tickets().iter().map(|t| t.queue_position).collect();
        assert_eq!(positions, vec![0, 1]);
    }

    #[test]
    fn full_control_implies_input_but_never_clipboard_files_recording_or_secure_desktop() {
        let grants = Grants::from_role(Role::FullControl);
        assert!(grants.view && grants.input);
        assert!(!grants.clipboard_read);
        assert!(!grants.clipboard_write);
        assert!(!grants.file_transfer);
        assert!(!grants.recording);
        assert!(!grants.secure_desktop);
    }

    #[test]
    fn rate_limiter_allows_the_quota_then_refuses_within_the_minute() {
        let mut limiter = ConsentRateLimiter::new();
        let start = Instant::now();
        for i in 0..CONSENT_RATE_PER_MINUTE {
            limiter
                .check_at(peer(1), start + Duration::from_secs(u64::from(i)))
                .unwrap();
        }
        let refused = limiter.check_at(peer(1), start + Duration::from_secs(10));
        assert!(matches!(refused, Err(CoreError::ConsentRateLimited)));
        // A different peer has its own budget (§9.2 is per authenticated NodeId).
        limiter
            .check_at(peer(2), start + Duration::from_secs(10))
            .unwrap();
        // Once the window has passed the peer may ask again.
        limiter
            .check_at(peer(1), start + Duration::from_secs(61))
            .unwrap();
    }
}
