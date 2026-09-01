//! Session state machine and the host-side authorization decision point
//! (design doc §8, §8.3, §10).
//!
//! Public signatures follow §8.3. `Role`, `ConsentTicket` and `ConsentQueue`
//! are defined in [`crate::consent`] (§6 puts them there) and re-exported here
//! so that `session::Role` used by §8.3 resolves.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use rand::Rng as _;

use crate::NodeId;
use crate::audit::AuditEvent;
pub use crate::consent::{
    ConsentQueue, ConsentRateLimiter, ConsentTicket, ControlAction, ControlPolicy, Grants,
    IndependentGrant, Role,
};
use crate::constants::RECONNECT_WINDOW_SECS;
use crate::error::{CoreError, Result};
use crate::license::Plan;
use crate::protocol::InputEventPayload;

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
    /// Allowlist captured when consent was granted. §8.2: a later policy edit
    /// applies to future grants only and never widens a running session.
    allowed_actions: Vec<ControlAction>,
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
    limiter: ConsentRateLimiter,
    policy: ControlPolicy,
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
            limiter: ConsentRateLimiter::new(),
            policy: ControlPolicy::deny_all(),
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

    /// Replaces the `ControlLimited` policy (§8.2).
    ///
    /// Running sessions keep the allowlist they were granted under; the new
    /// policy applies to the next `ConsentGrant`.
    pub fn set_control_policy(&mut self, policy: ControlPolicy) {
        self.policy = policy;
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
    /// - [`CoreError::ConsentRateLimited`] when `peer` exceeded
    ///   `CONSENT_RATE_PER_MINUTE` (§9.2). Checked before the queue so that a
    ///   flooding peer cannot fill it.
    /// - [`CoreError::PendingConsentQueueFull`] as in [`Self::request_consent`].
    pub fn request_consent_as(&mut self, peer: NodeId, role: Role) -> Result<ConsentTicket> {
        self.limiter.check(peer)?;
        self.queue.push(peer, role)
    }

    /// Forgets `peer`'s consent-request history, e.g. after a session with it
    /// ended normally (docs/bugs/03-connection-list.md, task 2).
    ///
    /// The limiter stays private to this module — `crates/core` is the sole
    /// authorization point — so a caller that needs to reset it after a
    /// session ends goes through this wrapper rather than reaching into
    /// `ConsentRateLimiter` directly.
    pub fn forget_consent_rate(&mut self, peer: &NodeId) {
        self.limiter.forget(peer);
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
            .map_or_else(Self::new_session_id, |s| s.session_id);
        let allowed_actions = if role == Role::ControlLimited {
            self.policy.allowed_for(&peer)
        } else {
            Vec::new()
        };
        self.sessions.insert(
            peer,
            ActiveSession {
                session_id,
                role,
                grants: Grants::from_role(role),
                allowed_actions,
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

    /// Turns one independent grant of `peer` on or off (§8.2).
    ///
    /// Only the four grants of [`IndependentGrant`] can move this way: `view`
    /// and `input` follow the role and change only through [`Self::grant`] and
    /// [`Self::revoke`], so no caller can widen a session into a controller
    /// behind the host's back. The change applies to this session's own
    /// snapshot and dies with it — [`Self::revoke`] drops the whole session,
    /// and the next [`Self::grant`] starts again from [`Grants::from_role`],
    /// which grants none of the four.
    ///
    /// Returns the [`AuditEvent`] the caller is expected to append: the
    /// manager holds no sink of its own (§15).
    ///
    /// # Errors
    /// - [`CoreError::UnknownPeer`] if the peer holds no session.
    /// - [`CoreError::NotPermitted`] if the session is not `Active`; a
    ///   pending, reconnecting or ended session is never widened.
    pub fn set_grant(
        &mut self,
        peer: NodeId,
        which: IndependentGrant,
        allowed: bool,
    ) -> Result<AuditEvent> {
        let session = self.sessions.get_mut(&peer).ok_or(CoreError::UnknownPeer)?;
        if session.state != SessionState::Active {
            return Err(CoreError::NotPermitted);
        }
        session.grants.set(which, allowed);
        Ok(AuditEvent::GrantChanged {
            grant: which,
            enabled: allowed,
        })
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

    /// Decides whether one input event may reach the platform adapter (§11).
    ///
    /// Every event passes through here: the adapter is not allowed to inject
    /// anything the core has not authorized, and the check is per event rather
    /// than per session so that a revoke takes effect on the next event.
    ///
    /// # Errors
    /// - [`CoreError::UnknownPeer`] if the peer holds no session.
    /// - [`CoreError::NotPermitted`] if the session is not active, holds no
    ///   `input` grant, or asks for an action outside its allowlist (§8.2).
    pub fn authorize_input(&self, peer: &NodeId, event: &InputEventPayload) -> Result<()> {
        let session = self.sessions.get(peer).ok_or(CoreError::UnknownPeer)?;
        if session.state != SessionState::Active {
            return Err(CoreError::NotPermitted);
        }
        let permitted = match session.role {
            // `view` only: nothing to inject, ever.
            Role::ViewOnly => false,
            // The grant is the authority, not the role name.
            Role::FullControl => session.grants.input,
            Role::ControlLimited => session.allowed_actions.contains(&ControlAction::of(event)),
        };
        if permitted {
            Ok(())
        } else {
            Err(CoreError::NotPermitted)
        }
    }

    /// Actions `peer` may perform right now, as captured at grant time.
    #[must_use]
    pub fn allowed_actions(&self, peer: &NodeId) -> &[ControlAction] {
        self.sessions
            .get(peer)
            .map_or(&[], |session| session.allowed_actions.as_slice())
    }

    /// Every guest holding grants, in no particular order.
    #[must_use]
    pub fn active(&self) -> Vec<(NodeId, Role, Grants)> {
        self.sessions
            .iter()
            .map(|(peer, session)| (*peer, session.role, session.grants))
            .collect()
    }

    /// Session id assigned to `peer`, if it has a session.
    #[must_use]
    pub fn session_id(&self, peer: &NodeId) -> Option<[u8; 16]> {
        self.sessions.get(peer).map(|s| s.session_id)
    }

    /// Role currently held by `peer`, if it has an active session.
    #[must_use]
    pub fn role(&self, peer: &NodeId) -> Option<Role> {
        self.sessions.get(peer).map(|s| s.role)
    }

    /// Ends every session and drops the queue: screen lock, user switch,
    /// license expiry (§8.1, §18).
    pub fn end_all(&mut self) {
        self.queue.clear();
        self.sessions.clear();
    }

    /// Fresh session id from the CSPRNG, generated by the host after the
    /// authenticated handshake (§9.1). It is never derived from peer identity.
    #[must_use]
    pub fn new_session_id() -> [u8; 16] {
        let mut id = [0u8; 16];
        rand::rng().fill_bytes(&mut id);
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use proptest::prelude::*;

    use super::*;
    use crate::constants::CONSENT_RATE_PER_MINUTE;

    fn peer(n: u8) -> NodeId {
        iroh_base::SecretKey::from_bytes(&[n; 32]).public()
    }

    const ALL_STATES: [SessionState; 8] = [
        SessionState::Idle,
        SessionState::Invited,
        SessionState::Connecting,
        SessionState::Authenticating,
        SessionState::AwaitingConsent,
        SessionState::Active,
        SessionState::Reconnecting,
        SessionState::Ended,
    ];

    #[test]
    fn trial_and_pro_admit_exactly_one_guest() {
        for plan in [Plan::Trial, Plan::Pro] {
            let mut manager = SessionManager::with_plan(plan);
            manager.grant(peer(1), Role::ViewOnly).unwrap();
            let second = manager.grant(peer(2), Role::ViewOnly);
            assert!(matches!(
                second,
                Err(CoreError::ConcurrentGuestLimit { limit: 1 })
            ));
            assert_eq!(manager.active_guest_count(), 1);
        }
    }

    /// docs/bugs/03-connection-list.md, task 2: a peer that completed a
    /// clean session gets its consent-request counter forgotten, so a normal
    /// reconnect right after does not inherit the count from the session
    /// that just ended.
    #[test]
    fn forgetting_the_consent_rate_lets_a_sixth_request_through() {
        let mut manager = SessionManager::new();
        let who = peer(1);
        // Each cycle resolves before the next request, the way a real
        // grant-then-revoke does — `MAX_PENDING_CONSENTS` (unresolved
        // requests) is a different, smaller limit than the rate limiter
        // under test here, and would otherwise trip first.
        for _ in 0..CONSENT_RATE_PER_MINUTE {
            manager.request_consent(who).unwrap();
            manager.grant(who, Role::ViewOnly).unwrap();
            manager.revoke(who).unwrap();
        }
        assert!(matches!(
            manager.request_consent(who),
            Err(CoreError::ConsentRateLimited)
        ));

        manager.forget_consent_rate(&who);
        assert!(manager.request_consent(who).is_ok());
    }

    #[test]
    fn team_counts_the_controller_inside_the_ceiling_of_five() {
        let mut manager = SessionManager::with_plan(Plan::Team);
        manager.grant(peer(1), Role::FullControl).unwrap();
        for n in 2..=5 {
            manager.grant(peer(n), Role::ViewOnly).unwrap();
        }
        assert_eq!(manager.active_guest_count(), 5);
        let sixth = manager.grant(peer(6), Role::ViewOnly);
        assert!(matches!(
            sixth,
            Err(CoreError::ConcurrentGuestLimit { limit: 5 })
        ));
    }

    #[test]
    fn a_second_controller_needs_the_first_revoked() {
        let mut manager = SessionManager::with_plan(Plan::Team);
        manager.grant(peer(1), Role::ControlLimited).unwrap();
        let clash = manager.grant(peer(2), Role::FullControl);
        assert!(matches!(clash, Err(CoreError::ControllerAlreadyGranted)));
        manager.revoke(peer(1)).unwrap();
        manager.grant(peer(2), Role::FullControl).unwrap();
        assert_eq!(manager.controller(), Some(peer(2)));
    }

    fn key_event() -> InputEventPayload {
        InputEventPayload {
            logical: 65,
            scancode: 30,
            modifiers: 0,
            detail: crate::protocol::InputDetail::Press,
        }
    }

    fn move_event() -> InputEventPayload {
        InputEventPayload {
            logical: 0,
            scancode: 0,
            modifiers: 0,
            detail: crate::protocol::InputDetail::PointerMove { x: 1, y: 2 },
        }
    }

    #[test]
    fn view_only_may_never_inject_anything() {
        let mut manager = SessionManager::new();
        manager.grant(peer(1), Role::ViewOnly).unwrap();
        assert!(matches!(
            manager.authorize_input(&peer(1), &key_event()),
            Err(CoreError::NotPermitted)
        ));
        assert!(matches!(
            manager.authorize_input(&peer(1), &move_event()),
            Err(CoreError::NotPermitted)
        ));
    }

    #[test]
    fn full_control_may_inject_and_an_unknown_peer_may_not() {
        let mut manager = SessionManager::new();
        manager.grant(peer(1), Role::FullControl).unwrap();
        manager.authorize_input(&peer(1), &key_event()).unwrap();
        assert!(matches!(
            manager.authorize_input(&peer(2), &key_event()),
            Err(CoreError::UnknownPeer)
        ));
    }

    #[test]
    fn control_limited_is_deny_by_default_and_follows_the_allowlist() {
        let policy = ControlPolicy::from_toml(
            r#"
            [defaults]
            allow = ["pointer_move"]
            "#,
        )
        .unwrap();

        let mut manager = SessionManager::new();
        // Without a policy nothing is permitted, not even with the role.
        manager.grant(peer(1), Role::ControlLimited).unwrap();
        assert!(matches!(
            manager.authorize_input(&peer(1), &move_event()),
            Err(CoreError::NotPermitted)
        ));

        manager.revoke(peer(1)).unwrap();
        manager.set_control_policy(policy);
        manager.grant(peer(1), Role::ControlLimited).unwrap();
        manager.authorize_input(&peer(1), &move_event()).unwrap();
        // Keys are not on the allowlist.
        assert!(matches!(
            manager.authorize_input(&peer(1), &key_event()),
            Err(CoreError::NotPermitted)
        ));
    }

    #[test]
    fn a_policy_edit_does_not_widen_a_running_session() {
        let mut manager = SessionManager::new();
        manager.grant(peer(1), Role::ControlLimited).unwrap();

        // The host widens the policy while the session runs (§8.2).
        manager.set_control_policy(
            ControlPolicy::from_toml(
                r#"
                [defaults]
                allow = ["pointer_move", "key_press"]
                "#,
            )
            .unwrap(),
        );
        assert!(manager.allowed_actions(&peer(1)).is_empty());
        assert!(matches!(
            manager.authorize_input(&peer(1), &move_event()),
            Err(CoreError::NotPermitted)
        ));

        // Only the next grant picks it up.
        manager.revoke(peer(1)).unwrap();
        manager.grant(peer(1), Role::ControlLimited).unwrap();
        manager.authorize_input(&peer(1), &move_event()).unwrap();
    }

    #[test]
    fn a_per_peer_rule_adds_to_the_defaults() {
        let policy = ControlPolicy::from_toml(&format!(
            r#"
            [defaults]
            allow = ["pointer_move"]

            [[peers]]
            peer = "{}"
            allow = ["scroll"]
            "#,
            peer(1)
        ))
        .unwrap();
        assert_eq!(
            policy.allowed_for(&peer(1)),
            vec![ControlAction::PointerMove, ControlAction::Scroll]
        );
        assert_eq!(
            policy.allowed_for(&peer(2)),
            vec![ControlAction::PointerMove]
        );
    }

    #[test]
    fn a_malformed_policy_does_not_become_a_permissive_one() {
        assert!(ControlPolicy::from_toml("this is not toml [[[").is_err());
        assert!(
            ControlPolicy::from_toml(
                r#"[defaults]
            allow = ["fly_the_plane"]"#
            )
            .is_err()
        );
    }

    #[test]
    fn the_checked_in_policy_file_permits_nothing() {
        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../config/control_policy.toml"
        ))
        .unwrap();
        let policy = ControlPolicy::from_toml(&text).unwrap();
        assert!(policy.allowed_for(&peer(1)).is_empty());
    }

    #[test]
    fn a_revoked_session_stops_authorizing_on_the_next_event() {
        let mut manager = SessionManager::new();
        manager.grant(peer(1), Role::FullControl).unwrap();
        manager.authorize_input(&peer(1), &key_event()).unwrap();
        manager.revoke(peer(1)).unwrap();
        assert!(matches!(
            manager.authorize_input(&peer(1), &key_event()),
            Err(CoreError::UnknownPeer)
        ));
    }

    #[test]
    fn a_disconnected_session_does_not_authorize_input() {
        let mut manager = SessionManager::new();
        manager.grant(peer(1), Role::FullControl).unwrap();
        manager.on_disconnect(peer(1)).unwrap();
        assert!(matches!(
            manager.authorize_input(&peer(1), &key_event()),
            Err(CoreError::NotPermitted)
        ));
    }

    #[test]
    fn revoke_drops_every_grant() {
        let mut manager = SessionManager::new();
        manager.grant(peer(1), Role::FullControl).unwrap();
        manager.revoke(peer(1)).unwrap();
        assert_eq!(manager.grants(&peer(1)), None);
        assert_eq!(manager.state(&peer(1)), SessionState::Idle);
        assert!(matches!(
            manager.revoke(peer(1)),
            Err(CoreError::UnknownPeer)
        ));
    }

    #[test]
    fn session_ids_are_random_not_derived_from_the_peer() {
        let mut manager = SessionManager::new();
        manager.grant(peer(1), Role::ViewOnly).unwrap();
        let first = manager.session_id(&peer(1)).unwrap();
        manager.revoke(peer(1)).unwrap();
        manager.grant(peer(1), Role::ViewOnly).unwrap();
        let second = manager.session_id(&peer(1)).unwrap();
        assert_ne!(first, second);
        assert_ne!(first, [0u8; 16]);
        assert!(!peer(1).as_bytes().starts_with(&first));
    }

    #[test]
    fn reconnect_needs_a_disconnected_session() {
        let mut manager = SessionManager::new();
        assert!(matches!(
            manager.on_reconnect(peer(1)),
            ReconnectDecision::Reject {
                reason: RejectReason::UnknownSession
            }
        ));
        manager.grant(peer(1), Role::ViewOnly).unwrap();
        // Still connected: nothing to resume, the window never opened.
        assert!(matches!(
            manager.on_reconnect(peer(1)),
            ReconnectDecision::Reject {
                reason: RejectReason::WindowElapsed
            }
        ));
    }

    #[test]
    fn reconnect_inside_the_window_keeps_the_session_id_and_grants() {
        let mut manager = SessionManager::new();
        manager.grant(peer(1), Role::FullControl).unwrap();
        let before = manager.session_id(&peer(1)).unwrap();
        let grants = manager.grants(&peer(1)).unwrap();
        manager.on_disconnect(peer(1)).unwrap();
        assert_eq!(manager.state(&peer(1)), SessionState::Reconnecting);
        let ReconnectDecision::Resume { session_id } = manager.on_reconnect(peer(1)) else {
            panic!("expected resume inside the window");
        };
        assert_eq!(session_id, before);
        assert_eq!(manager.grants(&peer(1)), Some(grants));
        assert_eq!(manager.state(&peer(1)), SessionState::Active);
    }

    const ALL_INDEPENDENT: [IndependentGrant; 5] = [
        IndependentGrant::ClipboardRead,
        IndependentGrant::ClipboardWrite,
        IndependentGrant::FileTransfer,
        IndependentGrant::Recording,
        IndependentGrant::SecureDesktop,
    ];

    #[test]
    fn every_independent_grant_can_be_turned_on_and_off() {
        for which in ALL_INDEPENDENT {
            let mut manager = SessionManager::new();
            manager.grant(peer(1), Role::ViewOnly).unwrap();
            assert!(!manager.grants(&peer(1)).unwrap().get(which));

            let event = manager.set_grant(peer(1), which, true).unwrap();
            assert_eq!(
                event,
                AuditEvent::GrantChanged {
                    grant: which,
                    enabled: true
                }
            );
            assert!(manager.grants(&peer(1)).unwrap().get(which));
            // One grant moving leaves the other four where they were.
            for other in ALL_INDEPENDENT.into_iter().filter(|o| *o != which) {
                assert!(!manager.grants(&peer(1)).unwrap().get(other));
            }

            manager.set_grant(peer(1), which, false).unwrap();
            assert!(!manager.grants(&peer(1)).unwrap().get(which));
        }
    }

    #[test]
    fn a_grant_never_reaches_view_or_input() {
        let mut manager = SessionManager::new();
        manager.grant(peer(1), Role::ViewOnly).unwrap();
        for which in ALL_INDEPENDENT {
            manager.set_grant(peer(1), which, true).unwrap();
        }
        let grants = manager.grants(&peer(1)).unwrap();
        assert!(grants.view);
        assert!(!grants.input);
        assert!(matches!(
            manager.authorize_input(&peer(1), &key_event()),
            Err(CoreError::NotPermitted)
        ));
    }

    #[test]
    fn only_an_active_session_takes_a_grant() {
        let mut manager = SessionManager::new();
        // Unknown peer: nothing to widen.
        assert!(matches!(
            manager.set_grant(peer(1), IndependentGrant::FileTransfer, true),
            Err(CoreError::UnknownPeer)
        ));

        // Pending consent is not a session yet.
        manager.request_consent_as(peer(1), Role::ViewOnly).unwrap();
        assert!(matches!(
            manager.set_grant(peer(1), IndependentGrant::FileTransfer, true),
            Err(CoreError::UnknownPeer)
        ));

        // Inside the reconnect window the picture is not on screen; a grant
        // made here would land on a session nobody is watching.
        manager.grant(peer(1), Role::ViewOnly).unwrap();
        manager.on_disconnect(peer(1)).unwrap();
        assert!(matches!(
            manager.set_grant(peer(1), IndependentGrant::FileTransfer, true),
            Err(CoreError::NotPermitted)
        ));

        // Ended: revoke removed the session outright.
        manager.revoke(peer(1)).unwrap();
        assert!(matches!(
            manager.set_grant(peer(1), IndependentGrant::FileTransfer, true),
            Err(CoreError::UnknownPeer)
        ));
    }

    #[test]
    fn revoke_drops_independent_grants_and_a_new_grant_does_not_restore_them() {
        let mut manager = SessionManager::new();
        manager.grant(peer(1), Role::FullControl).unwrap();
        manager
            .set_grant(peer(1), IndependentGrant::FileTransfer, true)
            .unwrap();
        manager
            .set_grant(peer(1), IndependentGrant::Recording, true)
            .unwrap();

        manager.revoke(peer(1)).unwrap();
        assert_eq!(manager.grants(&peer(1)), None);

        manager.grant(peer(1), Role::FullControl).unwrap();
        assert_eq!(
            manager.grants(&peer(1)),
            Some(Grants::from_role(Role::FullControl))
        );
    }

    proptest! {
        /// No sequence of `set_grant` can reach `view` or `input`: those two
        /// follow the role, and this is the property the split of §8.2 into a
        /// role plus four independent grants exists to hold.
        #[test]
        fn set_grant_never_moves_view_or_input(
            role_index in 0usize..3,
            steps in prop::collection::vec((0usize..ALL_INDEPENDENT.len(), any::<bool>()), 0..24),
        ) {
            let role = [Role::ViewOnly, Role::ControlLimited, Role::FullControl][role_index];
            let mut manager = SessionManager::new();
            manager.grant(peer(1), role).unwrap();
            let expected = Grants::from_role(role);

            for (which, allowed) in steps {
                manager.set_grant(peer(1), ALL_INDEPENDENT[which], allowed).unwrap();
                let grants = manager.grants(&peer(1)).unwrap();
                prop_assert_eq!(grants.view, expected.view);
                prop_assert_eq!(grants.input, expected.input);
            }
        }

        /// Only the edges drawn in §8.1 are accepted, in both directions of the
        /// pair, and every state may end.
        #[test]
        fn transitions_match_the_state_machine(
            from in 0usize..ALL_STATES.len(),
            to in 0usize..ALL_STATES.len(),
        ) {
            let from = ALL_STATES[from];
            let to = ALL_STATES[to];
            let allowed = SessionManager::check_transition(from, to).is_ok();
            // Spelled out edge by edge on purpose: the point of the test is to
            // restate §8.1 independently of how `check_transition` groups them.
            #[allow(clippy::unnested_or_patterns, reason = "mirrors the §8.1 table")]
            let expected = matches!(
                (from, to),
                (SessionState::Idle, SessionState::Invited)
                    | (SessionState::Invited, SessionState::Connecting)
                    | (SessionState::Connecting, SessionState::Authenticating)
                    | (SessionState::Authenticating, SessionState::AwaitingConsent)
                    | (SessionState::AwaitingConsent, SessionState::Active)
                    | (SessionState::Active, SessionState::Reconnecting)
                    | (SessionState::Reconnecting, SessionState::Active)
            ) || (to == SessionState::Ended && from != SessionState::Ended);
            prop_assert_eq!(allowed, expected);
        }

        /// `Ended` is terminal and no state may loop back into itself.
        #[test]
        fn ended_is_terminal_and_no_self_loops(index in 0usize..ALL_STATES.len()) {
            let state = ALL_STATES[index];
            prop_assert!(SessionManager::check_transition(SessionState::Ended, state).is_err());
            prop_assert!(SessionManager::check_transition(state, state).is_err());
        }
    }
}
