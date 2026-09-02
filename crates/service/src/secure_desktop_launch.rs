//! Crossing the session boundary to capture the secure desktop (ADR 0056).
//!
//! ADR 0049 tried to snapshot `Winsta0\Winlogon` from the service's own
//! thread, which runs as `LocalSystem` in **session 0**. A window station is
//! a per-session object: session 0 cannot name, let alone switch onto, the
//! console session's `WinSta0`, so that capture never reached the desktop a
//! local administrator authenticates on. It always returned nothing.
//!
//! The supported way for a session-0 service to run code on the interactive
//! secure desktop — the same one remote-assistance services use to "show the
//! UAC prompt" — is to launch a process **into the console session, on the
//! `Winlogon` desktop**:
//!
//! - [`capture_via_worker`] (service side, session 0) finds the console
//!   session, duplicates the service's own `LocalSystem` token, re-stamps the
//!   duplicate with that session id (which needs `SeTcbPrivilege`, held by
//!   `LocalSystem` and nobody unprivileged), and `CreateProcessAsUserW`s this
//!   same binary with [`SECURE_DESKTOP_WORKER_ARG`] and
//!   `STARTUPINFOW.lpDesktop = "WinSta0\\Winlogon"`.
//! - [`run_worker`] (worker side, console session, on `Winlogon`) opens the
//!   mapping the service is holding, takes one GDI snapshot of the desktop it
//!   was launched onto, writes it, and exits. Its exit code is the whole
//!   answer.
//!
//! The worker is short-lived by construction: a `LocalSystem` process on the
//! interactive secure desktop exists only for the one capture, then is gone.
//! Every failure here — no console session, token duplication refused, the
//! child not spawning, a non-zero exit, a timeout — collapses to the same
//! `false`/non-zero, and the caller falls back to
//! `docs/bugs/11-uac-degradation.md`'s honest message (ADR 0056).

#![allow(
    unsafe_code,
    reason = "token duplication and CreateProcessAsUserW have no safe bindings; \
              same justification standard as SendInput (ADR 0012) and the rest \
              of this crate's Win32 surface (ADR 0043, ADR 0049)"
)]

use core::ffi::c_void;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityIdentification, SetTokenInformation, TOKEN_ACCESS_MASK,
    TOKEN_ALL_ACCESS, TOKEN_DUPLICATE, TOKEN_QUERY, TokenPrimary, TokenSessionId,
};
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess,
    OpenProcessToken, PROCESS_INFORMATION, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};

use lumepeer_service::{SECURE_DESKTOP_INPUT_WORKER_ARG, SECURE_DESKTOP_WORKER_ARG};

/// The secure desktop, as a desktop path for `STARTUPINFOW.lpDesktop`.
const WINLOGON_DESKTOP: &str = r"WinSta0\Winlogon";

/// How long the service waits for one worker capture before killing it and
/// reporting refusal, in milliseconds.
///
/// A GDI snapshot of one screen is milliseconds of work; this is a generous
/// ceiling whose only job is to keep a wedged worker from blocking the
/// service's single-threaded accept loop. It is deliberately shorter than the
/// pipe's own `PIPE_TIMEOUT_MS` (5 s) so the service answers the client before
/// the client gives up on the connection.
const SECURE_DESKTOP_WORKER_TIMEOUT_MS: u32 = 4_000;

/// The exit code the worker returns for "a frame is in the mapping".
const WORKER_OK: u32 = 0;

/// A null-terminated UTF-16 copy of `text`, for the `W` entry points.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The worker body: opens the mapping the service already created, captures
/// the desktop this process was launched onto, and writes the frame.
///
/// Returns the process exit code — [`WORKER_OK`] on success, non-zero for
/// every failure, matching the "one bit of outcome, no oracle" rule the pipe
/// reply already follows. Runs in the console session on `Winlogon` (the
/// service put it there via `lpDesktop`), so [`crate::secure_desktop::capture`]
/// finally has the right desktop to snapshot.
#[must_use]
pub fn run_worker() -> u32 {
    let Some(writer) = lumepeer_service::frame::Writer::open() else {
        tracing::error!("secure-desktop worker: cannot open the frame mapping");
        return 1;
    };
    let Some((width, height, data)) = crate::secure_desktop::capture() else {
        tracing::warn!("secure-desktop worker: nothing to capture");
        return 1;
    };
    if writer.write(width, height, &data) {
        tracing::info!(width, height, "secure-desktop worker: published a frame");
        WORKER_OK
    } else {
        tracing::error!("secure-desktop worker: the frame did not fit the mapping");
        1
    }
}

