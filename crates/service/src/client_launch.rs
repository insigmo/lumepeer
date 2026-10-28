//! Starting the owner's client the moment a guest signs the machine in
//! (ADR 0164).
//!
//! The logon host (ADR 0126) steps down at sign-in, and the guest who just
//! typed the password dials again. Until now what answered that dial was
//! whatever autostart arranged — and where autostart was off, nothing did: the
//! guest signed the machine in and lost it. So when the logon host leaves with
//! [`crate::LOGON_HOST_EXIT_HANDED_OVER`], the service starts the client in the
//! signed-in session itself.
//!
//! Only for the account the logon host was hosting as: a guest's redial is for
//! that identity, and another account's client would be a different host.
//! Only when no client already holds the host role: the autostart task may
//! have been quicker, and a second launch would only raise its window.
//!
//! The client is `requireAdministrator` (ADR 0057), so it gets what the
//! autostart task's `HighestAvailable` gives it (ADR 0140): the full half of
//! the user's split UAC token, the user's own environment, the ordinary
//! desktop. The path is the desktop binary beside this one and the command line
//! carries nothing else (ADR 0043 §4).
//!
//! The session host (ADR 0169 §7) starts the same client for the person signed
//! in — with the limited half of their token, since a client that is only a
//! guest and a panel needs nothing more ([`start_client`]).

#![allow(
    unsafe_code,
    reason = "GetTokenInformation, CreateEnvironmentBlock and CreateProcessAsUserW \
              have no safe bindings; same justification standard as the rest of \
              this crate's Win32 surface (ADR 0043, ADR 0085, ADR 0126)"
)]

use core::ffi::c_void;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, ERROR_ELEVATION_REQUIRED, HANDLE};
use windows::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION_TYPE, TOKEN_LINKED_TOKEN, TokenElevationType,
    TokenElevationTypeLimited, TokenLinkedToken,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, PROCESS_INFORMATION, STARTUPINFOW,
};
use windows::core::{HRESULT, PCWSTR, PWSTR};

use crate::agent_launch::{primary_token_of, query_user_token, sid_of_token};
use crate::host_role::HostRole;

/// File name of the client, resolved beside this binary — the one the logon
/// host itself was started from.
const CLIENT_EXE: &str = "lumepeer-desktop.exe";

/// The ordinary interactive desktop, where the signed-in person is.
const DEFAULT_DESKTOP: &str = r"WinSta0\Default";

/// How long a session that has just been signed in to is given to hand out
/// its user's token.
///
/// The service sees the user name the moment Winlogon sets it; the token
/// `WTSQueryUserToken` returns can lag that by a moment of the logon still
/// being finished.
const SIGN_IN_SETTLE: Duration = Duration::from_secs(10);

/// How often the token is asked for within [`SIGN_IN_SETTLE`].
const SIGN_IN_POLL: Duration = Duration::from_millis(250);

/// A null-terminated UTF-16 copy of `text`, for the `W` entry points.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// What to do about the session a guest just signed in to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// Start the owner's client there.
    Start,
    /// Somebody other than the logon host's owner signed in, or the feature
    /// was turned off meanwhile: the guest's host is not theirs to start.
    NotTheOwner,
    /// A client already holds the host role.
    AlreadyHosted,
}

/// Decides [`Verdict`] from the owner on file, who signed in, and whether the
/// host role is held.
fn verdict(owner: Option<&str>, signed_in: &str, role_held: bool) -> Verdict {
    if owner != Some(signed_in) {
        Verdict::NotTheOwner
    } else if role_held {
        Verdict::AlreadyHosted
    } else {
        Verdict::Start
    }
}

/// Starts the logon host's owner's client in `session`, if they are who signed
/// in there and nothing is hosting yet; says whether a client was started.
///
/// Every failure is logged: the caller has nothing else to do about one.
#[must_use]
pub fn start_owner_client(session: u32) -> bool {
    let Some(user) = user_token_once_signed_in(session) else {
        tracing::warn!(
            session,
            "cannot start the client: no token for whoever signed in"
        );
        return false;
    };
    let started = start_as(user, session);
    // SAFETY: `user` came from `query_user_token` and is not used again.
    unsafe {
        let _ = CloseHandle(user);
    }
    started
}

