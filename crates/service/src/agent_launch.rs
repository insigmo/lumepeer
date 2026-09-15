//! Starting and watching the session agent (ADR 0085 §1).
//!
//! The mechanism is the one `secure_desktop_launch.rs` already uses, widened
//! from a worker that lives milliseconds to one that lives a session — and
//! narrowed in the way that matters, because the two are not the same kind of
//! process at all:
//!
//! | | secure-desktop worker (ADR 0056) | session agent (ADR 0085) |
//! | --- | --- | --- |
//! | Runs as | `LocalSystem`, token re-stamped into the console session | **the signed-in user** |
//! | Desktop | `WinSta0\Winlogon` | `WinSta0\Default` |
//! | Lives for | one capture or one event | the whole session |
//!
//! The row that matters is the first. The worker keeps `LocalSystem` because
//! nothing else can stand on the secure desktop; the agent must **not**, and
//! this module is the place that decides so. Its token comes from
//! [`WTSQueryUserToken`], which hands back the token of the person signed in
//! at the console, so the agent can do exactly what that person can do and
//! nothing more. A `LocalSystem` process that captures screens and injects
//! input is a process whose compromise is the machine; this one is a process
//! whose compromise is a session that was already going to be shown to a
//! guest.
//!
//! What the privileged side keeps for itself is the launch: the path is this
//! binary's own directory, the argument is a constant, and nothing a peer or
//! a local user supplies reaches the command line — the same rule
//! `secure_desktop_launch::spawn_worker` already follows and for the same
//! reason (ADR 0043 §4: elevation runs *our* binary, never a shell).
//!
//! [`WTSQueryUserToken`]: windows::Win32::System::RemoteDesktop::WTSQueryUserToken

#![allow(
    unsafe_code,
    reason = "WTSQueryUserToken, CreateProcessAsUserW and the token queries \
              behind them have no safe bindings; same justification standard \
              as SendInput (ADR 0012) and the rest of this crate's Win32 \
              surface (ADR 0043, ADR 0049, ADR 0056)"
)]

use core::ffi::c_void;

use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree, WAIT_TIMEOUT};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{
    DuplicateTokenEx, GetTokenInformation, SecurityImpersonation, TOKEN_ALL_ACCESS, TOKEN_USER,
    TokenPrimary, TokenUser,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTSGetActiveConsoleSessionId, WTSQueryUserToken,
};
use windows::Win32::System::Threading::{
    CREATE_NEW_CONSOLE, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, GetCurrentProcessId,
    GetExitCodeProcess, PROCESS_INFORMATION, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};

use crate::SESSION_AGENT_ARG;

/// The ordinary interactive desktop, as a path for `STARTUPINFOW.lpDesktop`.
///
/// Not `WinSta0\Winlogon`: the agent serves the desktop the signed-in person
/// is actually using, and it holds none of the privileges that would let it
/// reach the secure one even if it asked. The secure desktop stays the
/// short-lived worker's business (ADR 0056, ADR 0057), which is the whole
/// reason that worker still exists after this.
const DEFAULT_DESKTOP: &str = r"WinSta0\Default";

/// File name of the agent, resolved beside this binary.
///
/// The desktop application in agent mode, not a binary of its own: the agent
/// captures, encodes, injects input and puts up the session indicator, all of
/// which this project already has exactly one implementation of. A second
/// implementation kept in step by review is how a host ends up showing an
/// indicator in one process and not in the other.
///
/// Resolved beside `current_exe`, the same rule `service_control::service_exe`
/// uses in the other direction, so an installed app and a `cargo build` tree
/// are covered by one rule.
const AGENT_EXE: &str = "lumepeer-desktop.exe";

/// How long [`SessionAgent::stop`] waits for an agent to leave on its own
/// before it is terminated, in milliseconds.
///
/// An agent that was told to shut down has the indicator to take down and a
/// capture backend to release. Two seconds is generous for both and short
/// enough that a host handing the role over (ADR 0085 §4) does not look
/// stuck. A run that overruns it is killed, because the alternative is a host
/// that cannot stop hosting.
const AGENT_STOP_TIMEOUT_MS: u32 = 2_000;