/// The input worker body: performs one already-validated event on the desktop
/// this process was launched onto and exits (ADR 0057).
///
/// Runs in the console session on `Winlogon` (the service put it there via
/// `lpDesktop`), so its `SendInput` reaches the secure desktop that the
/// elevated client's own thread cannot. Returns [`WORKER_OK`] when the OS
/// accepted the event, non-zero otherwise — the one bit of outcome the
/// service reports back, same as the capture worker.
#[must_use]
pub fn run_input_worker(action: lumepeer_service::protocol::InjectAction) -> u32 {
    if crate::secure_desktop_input::perform(action) {
        WORKER_OK
    } else {
        tracing::warn!("secure-desktop input worker: SendInput did not accept the event");
        1
    }
}

/// Launches the worker into the console session's secure desktop and waits
/// for it (service side, session 0).
///
/// Returns whether the worker reported a frame. The mapping the worker fills
/// is the one the caller must already be holding open (`serve_secure_desktop_
/// capture`), so it survives the worker's exit for the client to read — this
/// function never touches the mapping itself, only the child process.
#[must_use]
pub fn capture_via_worker() -> bool {
    run_worker_with_args(SECURE_DESKTOP_WORKER_ARG)
}

/// Launches the worker to perform one input event on the console session's
/// secure desktop and waits for it (service side, session 0; ADR 0057).
///
/// The mirror of [`capture_via_worker`], with the event carried as the four
/// bounded integers [`lumepeer_service::protocol::inject_to_args`] produces —
/// never a peer string, so `spawn_worker`'s command line stays built from
/// values this process controls.
#[must_use]
pub fn inject_via_worker(action: lumepeer_service::protocol::InjectAction) -> bool {
    let [kind, logical, x, y] = lumepeer_service::protocol::inject_to_args(action);
    run_worker_with_args(&format!(
        "{SECURE_DESKTOP_INPUT_WORKER_ARG} {kind} {logical} {x} {y}"
    ))
}

/// Duplicates this `LocalSystem` process's token into the console session and
/// launches this binary there with `arg_tail`, waiting for it — the shared
/// body of [`capture_via_worker`] and [`inject_via_worker`].
fn run_worker_with_args(arg_tail: &str) -> bool {
    // SAFETY: a plain kernel32 export with no arguments; `0xFFFF_FFFF` is the
    // documented "no session attached to the console" answer.
    let console_session = unsafe { WTSGetActiveConsoleSessionId() };
    if console_session == u32::MAX {
        tracing::warn!("no active console session for the secure-desktop worker");
        return false;
    }

    let Some(token) = duplicate_own_token_for_session(console_session) else {
        return false;
    };
    let spawned = spawn_worker(token, arg_tail);
    // SAFETY: `token` is a live handle from `duplicate_own_token_for_session`
    // and is not used again after the spawn above copied what it needed.
    unsafe {
        let _ = CloseHandle(token);
    }
    spawned
}

/// Duplicates this process's (`LocalSystem`) primary token and stamps the
/// copy with `session`, so a process created with it lands in that session.
///
/// `None` on any failure. Setting the session id needs `SeTcbPrivilege`,
/// which `LocalSystem` holds; an unprivileged run (a developer's `--console`)
/// fails here cleanly rather than reaching for a desktop it could not open
/// anyway.
fn duplicate_own_token_for_session(session: u32) -> Option<HANDLE> {
    let mut process_token = HANDLE::default();
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle needing no close;
    // `process_token` is a local that outlives the call and is only written.
    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ACCESS_MASK(TOKEN_DUPLICATE.0 | TOKEN_QUERY.0),
            &raw mut process_token,
        )
    }
    .inspect_err(|error| tracing::warn!(%error, "cannot open this process's token"))
    .ok()?;

    let mut duplicate = HANDLE::default();
    // SAFETY: `process_token` was just opened with `TOKEN_DUPLICATE`;
    // `duplicate` is a local that outlives the call and is only written.
    let duplicated = unsafe {
        DuplicateTokenEx(
            process_token,
            TOKEN_ALL_ACCESS,
            None,
            SecurityIdentification,
            TokenPrimary,
            &raw mut duplicate,
        )
    };
    // SAFETY: closing the source token handle this function opened, once; the
    // duplicate is independent of it.
    unsafe {
        let _ = CloseHandle(process_token);
    }
    duplicated
        .inspect_err(|error| tracing::warn!(%error, "cannot duplicate this process's token"))
        .ok()?;

    // SAFETY: `duplicate` is a live primary token; `session` is a 4-byte
    // value that outlives the call, and `TokenSessionId` takes exactly a
    // `u32`.
    let stamped = unsafe {
        SetTokenInformation(
            duplicate,
            TokenSessionId,
            (&raw const session).cast::<c_void>(),
            u32::try_from(size_of::<u32>()).unwrap_or(4),
        )
    };
    if let Err(error) = stamped {
        tracing::warn!(%error, "cannot move the duplicated token into the console session");
        // SAFETY: `duplicate` is live and owned here; nothing else holds it.
        unsafe {
            let _ = CloseHandle(duplicate);
        }
        return None;
    }
    Some(duplicate)
}

