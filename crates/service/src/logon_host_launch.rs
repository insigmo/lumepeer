//! Starting and watching the logon host (ADR 0126).
//!
//! While nobody is signed in at the console, the service keeps the desktop
//! application running on that session's `WinSta0\Winlogon` desktop as
//! `LocalSystem`, with [`LOGON_HOST_ARG`]. That process is a whole host — the
//! identity, the device password and the address book of the account that
//! turned the feature on — so a guest reaches the same saved host at the logon
//! screen as after it. The moment somebody signs in, it is asked to give the
//! host role up and stopped, and the ordinary client takes over.
//!
//! The service decides only *when*: somebody signed in or not. Whether the
//! feature is on, and whose store to open, is the logon host's own question,
//! answered from the admin-only machine tree; a host that finds it off exits
//! with [`LOGON_HOST_EXIT_NOT_ENABLED`] and is not started again until the
//! console changes. So this binary still reads no configuration of its own
//! (ADR 0043).
//!
//! The launch is [`crate::system_injector_launch`]'s with the desktop swapped
//! for `Winlogon` and the argument for [`LOGON_HOST_ARG`].

#![allow(
    unsafe_code,
    reason = "CreateProcessAsUserW and the process queries around it have no \
              safe bindings; same justification standard as the rest of this \
              crate's Win32 surface (ADR 0043, ADR 0056, ADR 0114, ADR 0126)"
)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateProcessAsUserW, GetExitCodeProcess, PROCESS_INFORMATION, STARTUPINFOW,
    TerminateProcess, WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};

use lumepeer_service::agent_launch::{anybody_signed_in, console_session};
use lumepeer_service::{LOGON_HOST_ARG, LOGON_HOST_EXIT_NOT_ENABLED, LOGON_HOST_EXIT_ROLE_TAKEN};

/// The logon screen's desktop, as a path for `STARTUPINFOW.lpDesktop`.
///
/// Every thread the logon host starts lands on it, so its capture duplicates
/// the logon screen and its `SendInput` types into it without any thread
/// having to move desktops.
const LOGON_DESKTOP: &str = r"WinSta0\Winlogon";

/// File name of the logon host, resolved beside this binary: the desktop
/// application, for the reason `system_injector_launch::INJECTOR_EXE` is.
const LOGON_HOST_EXE: &str = "lumepeer-desktop.exe";

/// How long [`LogonHost::stop`] waits for the host to give the role up and
/// leave before it is terminated, in milliseconds.
///
/// Longer than the injector's: this one has sessions to close and an endpoint
/// to shut down, and a guest told the session ended is better off than one
/// left to time a dead link out.
const LOGON_HOST_STOP_TIMEOUT_MS: u32 = 5_000;

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

/// A null-terminated UTF-16 copy of `text`, for the `W` entry points.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

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
        let Some(host) = LogonHost::start(session) else {
            // `LogonHost::start` has already said why.
            std::thread::sleep(RELAUNCH_BACKOFF);
            continue;
        };
        match watch(host, stopping) {
            Ended::Refused => parked = Some((session, Instant::now())),
            Ended::Crashed => std::thread::sleep(RELAUNCH_BACKOFF),
            Ended::Stopped => {}
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
fn watch(host: LogonHost, stopping: &AtomicBool) -> Ended {
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
            host.stop();
            return Ended::Stopped;
        }
        std::thread::sleep(SUPERVISION_TICK);
    }
}

/// A running logon host.
#[derive(Debug)]
struct LogonHost {
    process: HANDLE,
    thread: HANDLE,
    pid: u32,
    session: u32,
}

impl LogonHost {
    /// Launches the logon host into `session` as `LocalSystem`; `None` on
    /// every failure, each of which is logged.
    fn start(session: u32) -> Option<Self> {
        let exe = std::env::current_exe().ok()?;
        let Some(host) = exe.parent().map(|dir| dir.join(LOGON_HOST_EXE)) else {
            tracing::warn!("logon host: this executable has no directory");
            return None;
        };
        if !host.is_file() {
            tracing::warn!(path = %host.display(), "logon host: no desktop binary beside this one");
            return None;
        }
        let host = host.to_string_lossy().into_owned();
        if host.contains('"') {
            tracing::warn!("logon host: refusing an executable path that contains a quote");
            return None;
        }
        let token = crate::secure_desktop_launch::duplicate_own_token_for_session(session)?;
        let spawned = spawn(token, session, &host);
        // SAFETY: `token` is a live handle from `duplicate_own_token_for_session`
        // and is not used again after the spawn above copied what it needed.
        unsafe {
            let _ = CloseHandle(token);
        }
        spawned
    }

