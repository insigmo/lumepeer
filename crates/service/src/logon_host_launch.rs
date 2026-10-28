//! Starting and watching the logon host (ADR 0126).
//!
//! While nobody is signed in at the console, the service keeps the desktop
//! application running on that session's `WinSta0\Winlogon` desktop as
//! `LocalSystem`, with [`LOGON_HOST_ARG`]. That process is a whole host — the
//! identity, the device password and the address book of the account that
//! turned the feature on — so a guest reaches the same saved host at the logon
//! screen as after it. The moment somebody signs in, it is asked to give the
//! host role up and stopped, and the ordinary client takes over — started
//! here, when the logon host leaves with a guest still on it (ADR 0164) —
//! or, where ADR 0169 is on, the session host does.
//!
//! The service decides only *when*: somebody signed in or not. Whether the
//! feature is on, and whose store to open, is the logon host's own question,
//! answered from the admin-only machine tree; a host that finds it off exits
//! with [`LOGON_HOST_EXIT_NOT_ENABLED`] and is not started again until the
//! console changes. So this binary still reads no configuration of its own
//! (ADR 0043).
//!
//! The launch is [`crate::host_process`]'s onto `Winlogon` with
//! [`LOGON_HOST_ARG`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use lumepeer_service::agent_launch::{anybody_signed_in, console_session};
use lumepeer_service::{
    LOGON_HOST_ARG, LOGON_HOST_EXIT_HANDED_OVER, LOGON_HOST_EXIT_NOT_ENABLED,
    LOGON_HOST_EXIT_ROLE_TAKEN,
};

use crate::host_process::HostProcess;

/// The logon screen's desktop, as a path for `STARTUPINFOW.lpDesktop`.
///
/// Every thread the logon host starts lands on it, so its capture duplicates
/// the logon screen and its `SendInput` types into it without any thread
/// having to move desktops.
const LOGON_DESKTOP: &str = r"WinSta0\Winlogon";

/// How often the supervisor looks at the console.
const SUPERVISION_TICK: Duration = Duration::from_millis(500);

/// How long after an unexpected exit the logon host is started again.
///
/// Seconds rather than the injector's one: this is a whole host with an
/// endpoint to bind, and one that crashes on start must not turn the logon
/// screen into a process launcher.
const RELAUNCH_BACKOFF: Duration = Duration::from_secs(5);

/// How long a logon host that exited [`LOGON_HOST_EXIT_NOT_ENABLED`] or
/// [`LOGON_HOST_EXIT_ROLE_TAKEN`] is left parked on an unchanged console
/// before it is asked again.
///
/// Normally the console changing is what unparks it — turning the feature on
/// takes a signed-in account, and signing out starts a new session. This
/// bounds the rest: the feature turned on over a remote desktop while the
/// console sits at its logon screen, or a client in a disconnected session
/// that has since let the role go.
const PARKED_RECHECK: Duration = Duration::from_mins(5);

/// Keeps a logon host on the console for as long as nobody is signed in
/// there, until `stopping` is set.
pub fn supervise(stopping: &AtomicBool) {
    // The session a logon host last refused to serve, and since when. Nothing
    // about that refusal changes on an unchanged console, so it is not asked
    // again until the console changes or `PARKED_RECHECK` passes.
    let mut parked: Option<(u32, Instant)> = None;
    while !stopping.load(Ordering::SeqCst) {
        let console = console_session();
        let signed_in = console.and_then(anybody_signed_in);
        let Some(session) = serve_now(console, signed_in) else {
            parked = None;
            std::thread::sleep(SUPERVISION_TICK);
            continue;
        };
        if let Some((at, since)) = parked
            && at == session
            && since.elapsed() < PARKED_RECHECK
        {
            std::thread::sleep(SUPERVISION_TICK);
            continue;
        }
        parked = None;
        let Some(host) = HostProcess::start(session, LOGON_DESKTOP, LOGON_HOST_ARG, "logon host")
        else {
            // `HostProcess::start` has already said why.
            std::thread::sleep(RELAUNCH_BACKOFF);
            continue;
        };
        match watch(host, stopping) {
            Ended::Refused => parked = Some((session, Instant::now())),
            Ended::Crashed => std::thread::sleep(RELAUNCH_BACKOFF),
            Ended::Stopped => {}
            Ended::HandedOver => hand_over_to_client(),
        }
    }
    tracing::info!("logon host supervisor stopped");
}

/// The console session to serve right now, or `None`: there is no console
/// session, somebody is signed in to it, or it cannot be told whether anybody
/// is — which reads as "somebody", the direction that starts nothing.
const fn serve_now(console: Option<u32>, signed_in: Option<bool>) -> Option<u32> {
    match (console, signed_in) {
        (Some(session), Some(false)) => Some(session),
        _ => None,
    }
}

