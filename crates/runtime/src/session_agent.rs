//! What a host running as a service believes about the machine's own screen
//! (ADR 0085 §3).
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
//!   honest "no picture" state (§18; ADR 0024), never a frozen frame and never
//!   a black one.
//! - The indicator goes up **before** the first frame leaves, not as a
//!   consequence of it. An agent that started capture and then raised the
//!   banner would have a window, however short, in which pixels left a machine
//!   showing no sign of it — which is exactly what §2.2 forbids and what a
//!   machine with nobody in front of it makes impossible to notice.
//! - An agent dies with its session. A host that stopped asking would go on
//!   believing there is a screen long after the person signed out, and would
//!   keep serving a picture that never changes.
//!
//! Nothing here talks to an agent. It decides *what should be said and in what
//! order*, and the caller carries it out over
//! [`lumepeer_service::agent_channel`]; keeping the two apart is what lets the
//! order be tested on a machine with no service installed on it at all.

use lumepeer_core::consent::HostAttendance;
use lumepeer_service::agent_protocol::{AgentCommand, AgentEvent, CaptureUnavailableReason};

/// Where the machine's screen is, as far as the host can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenState {
    /// No interactive session, so no agent: nothing to capture, and nobody who
    /// could see an indicator. The state a service host is in on a machine at
    /// its logon screen, and the state it returns to when the last person
    /// signs out.
    NoSession,
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
    /// Nobody is signed in at this machine.
    NoInteractiveSession,
    /// Somebody is, and their agent is still starting.
    AgentStarting,
    /// The agent went away.
    AgentGone,
    /// The agent is there and says this session cannot produce a picture.
    CaptureUnavailable(CaptureUnavailableReason),
}

/// The host's side of one session agent's lifetime.
///
/// One per machine, not one per guest: the screen is a fact about the machine,
/// and every guest sees the same answer.
#[derive(Debug)]
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
    /// not while one is starting. A dialog queued against a process that has
    /// not drawn anything yet is a dialog nobody has seen, and ADR 0085 §2's
    /// refusal is built on this answer being the truth about right now rather
    /// than a prediction about a moment from now.
    #[must_use]
    pub const fn attendance(&self) -> HostAttendance {
        match self.state {
            ScreenState::Serving { .. } => HostAttendance::Attended,
            ScreenState::NoSession | ScreenState::Starting { .. } | ScreenState::Lost => {
                HostAttendance::Unattended
            }
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
            ScreenState::Starting { .. } => Some(NoPicture::AgentStarting),
            ScreenState::Lost => Some(NoPicture::AgentGone),
            ScreenState::Serving { .. } => None,
        }
    }

    /// Whether the host may ask for frames at all.
    ///
    /// The invariant this whole type exists for: false in every state but
    /// [`ScreenState::Serving`], which is only reachable through an
    /// [`AgentEvent::Attached`] that has already been answered with the
    /// indicator.
    #[must_use]
    pub const fn may_capture(&self) -> bool {
        matches!(self.state, ScreenState::Serving { .. }) && self.capture_refusal.is_none()
    }

    /// The host launched an agent into `session`.
    ///
    /// Called after `SessionAgent::start` succeeded, never before: a state
    /// that says an agent is starting when no process was created would leave
    /// the host waiting on an attachment that cannot arrive.
    pub fn agent_launched(&mut self, session: u32) {
        self.state = ScreenState::Starting { session };
        self.capture_refusal = None;
        self.last_sequence = None;
        self.capturing = false;
    }

    /// There is no interactive session to launch an agent into.
    pub fn no_interactive_session(&mut self) {
        self.state = ScreenState::NoSession;
        self.capture_refusal = None;
        self.last_sequence = None;
        self.capturing = false;
    }

    /// The agent is gone: its process exited, its session ended, or the
    /// channel broke.
    ///
    /// Every one of those means the same thing to a guest, which is why they
    /// are one call: there is no picture, and the last frame is not the
    /// current one.
    pub fn agent_gone(&mut self) {
        self.state = ScreenState::Lost;
        self.capture_refusal = None;
        self.last_sequence = None;
        self.capturing = false;
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
            AgentEvent::Attached { session } => {
                self.state = ScreenState::Serving { session };
                self.capture_refusal = None;
                self.last_sequence = None;
                self.capturing = true;
                vec![
                    // First. Not "also": see the type's own documentation and
                    // ADR 0085 §3.
                    AgentCommand::ShowIndicator { on: true },
                    AgentCommand::StartCapture { monitor },
                ]
            }
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

    /// Every state that is not `Serving` names why there is no picture, so a
    /// guest is never left with silence and a host is never left guessing.
    #[test]
    fn every_pictureless_state_says_why() {
        let mut screen = SessionScreen::new();
        assert_eq!(
            screen.no_picture(),
            Some(NoPicture::NoInteractiveSession),
            "nobody is signed in"
        );

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
}