    /// Whether the logon host is still running; a handle that cannot be
    /// waited on reads as not running.
    fn is_alive(&self) -> bool {
        // SAFETY: `self.process` is a live process handle owned by this value.
        unsafe { WaitForSingleObject(self.process, 0) == WAIT_TIMEOUT }
    }

    /// The exit code, once it has exited.
    fn exit_code(&self) -> Option<u32> {
        if self.is_alive() {
            return None;
        }
        let mut code = 0u32;
        // SAFETY: `self.process` is a live handle and `code` is a local the call
        // writes.
        unsafe { GetExitCodeProcess(self.process, &raw mut code) }
            .ok()
            .map(|()| code)
    }

    /// Asks the logon host to give the host role up and leave, and terminates
    /// it if it has not within [`LOGON_HOST_STOP_TIMEOUT_MS`].
    fn stop(self) {
        // The request the desktop client makes too (ADR 0085 §4): the host
        // ends its sessions and releases the role on its own.
        let asked = lumepeer_service::host_role::request_release();
        // SAFETY: `self.process` is a live process handle owned by this value.
        let waited = unsafe {
            WaitForSingleObject(
                self.process,
                if asked { LOGON_HOST_STOP_TIMEOUT_MS } else { 0 },
            )
        };
        if waited == WAIT_TIMEOUT {
            tracing::warn!(
                pid = self.pid,
                "the logon host did not leave; terminating it"
            );
            // SAFETY: `self.process` is still live; terminating it is always
            // valid.
            unsafe {
                let _ = TerminateProcess(self.process, 1);
            }
        }
    }
}

impl Drop for LogonHost {
    fn drop(&mut self) {
        // SAFETY: both handles came from a successful create and are not used
        // again after this.
        unsafe {
            let _ = CloseHandle(self.thread);
            let _ = CloseHandle(self.process);
        }
    }
}

/// `CreateProcessAsUserW`s the logon host onto the console session's
/// `Winlogon` desktop as `LocalSystem`.
fn spawn(token: HANDLE, session: u32, host: &str) -> Option<LogonHost> {
    let application = wide(host);
    // `"exe" --logon-host`: the quotes keep a `C:\Program Files\...` path whole,
    // and the tail is a constant.
    let mut command_line = wide(&format!("\"{host}\" {LOGON_HOST_ARG}"));
    let mut desktop = wide(LOGON_DESKTOP);
    let startup = STARTUPINFOW {
        cb: u32::try_from(size_of::<STARTUPINFOW>()).unwrap_or(0),
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    // SAFETY: `token` is a live primary `LocalSystem` token stamped for the
    // console session; `application`, `command_line` and `desktop` are
    // null-terminated wide buffers that outlive the call; `startup` borrows
    // `desktop` and outlives the call; `process` is written by the call. No
    // handles are inherited: the host reaches everything by name.
    let created = unsafe {
        CreateProcessAsUserW(
            Some(token),
            PCWSTR(application.as_ptr()),
            Some(PWSTR(command_line.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_NO_WINDOW,
            None,
            PCWSTR::null(),
            &raw const startup,
            &raw mut process,
        )
    };
    if let Err(error) = created {
        tracing::warn!(session, %error, "logon host: CreateProcessAsUser refused");
        return None;
    }
    tracing::info!(
        session,
        pid = process.dwProcessId,
        "logon host started on the logon screen"
    );
    Some(LogonHost {
        process: process.hProcess,
        thread: process.hThread,
        pid: process.dwProcessId,
        session,
    })
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
        assert!(!LOGON_HOST_EXE.contains('\\') && !LOGON_HOST_EXE.contains('/'));
    }
}