/// The token of the user signed in at `session`, waited for up to
/// [`SIGN_IN_SETTLE`].
fn user_token_once_signed_in(session: u32) -> Option<HANDLE> {
    let deadline = Instant::now() + SIGN_IN_SETTLE;
    loop {
        if let Some(token) = query_user_token(session) {
            return Some(token);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(SIGN_IN_POLL);
    }
}

/// [`start_owner_client`] once the user's token is in hand.
fn start_as(user: HANDLE, session: u32) -> bool {
    let Some(sid) = sid_of_token(user) else {
        tracing::warn!(
            session,
            "cannot start the client: cannot read who signed in"
        );
        return false;
    };
    let owner = crate::program_data::logon_host_owner();
    match verdict(owner.as_deref(), &sid, HostRole::is_held()) {
        Verdict::Start => {}
        Verdict::NotTheOwner => {
            tracing::info!(
                session,
                "somebody other than the logon host's owner signed in; not starting a client"
            );
            return false;
        }
        Verdict::AlreadyHosted => {
            tracing::info!(session, "a client already hosts this machine");
            return false;
        }
    }
    let Some(client) = client_path() else {
        return false;
    };
    let Some(token) = elevated_primary(user) else {
        tracing::warn!(session, "cannot start the client: no token to run it with");
        return false;
    };
    let started = spawn(token, session, &client)
        .inspect(|()| {
            tracing::info!(
                session,
                "started the owner's client for the guest who signed in (ADR 0164)"
            );
        })
        .inspect_err(
            |error| tracing::warn!(session, %error, "cannot start the client: CreateProcessAsUser refused"),
        )
        .is_ok();
    // SAFETY: `token` is a live token handle and is not used again after the
    // spawn above.
    unsafe {
        let _ = CloseHandle(token);
    }
    started
}

/// Starts the client of the person signed in at `session`, for the session
/// host that has a session nobody can see yet (ADR 0169 §7); says whether one
/// was started.
///
/// With the person's own token as `WTSQueryUserToken` hands it out — the
/// limited half of an administrator's — because the client is their guest
/// and their panel and needs nothing more. Until the client's manifest stops
/// asking for administrator rights (pack 09), that launch is refused with
/// `ERROR_ELEVATION_REQUIRED`, and the elevated half is used instead, as
/// [`start_owner_client`] does. Every failure is logged.
#[must_use]
pub fn start_client(session: u32) -> bool {
    let Some(user) = query_user_token(session) else {
        tracing::warn!(
            session,
            "cannot start the client: nobody's token in this session"
        );
        return false;
    };
    let started = client_path().is_some_and(|client| start_limited(user, session, &client));
    // SAFETY: `user` came from `query_user_token` and is not used again.
    unsafe {
        let _ = CloseHandle(user);
    }
    started
}

/// [`start_client`] once the person's token and the client's path are in
/// hand.
fn start_limited(user: HANDLE, session: u32, client: &str) -> bool {
    let Some(limited) = primary_token_of(user) else {
        tracing::warn!(session, "cannot start the client: no token to run it with");
        return false;
    };
    let result = spawn(limited, session, client);
    // SAFETY: `limited` is a live token handle and is not used again.
    unsafe {
        let _ = CloseHandle(limited);
    }
    let result = match result {
        Err(error) if error.code() == HRESULT::from_win32(ERROR_ELEVATION_REQUIRED.0) => {
            let Some(elevated) = elevated_primary(user) else {
                tracing::warn!(
                    session,
                    "cannot start the client: it asks for rights this person has not got"
                );
                return false;
            };
            let result = spawn(elevated, session, client);
            // SAFETY: `elevated` is a live token handle and is not used again.
            unsafe {
                let _ = CloseHandle(elevated);
            }
            result
        }
        result => result,
    };
    match result {
        Ok(()) => {
            tracing::info!(
                session,
                "started the client for a session nobody could see yet (ADR 0169)"
            );
            true
        }
        Err(error) => {
            tracing::warn!(session, %error, "cannot start the client: CreateProcessAsUser refused");
            false
        }
    }
}

/// The client beside this binary, as a string safe to quote on a command
/// line; `None`, logged, when there is none.
fn client_path() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let Some(client) = exe.parent().map(|dir| dir.join(CLIENT_EXE)) else {
        tracing::warn!("cannot start the client: this executable has no directory");
        return None;
    };
    if !client.is_file() {
        tracing::warn!(path = %client.display(), "cannot start the client: no desktop binary beside this one");
        return None;
    }
    let client = client.to_string_lossy().into_owned();
    if client.contains('"') {
        tracing::warn!(
            "cannot start the client: refusing an executable path that contains a quote"
        );
        return None;
    }
    Some(client)
}