/// A null-terminated UTF-16 copy of `text`, for the `W` entry points.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The session attached to the physical console, or `None` when there is
/// none.
///
/// `0xFFFF_FFFF` is the documented "nothing is attached" answer and is
/// reported as `None` rather than passed on as a session id: a host that
/// launched an agent into session `0xFFFF_FFFF` would be launching it
/// nowhere.
///
/// Session 0 is also `None`, and deliberately. It is the services' own
/// session, it has no interactive desktop, and an agent started there would
/// be exactly the thing ADR 0085 forbids — a capture running where no person
/// could see the indicator that says so.
#[must_use]
pub fn console_session() -> Option<u32> {
    // SAFETY: a plain kernel32 export with no arguments and no invariants.
    let session = unsafe { WTSGetActiveConsoleSessionId() };
    if session == u32::MAX || session == 0 {
        return None;
    }
    Some(session)
}

/// The Windows session this process is running in.
///
/// The agent's own answer to "where am I", for the one event that carries it
/// ([`crate::agent_protocol::AgentEvent::Attached`]). `None` when the kernel
/// will not say, which no ordinary process should ever see.
///
/// Here, in the crate that already holds this crate's Win32 surface, rather
/// than in the agent itself: the agent is the desktop binary and that crate is
/// `#![forbid(unsafe_code)]`. One export is a smaller thing to justify than an
/// exception to that rule (ADR 0043's standard, applied in the direction that
/// keeps the unprivileged side free of it).
#[must_use]
pub fn current_session() -> Option<u32> {
    let mut session = 0u32;
    // SAFETY: `session` is a local the call only writes; `GetCurrentProcessId`
    // takes no arguments and always succeeds.
    let read = unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &raw mut session) };
    read.inspect_err(|error| tracing::warn!(%error, "cannot read this process's session"))
        .ok()?;
    Some(session)
}

/// The string form of the SID of whoever is signed in at `session`.
///
/// Used for one thing: naming that user, and only that user, in the frame
/// mapping's access list ([`crate::frame::Writer::create_for_session_agent`]).
/// `None` when nobody is signed in there, or when this process does not hold
/// the privilege `WTSQueryUserToken` needs — an unprivileged run fails here
/// cleanly rather than reaching for a session it could not enter anyway.
#[must_use]
pub fn user_sid(session: u32) -> Option<String> {
    let token = query_user_token(session)?;
    let sid = sid_of_token(token);
    // SAFETY: `token` came from `query_user_token` and is not used again.
    unsafe {
        let _ = CloseHandle(token);
    }
    sid
}

/// Opens the token of the user signed in at `session`.
///
/// Needs `SeTcbPrivilege`, which `LocalSystem` holds and an ordinary process
/// does not. The handle is the caller's to close.
fn query_user_token(session: u32) -> Option<HANDLE> {
    let mut token = HANDLE::default();
    // SAFETY: `token` is a local that outlives the call and is only written.
    unsafe { WTSQueryUserToken(session, &raw mut token) }
        .inspect_err(|error| {
            tracing::warn!(session, %error, "cannot open the signed-in user's token");
        })
        .ok()?;
    Some(token)
}

/// Reads a token's user SID as a string.
fn sid_of_token(token: HANDLE) -> Option<String> {
    let mut needed = 0u32;
    // First call sizes the buffer. It is expected to fail with
    // `ERROR_INSUFFICIENT_BUFFER`; the size it writes is what matters.
    //
    // SAFETY: `token` is a live token handle; `needed` is a local that
    // outlives the call and is only written.
    let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &raw mut needed) };
    if needed == 0 {
        return None;
    }
    let mut buffer = vec![0u8; needed as usize];
    // SAFETY: `buffer` is exactly `needed` bytes and outlives the call;
    // `TOKEN_USER` is what `TokenUser` writes into it.
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast::<c_void>()),
            needed,
            &raw mut needed,
        )
    }
    .ok()?;
    // SAFETY: the call above filled `buffer` with a `TOKEN_USER` whose `Sid`
    // points inside that same buffer, which is still alive here.
    let sid = unsafe { buffer.as_ptr().cast::<TOKEN_USER>().read_unaligned() }
        .User
        .Sid;
    let mut text = PWSTR::null();
    // SAFETY: `sid` came from the token information above and is valid for as
    // long as `buffer` lives; `text` is a local the call allocates into.
    unsafe { ConvertSidToStringSidW(sid, &raw mut text) }.ok()?;
    if text.is_null() {
        return None;
    }
    // SAFETY: `text` is a null-terminated wide string the call just
    // allocated, and is freed immediately after it is copied.
    let owned = unsafe { text.to_string() }.ok();
    // SAFETY: `text` came from `ConvertSidToStringSidW`, whose documented
    // deallocator is `LocalFree`, and is not read again.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(text.as_ptr().cast::<c_void>())));
    }
    owned
}

