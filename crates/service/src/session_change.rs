//! What the service control manager says when a Windows session changes
//! (ADR 0088).
//!
//! A host that only polls `WTSGetActiveConsoleSessionId` learns about a sign-in
//! or a fast user switch on its next tick, which is late in the one way that
//! matters: for up to a tick it is still serving the session somebody has just
//! left. `SERVICE_CONTROL_SESSIONCHANGE` is the machine saying so at the moment
//! it happens, and this module is the part of reading it that has nothing to do
//! with Win32 — the mapping from the numbers the SCM passes to the thing that
//! happened.
//!
//! It lives here, in the crate both services link, and it is deliberately
//! platform-independent: the state machine that consumes these
//! (`lumepeer_runtime::session_agent`) is the one piece of the session-0 host
//! that can be tested on a machine with no service installed on it at all, and
//! a `#[cfg(windows)]` on this type would have taken that away.
//!
//! Nothing here trusts its input. The event codes arrive from the SCM, but the
//! same function is the one a test drives with arbitrary numbers, so an
//! unrecognized code is `None` — never a default, never a panic.

/// One thing that happened to one Windows session.
///
/// The eight `WTS_*` notifications a service can be sent that say something
/// about whether there is a desktop to serve. Each carries the session it
/// happened to, because the session is the whole point: a host serves the
/// console session and must not act on a notification about somebody else's
/// (ADR 0088 §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionChange {
    /// Somebody signed in.
    SignIn {
        /// Session they signed in to.
        session: u32,
    },
    /// Somebody signed out. Their agent is gone with them, whether or not it
    /// has said so yet.
    SignOut {
        /// Session that ended.
        session: u32,
    },
    /// The session was locked: its desktop is now `Winlogon`, and an agent
    /// running as that user can no longer capture it.
    Lock {
        /// Session that locked.
        session: u32,
    },
    /// The session was unlocked and its ordinary desktop is back.
    Unlock {
        /// Session that unlocked.
        session: u32,
    },
    /// The session was attached to the physical console — the second half of a
    /// fast user switch, and what a machine says at start-up.
    ConsoleConnect {
        /// Session now on the console.
        session: u32,
    },
    /// The session was detached from the console: somebody switched away from
    /// it without signing out.
    ConsoleDisconnect {
        /// Session that left the console.
        session: u32,
    },
    /// A remote (RDP) client connected to the session.
    RemoteConnect {
        /// Session the client connected to.
        session: u32,
    },
    /// A remote client disconnected from the session.
    RemoteDisconnect {
        /// Session the client left.
        session: u32,
    },
}

impl SessionChange {
    /// The session this happened to.
    #[must_use]
    pub const fn session(self) -> u32 {
        match self {
            Self::SignIn { session }
            | Self::SignOut { session }
            | Self::Lock { session }
            | Self::Unlock { session }
            | Self::ConsoleConnect { session }
            | Self::ConsoleDisconnect { session }
            | Self::RemoteConnect { session }
            | Self::RemoteDisconnect { session } => session,
        }
    }
}

/// `WTS_CONSOLE_CONNECT`: the session was attached to the console.
const WTS_CONSOLE_CONNECT: u32 = 0x1;
/// `WTS_CONSOLE_DISCONNECT`: the session left the console.
const WTS_CONSOLE_DISCONNECT: u32 = 0x2;
/// `WTS_REMOTE_CONNECT`: a remote client attached to the session.
const WTS_REMOTE_CONNECT: u32 = 0x3;
/// `WTS_REMOTE_DISCONNECT`: a remote client left the session.
const WTS_REMOTE_DISCONNECT: u32 = 0x4;
/// `WTS_SESSION_LOGON`: somebody signed in.
const WTS_SESSION_LOGON: u32 = 0x5;
/// `WTS_SESSION_LOGOFF`: somebody signed out.
const WTS_SESSION_LOGOFF: u32 = 0x6;
/// `WTS_SESSION_LOCK`: the session locked.
const WTS_SESSION_LOCK: u32 = 0x7;
/// `WTS_SESSION_UNLOCK`: the session unlocked.
const WTS_SESSION_UNLOCK: u32 = 0x8;