/// `CreateProcessAsUserW`s this binary with `arg_tail` onto the console
/// session's `Winlogon` desktop, waits for it, and returns whether it exited
/// [`WORKER_OK`].
///
/// `arg_tail` is always a constant plus, for the input worker, integers this
/// process formatted itself ([`inject_via_worker`]) — never a value from the
/// peer, so the command line below cannot be steered by whatever asked.
fn spawn_worker(token: HANDLE, arg_tail: &str) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        tracing::warn!("secure-desktop worker: cannot locate this executable");
        return false;
    };
    let exe = exe.to_string_lossy().into_owned();
    if exe.contains('"') {
        // A Windows path cannot hold a quote; if one is here the string was
        // built rather than read from the OS, and it is not going onto a
        // command line.
        tracing::warn!("secure-desktop worker: refusing an executable path that contains a quote");
        return false;
    }
    let application = wide(&exe);
    // The command line is `"exe" <arg_tail>`; the quotes keep a
    // `C:\Program Files\...` path from being split into a command plus args.
    let mut command_line = wide(&format!("\"{exe}\" {arg_tail}"));
    let mut desktop = wide(WINLOGON_DESKTOP);

    let startup = STARTUPINFOW {
        cb: u32::try_from(size_of::<STARTUPINFOW>()).unwrap_or(0),
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();

    // SAFETY: `token` is a live primary token stamped for the console session;
    // `application`, `command_line` and `desktop` are null-terminated wide
    // buffers that outlive the call; `startup` borrows `desktop` and outlives
    // the call; `process` is written by the call. `binherithandles = false`
    // because the worker reaches the mapping by name, not by an inherited
    // handle.
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
        tracing::warn!(%error, "secure-desktop worker: CreateProcessAsUser refused");
        return false;
    }

    let outcome = wait_for_worker(process.hProcess);
    // SAFETY: both handles came from the successful create above and are not
    // used again after this.
    unsafe {
        let _ = CloseHandle(process.hThread);
        let _ = CloseHandle(process.hProcess);
    }
    outcome
}

/// Waits for the worker, bounded by [`SECURE_DESKTOP_WORKER_TIMEOUT_MS`], and
/// reports whether it exited [`WORKER_OK`]. A worker that overruns is killed
/// and counted as a failure, so it cannot wedge the accept loop.
fn wait_for_worker(process: HANDLE) -> bool {
    // SAFETY: `process` is a live process handle from a successful create.
    let waited = unsafe { WaitForSingleObject(process, SECURE_DESKTOP_WORKER_TIMEOUT_MS) };
    if waited != WAIT_OBJECT_0 {
        tracing::warn!("secure-desktop worker: timed out; terminating it");
        // SAFETY: `process` is still live; terminating it is always valid.
        unsafe {
            let _ = TerminateProcess(process, 1);
        }
        return false;
    }
    let mut exit_code = 1u32;
    // SAFETY: `process` is live and `exit_code` is a local that outlives the
    // call and is only written.
    if let Err(error) = unsafe { GetExitCodeProcess(process, &raw mut exit_code) } {
        tracing::warn!(%error, "secure-desktop worker: cannot read the worker's exit code");
        return false;
    }
    exit_code == WORKER_OK
}
