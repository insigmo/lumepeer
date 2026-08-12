//! Session state machine and the host-side authorization decision point
//! (design doc §8, §8.3, §10).
//!
//! Public signatures follow §8.3. `Role`, `ConsentTicket` and `ConsentQueue`
//! are defined in [`crate::consent`] (§6 puts them there) and re-exported here
//! so that `session::Role` used by §8.3 resolves.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::NodeId;
pub use crate::consent::{ConsentQueue, ConsentTicket, Grants, Role};
use crate::constants::RECONNECT_WINDOW_SECS;
use crate::error::{CoreError, Result};
use crate::license::Plan;

/// States of a single guest session (§8.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// No peer attached.
    Idle,
    /// An invite was issued but not yet claimed.
    Invited,
    /// Transport dial in progress.
    Connecting,
    /// Handshake and invite proof verification in progress.
    Authenticating,
    /// Waiting for the host user's decision.
    AwaitingConsent,
    /// Consent granted; grants are live.
    Active,
    /// Transport lost, inside the reconnect window (§10).
    Reconnecting,
    /// Terminal state.
    Ended,
}

impl SessionState {
    const fn name(self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::Invited => "Invited",
            Self::Connecting => "Connecting",
            Self::Authenticating => "Authenticating",
            Self::AwaitingConsent => "AwaitingConsent",
            Self::Active => "Active",
            Self::Reconnecting => "Reconnecting",
            Self::Ended => "Ended",
        }
    }
}

/// Outcome of a reconnect attempt (§10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconnectDecision {
    /// Same peer, same session, unchanged grants, inside the window.
    Resume {
        /// Session that may be resumed.
        session_id: [u8; 16],
    },
    /// Reconnect refused; a new invite and a new consent are required.
    Reject {
        /// Machine-readable reason, mirrored into the audit log (§15).
        reason: RejectReason,
    },
}

/// Why a reconnect was refused (§10, §18).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// The reconnect window of `RECONNECT_WINDOW_SECS` has elapsed.
    WindowElapsed,
    /// The peer has no session in `Reconnecting` state.
    UnknownSession,
}

/// One authorized guest.
#[derive(Debug, Clone)]
struct ActiveSession {
    session_id: [u8; 16],
    role: Role,
    grants: Grants,
    state: SessionState,
    /// Monotonic instant the session entered `Reconnecting` (§12.3: never
    /// wall-clock).
    disconnected_at: Option<Instant>,
}

