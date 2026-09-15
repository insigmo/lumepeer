//! Input, for a host that must not press a key (ADR 0085 §1).
//!
//! The rule this exists to keep: **a `LocalSystem` process that calls
//! `SendInput` is a process whose compromise is the machine.** A
//! `LocalSystem` process that forwards an already-authorized event to an agent
//! running as the signed-in user, which performs it with exactly that person's
//! rights, is not — and the difference is the whole reason the privileged side
//! links no injector at all.
//!
//! So this is not an injector. It is the last few metres of one: every event
//! reaching [`AgentInjector::inject`] has already been through
//! `SessionManager::authorize_input` in the actor, exactly as it is for the
//! in-process injector a desktop client uses, and all that is left is to say
//! which of four things happened and hand it across the channel.
//!
//! **Nothing is decided here, and nothing could be.** The agent channel has no
//! message that names a peer, a role or a grant, so there is no shape this
//! module could give an event that would widen anything. What it can do is
//! refuse, which it does whenever there is no agent attached — and that
//! refusal is the honest one §18 asks for rather than an event dropped on the
//! floor.

use std::sync::Arc;

use lumepeer_core::protocol::{InputDetail, InputEventPayload};
use lumepeer_media::MediaError;
use lumepeer_media::capture::{InputCapability, InputInjector};
use lumepeer_runtime::session_agent::ScreenState;
use lumepeer_service::agent_protocol::AgentCommand;

use crate::view::AgentScreen;

/// Forwards authorized input to whichever agent is serving this machine.
#[derive(Debug)]
pub struct AgentInjector {
    screen: Arc<AgentScreen>,
}

impl AgentInjector {
    /// Wraps the screen the supervision thread drives.
    #[must_use]
    pub const fn new(screen: Arc<AgentScreen>) -> Self {
        Self { screen }
    }
}

/// The command that carries `event`, or `None` for one this channel has no
/// shape for.
///
/// Total today — every `InputDetail` has a command — and written as a match
/// with no catch-all so that it stops compiling rather than silently dropping
/// events if a fifth kind is ever added. A guest whose scroll wheel quietly
/// did nothing is the failure this shape is guarding against.
const fn command_for(event: &InputEventPayload) -> AgentCommand {
    match event.detail {
        InputDetail::Press => AgentCommand::Press {
            logical: event.logical,
        },
        InputDetail::Release => AgentCommand::Release {
            logical: event.logical,
        },
        InputDetail::PointerMove { x, y } => AgentCommand::PointerMove { x, y },
        InputDetail::Wheel { dx, dy } => AgentCommand::Wheel { dx, dy },
    }
}

impl InputInjector for AgentInjector {
    fn inject(&mut self, event: &InputEventPayload) -> Result<(), MediaError> {
        if self.screen.command(command_for(event)) {
            return Ok(());
        }
        // No agent, or a channel that has gone. `InputUnavailable` rather than
        // `Ok(())` because the caller's next move depends on knowing: §18 says
        // a session that cannot inject degrades to view-only *and says so*,
        // and an event silently swallowed here would leave a guest clicking at
        // a screen that never answers and blaming the network.
        Err(MediaError::InputUnavailable(
            "this machine has no session agent to perform input on it".to_owned(),
        ))
    }

    /// What this host can do about input *right now*.
    ///
    /// Read from the screen state every time rather than settled once: an
    /// agent attaching is the moment input becomes possible, and an agent
    /// dying is the moment it stops. A capability cached at start-up would say
    /// `None` forever on a machine that was at its logon screen when the
    /// service came up, which is the ordinary case rather than the unusual
    /// one.
    fn capability(&self) -> InputCapability {
        match self.screen.state() {
            // The agent runs as the signed-in user on their own desktop, so
            // what it can inject is what they could type — which is `Full` in
            // this enum's terms.
            ScreenState::Serving { .. } => InputCapability::Full,
            ScreenState::NoSession | ScreenState::Starting { .. } | ScreenState::Lost => {
                InputCapability::None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumepeer_service::agent_protocol::AgentEvent;

    /// Builds a payload carrying `detail` and nothing else that matters here.
    fn event(logical: u32, detail: InputDetail) -> InputEventPayload {
        InputEventPayload {
            logical,
            scancode: 0,
            modifiers: 0,
            detail,
        }
    }

    /// Every kind of input the guest can send reaches the agent as exactly one
    /// command, with its payload intact. A wheel that arrived unsigned, or a
    /// press that arrived as a release, would be a guest's input carried out
    /// approximately.
    #[test]
    fn every_kind_of_input_crosses_the_channel_unchanged() {
        let screen = Arc::new(AgentScreen::new());
        let (tx, rx) = std::sync::mpsc::channel();
        screen.attach_commands(Some(tx));
        let mut injector = AgentInjector::new(Arc::clone(&screen));

        for (sent, expected) in [
            (
                event(0x0d, InputDetail::Press),
                AgentCommand::Press { logical: 0x0d },
            ),
            (
                event(0x0d, InputDetail::Release),
                AgentCommand::Release { logical: 0x0d },
            ),
            (
                event(0, InputDetail::PointerMove { x: 1, y: 65_535 }),
                AgentCommand::PointerMove { x: 1, y: 65_535 },
            ),
            (
                event(0, InputDetail::Wheel { dx: -3, dy: 120 }),
                AgentCommand::Wheel { dx: -3, dy: 120 },
            ),
        ] {
            assert!(injector.inject(&sent).is_ok());
            assert_eq!(rx.try_recv(), Ok(expected), "for {sent:?}");
        }
    }

    /// With no agent there is nobody to press a key, and the caller has to
    /// learn that rather than have the event vanish — §18's rule that a
    /// session which cannot inject degrades to view-only *and says so*.
    #[test]
    fn input_with_no_agent_is_refused_out_loud() {
        let screen = Arc::new(AgentScreen::new());
        let mut injector = AgentInjector::new(Arc::clone(&screen));

        assert!(matches!(
            injector.inject(&event(0x0d, InputDetail::Press)),
            Err(MediaError::InputUnavailable(_))
        ));
        assert_eq!(injector.capability(), InputCapability::None);
    }

    /// The capability follows the attachment, both ways. A host that reported
    /// `Full` while its agent was still starting would be promising a guest
    /// input against a process that has not drawn anything yet.
    #[test]
    fn the_capability_follows_the_attachment() {
        let screen = Arc::new(AgentScreen::new());
        let injector = AgentInjector::new(Arc::clone(&screen));

        screen.with_screen(|screen| screen.agent_launched(1));
        assert_eq!(injector.capability(), InputCapability::None);

        screen.with_screen(|screen| {
            let _ = screen.on_event(AgentEvent::Attached { session: 1 }, 0);
        });
        assert_eq!(injector.capability(), InputCapability::Full);

        screen.with_screen(lumepeer_runtime::session_agent::SessionScreen::agent_gone);
        assert_eq!(injector.capability(), InputCapability::None);
    }
}
