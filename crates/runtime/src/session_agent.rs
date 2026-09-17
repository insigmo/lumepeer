//! What a host running as a service believes about the machine's own screen
//! (ADR 0085 §3, ADR 0088).
//!
//! A host inside somebody's desktop session never has to ask this question:
//! it *is* the session, so there is always a screen and always somebody who
//! could see a banner on it. A host running as a service has to ask it
//! constantly — nobody may be signed in, somebody may sign in half an hour
//! into a session a guest was admitted to unattended, and somebody may sign
//! out while a guest is watching.
//!
//! The answer is this state machine, and it is written as one rather than as
//! a handful of booleans on the actor because ADR 0085 §3's whole argument is
//! about **order**:
//!
//! - There is no capture without an agent, and no agent without an
//!   interactive session. A guest admitted while nobody is signed in gets the
//!   logon screen if this machine can serve it and the honest "no picture"
//!   state if it cannot (§18; ADR 0024), never a frozen frame and never a
//!   black one.
//! - The indicator goes up **before** the first frame leaves, not as a
//!   consequence of it. An agent that started capture and then raised the
//!   banner would have a window, however short, in which pixels left a machine
//!   showing no sign of it — which is exactly what §2.2 forbids and what a
//!   machine with nobody in front of it makes impossible to notice.
//! - An agent dies with its session. A host that stopped asking would go on
//!   believing there is a screen long after the person signed out, and would
//!   keep serving a picture that never changes.
//!
//! ADR 0088 adds the fourth, which is about **the moment in between**. A
//! machine people sign in and out of moves between three sources of a picture
//! — nothing, the logon screen, a signed-in session's agent — and every move
//! between them has a gap. What must never appear in that gap is the last
//! frame of what came before: after a fast user switch that frame is another
//! person's desktop. So a transition is a state of its own
//! ([`ScreenState::Switching`]), it is **latched** rather than recomputed on
//! every tick, and it is left only for a state something confirmed — an agent
//! that attached, a logon screen that did, or the machine saying there is no
//! session at all.
//!
//! Nothing here talks to an agent. It decides *what should be said and in what
//! order*, and the caller carries it out over
//! [`lumepeer_service::agent_channel`]; keeping the two apart is what lets the
//! order be tested on a machine with no service installed on it at all.

use lumepeer_core::consent::HostAttendance;
use lumepeer_service::agent_protocol::{AgentCommand, AgentEvent, CaptureUnavailableReason};
use lumepeer_service::session_change::SessionChange;

/// Where the machine's screen is, as far as the host can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenState {
    /// No interactive session, so no agent and no logon screen either:
    /// nothing to capture, and nobody who could see an indicator.
    NoSession,
    /// A logon-screen worker has been launched into `session` and has not
    /// attached yet (ADR 0088 §1).
    LogonScreenStarting {
        /// Console session whose `Winlogon` desktop is being served.
        session: u32,
    },
    /// The logon screen of `session` is attached and may publish frames.
    ///
    /// Nobody is signed in — or somebody is and has locked their session —
    /// so this is a picture with no person behind it and no indicator on it.
    /// [`SessionScreen::attendance`] says `Unattended` here, which is what
    /// keeps ADR 0085 §2's credential-only admission path in force.
    LogonScreen {
        /// Console session whose `Winlogon` desktop is being served.
        session: u32,
    },
    /// An agent has been launched into `session` and has not attached yet.
    ///
    /// Deliberately distinct from [`Serving`](Self::Serving): a process that
    /// has been started is not a screen, and treating the two the same is how
    /// a host promises a guest a picture that is still a few hundred
    /// milliseconds of process start-up away.
    Starting {
        /// Windows session the agent was launched into.
        session: u32,
    },
    /// The agent is attached, the indicator is up, and capture may run.
    Serving {
        /// Windows session the agent is serving.
        session: u32,
    },
    /// The machine is between sessions: somebody signed in, signed out,
    /// locked, unlocked or switched away, and what this host was serving is
    /// not what is on the console any more (ADR 0088 §2).
    ///
    /// Latched. It is entered by a session change and left only by something
    /// that confirms the next state, never by a tick that re-reads the
    /// machine — the failure mode ADR 0056's flickering secure-desktop flag
    /// already cost this project once.
    Switching {
        /// Session the machine is moving to, when the notification named one.
        /// `None` for a sign-out or a switch away, where what comes next is
        /// not yet known.
        to: Option<u32>,
    },
    /// The agent is gone — it exited, its session ended, or it said something
    /// this host could not parse.
    ///
    /// Distinct from [`NoSession`](Self::NoSession) only for the log: the two
    /// behave identically, because the point of both is that there is no
    /// picture. Keeping them apart is what lets "nobody was ever signed in" be
    /// told from "somebody signed out mid-session" afterwards.
    Lost,
}

/// Why a host has no picture to send, in the terms the guest's own
/// `MediaUnavailable` is phrased in (§18; ADR 0024).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoPicture {
    /// Nobody is signed in at this machine and its logon screen is not being
    /// served.
    NoInteractiveSession,
    /// The logon screen's worker is still starting.
    LogonScreenStarting,
    /// Somebody is signed in, and their agent is still starting.
    AgentStarting,
    /// The agent went away.
    AgentGone,
    /// The machine is between sessions (ADR 0088 §2).
    ///
    /// Its own reason rather than one of the two above, because it is the one
    /// a guest is most likely to see and the one where the wrong answer is
    /// worst: "somebody is signing in" is honest, and the last frame of the
    /// session they are replacing is another person's screen.
    SessionSwitching,
    /// The agent is there and says this session cannot produce a picture.
    CaptureUnavailable(CaptureUnavailableReason),
}