/// Owner of all session state on the host. The only component allowed to move
/// a session from `AwaitingConsent` to `Active` (§2.3, §8.1).
#[derive(Debug)]
pub struct SessionManager {
    plan: Plan,
    queue: ConsentQueue,
    sessions: HashMap<NodeId, ActiveSession>,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionManager {
    /// Creates a manager on the trial plan; the license layer replaces the
    /// plan once a token is validated (§12).
    #[must_use]
    pub fn new() -> Self {
        Self::with_plan(Plan::Trial)
    }

    /// Creates a manager for an explicit plan.
    #[must_use]
    pub fn with_plan(plan: Plan) -> Self {
        Self {
            plan,
            queue: ConsentQueue::new(),
            sessions: HashMap::new(),
        }
    }

    /// Plan currently in force.
    #[must_use]
    pub const fn plan(&self) -> Plan {
        self.plan
    }

    /// Replaces the plan, e.g. after a license refresh (§12.2).
    pub const fn set_plan(&mut self, plan: Plan) {
        self.plan = plan;
    }

    /// Queues a consent request for `peer`.
    ///
    /// # Errors
    /// [`CoreError::PendingConsentQueueFull`] when the queue is full (§8.1);
    /// the existing queue is not modified and nothing is evicted.
    pub fn request_consent(&mut self, peer: NodeId) -> Result<ConsentTicket> {
        self.request_consent_as(peer, Role::ViewOnly)
    }

    /// Queues a consent request for an explicitly requested role.
    ///
    /// The requested role is advisory: the host may grant a lower one (§2.3).
    ///
    /// # Errors
    /// Same as [`Self::request_consent`].
    pub fn request_consent_as(&mut self, peer: NodeId, role: Role) -> Result<ConsentTicket> {
        self.queue.push(peer, role)
    }

    /// Grants `role` to `peer`, moving it to `Active`.
    ///
    /// # Errors
    /// - [`CoreError::ConcurrentGuestLimit`] if the plan ceiling would be
    ///   exceeded (§8.2).
    /// - [`CoreError::ControllerAlreadyGranted`] if another peer already holds
    ///   a controller role; the old controller must be revoked first (§8.2).
    pub fn grant(&mut self, peer: NodeId, role: Role) -> Result<()> {
        let limit = self.plan.max_concurrent_guests();
        let already_active = self.sessions.contains_key(&peer);
        if !already_active && self.active_guest_count() >= usize::from(limit) {
            return Err(CoreError::ConcurrentGuestLimit { limit });
        }
        if role.is_controller() && self.controller().is_some_and(|holder| holder != peer) {
            return Err(CoreError::ControllerAlreadyGranted);
        }

        self.queue.remove(&peer);
        let session_id = self
            .sessions
            .get(&peer)
            .map_or_else(|| Self::new_session_id(&peer), |s| s.session_id);
        self.sessions.insert(
            peer,
            ActiveSession {
                session_id,
                role,
                grants: Grants::from_role(role),
                state: SessionState::Active,
                disconnected_at: None,
            },
        );
        Ok(())
    }

    /// Revokes every grant of `peer` and ends its session (§8.1).
    ///
    /// # Errors
    /// [`CoreError::UnknownPeer`] if the peer has neither a queued request nor
    /// an active session.
    pub fn revoke(&mut self, peer: NodeId) -> Result<()> {
        let had_session = self.sessions.remove(&peer).is_some();
        let had_request = self.queue.remove(&peer).is_some();
        if had_session || had_request {
            Ok(())
        } else {
            Err(CoreError::UnknownPeer)
        }
    }

    /// Number of guests currently holding grants, controller included (§8.2).
    #[must_use]
    pub fn active_guest_count(&self) -> usize {
        self.sessions.len()
    }

    /// Decides whether `peer` may resume its session (§10).
    #[must_use]
    pub fn on_reconnect(&mut self, peer: NodeId) -> ReconnectDecision {
        let Some(session) = self.sessions.get(&peer) else {
            return ReconnectDecision::Reject {
                reason: RejectReason::UnknownSession,
            };
        };
        let within_window = session
            .disconnected_at
            .is_some_and(|at| at.elapsed() <= Duration::from_secs(RECONNECT_WINDOW_SECS));
        if !within_window {
            self.sessions.remove(&peer);
            return ReconnectDecision::Reject {
                reason: RejectReason::WindowElapsed,
            };
        }
        let session_id = session.session_id;
        if let Some(session) = self.sessions.get_mut(&peer) {
            session.state = SessionState::Active;
            session.disconnected_at = None;
        }
        ReconnectDecision::Resume { session_id }
    }

    /// Marks `peer` as disconnected, starting the reconnect window (§10).
    ///
    /// # Errors
    /// [`CoreError::UnknownPeer`] if the peer holds no session.
    pub fn on_disconnect(&mut self, peer: NodeId) -> Result<()> {
        let session = self.sessions.get_mut(&peer).ok_or(CoreError::UnknownPeer)?;
        session.state = SessionState::Reconnecting;
        session.disconnected_at = Some(Instant::now());
        Ok(())
    }

    /// Grants currently held by `peer`, if it has an active session.
    #[must_use]
    pub fn grants(&self, peer: &NodeId) -> Option<Grants> {
        self.sessions.get(peer).map(|s| s.grants)
    }

    /// State of `peer`'s session; `Idle` if it has none.
    #[must_use]
    pub fn state(&self, peer: &NodeId) -> SessionState {
        self.sessions
            .get(peer)
            .map_or(SessionState::Idle, |s| s.state)
    }

    /// Pending consent requests, oldest first.
    #[must_use]
    pub fn pending(&self) -> &[ConsentTicket] {
        self.queue.tickets()
    }

    /// Peer holding a controller role, if any (§8.2: at most one).
    #[must_use]
    pub fn controller(&self) -> Option<NodeId> {
        self.sessions
            .iter()
            .find(|(_, s)| s.role.is_controller())
            .map(|(peer, _)| *peer)
    }

    /// Ends every session and drops the queue: screen lock, user switch,
    /// license expiry (§8.1, §18).
    pub fn end_all(&mut self) {
        self.queue.clear();
        self.sessions.clear();
    }

    /// Placeholder session id derivation.
    ///
    /// Phase 1 replaces this with a CSPRNG-generated id produced by the host
    /// after the authenticated handshake (§9.1).
    fn new_session_id(peer: &NodeId) -> [u8; 16] {
        let mut id = [0u8; 16];
        id.copy_from_slice(&peer.as_bytes()[..16]);
        id
    }

    /// Validates a transition against the state machine of §8.1.
    ///
    /// # Errors
    /// [`CoreError::InvalidTransition`] when the transition is not allowed.
    #[allow(
        clippy::unnested_or_patterns,
        reason = "one arm per edge of the §8.1 state machine reads closer to the spec"
    )]
    pub const fn check_transition(from: SessionState, to: SessionState) -> Result<()> {
        use SessionState::{
            Active, Authenticating, AwaitingConsent, Connecting, Ended, Idle, Invited, Reconnecting,
        };
        let allowed = matches!(
            (from, to),
            (Idle, Invited)
                | (Invited, Connecting)
                | (Connecting, Authenticating)
                | (Authenticating, AwaitingConsent)
                | (AwaitingConsent, Active)
                | (Active, Reconnecting)
                | (Reconnecting, Active)
                | (
                    Idle | Invited
                        | Connecting
                        | Authenticating
                        | AwaitingConsent
                        | Active
                        | Reconnecting,
                    Ended
                )
        );
        if allowed {
            Ok(())
        } else {
            Err(CoreError::InvalidTransition {
                from: from.name(),
                to: to.name(),
            })
        }
    }
}