/// How one logon host's run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ended {
    /// It found nothing to serve with and said so; see [`PARKED_RECHECK`].
    Refused,
    /// It exited on its own for any other reason.
    Crashed,
    /// The supervisor stopped it: somebody signed in, the console moved, or
    /// the service is stopping.
    Stopped,
    /// The supervisor stopped it because somebody signed in or the console
    /// moved, and it had a guest on it (ADR 0164).
    HandedOver,
}

/// What an exit code the supervisor did not cause means.
const fn ended_by(code: Option<u32>) -> Ended {
    match code {
        Some(LOGON_HOST_EXIT_NOT_ENABLED | LOGON_HOST_EXIT_ROLE_TAKEN) => Ended::Refused,
        _ => Ended::Crashed,
    }
}

/// Watches one logon host until it exits, somebody signs in, the console
/// moves away from it or the service stops — stopping it in the last three
/// cases.
fn watch(host: HostProcess, stopping: &AtomicBool) -> Ended {
    loop {
        if !host.is_alive() {
            let code = host.exit_code();
            tracing::info!(pid = host.pid, ?code, "the logon host exited");
            return ended_by(code);
        }
        let leave = if stopping.load(Ordering::SeqCst) {
            Some("the service is stopping")
        } else if console_session() != Some(host.session) {
            Some("the console moved to another session")
        } else if anybody_signed_in(host.session) != Some(false) {
            Some("somebody signed in")
        } else {
            None
        };
        if let Some(reason) = leave {
            tracing::info!(pid = host.pid, reason, "stopping the logon host");
            let code = host.stop();
            return stopped_with(code, stopping.load(Ordering::SeqCst));
        }
        std::thread::sleep(SUPERVISION_TICK);
    }
}

/// How a logon host the supervisor stopped ended: handed over when it left on
/// its own saying it had a guest, unless the service itself is going away.
const fn stopped_with(code: Option<u32>, service_stopping: bool) -> Ended {
    match code {
        Some(LOGON_HOST_EXIT_HANDED_OVER) if !service_stopping => Ended::HandedOver,
        _ => Ended::Stopped,
    }
}

/// The guest on the logon host has just signed the console in and is dialing
/// again: starts the client it is dialing for (ADR 0164).
///
/// Where the session host serves the signed-in console (ADR 0169), the guest's
/// dial is for the session host, which its own supervisor starts with the same
/// identity — but closed until a client attaches. So the person's client is
/// started here all the same, as their guest and panel, and its attaching is
/// what opens the endpoint the guest is dialing.
fn hand_over_to_client() {
    let Some(session) =
        console_session().filter(|&session| anybody_signed_in(session) == Some(true))
    else {
        tracing::info!("the logon host handed over, but nobody is signed in at the console now");
        return;
    };
    if lumepeer_service::program_data::session_host_enabled() {
        tracing::info!(
            "the logon host handed over; starting the client the session host opens for (ADR 0169)"
        );
        let _ = lumepeer_service::client_launch::start_client(session);
        return;
    }
    let _ = lumepeer_service::client_launch::start_owner_client(session);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only an empty console is served: a signed-in one belongs to the
    /// client, and one nobody can read is treated as signed in.
    #[test]
    fn only_a_console_nobody_is_signed_in_to_is_served() {
        assert_eq!(serve_now(Some(1), Some(false)), Some(1));
        assert_eq!(serve_now(Some(1), Some(true)), None);
        assert_eq!(serve_now(Some(1), None), None);
        assert_eq!(serve_now(None, Some(false)), None);
        assert_eq!(serve_now(None, None), None);
    }

    /// A stopped host that says it had a guest hands over to the client,
    /// unless the service is stopping; one that was terminated, or had nobody,
    /// is simply stopped.
    #[test]
    fn a_stopped_host_with_a_guest_hands_over() {
        assert_eq!(
            stopped_with(Some(LOGON_HOST_EXIT_HANDED_OVER), false),
            Ended::HandedOver
        );
        assert_eq!(
            stopped_with(Some(LOGON_HOST_EXIT_HANDED_OVER), true),
            Ended::Stopped
        );
        assert_eq!(stopped_with(Some(0), false), Ended::Stopped);
        assert_eq!(stopped_with(None, false), Ended::Stopped);
    }

    /// A host that refused parks the console; anything else it exits with is
    /// a crash to back off from.
    #[test]
    fn a_refusal_parks_and_anything_else_is_a_crash() {
        assert_eq!(ended_by(Some(LOGON_HOST_EXIT_NOT_ENABLED)), Ended::Refused);
        assert_eq!(ended_by(Some(LOGON_HOST_EXIT_ROLE_TAKEN)), Ended::Refused);
        assert_eq!(ended_by(Some(0)), Ended::Crashed);
        assert_eq!(ended_by(Some(1)), Ended::Crashed);
        assert_eq!(ended_by(None), Ended::Crashed);
    }

    /// The host is launched onto the logon screen and nowhere else.
    #[test]
    fn the_host_is_launched_onto_the_logon_screen() {
        assert_eq!(LOGON_DESKTOP, r"WinSta0\Winlogon");
    }
}