/// A primary token for the client: the full administrator half of a split
/// UAC token, which is what `HighestAvailable` gives the autostart task
/// (ADR 0140); the token itself when it is not split.
///
/// A standard user's token is returned as it is, and `CreateProcessAsUserW`
/// then refuses the `requireAdministrator` client — exactly what that user's
/// own autostart task would meet.
fn elevated_primary(user: HANDLE) -> Option<HANDLE> {
    let mut kind = TOKEN_ELEVATION_TYPE::default();
    let mut written = 0u32;
    // SAFETY: `user` is a live token handle; `kind` is a local of exactly the
    // size passed, which the call writes; `written` is a local it writes.
    unsafe {
        GetTokenInformation(
            user,
            TokenElevationType,
            Some((&raw mut kind).cast::<c_void>()),
            u32::try_from(size_of::<TOKEN_ELEVATION_TYPE>()).unwrap_or(0),
            &raw mut written,
        )
    }
    .inspect_err(|error| tracing::warn!(%error, "cannot read the user's elevation type"))
    .ok()?;
    if kind != TokenElevationTypeLimited {
        return primary_token_of(user);
    }
    let mut linked = TOKEN_LINKED_TOKEN::default();
    // SAFETY: as above, with `linked` the local the call writes. `LocalSystem`
    // holds `SeTcbPrivilege`, so the linked token is one a process can be
    // created with rather than an identification-level copy.
    unsafe {
        GetTokenInformation(
            user,
            TokenLinkedToken,
            Some((&raw mut linked).cast::<c_void>()),
            u32::try_from(size_of::<TOKEN_LINKED_TOKEN>()).unwrap_or(0),
            &raw mut written,
        )
    }
    .inspect_err(|error| tracing::warn!(%error, "cannot read the user's elevated token"))
    .ok()?;
    let primary = primary_token_of(linked.LinkedToken);
    // SAFETY: `linked.LinkedToken` is a handle the call above opened for this
    // process, and is not used again after `primary_token_of` copied it.
    unsafe {
        let _ = CloseHandle(linked.LinkedToken);
    }
    primary
}

/// `CreateProcessAsUserW`s the client onto `session`'s ordinary desktop as the
/// user `token` names, in that user's own environment.
fn spawn(token: HANDLE, session: u32, client: &str) -> windows::core::Result<()> {
    // The client keeps its settings, logs and webview under the user's
    // `LOCALAPPDATA`; started with this service's environment it would be
    // `LocalSystem`'s, a different client with none of the user's state.
    let mut environment: *mut c_void = std::ptr::null_mut();
    // SAFETY: `environment` is a local the call writes an allocation into;
    // `token` is a live primary token. `false` inherits nothing from this
    // process's own environment.
    if let Err(error) = unsafe { CreateEnvironmentBlock(&raw mut environment, Some(token), false) }
    {
        tracing::warn!(session, %error, "cannot start the client: no environment for the user");
        return Err(error);
    }
    let application = wide(client);
    // `"exe"`: the quotes keep a `C:\Program Files\...` path whole, and there
    // is nothing after it — the same launch the autostart task makes.
    let mut command_line = wide(&format!("\"{client}\""));
    let mut desktop = wide(DEFAULT_DESKTOP);
    let startup = STARTUPINFOW {
        cb: u32::try_from(size_of::<STARTUPINFOW>()).unwrap_or(0),
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    // SAFETY: `token` is a live primary token for the signed-in user;
    // `application`, `command_line` and `desktop` are null-terminated wide
    // buffers that outlive the call; `startup` borrows `desktop` and outlives
    // the call; `environment` is the block allocated above; `process` is
    // written by the call. No handles are inherited.
    let created = unsafe {
        CreateProcessAsUserW(
            Some(token),
            PCWSTR(application.as_ptr()),
            Some(PWSTR(command_line.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_UNICODE_ENVIRONMENT,
            Some(environment),
            PCWSTR::null(),
            &raw const startup,
            &raw mut process,
        )
    };
    // SAFETY: `environment` came from `CreateEnvironmentBlock` above and is not
    // read again; the child holds its own copy by now.
    unsafe {
        let _ = DestroyEnvironmentBlock(environment);
    }
    created?;
    tracing::debug!(session, pid = process.dwProcessId, "client process created");
    // The client lives on its own; nothing here waits on it.
    //
    // SAFETY: both handles came from the successful create above and are not
    // used again.
    unsafe {
        let _ = CloseHandle(process.hThread);
        let _ = CloseHandle(process.hProcess);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: &str = "S-1-5-21-1-2-3-1001";

    /// Only the owner's sign-in starts a client, and only while nothing hosts.
    #[test]
    fn only_the_owner_gets_a_client_and_only_once() {
        assert_eq!(verdict(Some(OWNER), OWNER, false), Verdict::Start);
        assert_eq!(verdict(Some(OWNER), OWNER, true), Verdict::AlreadyHosted);
        assert_eq!(
            verdict(Some(OWNER), "S-1-5-21-1-2-3-1002", false),
            Verdict::NotTheOwner
        );
        assert_eq!(verdict(None, OWNER, false), Verdict::NotTheOwner);
    }

    /// The client is the desktop binary beside this one, on the ordinary
    /// desktop — never the secure one the logon host lived on.
    #[test]
    fn the_client_is_the_desktop_binary_on_the_ordinary_desktop() {
        assert!(!CLIENT_EXE.contains('\\') && !CLIENT_EXE.contains('/'));
        assert_eq!(DEFAULT_DESKTOP, r"WinSta0\Default");
    }
}
