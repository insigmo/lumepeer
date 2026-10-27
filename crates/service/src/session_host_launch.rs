//! Starting and watching the session host (ADR 0169 §1).
//!
//! The logon host's supervisor (`logon_host_launch`) turned the other way
//! round: while somebody is signed in at the console, the service keeps the
//! desktop application running on that session's `WinSta0\Default` desktop as
//! `LocalSystem`, with [`SESSION_HOST_ARG`]. At sign-out, at a console switch
//! and when the service stops, it is asked to give the host role up and
//! stopped; the logon host then takes the console back.
//!
//! The service decides only *when*: somebody signed in or not. Whether the
//! scheme is on and whose store to open is the session host's own question;
//! one that finds nothing to serve with exits [`SESSION_HOST_EXIT_NOT_ENABLED`],
//! [`SESSION_HOST_EXIT_NO_STORE`] or [`SESSION_HOST_EXIT_ROLE_TAKEN`] and is
//! left parked until the console changes, or [`PARKED_RECHECK`] passes. So
//! this binary still reads no configuration of its own (ADR 0043).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use lumepeer_service::agent_launch::{anybody_signed_in, console_session};
use lumepeer_service::{
    SESSION_HOST_ARG, SESSION_HOST_EXIT_CLOSED, SESSION_HOST_EXIT_NO_STORE,
    SESSION_HOST_EXIT_NOT_ENABLED, SESSION_HOST_EXIT_ROLE_TAKEN,
};

use crate::host_process::HostProcess;

/// The ordinary interactive desktop, where the signed-in person is — and
/// where the ADR 0114 injector stands, which is the position ADR 0169 gives
/// the whole host.
const DEFAULT_DESKTOP: &str = r"WinSta0\Default";

/// How often the supervisor looks at the console, as the logon host's does.
const SUPERVISION_TICK: Duration = Duration::from_millis(500);

/// How long after an unexpected exit the session host is started again, as
/// the logon host's: a host that crashes on start must not turn the signed-in
/// desktop into a process launcher.
const RELAUNCH_BACKOFF: Duration = Duration::from_secs(5);

/// How long after a session host closed its endpoint
/// ([`SESSION_HOST_EXIT_CLOSED`]) the next, closed one is started: at once,
/// in effect — a client may attach any moment — but never in a tight loop.
const CLOSED_RELAUNCH: Duration = Duration::from_secs(1);

/// How long a session host that refused is left parked on an unchanged
/// console before it is asked again.
///
/// Normally the console changing is what unparks it. This bounds the rest:
/// the marker written by an administrator in the middle of a session, the
/// first run of the client minting the store, an elevated client that held
/// the role and has since quit.
const PARKED_RECHECK: Duration = Duration::from_mins(5);

/// Keeps a session host on the console for as long as somebody is signed in
/// there, until `stopping` is set.
pub fn supervise(stopping: &AtomicBool) {
    // The session a session host last refused to serve, and since when.
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
        let Some(host) =
            HostProcess::start(session, DEFAULT_DESKTOP, SESSION_HOST_ARG, "session host")
        else {
            // `HostProcess::start` has already said why.
            std::thread::sleep(RELAUNCH_BACKOFF);
            continue;
        };
        match watch(host, stopping) {
            Ended::Refused => parked = Some((session, Instant::now())),
            Ended::Crashed => std::thread::sleep(RELAUNCH_BACKOFF),
            Ended::Closed => std::thread::sleep(CLOSED_RELAUNCH),
            Ended::Stopped => {}
        }
    }
    tracing::info!("session host supervisor stopped");
}

/// The console session to serve right now, or `None`: there is no console
/// session, nobody is signed in to it, or it cannot be told whether anybody
/// is — which starts nothing.
const fn serve_now(console: Option<u32>, signed_in: Option<bool>) -> Option<u32> {
    match (console, signed_in) {
        (Some(session), Some(true)) => Some(session),
        _ => None,
    }
}

/// How one session host's run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ended {
    /// It found nothing to serve with and said so; see [`PARKED_RECHECK`].
    Refused,
    /// It closed its endpoint with nobody to serve; the next one starts
    /// closed, listening for a client.
    Closed,
    /// It exited on its own for any other reason.
    Crashed,
    /// The supervisor stopped it: everybody signed out, the console moved, or
    /// the service is stopping.
    Stopped,
}