/// The change `event_type` names in `session`, or `None` for a notification
/// this host has nothing to do about.
///
/// The three deliberately unhandled ones are `WTS_SESSION_REMOTE_CONTROL`,
/// `WTS_SESSION_CREATE` and `WTS_SESSION_TERMINATE`. None of them says whether
/// there is a desktop to serve: a session can be created long before anybody
/// signs into it and terminated long after they signed out, and both of those
/// moments are already covered by the logon and logoff notifications above.
/// Acting on them as well would mean tearing an attachment down twice for one
/// thing that happened.
///
/// A code this function does not know is `None` rather than a guess. The
/// numbers come from the operating system, but treating an unknown one as
/// "probably a sign-out" is how a host ends up dropping a live session because
/// a future Windows added a notification.
#[must_use]
pub const fn from_wts_event(event_type: u32, session: u32) -> Option<SessionChange> {
    match event_type {
        WTS_CONSOLE_CONNECT => Some(SessionChange::ConsoleConnect { session }),
        WTS_CONSOLE_DISCONNECT => Some(SessionChange::ConsoleDisconnect { session }),
        WTS_REMOTE_CONNECT => Some(SessionChange::RemoteConnect { session }),
        WTS_REMOTE_DISCONNECT => Some(SessionChange::RemoteDisconnect { session }),
        WTS_SESSION_LOGON => Some(SessionChange::SignIn { session }),
        WTS_SESSION_LOGOFF => Some(SessionChange::SignOut { session }),
        WTS_SESSION_LOCK => Some(SessionChange::Lock { session }),
        WTS_SESSION_UNLOCK => Some(SessionChange::Unlock { session }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every notification this host acts on maps to exactly one change, with
    /// the session it happened to carried through. A pair swapped here would
    /// mean a host that tore an attachment down on an unlock.
    #[test]
    fn every_handled_notification_names_one_change() {
        for (event, expected) in [
            (
                WTS_CONSOLE_CONNECT,
                SessionChange::ConsoleConnect { session: 3 },
            ),
            (
                WTS_CONSOLE_DISCONNECT,
                SessionChange::ConsoleDisconnect { session: 3 },
            ),
            (
                WTS_REMOTE_CONNECT,
                SessionChange::RemoteConnect { session: 3 },
            ),
            (
                WTS_REMOTE_DISCONNECT,
                SessionChange::RemoteDisconnect { session: 3 },
            ),
            (WTS_SESSION_LOGON, SessionChange::SignIn { session: 3 }),
            (WTS_SESSION_LOGOFF, SessionChange::SignOut { session: 3 }),
            (WTS_SESSION_LOCK, SessionChange::Lock { session: 3 }),
            (WTS_SESSION_UNLOCK, SessionChange::Unlock { session: 3 }),
        ] {
            assert_eq!(from_wts_event(event, 3), Some(expected));
            assert_eq!(expected.session(), 3);
        }
    }

    /// Everything else is `None`, over the whole range rather than over a
    /// handful of examples — including the three session-lifecycle codes this
    /// host deliberately ignores, and every code a future Windows might add.
    #[test]
    fn no_other_code_invents_a_change() {
        for event in 0..=u16::MAX {
            let event = u32::from(event);
            let handled = (WTS_CONSOLE_CONNECT..=WTS_SESSION_UNLOCK).contains(&event);
            assert_eq!(
                from_wts_event(event, 1).is_some(),
                handled,
                "code {event:#x} must not parse as a change this host acts on"
            );
        }
        assert_eq!(from_wts_event(u32::MAX, 1), None);
    }

    /// A session id is carried, never interpreted: `0` is the services' own
    /// session and `0xffffffff` is the "no session" sentinel, and deciding what
    /// either means belongs to the state machine, not to this parse.
    #[test]
    fn a_session_id_is_carried_rather_than_judged() {
        assert_eq!(
            from_wts_event(WTS_SESSION_LOGON, 0),
            Some(SessionChange::SignIn { session: 0 })
        );
        assert_eq!(
            from_wts_event(WTS_SESSION_LOGON, u32::MAX),
            Some(SessionChange::SignIn { session: u32::MAX })
        );
    }
}
