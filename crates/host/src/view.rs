//! The actor's window seam, for a host that has no windows (ADR 0085 §1).
//!
//! `lumepeer_runtime::view::ViewWindows` is the trait the session runtime asks
//! three questions through: open a view onto a peer, put the host's own bar up,
//! and — the one that matters here — *is there anybody in front of this machine
//! to answer a dialog*.
//!
//! The Tauri implementation answers all three by having a desktop. This one has
//! none, and every answer below is a consequence of that rather than a
//! placeholder:
//!
//! - **There is no view window, because a service is nobody's guest.** A
//!   `LocalSystem` process that opened a window onto somebody else's screen
//!   would have to draw it somewhere, and every "somewhere" available to
//!   session 0 is a desktop no human ever sees. The runtime only asks for one
//!   after *this* node dialled *out*, which a host service does not do.
//! - **There is no host bar, and lowering one is the part that would be
//!   wrong.** See [`AgentScreen::set_host_bar`].
//! - **Attendance is the session agent's state and nothing else.** Not a flag
//!   somebody sets, not a guess from an uptime: [`SessionScreen`] is the one
//!   answer, and it says `Attended` only while an agent is actually serving.
//!
//! The runtime deliberately makes its own no-op implementation `#[cfg(test)]`,
//! so that a shipped binary cannot silently run a session nobody can see. This
//! is the shipped answer for a host with no desktop, and it is not that: it
//! reports `Unattended` exactly when nobody could have seen a dialog, which is
//! what sends ADR 0085 §2's credential-only path down the refusal it is built
//! around.

use std::sync::{Arc, Mutex, PoisonError};

use lumepeer_core::consent::HostAttendance;
use lumepeer_runtime::session_agent::{ScreenState, SessionScreen};
use lumepeer_runtime::view::ViewWindows;
use lumepeer_service::agent_protocol::AgentCommand;

/// The machine's screen, as the host believes it to be, and the way to reach it.
///
/// One per machine, not one per guest — the screen is a fact about the machine,
/// and every guest sees the same answer. Shared between the supervision thread,
/// which drives the state machine from what the agent says, and the actor,
/// which reads it through [`AgentViewWindows`].
#[derive(Debug)]
pub struct AgentScreen {
    /// The lifecycle state machine of ADR 0085 §3.
    ///
    /// A `std` mutex rather than a `tokio` one because the actor asks through
    /// [`ViewWindows`], which is synchronous, and because nothing held across
    /// it ever awaits: every critical section is a few field reads.
    screen: Mutex<SessionScreen>,
    /// Commands bound for whichever agent is attached, or nothing when none
    /// is.
    ///
    /// Dropped rather than queued when there is no agent, deliberately: a
    /// command that waited for an agent to appear would be an already-
    /// authorized instruction carried out against a session the authorization
    /// was never about. The guest learns the same way it learns everything
    /// else here — through the honest "no picture" state.
    commands: Mutex<Option<std::sync::mpsc::Sender<AgentCommand>>>,
}