/// A running session agent.
///
/// Holding one of these is what "there is a screen right now" means to the
/// privileged host. Dropping it does not stop the agent — use
/// [`stop`](Self::stop) for that — it only stops watching, so a value that
/// falls out of scope cannot silently kill a session somebody is using.
#[derive(Debug)]
pub struct SessionAgent {
    process: HANDLE,
    thread: HANDLE,
    pid: u32,
    session: u32,
}

// SAFETY: both handles are ordinary kernel handles, valid process-wide and not
// bound to the thread that created them; moving the value between threads is
// all `Send` promises.
unsafe impl Send for SessionAgent {}

impl SessionAgent {
    /// Launches the agent into `session`, as the user signed in there.
    ///
    /// `None` on every failure — nobody signed in, the privilege missing, the
    /// executable not found beside this one, `CreateProcessAsUserW` refusing —
    /// because the caller's answer to all of them is the same: there is no
    /// screen, so serve the guest the honest "no picture" state rather than a
    /// frozen frame (§18; ADR 0024, ADR 0085 §3).
    #[must_use]
    pub fn start(session: u32) -> Option<Self> {
        let Ok(exe) = std::env::current_exe() else {
            tracing::warn!("session agent: cannot locate this executable");
            return None;
        };
        let Some(agent) = exe.parent().map(|dir| dir.join(AGENT_EXE)) else {
            tracing::warn!("session agent: this executable has no directory");
            return None;
        };
        if !agent.is_file() {
            tracing::warn!(path = %agent.display(), "session agent: no agent beside this binary");
            return None;
        }
        let agent = agent.to_string_lossy().into_owned();
        if agent.contains('"') {
            // A Windows path cannot hold a quote; if one is here the string
            // was built rather than read from the OS, and it is not going onto
            // a command line (`secure_desktop_launch` applies the same rule).
            tracing::warn!("session agent: refusing an executable path that contains a quote");
            return None;
        }

        let user = query_user_token(session)?;
        let primary = primary_token_of(user);
        // SAFETY: `user` came from `query_user_token` and is not used again
        // after `primary_token_of` copied what it needed.
        unsafe {
            let _ = CloseHandle(user);
        }
        let primary = primary?;

        let spawned = spawn_agent(primary, session, &agent);
        // SAFETY: `primary` is a live token handle and is not used again after
        // the spawn above.
        unsafe {
            let _ = CloseHandle(primary);
        }
        spawned
    }

    /// The Windows session this agent is serving.
    #[must_use]
    pub const fn session(&self) -> u32 {
        self.session
    }