/// What the host should do about a [`SessionChange`] (ADR 0088 §2).
///
/// Three answers, because there are three: the change was about a session this
/// host does not serve, or it was about the one it does and either ends the
/// attachment or does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAction {
    /// The change happened in a session this host does not serve — an RDP
    /// session, or one somebody left running in the background. Nothing
    /// changes: the console session is the only one served, and pretending
    /// otherwise is what ADR 0088 §4 refuses to do without its own decision.
    Ignore,
    /// The change is about the served session and does not end the
    /// attachment.
    KeepServing,
    /// Whatever is attached must go, and the screen is now a transition until
    /// something confirms the next state.
    EndAttachment,
}

/// What kind of screen the host is serving, for the one decision that differs
/// between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// The signed-in user's own desktop, through the session agent.
    Agent,
    /// The console session's `Winlogon` desktop, through the logon-screen
    /// worker.
    LogonScreen,
}

/// The host's side of one session agent's lifetime.
///
/// One per machine, not one per guest: the screen is a fact about the machine,
/// and every guest sees the same answer.
#[derive(Debug)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "four independent facts from four sources — the agent, the               session notifications, the runtime's session count, and the               attachment — and folding any two into one enum would invent               combinations that mean nothing"
)]
pub struct SessionScreen {
    state: ScreenState,
    /// Set when the agent reported it cannot capture, cleared by anything that
    /// changes the attachment. Held separately from [`ScreenState`] because an
    /// agent that is attached and cannot capture is still an agent — the
    /// indicator is up and a person can see it — which is a different thing
    /// from having no agent at all.
    capture_refusal: Option<CaptureUnavailableReason>,
    /// The agent's own frame counter, as last seen.
    last_sequence: Option<u32>,
    /// Whether capture has been asked for on this attachment.
    capturing: bool,
    /// The session attached to the physical console, as the host last read it.
    ///
    /// Not a screen state: it is the machine's own answer to "which session is
    /// at the keyboard", and it is what decides whether a notification is
    /// about the session this host serves (ADR 0088 §4).
    console: Option<u32>,
    /// Whether the console session is locked, latched from the notifications
    /// rather than guessed from a failing capture.
    locked: bool,
    /// Whether at least one guest session is live right now.
    ///
    /// Fed by the runtime's own `set_host_bar`, which is called on every
    /// change to the active session count. It is not forwarded to the
    /// indicator — see `crates/host/src/view.rs` for why that would be wrong —
    /// it is remembered, so that somebody signing in can be told they are
    /// walking into a session already being watched (ADR 0088 §3).
    guests_live: bool,
    /// Whether the attachment now standing began while a guest was already
    /// connected.
    arrived_during_live_session: bool,
    /// Which kind of screen the current attachment is.
    source: Source,
}