impl Default for AgentScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentScreen {
    /// A host that has not looked at the machine yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            screen: Mutex::new(SessionScreen::new()),
            commands: Mutex::new(None),
        }
    }

    /// Runs `with` over the state machine.
    ///
    /// A closure rather than a returned guard so no caller can hold the lock
    /// across anything slow: the actor reads this on the handshake path, and a
    /// supervision thread blocked inside a pipe write must never be what makes
    /// a guest wait.
    pub fn with_screen<T>(&self, with: impl FnOnce(&mut SessionScreen) -> T) -> T {
        // A poisoned lock means a thread panicked holding it. The state
        // machine is plain data with no invariant a panic could have torn, and
        // refusing to answer would turn one panic into a host that never
        // admits anybody again.
        with(&mut self.screen.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Where the screen is right now.
    #[must_use]
    pub fn state(&self) -> ScreenState {
        self.with_screen(|screen| screen.state())
    }

    /// Points this screen at a live agent's command queue.
    ///
    /// Called by the supervision thread when an agent attaches, and with
    /// `None` the moment it is gone — the second half matters more than the
    /// first: a stale sender would let an authorized instruction be written
    /// into a pipe whose far end is a dead process, and the write would
    /// succeed until the buffer filled.
    pub fn attach_commands(&self, sender: Option<std::sync::mpsc::Sender<AgentCommand>>) {
        *self.commands.lock().unwrap_or_else(PoisonError::into_inner) = sender;
    }

    /// Sends one already-authorized command to the agent.
    ///
    /// Returns whether there was an agent to send it to. Every caller has
    /// already made the decision this carries out — `SessionManager` in the
    /// actor, or [`SessionScreen`] in the supervision loop — so there is
    /// nothing to check here and nothing that could widen anything if there
    /// were.
    pub fn command(&self, command: AgentCommand) -> bool {
        let guard = self.commands.lock().unwrap_or_else(PoisonError::into_inner);
        guard
            .as_ref()
            .is_some_and(|sender| sender.send(command).is_ok())
    }
}

/// [`ViewWindows`] for a host whose screen belongs to a session agent.
#[derive(Debug)]
pub struct AgentViewWindows {
    screen: Arc<AgentScreen>,
}

impl AgentViewWindows {
    /// Wraps the screen the supervision thread drives.
    #[must_use]
    pub const fn new(screen: Arc<AgentScreen>) -> Self {
        Self { screen }
    }
}

impl ViewWindows for AgentViewWindows {
    /// A service host never views another machine, so there is never one of
    /// these to open. Said out loud rather than silently ignored: if this ever
    /// fires, something asked a session-0 process to draw.
    fn open(&self, label: &str, _peer_label: &str, _input: bool) {
        tracing::warn!(
            window = %label,
            "a session-0 host was asked to open a view window; there is no desktop to open it on"
        );
    }

    fn close(&self, _label: &str) {}

    /// Nothing. The indicator this host has is not a bar that follows the
    /// session count, and making it one is the mistake worth naming.
    ///
    /// The runtime calls this whenever the number of active sessions changes,
    /// and on a desktop client that is exactly right: the bar carries the
    /// revoke, and it belongs up while somebody is connected. Forwarding it
    /// here as `ShowIndicator { on: visible }` would look like the same thing
    /// and would not be: `false` arrives on every drop to zero, including the
    /// gap between one guest leaving and the next arriving, and ADR 0085 §3b's
    /// whole property is that the indicator is up *before* a frame can leave —
    /// not that it is up once one has. A banner that blinks off between
    /// sessions is a banner somebody can be filmed under.
    ///
    /// So the indicator belongs to the attachment, not to the session count:
    /// [`SessionScreen::on_event`] raises it the moment an agent attaches and
    /// it stays up for as long as that agent is serving. What that costs is an
    /// indicator on a machine nobody is connected to, which is the direction
    /// this project errs in on purpose (§2.2).
    fn set_host_bar(&self, visible: bool) {
        tracing::debug!(
            visible,
            "session-0 host: the agent's indicator follows the attachment, not the session count"
        );
    }

    /// Whether anybody is in front of this host to answer a consent dialog.
    ///
    /// Read from the state machine every time, never cached: somebody signing
    /// in while a session is already running changes the answer, and the next
    /// guest to arrive gets the dialog the one before it could not have been
    /// shown (ADR 0085 §3c).
    fn attendance(&self) -> HostAttendance {
        self.screen.with_screen(|screen| screen.attendance())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumepeer_service::agent_protocol::AgentEvent;

    /// The property ADR 0085 §2's refusal is built on: with no agent there is
    /// nobody to show a dialog to, and this seam says so rather than letting
    /// the actor queue one against a screen that does not exist.
    #[test]
    fn a_host_with_no_agent_is_unattended() {
        let screen = Arc::new(AgentScreen::new());
        let windows = AgentViewWindows::new(Arc::clone(&screen));
        assert_eq!(windows.attendance(), HostAttendance::Unattended);

        // A launched process is not a screen either — the state exists exactly
        // so that these two are not the same answer.
        screen.with_screen(|screen| screen.agent_launched(1));
        assert_eq!(windows.attendance(), HostAttendance::Unattended);

        screen.with_screen(|screen| {
            let _ = screen.on_event(AgentEvent::Attached { session: 1 }, 0);
        });
        assert_eq!(windows.attendance(), HostAttendance::Attended);

        screen.with_screen(SessionScreen::agent_gone);
        assert_eq!(windows.attendance(), HostAttendance::Unattended);
    }

    /// A command with no agent attached is dropped and says so. It must not
    /// look like it was delivered: the caller's next move — telling the guest
    /// there is no picture — depends on knowing it was not.
    #[test]
    fn a_command_with_no_agent_is_refused_rather_than_queued() {
        let screen = AgentScreen::new();
        assert!(!screen.command(AgentCommand::ShowIndicator { on: true }));

        let (tx, rx) = std::sync::mpsc::channel();
        screen.attach_commands(Some(tx));
        assert!(screen.command(AgentCommand::ShowIndicator { on: true }));
        assert_eq!(rx.try_recv(), Ok(AgentCommand::ShowIndicator { on: true }));

        // And detaching has to take it away again, or an authorized
        // instruction goes on being written at a pipe whose far end has gone.
        screen.attach_commands(None);
        assert!(!screen.command(AgentCommand::StopCapture));
    }
}