    /// The agent's process id.
    ///
    /// The host compares it against the process at the far end of the agent
    /// channel, so a different process in the same session cannot stand in
    /// for the agent the host actually launched.
    #[must_use]
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// Whether the agent is still running.
    ///
    /// The other half of ADR 0085 §3: an agent dies with its session, so a
    /// host that stopped asking would go on believing there was a screen long
    /// after the person signed out — and would keep sending the guest a frame
    /// that never changes. Asked on every turn that depends on the answer,
    /// never cached.
    ///
    /// A handle that cannot be waited on reads as **not** running, which is
    /// the safe direction: it costs a relaunch, where the other reading costs
    /// a guest a frozen picture it is told is live.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        // SAFETY: `self.process` is a live process handle owned by this value.
        unsafe { WaitForSingleObject(self.process, 0) == WAIT_TIMEOUT }
    }

    /// Whether this agent is still the one serving the console session.
    ///
    /// A fast user switch moves the console to another session while this
    /// agent's own process keeps running in the one it was started for —
    /// [`is_alive`](Self::is_alive) alone would call that healthy. Following
    /// the switch is pack `26`'s work; what belongs here is noticing that the
    /// answer has changed, so a host does not go on serving a desktop nobody
    /// is looking at.
    #[must_use]
    pub fn serves_the_console(&self) -> bool {
        console_session() == Some(self.session)
    }

    /// Stops the agent, terminating it if it will not leave.
    ///
    /// The caller is expected to have sent
    /// [`AgentCommand::Shutdown`](crate::agent_protocol::AgentCommand::Shutdown)
    /// first, which is the polite path; this is what makes "stop hosting"
    /// actually true either way. A host that could not stop its own agent
    /// could not hand the role over (ADR 0085 §4).
    pub fn stop(self) {
        // SAFETY: `self.process` is a live process handle owned by this value.
        let waited = unsafe { WaitForSingleObject(self.process, AGENT_STOP_TIMEOUT_MS) };
        if waited == WAIT_TIMEOUT {
            tracing::warn!(
                pid = self.pid,
                "session agent did not leave; terminating it"
            );
            // SAFETY: `self.process` is still live; terminating it is always
            // valid.
            unsafe {
                let _ = TerminateProcess(self.process, 1);
            }
        }
    }

    /// The agent's exit code, once it has exited.
    ///
    /// `None` while it is still running or when the code cannot be read. Used
    /// for the log rather than for a decision: every reason an agent is gone
    /// leads to the same next step, which is to start another one if the
    /// console session still has somebody in it.
    #[must_use]
    pub fn exit_code(&self) -> Option<u32> {
        if self.is_alive() {
            return None;
        }
        let mut code = 0u32;
        // SAFETY: `self.process` is a live handle and `code` is a local that
        // outlives the call and is only written.
        unsafe { GetExitCodeProcess(self.process, &raw mut code) }
            .ok()
            .map(|()| code)
    }
}

impl Drop for SessionAgent {
    fn drop(&mut self) {
        // Closing a process handle does not end the process, which is the
        // point: dropping this value stops watching an agent, it does not kill
        // a session somebody may be using. `stop` is the one that ends it.
        //
        // SAFETY: both handles came from a successful create and are not used
        // again after this.
        unsafe {
            let _ = CloseHandle(self.thread);
            let _ = CloseHandle(self.process);
        }
    }
}

/// Turns the impersonation token `WTSQueryUserToken` hands back into a primary
/// token, which is the only kind `CreateProcessAsUserW` accepts.
fn primary_token_of(user: HANDLE) -> Option<HANDLE> {
    let mut primary = HANDLE::default();
    // SAFETY: `user` is a live token handle; `primary` is a local that
    // outlives the call and is only written.
    unsafe {
        DuplicateTokenEx(
            user,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &raw mut primary,
        )
    }
    .inspect_err(|error| tracing::warn!(%error, "cannot duplicate the user's token"))
    .ok()?;
    Some(primary)
}