impl Default for SessionScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionScreen {
    /// A host that has not looked at the machine yet.
    ///
    /// Starts at [`ScreenState::NoSession`] rather than at anything
    /// optimistic: until an agent has actually attached, "there is no picture"
    /// is the true answer and also the safe one.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: ScreenState::NoSession,
            capture_refusal: None,
            last_sequence: None,
            capturing: false,
            console: None,
            locked: false,
            guests_live: false,
            arrived_during_live_session: false,
            source: Source::Agent,
        }
    }

    /// Where the screen is right now.
    #[must_use]
    pub const fn state(&self) -> ScreenState {
        self.state
    }

    /// Whether a consent dialog could be shown to somebody (ADR 0085 §2).
    ///
    /// [`HostAttendance::Attended`] only while an agent is actually serving —
    /// not while one is starting, and not while the logon screen is up. A
    /// dialog queued against a process that has not drawn anything yet is a
    /// dialog nobody has seen, and one queued against a logon screen is one
    /// queued against a machine with nobody at it at all; ADR 0085 §2's
    /// refusal is built on this answer being the truth about right now rather
    /// than a prediction about a moment from now.
    #[must_use]
    pub const fn attendance(&self) -> HostAttendance {
        match self.state {
            ScreenState::Serving { .. } => HostAttendance::Attended,
            ScreenState::NoSession
            | ScreenState::LogonScreenStarting { .. }
            | ScreenState::LogonScreen { .. }
            | ScreenState::Starting { .. }
            | ScreenState::Switching { .. }
            | ScreenState::Lost => HostAttendance::Unattended,
        }
    }

    /// Why there is no picture, or `None` when there is one to be had.
    #[must_use]
    pub const fn no_picture(&self) -> Option<NoPicture> {
        if let Some(reason) = self.capture_refusal {
            return Some(NoPicture::CaptureUnavailable(reason));
        }
        match self.state {
            ScreenState::NoSession => Some(NoPicture::NoInteractiveSession),
            ScreenState::LogonScreenStarting { .. } => Some(NoPicture::LogonScreenStarting),
            ScreenState::Starting { .. } => Some(NoPicture::AgentStarting),
            ScreenState::Switching { .. } => Some(NoPicture::SessionSwitching),
            ScreenState::Lost => Some(NoPicture::AgentGone),
            ScreenState::LogonScreen { .. } | ScreenState::Serving { .. } => None,
        }
    }

    /// Whether the host may ask for frames at all.
    ///
    /// The invariant this whole type exists for: true only where something has
    /// attached and said so — an agent, or the logon screen's worker — and
    /// false in every transition between them.
    #[must_use]
    pub const fn may_capture(&self) -> bool {
        matches!(
            self.state,
            ScreenState::Serving { .. } | ScreenState::LogonScreen { .. }
        ) && self.capture_refusal.is_none()
    }

    /// Whether this host's own screen is the secure desktop right now
    /// (ADR 0088 §1).
    ///
    /// What the actor asks before it forwards a guest's keystroke to the logon
    /// screen: typing there is gated by `secure_desktop_input` and not by the
    /// controller role, exactly as it is on a desktop client whose capture is
    /// blocked by a UAC prompt (ADR 0057, ADR 0061).
    #[must_use]
    pub const fn on_secure_desktop(&self) -> bool {
        matches!(
            self.state,
            ScreenState::LogonScreenStarting { .. } | ScreenState::LogonScreen { .. }
        )
    }

    /// The session this host is serving or starting to serve, if any.
    #[must_use]
    pub const fn served_session(&self) -> Option<u32> {
        match self.state {
            ScreenState::LogonScreenStarting { session }
            | ScreenState::LogonScreen { session }
            | ScreenState::Starting { session }
            | ScreenState::Serving { session } => Some(session),
            ScreenState::NoSession | ScreenState::Switching { .. } | ScreenState::Lost => None,
        }
    }

    /// Whether the console session is locked, as the notifications last said.
    ///
    /// The host serves the logon screen rather than an agent while this is
    /// true: a locked session's desktop *is* `Winlogon`, and an agent running
    /// as that user cannot capture it however healthy it looks.
    #[must_use]
    pub const fn locked(&self) -> bool {
        self.locked
    }

    /// Whether nobody at all is signed in to this machine (ADR 0088 §3).
    ///
    /// True at the logon screen and on a machine with no session, and false
    /// for a *locked* session: that one has an owner, and an indicator waiting
    /// on their desktop for when they unlock it.
    #[must_use]
    pub const fn nobody_signed_in(&self) -> bool {
        !self.locked
            && matches!(
                self.state,
                ScreenState::NoSession
                    | ScreenState::LogonScreenStarting { .. }
                    | ScreenState::LogonScreen { .. }
            )
    }

    /// Whether the attachment standing now began while a guest was already
    /// connected (ADR 0088 §3).
    ///
    /// The fact behind "somebody sat down at a machine that was already being
    /// watched". The indicator is up either way — it is the first thing an
    /// attachment is answered with — but this is what makes the arrival
    /// something the log records rather than something only the person at the
    /// keyboard notices.
    #[must_use]
    pub const fn arrived_during_live_session(&self) -> bool {
        self.arrived_during_live_session
    }

    /// Tells this screen which session is attached to the console.
    ///
    /// A fact about the machine, not a state of the screen: it decides whether
    /// a notification is about the session this host serves, and it is read
    /// from the machine rather than latched because the answer cannot flicker
    /// — `WTSGetActiveConsoleSessionId` returns one number and it changes only
    /// when the console actually moves.
    pub const fn console_session_is(&mut self, session: Option<u32>) {
        self.console = session;
    }

    /// Whether this host serves `session`.
    ///
    /// The whole of ADR 0088 §4: one machine, one console session, and every
    /// other session — an RDP client's, a service's, one somebody left
    /// running — is not served and is told so rather than quietly joined.
    #[must_use]
    pub const fn serves(&self, session: u32) -> bool {
        match self.console {
            Some(console) => console == session,
            // Nothing read yet. The session this host already attached to is
            // the only one it can be serving.
            None => match self.served_session() {
                Some(served) => served == session,
                None => false,
            },
        }
    }

    /// Records how many guests are watching, as the runtime's own
    /// `set_host_bar` reports it.
    ///
    /// Deliberately not forwarded to the indicator: an indicator that followed
    /// the session count would drop between one guest leaving and the next
    /// arriving, and a banner that blinks off is a banner somebody can be
    /// filmed under (ADR 0087 §3).
    pub const fn guests_changed(&mut self, live: bool) {
        self.guests_live = live;
    }

    /// The host launched an agent into `session`.
    ///
    /// Called after `SessionAgent::start` succeeded, never before: a state
    /// that says an agent is starting when no process was created would leave
    /// the host waiting on an attachment that cannot arrive.
    pub const fn agent_launched(&mut self, session: u32) {
        self.state = ScreenState::Starting { session };
        self.source = Source::Agent;
        self.forget_the_attachment();
    }

    /// The host launched a logon-screen worker into `session` (ADR 0088 §1).
    ///
    /// The logon screen's own [`agent_launched`](Self::agent_launched), and
    /// separate from it for the reason the two states are separate: what
    /// attaches next is a picture with nobody behind it, so it answers
    /// `Unattended` and raises no indicator.
    pub const fn logon_screen_launched(&mut self, session: u32) {
        self.state = ScreenState::LogonScreenStarting { session };
        self.source = Source::LogonScreen;
        self.forget_the_attachment();
    }

    /// There is no interactive session to serve at all.
    ///
    /// A confirmation rather than a guess, and the caller is expected to treat
    /// it as one: it is what leaves [`ScreenState::Switching`], so a host must
    /// call it when the machine said there is no console session, not on a
    /// tick that merely failed to find one.
    pub const fn no_interactive_session(&mut self) {
        self.state = ScreenState::NoSession;
        self.forget_the_attachment();
    }

    /// The agent, or the logon screen's worker, is gone: its process exited,
    /// its session ended, or the channel broke.
    ///
    /// Every one of those means the same thing to a guest, which is why they
    /// are one call: there is no picture, and the last frame is not the
    /// current one.
    pub const fn agent_gone(&mut self) {
        self.state = ScreenState::Lost;
        self.forget_the_attachment();
    }

    /// Folds one session change into this state, and answers with what the
    /// host must do about it (ADR 0088 §2).
    ///
    /// The rule every arm below is an instance of: **a change to the served
    /// session ends the attachment**, because whatever is on the console after
    /// it is not what this host attached to — and after a fast user switch it
    /// is somebody else's desktop entirely. The transition is latched here and
    /// left only by [`on_event`](Self::on_event) or by
    /// [`no_interactive_session`](Self::no_interactive_session).
    pub const fn session_changed(&mut self, change: SessionChange) -> SessionAction {
        let session = change.session();
        if !self.serves(session) {
            return SessionAction::Ignore;
        }
        match change {
            // Somebody signed in where the logon screen was, or in place of
            // whoever was there before — or unlocked, which gives their
            // ordinary desktop back. Either way their agent is what serves
            // them, and this host has nothing attached that belongs to them.
            SessionChange::SignIn { session } | SessionChange::Unlock { session } => {
                self.locked = false;
                self.switch_to(Some(session))
            }
            // A locked session's desktop *is* `Winlogon`: the agent running as
            // that user cannot capture it, so the logon screen takes over. The
            // lock is latched rather than inferred from a capture that started
            // failing, which is the mistake the secure-desktop flag made.
            SessionChange::Lock { session } => {
                self.locked = true;
                self.switch_to(Some(session))
            }
            // The session is over, or has left the console. What comes next is
            // not known yet — another user may sign in, or the machine may sit
            // at its logon screen — so the transition names no session.
            SessionChange::SignOut { .. }
            | SessionChange::ConsoleDisconnect { .. }
            | SessionChange::RemoteConnect { .. }
            | SessionChange::RemoteDisconnect { .. } => {
                self.locked = false;
                self.switch_to(None)
            }
            // The console moved to this session. Serving it already is the one
            // case where nothing has to end.
            SessionChange::ConsoleConnect { session } => {
                self.console = Some(session);
                match self.served_session() {
                    Some(served) if served == session => SessionAction::KeepServing,
                    _ => self.switch_to(Some(session)),
                }
            }
        }
    }

    /// Enters the latched transition state.
    const fn switch_to(&mut self, to: Option<u32>) -> SessionAction {
        self.state = ScreenState::Switching { to };
        self.forget_the_attachment();
        SessionAction::EndAttachment
    }

    /// Drops everything that belonged to the attachment that just ended.
    ///
    /// The frame counter above all: a sequence carried across a transition is
    /// how a host ends up treating the last frame of somebody else's session
    /// as the current picture.
    const fn forget_the_attachment(&mut self) {
        self.capture_refusal = None;
        self.last_sequence = None;
        self.capturing = false;
        self.arrived_during_live_session = false;
    }

    /// Folds one thing the agent said into this state, and answers with what
    /// to say back.
    ///
    /// The returned commands are in the order they must be sent, and the order
    /// is the point (ADR 0085 §3): an attachment is answered with the
    /// indicator first and capture second, always, so there is no arrangement
    /// of events that gets a frame out of an agent whose banner is not up.
    #[must_use]
    pub fn on_event(&mut self, event: AgentEvent, monitor: u32) -> Vec<AgentCommand> {
        match event {
            AgentEvent::Attached { session } => self.attached(session, monitor),
            AgentEvent::CaptureUnavailable { reason } => {
                // The agent is still attached and the indicator is still up —
                // what it is saying is that this session has no picture to
                // give, which is the honest `MediaUnavailable` the guest
                // already knows how to receive.
                self.capture_refusal = Some(reason);
                self.capturing = false;
                Vec::new()
            }
            AgentEvent::FramePublished { sequence } => {
                if !self.may_capture() {
                    // A frame from an agent this host is not serving through
                    // is not a frame. Dropping it rather than trusting it is
                    // what keeps a stale or unexpected publish from becoming
                    // the picture a guest is shown.
                    return Vec::new();
                }
                // A repeat or a jump reads as "no new frame" rather than as an
                // error worth ending an attachment over: the counter exists to
                // tell a fresh frame from one already read, not to police the
                // agent.
                self.last_sequence = Some(sequence);
                Vec::new()
            }
            AgentEvent::Detaching => {
                self.agent_gone();
                Vec::new()
            }
        }
    }

    /// Something attached and says it is in `session`.
    ///
    /// Answered with commands only when it is the thing this host launched,
    /// into the session it launched it into. Anything else is told to shut
    /// down: a process that attached out of turn is not a screen this host has
    /// any reason to believe in, and serving it would be showing a guest a
    /// session nobody authorized (ADR 0088 §4).
    fn attached(&mut self, session: u32, monitor: u32) -> Vec<AgentCommand> {
        let expected = match self.state {
            ScreenState::Starting { session } | ScreenState::LogonScreenStarting { session } => {
                Some(session)
            }
            _ => None,
        };
        if expected != Some(session) {
            tracing::warn!(
                session,
                state = ?self.state,
                "something attached to this host that it did not launch; telling it to stop"
            );
            return vec![AgentCommand::Shutdown];
        }
        self.capture_refusal = None;
        self.last_sequence = None;
        self.capturing = true;
        self.arrived_during_live_session = self.guests_live;
        match self.source {
            Source::Agent => {
                self.state = ScreenState::Serving { session };
                if self.arrived_during_live_session {
                    // §2.2 with nobody around to rely on: the person who just
                    // signed in is walking into a session that is already
                    // being watched, and the indicator below is up before the
                    // command after it can produce a single frame of their
                    // desktop.
                    tracing::warn!(
                        session,
                        "somebody signed in while a guest was already connected; the indicator \
                         goes up before this session is captured"
                    );
                }
                vec![
                    // First. Not "also": see the type's own documentation and
                    // ADR 0085 §3.
                    AgentCommand::ShowIndicator { on: true },
                    AgentCommand::StartCapture { monitor },
                ]
            }
            // No indicator, and it is not an omission (ADR 0088 §3). The logon
            // screen belongs to nobody: there is no signed-in person to show a
            // banner to, and the only desktop it could be drawn on is
            // `Winlogon` — where a `LocalSystem` process putting up a window
            // is a worse idea than the problem it solves. What discloses this
            // capture is the audit record written when a guest is admitted to
            // a machine nobody is signed in to, and the indicator that goes up
            // the instant somebody does.
            Source::LogonScreen => {
                self.state = ScreenState::LogonScreen { session };
                vec![AgentCommand::StartCapture { monitor }]
            }
        }
    }

    /// The newest frame counter this host has seen on this attachment.
    #[must_use]
    pub const fn last_sequence(&self) -> Option<u32> {
        self.last_sequence
    }

    /// Whether capture has been asked for and not withdrawn on this
    /// attachment.
    #[must_use]
    pub const fn capturing(&self) -> bool {
        self.capturing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The monitor index is not what these tests are about; naming it once
    /// keeps every call reading as "and the monitor, whichever".
    const MONITOR: u32 = 0;

    /// A host serving an agent in `session`, which is the console session.
    fn serving(session: u32) -> SessionScreen {
        let mut screen = SessionScreen::new();
        screen.console_session_is(Some(session));
        screen.agent_launched(session);
        let _ = screen.on_event(AgentEvent::Attached { session }, MONITOR);
        screen
    }

    /// A host serving the logon screen of `session`.
    fn serving_the_logon_screen(session: u32) -> SessionScreen {
        let mut screen = SessionScreen::new();
        screen.console_session_is(Some(session));
        screen.logon_screen_launched(session);
        let _ = screen.on_event(AgentEvent::Attached { session }, MONITOR);
        screen
    }

    /// The invariant the type exists for: nothing but a live attachment lets
    /// a host ask for a frame.
    #[test]
    fn nothing_but_a_live_attachment_may_capture() {
        let mut screen = SessionScreen::new();
        assert!(!screen.may_capture(), "a fresh host has no screen yet");

        screen.agent_launched(1);
        assert!(
            !screen.may_capture(),
            "a process that has been started is not a screen"
        );

        let _ = screen.on_event(AgentEvent::Attached { session: 1 }, MONITOR);
        assert!(screen.may_capture());

        screen.agent_gone();
        assert!(!screen.may_capture());

        screen.no_interactive_session();
        assert!(!screen.may_capture());
    }

    /// ADR 0085 §3, as an order and not as a pair: the indicator is the first
    /// thing said to a fresh attachment, and capture the second. An agent that
    /// captured first would have a window in which pixels left a machine
    /// showing no sign of it.
    #[test]
    fn the_indicator_is_raised_before_capture_is_ever_asked_for() {
        let mut screen = SessionScreen::new();
        screen.agent_launched(2);
        let commands = screen.on_event(AgentEvent::Attached { session: 2 }, MONITOR);
        assert_eq!(
            commands,
            vec![
                AgentCommand::ShowIndicator { on: true },
                AgentCommand::StartCapture { monitor: MONITOR },
            ],
            "the indicator must precede capture, always and in that order"
        );
    }

    /// Nothing else ever produces a `StartCapture`. Stated as a sweep so a
    /// future event cannot quietly become a second way to start a capture
    /// without raising an indicator first.
    #[test]
    fn no_other_event_starts_a_capture() {
        for event in [
            AgentEvent::FramePublished { sequence: 1 },
            AgentEvent::CaptureUnavailable {
                reason: CaptureUnavailableReason::Failed,
            },
            AgentEvent::Detaching,
        ] {
            let mut screen = SessionScreen::new();
            screen.agent_launched(3);
            let commands = screen.on_event(event, MONITOR);
            assert!(
                !commands
                    .iter()
                    .any(|c| matches!(c, AgentCommand::StartCapture { .. })),
                "{event:?} must not start a capture"
            );
        }
    }

    /// ADR 0085 §2 meets §3: a host is attended exactly while an agent is
    /// serving, so the credential-only admission path is what a machine with
    /// nobody signed in offers — and a machine whose agent is still starting
    /// is still nobody signed in, as far as a dialog is concerned.
    #[test]
    fn a_host_is_attended_only_while_an_agent_serves() {
        let mut screen = SessionScreen::new();
        assert_eq!(screen.attendance(), HostAttendance::Unattended);

        screen.agent_launched(4);
        assert_eq!(
            screen.attendance(),
            HostAttendance::Unattended,
            "a dialog queued against a process that has drawn nothing is a dialog nobody saw"
        );

        let _ = screen.on_event(AgentEvent::Attached { session: 4 }, MONITOR);
        assert_eq!(screen.attendance(), HostAttendance::Attended);

        let _ = screen.on_event(AgentEvent::Detaching, MONITOR);
        assert_eq!(screen.attendance(), HostAttendance::Unattended);
    }

    /// Signing out mid-session leaves an honest state, not the last frame.
    #[test]
    fn an_agent_that_goes_away_leaves_no_picture_rather_than_a_stale_one() {
        let mut screen = SessionScreen::new();
        screen.agent_launched(5);
        let _ = screen.on_event(AgentEvent::Attached { session: 5 }, MONITOR);
        let _ = screen.on_event(AgentEvent::FramePublished { sequence: 7 }, MONITOR);
        assert_eq!(screen.last_sequence(), Some(7));

        screen.agent_gone();
        assert_eq!(screen.state(), ScreenState::Lost);
        assert_eq!(
            screen.no_picture(),
            Some(NoPicture::AgentGone),
            "a guest must be told the screen is gone, not shown the frame before it went"
        );
        assert_eq!(
            screen.last_sequence(),
            None,
            "the frame from the attachment that ended is not the current frame"
        );
        assert!(!screen.capturing());
    }

    /// A frame announced when this host is not serving through that agent is
    /// not a frame.
    #[test]
    fn a_frame_from_no_attachment_is_ignored() {
        let mut screen = SessionScreen::new();
        let commands = screen.on_event(AgentEvent::FramePublished { sequence: 3 }, MONITOR);
        assert!(commands.is_empty());
        assert_eq!(screen.last_sequence(), None);

        screen.agent_launched(6);
        let _ = screen.on_event(AgentEvent::FramePublished { sequence: 4 }, MONITOR);
        assert_eq!(
            screen.last_sequence(),
            None,
            "an agent that has not attached cannot have published a frame this host may use"
        );
    }

    /// An attached agent with no picture is still an attached agent: the
    /// indicator is up and a person can see it, which is a different state
    /// from having no agent at all — and the guest gets the reason rather than
    /// silence.
    #[test]
    fn an_attached_agent_that_cannot_capture_still_counts_as_a_screen() {
        let mut screen = SessionScreen::new();
        screen.agent_launched(7);
        let _ = screen.on_event(AgentEvent::Attached { session: 7 }, MONITOR);
        let _ = screen.on_event(
            AgentEvent::CaptureUnavailable {
                reason: CaptureUnavailableReason::NoBackend,
            },
            MONITOR,
        );

        assert!(!screen.may_capture());
        assert_eq!(
            screen.no_picture(),
            Some(NoPicture::CaptureUnavailable(
                CaptureUnavailableReason::NoBackend
            ))
        );
        assert_eq!(
            screen.attendance(),
            HostAttendance::Attended,
            "somebody is signed in and can see the indicator, so a dialog still reaches them"
        );
    }

    /// Every state that is not a live attachment names why there is no
    /// picture, so a guest is never left with silence and a host is never left
    /// guessing.
    #[test]
    fn every_pictureless_state_says_why() {
        let mut screen = SessionScreen::new();
        assert_eq!(
            screen.no_picture(),
            Some(NoPicture::NoInteractiveSession),
            "nobody is signed in"
        );

        screen.logon_screen_launched(8);
        assert_eq!(screen.no_picture(), Some(NoPicture::LogonScreenStarting));

        let _ = screen.on_event(AgentEvent::Attached { session: 8 }, MONITOR);
        assert_eq!(
            screen.no_picture(),
            None,
            "the logon screen is a picture like any other"
        );

        screen.console_session_is(Some(8));
        let _ = screen.session_changed(SessionChange::SignIn { session: 8 });
        assert_eq!(screen.no_picture(), Some(NoPicture::SessionSwitching));

        screen.agent_launched(8);
        assert_eq!(screen.no_picture(), Some(NoPicture::AgentStarting));

        let _ = screen.on_event(AgentEvent::Attached { session: 8 }, MONITOR);
        assert_eq!(screen.no_picture(), None, "there is a picture to be had");
    }

    /// A relaunch after a sign-out starts from nothing: no carried-over
    /// capture, no carried-over frame counter, no carried-over refusal.
    #[test]
    fn a_relaunch_carries_nothing_over_from_the_attachment_before_it() {
        let mut screen = SessionScreen::new();
        screen.agent_launched(9);
        let _ = screen.on_event(AgentEvent::Attached { session: 9 }, MONITOR);
        let _ = screen.on_event(AgentEvent::FramePublished { sequence: 11 }, MONITOR);
        let _ = screen.on_event(
            AgentEvent::CaptureUnavailable {
                reason: CaptureUnavailableReason::NoDisplay,
            },
            MONITOR,
        );
        screen.agent_gone();

        screen.agent_launched(10);
        assert_eq!(screen.state(), ScreenState::Starting { session: 10 });
        assert_eq!(screen.last_sequence(), None);
        assert_eq!(screen.no_picture(), Some(NoPicture::AgentStarting));
        assert!(!screen.capturing());
    }

    /// ADR 0088 §1: the logon screen is a capture target like any other, and
    /// it is one nobody is signed in to — so capture is asked for and no
    /// indicator is, because there is no desktop to draw one on and no person
    /// to read it.
    #[test]
    fn the_logon_screen_is_captured_without_an_indicator() {
        let mut screen = SessionScreen::new();
        screen.logon_screen_launched(1);
        let commands = screen.on_event(AgentEvent::Attached { session: 1 }, MONITOR);

        assert_eq!(
            commands,
            vec![AgentCommand::StartCapture { monitor: MONITOR }]
        );
        assert!(screen.may_capture());
        assert!(screen.on_secure_desktop());
        assert_eq!(
            screen.attendance(),
            HostAttendance::Unattended,
            "nobody is signed in, so there is nobody a dialog could be shown to"
        );
    }

    /// The whole of ADR 0088 §2, as the sequence the machine actually goes
    /// through: signed in, locked, unlocked, signed out. Every step is a
    /// transition first and a picture second, and no step carries the frame
    /// counter of the one before it.
    #[test]
    fn every_session_transition_is_a_state_of_its_own() {
        let mut screen = serving(1);
        let _ = screen.on_event(AgentEvent::FramePublished { sequence: 4 }, MONITOR);

        for change in [
            SessionChange::Lock { session: 1 },
            SessionChange::Unlock { session: 1 },
            SessionChange::SignOut { session: 1 },
            SessionChange::ConsoleDisconnect { session: 1 },
        ] {
            let mut screen = serving(1);
            let _ = screen.on_event(AgentEvent::FramePublished { sequence: 4 }, MONITOR);

            assert_eq!(
                screen.session_changed(change),
                SessionAction::EndAttachment,
                "{change:?} must end the attachment"
            );
            assert!(matches!(screen.state(), ScreenState::Switching { .. }));
            assert_eq!(screen.no_picture(), Some(NoPicture::SessionSwitching));
            assert!(!screen.may_capture());
            assert_eq!(
                screen.last_sequence(),
                None,
                "the frame from before {change:?} is not the current frame"
            );
            assert_eq!(screen.attendance(), HostAttendance::Unattended);
        }
    }

    /// A fast user switch is the case the rule is written for: the session
    /// that walks in is not the session that walked out, and until its own
    /// agent attaches there is nothing this host may show.
    #[test]
    fn a_fast_user_switch_shows_nothing_from_the_session_it_left() {
        let mut screen = serving(1);
        let _ = screen.on_event(AgentEvent::FramePublished { sequence: 9 }, MONITOR);

        assert_eq!(
            screen.session_changed(SessionChange::ConsoleDisconnect { session: 1 }),
            SessionAction::EndAttachment
        );
        screen.console_session_is(Some(2));
        assert_eq!(
            screen.session_changed(SessionChange::ConsoleConnect { session: 2 }),
            SessionAction::EndAttachment
        );
        assert_eq!(screen.state(), ScreenState::Switching { to: Some(2) });
        assert_eq!(screen.last_sequence(), None);

        // And a frame the old session's agent announces on its way out is not
        // a frame either.
        let _ = screen.on_event(AgentEvent::FramePublished { sequence: 10 }, MONITOR);
        assert_eq!(screen.last_sequence(), None);
    }

    /// The latch: a transition is left by something that confirmed the next
    /// state, not by looking again. Nothing here re-reads the machine, and the
    /// state does not move until an attachment or a confirmed empty machine
    /// arrives.
    #[test]
    fn a_transition_is_latched_until_something_confirms_what_comes_next() {
        let mut screen = serving(1);
        let _ = screen.session_changed(SessionChange::SignOut { session: 1 });

        // Reading the state a hundred times does not change it. This is the
        // shape of the bug the secure-desktop flag had: an answer that flipped
        // because it was recomputed on a tick faster than the thing it
        // described.
        for _ in 0..100 {
            assert_eq!(screen.state(), ScreenState::Switching { to: None });
            assert_eq!(screen.no_picture(), Some(NoPicture::SessionSwitching));
        }

        screen.no_interactive_session();
        assert_eq!(screen.state(), ScreenState::NoSession);
    }

    /// ADR 0088 §4: one machine, one console session. A notification about any
    /// other one changes nothing at all — not the state, not the lock, not the
    /// attachment.
    #[test]
    fn a_session_this_host_does_not_serve_changes_nothing() {
        let mut screen = serving(1);
        let _ = screen.on_event(AgentEvent::FramePublished { sequence: 2 }, MONITOR);

        for change in [
            SessionChange::SignIn { session: 3 },
            SessionChange::SignOut { session: 3 },
            SessionChange::Lock { session: 3 },
            SessionChange::RemoteConnect { session: 3 },
            SessionChange::RemoteDisconnect { session: 3 },
            SessionChange::ConsoleDisconnect { session: 3 },
        ] {
            assert_eq!(
                screen.session_changed(change),
                SessionAction::Ignore,
                "{change:?} is not about the session this host serves"
            );
            assert_eq!(screen.state(), ScreenState::Serving { session: 1 });
            assert_eq!(screen.last_sequence(), Some(2));
            assert!(!screen.locked());
        }
        assert!(!screen.serves(3), "an RDP session is not served");
        assert!(screen.serves(1));
    }

    /// A lock is latched from the notification rather than inferred from a
    /// capture that started failing — and it is what sends the host to the
    /// logon screen instead of to an agent that cannot see `Winlogon`.
    #[test]
    fn a_lock_latches_and_an_unlock_clears_it() {
        let mut screen = serving(1);
        assert!(!screen.locked());

        let _ = screen.session_changed(SessionChange::Lock { session: 1 });
        assert!(screen.locked());
        for _ in 0..10 {
            assert!(screen.locked(), "the lock must not flicker on a re-read");
        }

        let _ = screen.session_changed(SessionChange::Unlock { session: 1 });
        assert!(!screen.locked());

        // And a sign-out leaves nothing locked behind for the next person.
        let _ = screen.session_changed(SessionChange::Lock { session: 1 });
        let _ = screen.session_changed(SessionChange::SignOut { session: 1 });
        assert!(!screen.locked());
    }

    /// ADR 0088 §3: somebody signing in while a guest is connected walks into
    /// a session that is already being watched. The indicator is the first
    /// thing their agent is told, before the command that could produce a
    /// frame of their desktop, and the arrival is a fact the host can record.
    #[test]
    fn signing_in_while_a_guest_watches_raises_the_banner_before_any_frame() {
        let mut screen = serving_the_logon_screen(1);
        screen.guests_changed(true);

        let _ = screen.session_changed(SessionChange::SignIn { session: 1 });
        screen.agent_launched(1);
        let commands = screen.on_event(AgentEvent::Attached { session: 1 }, MONITOR);

        assert_eq!(
            commands.first(),
            Some(&AgentCommand::ShowIndicator { on: true }),
            "the banner is the first thing said to a session somebody just signed in to"
        );
        assert_eq!(
            commands.last(),
            Some(&AgentCommand::StartCapture { monitor: MONITOR })
        );
        assert!(screen.arrived_during_live_session());
    }

    /// The same arrival with nobody watching is an ordinary attachment: the
    /// indicator still goes up first, and there is no arrival to record.
    #[test]
    fn signing_in_with_no_guest_connected_records_no_arrival() {
        let mut screen = serving_the_logon_screen(1);
        screen.guests_changed(false);

        let _ = screen.session_changed(SessionChange::SignIn { session: 1 });
        screen.agent_launched(1);
        let _ = screen.on_event(AgentEvent::Attached { session: 1 }, MONITOR);

        assert!(!screen.arrived_during_live_session());
        assert_eq!(screen.attendance(), HostAttendance::Attended);
    }

    /// Nothing a guest can do lowers the indicator. The only command that
    /// takes it down is the one this state machine never produces: a host
    /// lowers it by ending the attachment, which is what `Shutdown` and a dead
    /// channel already do.
    #[test]
    fn no_event_ever_lowers_the_indicator() {
        for event in [
            AgentEvent::Attached { session: 1 },
            AgentEvent::FramePublished { sequence: 1 },
            AgentEvent::CaptureUnavailable {
                reason: CaptureUnavailableReason::Failed,
            },
            AgentEvent::Detaching,
        ] {
            let mut screen = serving(1);
            let commands = screen.on_event(event, MONITOR);
            assert!(
                !commands
                    .iter()
                    .any(|c| matches!(c, AgentCommand::ShowIndicator { on: false })),
                "{event:?} must never lower the indicator"
            );
        }
    }

    /// Something that attaches out of turn — a stale worker, a process that
    /// connected in a session this host did not launch into — is told to stop
    /// rather than served. Its pixels belong to a session nobody authorized.
    #[test]
    fn an_attachment_this_host_did_not_launch_is_turned_away() {
        // Right kind, wrong session.
        let mut screen = SessionScreen::new();
        screen.agent_launched(1);
        assert_eq!(
            screen.on_event(AgentEvent::Attached { session: 2 }, MONITOR),
            vec![AgentCommand::Shutdown]
        );
        assert_eq!(
            screen.state(),
            ScreenState::Starting { session: 1 },
            "the host is still waiting for the agent it actually launched"
        );

        // Right session, but nothing was launched at all.
        let mut screen = SessionScreen::new();
        assert_eq!(
            screen.on_event(AgentEvent::Attached { session: 1 }, MONITOR),
            vec![AgentCommand::Shutdown]
        );
        assert_eq!(screen.state(), ScreenState::NoSession);
        assert!(!screen.may_capture());
    }

    /// A locked session still belongs to somebody, so a guest admitted to it
    /// is an ordinary unattended admission; only a machine at its logon screen
    /// with nobody behind it is audited as empty (ADR 0088 §3).
    #[test]
    fn a_locked_session_is_not_an_empty_machine() {
        assert!(SessionScreen::new().nobody_signed_in());
        assert!(serving_the_logon_screen(1).nobody_signed_in());
        assert!(!serving(1).nobody_signed_in());

        let mut screen = serving(1);
        let _ = screen.session_changed(SessionChange::Lock { session: 1 });
        screen.logon_screen_launched(1);
        let _ = screen.on_event(AgentEvent::Attached { session: 1 }, MONITOR);
        assert!(!screen.nobody_signed_in(), "the lock screen has an owner");
    }

    /// The secure-desktop answer follows the logon screen and nothing else: it
    /// is what gates a guest's keystrokes on `secure_desktop_input`, so a host
    /// that answered `true` while an ordinary session was up would be asking
    /// for a grant nobody needs, and one that answered `false` on the logon
    /// screen would be typing there without it.
    #[test]
    fn only_the_logon_screen_is_the_secure_desktop() {
        assert!(serving_the_logon_screen(1).on_secure_desktop());
        assert!(!serving(1).on_secure_desktop());

        let mut screen = SessionScreen::new();
        assert!(!screen.on_secure_desktop());
        screen.logon_screen_launched(1);
        assert!(
            screen.on_secure_desktop(),
            "a worker that has been launched is already pointed at Winlogon"
        );
        screen.agent_gone();
        assert!(!screen.on_secure_desktop());
    }
}