/// What an exit code the supervisor did not cause means.
const fn ended_by(code: Option<u32>) -> Ended {
    match code {
        Some(
            SESSION_HOST_EXIT_NOT_ENABLED
            | SESSION_HOST_EXIT_NO_STORE
            | SESSION_HOST_EXIT_ROLE_TAKEN,
        ) => Ended::Refused,
        Some(SESSION_HOST_EXIT_CLOSED) => Ended::Closed,
        _ => Ended::Crashed,
    }
}

/// Watches one session host until it exits, everybody signs out, the console
/// moves away from it or the service stops — stopping it in the last three
/// cases.
///
/// "Signed out" is only what the console says plainly: a session whose user
/// cannot be read for a moment keeps its host, since stopping it drops every
/// guest.
fn watch(host: HostProcess, stopping: &AtomicBool) -> Ended {
    loop {
        if !host.is_alive() {
            let code = host.exit_code();
            tracing::info!(pid = host.pid, ?code, "the session host exited");
            return ended_by(code);
        }
        let leave = if stopping.load(Ordering::SeqCst) {
            Some("the service is stopping")
        } else if console_session() != Some(host.session) {
            Some("the console moved to another session")
        } else if anybody_signed_in(host.session) == Some(false) {
            Some("everybody signed out")
        } else {
            None
        };
        if let Some(reason) = leave {
            tracing::info!(pid = host.pid, reason, "stopping the session host");
            let _ = host.stop();
            return Ended::Stopped;
        }
        std::thread::sleep(SUPERVISION_TICK);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only a signed-in console is served: an empty one belongs to the logon
    /// host, and one nobody can read starts nothing.
    #[test]
    fn only_a_signed_in_console_is_served() {
        assert_eq!(serve_now(Some(1), Some(true)), Some(1));
        assert_eq!(serve_now(Some(1), Some(false)), None);
        assert_eq!(serve_now(Some(1), None), None);
        assert_eq!(serve_now(None, Some(true)), None);
        assert_eq!(serve_now(None, None), None);
    }

    /// A host that refused — the scheme off, no store, the role held — parks
    /// the console; one that closed its endpoint is started again closed;
    /// anything else it exits with is a crash to back off from.
    #[test]
    fn a_refusal_parks_and_anything_else_is_a_crash() {
        assert_eq!(
            ended_by(Some(SESSION_HOST_EXIT_NOT_ENABLED)),
            Ended::Refused
        );
        assert_eq!(ended_by(Some(SESSION_HOST_EXIT_NO_STORE)), Ended::Refused);
        assert_eq!(ended_by(Some(SESSION_HOST_EXIT_ROLE_TAKEN)), Ended::Refused);
        assert_eq!(ended_by(Some(SESSION_HOST_EXIT_CLOSED)), Ended::Closed);
        assert_eq!(ended_by(Some(0)), Ended::Crashed);
        assert_eq!(ended_by(Some(1)), Ended::Crashed);
        assert_eq!(ended_by(None), Ended::Crashed);
    }

    /// The session host is launched onto the signed-in desktop, never the
    /// logon screen the logon host owns.
    #[test]
    fn the_host_is_launched_onto_the_ordinary_desktop() {
        assert_eq!(DEFAULT_DESKTOP, r"WinSta0\Default");
    }

    /// The exit codes the two hosts refuse with stay apart, so the service's
    /// log names which host said what.
    #[test]
    fn the_session_host_refuses_with_codes_of_its_own() {
        use lumepeer_service::{
            LOGON_HOST_EXIT_HANDED_OVER, LOGON_HOST_EXIT_NOT_ENABLED, LOGON_HOST_EXIT_ROLE_TAKEN,
        };
        let logon = [
            LOGON_HOST_EXIT_NOT_ENABLED,
            LOGON_HOST_EXIT_ROLE_TAKEN,
            LOGON_HOST_EXIT_HANDED_OVER,
        ];
        for code in [
            SESSION_HOST_EXIT_NOT_ENABLED,
            SESSION_HOST_EXIT_NO_STORE,
            SESSION_HOST_EXIT_ROLE_TAKEN,
            SESSION_HOST_EXIT_CLOSED,
        ] {
            assert!(!logon.contains(&code), "{code} is the logon host's too");
            assert!(code > 1, "{code} collides with an ordinary failure");
        }
    }
}