/// `CreateProcessAsUserW`s the agent onto the interactive desktop of
/// `session`, as the user `token` names.
fn spawn_agent(token: HANDLE, session: u32, agent: &str) -> Option<SessionAgent> {
    let application = wide(agent);
    // The command line is `"exe" --session-agent`; the quotes keep a
    // `C:\Program Files\...` path from being split into a command plus args,
    // and the tail is a constant — nothing here is supplied by a peer or by a
    // local user.
    let mut command_line = wide(&format!("\"{agent}\" {SESSION_AGENT_ARG}"));
    let mut desktop = wide(DEFAULT_DESKTOP);

    // The user's own environment, not the service's. Without this the agent
    // inherits `LocalSystem`'s `APPDATA`, `TEMP` and profile paths, and every
    // per-user path it touches would be the service account's — which is both
    // wrong and a way for one user's session to write into a place no user
    // owns.
    let mut environment: *mut c_void = std::ptr::null_mut();
    // SAFETY: `environment` is a local the call writes an allocation into;
    // `token` is a live primary token. `false` inherits nothing from this
    // process's own environment.
    let has_environment =
        unsafe { CreateEnvironmentBlock(&raw mut environment, Some(token), false) }.is_ok();
    if !has_environment {
        tracing::warn!("session agent: no user environment block; starting without one");
    }

    let startup = STARTUPINFOW {
        cb: u32::try_from(size_of::<STARTUPINFOW>()).unwrap_or(0),
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();

    let flags = if has_environment {
        CREATE_UNICODE_ENVIRONMENT | CREATE_NEW_CONSOLE
    } else {
        CREATE_NEW_CONSOLE
    };
    // SAFETY: `token` is a live primary token for the signed-in user;
    // `application`, `command_line` and `desktop` are null-terminated wide
    // buffers that outlive the call; `startup` borrows `desktop` and outlives
    // the call; `process` is written by the call. `binherithandles = false`
    // because the agent reaches the mapping and the channel by name, never by
    // an inherited handle — a handle it did not open is a handle nobody
    // checked its access against.
    let created = unsafe {
        CreateProcessAsUserW(
            Some(token),
            PCWSTR(application.as_ptr()),
            Some(PWSTR(command_line.as_mut_ptr())),
            None,
            None,
            false,
            flags,
            if has_environment {
                Some(environment)
            } else {
                None
            },
            PCWSTR::null(),
            &raw const startup,
            &raw mut process,
        )
    };
    if has_environment {
        // SAFETY: `environment` came from `CreateEnvironmentBlock` above and
        // is not read again; the child holds its own copy by now.
        unsafe {
            let _ = DestroyEnvironmentBlock(environment);
        }
    }
    if let Err(error) = created {
        tracing::warn!(session, %error, "session agent: CreateProcessAsUser refused");
        return None;
    }
    tracing::info!(
        session,
        pid = process.dwProcessId,
        "session agent started in the console session"
    );
    Some(SessionAgent {
        process: process.hProcess,
        thread: process.hThread,
        pid: process.dwProcessId,
        session,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The console session is always an answer, never a panic, and never
    /// session 0 — a host that started an agent in the services' own session
    /// would be capturing a desktop no person could see the indicator on.
    #[test]
    fn the_console_session_is_never_session_zero() {
        if let Some(session) = console_session() {
            assert_ne!(session, 0, "session 0 has no interactive desktop");
            assert_ne!(session, u32::MAX, "0xffffffff means no session at all");
        }
    }

    /// The agent is launched onto the ordinary desktop, never the secure one.
    /// Reaching `Winlogon` is the short-lived worker's business and stays
    /// behind its own grants (ADR 0056, ADR 0057); an agent that lived there
    /// would be a long-running process on the desktop where administrator
    /// passwords are typed.
    #[test]
    fn the_agent_is_launched_onto_the_ordinary_desktop() {
        assert_eq!(DEFAULT_DESKTOP, r"WinSta0\Default");
        assert!(!DEFAULT_DESKTOP.contains("Winlogon"));
    }

    /// Asking for the SID of a session nobody is signed in to is an answer,
    /// not a panic. `u32::MAX` is the "no console session" sentinel, so
    /// nothing is ever signed in there.
    #[test]
    fn a_session_with_nobody_in_it_has_no_sid() {
        assert!(user_sid(u32::MAX).is_none());
    }

    /// The agent is the desktop application in agent mode, resolved beside
    /// this binary. Two implementations of capture, injection and the session
    /// indicator would be two chances to show one of them and not the other.
    #[test]
    fn the_agent_is_the_desktop_binary_beside_this_one() {
        assert_eq!(
            std::path::Path::new(AGENT_EXE).extension(),
            Some(std::ffi::OsStr::new("exe"))
        );
        assert!(
            !AGENT_EXE.contains('\\') && !AGENT_EXE.contains('/'),
            "the agent is resolved beside this binary, never by a path"
        );
    }
}
