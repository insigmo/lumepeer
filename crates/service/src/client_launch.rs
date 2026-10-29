//! Starting the signed-in person's client (ADR 0164, ADR 0169 §7).
//!
//! Two moments need a client nobody started by hand. The logon host (ADR 0126)
//! steps down at sign-in, and the guest who just typed the password dials
//! again — into the session host, which keeps its endpoint closed until a
//! client attaches (ADR 0169 §1). And the session host has a session running
//! that nobody at the machine can see yet (§7). Either way the service starts
//! the client in the signed-in session itself, whether or not autostart is
//! on.
//!
//! The client is `asInvoker` and only the person's interface — their guest
//! and the host's panel, never a host — so it is started with the token
//! `WTSQueryUserToken` hands out: for an administrator, the limited half, at
//! medium integrity, exactly what the person's own double-click gives it. The
//! user's own environment, the ordinary desktop. The path is the desktop
//! binary beside this one and the command line carries nothing else
//! (ADR 0043 §4).

#![allow(
    unsafe_code,
    reason = "CreateEnvironmentBlock and CreateProcessAsUserW \
              have no safe bindings; same justification standard as the rest of \
              this crate's Win32 surface (ADR 0043, ADR 0085, ADR 0126)"
)]

use core::ffi::c_void;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, PROCESS_INFORMATION, STARTUPINFOW,
};
use windows::core::{PCWSTR, PWSTR};

use crate::agent_launch::{primary_token_of, query_user_token};

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

/// Starts the client of the person signed in at `session` — the one a guest
/// just signed in, or the one the session host has a session for that nobody
/// can see yet (ADR 0169 §7); says whether one was started.
///
/// With the person's own token as `WTSQueryUserToken` hands it out, waited
/// for up to [`SIGN_IN_SETTLE`] after a fresh sign-in. Every failure is
/// logged: the caller has nothing else to do about one.
#[must_use]
pub fn start_client(session: u32) -> bool {
    let Some(user) = user_token_once_signed_in(session) else {
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
    match result {
        Ok(()) => {
            tracing::info!(session, "started the person's client (ADR 0169)");
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
    // is nothing after it — the same launch the autostart entry makes.
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

    /// The client is the desktop binary beside this one, on the ordinary
    /// desktop — never the secure one the logon host lived on.
    #[test]
    fn the_client_is_the_desktop_binary_on_the_ordinary_desktop() {
        assert!(!CLIENT_EXE.contains('\\') && !CLIENT_EXE.contains('/'));
        assert_eq!(DEFAULT_DESKTOP, r"WinSta0\Default");
    }
}
