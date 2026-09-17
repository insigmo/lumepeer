//! Serving the logon screen as an ordinary capture target (ADR 0088 §1).
//!
//! ADR 0056 put a `LocalSystem` process on `Winsta0\Winlogon` for exactly one
//! GDI snapshot and then took it away again, and the shortness of that life
//! was half of the argument for it: a process on the secure desktop is a
//! process on the desktop where administrator passwords are typed. It was the
//! right shape for what it did — a UAC prompt a guest needs to see for a
//! second, once.
//!
//! It is the wrong shape for a machine nobody has signed into yet. A worker
//! per frame means a `CreateProcessAsUserW`, a token duplication and a process
//! start for every picture a guest is shown, and the throttle that made that
//! bearable (`SECURE_DESKTOP_CAPTURE_INTERVAL_MS`, 500 ms) is what a person
//! typing their password would be watching their own keystrokes through. So
//! the logon screen gets a worker that **lives as long as the screen does**,
//! and this module is what starts, watches and stops it.
//!
//! What is deliberately *not* widened along with the lifetime:
//!
//! - **The worker is still only a capture and an input.** It speaks
//!   [`crate::agent_protocol`] — the same closed list of commands a session
//!   agent obeys — and there is no message in it that names a path, a peer, a
//!   grant or a program to run.
//! - **It is still launched by the privileged side, never named by a peer.**
//!   The executable is this binary, resolved beside the host's own, and the
//!   argument is a constant.
//! - **It still has no channel of its own.** One pipe, admitting
//!   `LocalSystem` and administrators and nobody else
//!   (`agent_channel::SYSTEM_ONLY_SDDL`), plus the process check the host
//!   already applies to its agent.
//! - **The input it performs is still gated by `secure_desktop_input`.** That
//!   grant is checked in the actor before an event ever reaches this worker,
//!   exactly as it is on a desktop client whose own capture is blocked by a
//!   UAC prompt (ADR 0057, ADR 0061).
//!
//! The one thing this does that the ADR 0056 worker does not is *stay*, and
//! that is the trade this ADR 0088 makes on purpose: a long-lived
//! `LocalSystem` process on the secure desktop, for as long as a guest is
//! being shown that desktop and no longer.

#![allow(
    unsafe_code,
    reason = "token duplication and CreateProcessAsUserW have no safe \
              bindings; same justification standard as agent_launch.rs and \
              the rest of this crate's Win32 surface (ADR 0043, ADR 0049, \
              ADR 0056, ADR 0085)"
)]

use core::ffi::c_void;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_TIMEOUT};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityIdentification, SetTokenInformation, TOKEN_ACCESS_MASK,
    TOKEN_ALL_ACCESS, TOKEN_DUPLICATE, TOKEN_QUERY, TokenPrimary, TokenSessionId,
};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateProcessAsUserW, GetCurrentProcess, OpenProcessToken,
    PROCESS_INFORMATION, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};

use crate::LOGON_SCREEN_WORKER_ARG;
use crate::agent_launch::console_session;

/// The secure desktop, as a desktop path for `STARTUPINFOW.lpDesktop`.
///
/// The logon screen *is* this desktop: what a person sees before they sign in,
/// what they see when they lock, and what a UAC prompt raises. A worker
/// launched anywhere else would capture a desktop nobody is looking at.
const WINLOGON_DESKTOP: &str = r"WinSta0\Winlogon";

/// File name of the worker, resolved beside the binary that launches it.
///
/// This crate's own binary rather than the desktop application: the agent is
/// the desktop app because it needs a window for the indicator and an encoder
/// for its frames, and the logon screen needs neither — it has nobody to show
/// an indicator to, and it publishes the same raw `BGRA8` the secure-desktop
/// worker already does. Using this binary also means the GDI capture on
/// `Winlogon` exists once, in the crate that already carries that `unsafe`.
const WORKER_EXE: &str = "lumepeer-service.exe";

/// How long the worker waits between captures of the logon screen, in
/// milliseconds.
///
/// A deliberate rate, not an inherited throttle. Two things bound it from
/// opposite sides:
///
/// - The logon screen is nearly static, and every frame of it costs a GDI
///   capture by a `LocalSystem` process plus a screen's worth of `BGRA8`
///   across a mapping. There is no reason to pay that sixty times a second.
/// - Somebody is *typing a password* on it, and moving a pointer. Feedback
///   slower than about five frames a second is what makes a remote logon
///   screen unusable rather than merely slow — which is the failure the old
///   500 ms per-frame worker actually had.
///
/// Two hundred milliseconds is the slowest rate that still shows a keystroke
/// before the next one is typed. Local to this crate rather than in
/// `lumepeer-core`'s constants for the reason ADR 0049 §2 records: this crate
/// deliberately names no lumepeer dependency, and that is part of its security
/// argument.
pub const LOGON_SCREEN_CAPTURE_INTERVAL_MS: u64 = 200;

/// How long [`LogonScreenWorker::stop`] waits for the worker to leave on its
/// own before terminating it, in milliseconds.
///
/// Shorter than the session agent's two seconds, because there is less to wind
/// down: the worker has no indicator to take down and no encoder to release —
/// it closes a GDI device context and a view of a mapping. What it must not do
/// is outlive the screen it was serving, and a host that waited on it would be
/// a host that cannot stop hosting.
const WORKER_STOP_TIMEOUT_MS: u32 = 500;

/// A null-terminated UTF-16 copy of `text`, for the `W` entry points.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A running logon-screen worker.
///
/// The same shape as [`crate::agent_launch::SessionAgent`], and for the same
/// reasons: dropping it stops *watching* the worker rather than killing it, so
/// a value that falls out of scope cannot silently take a guest's picture
/// away, and [`stop`](Self::stop) is what actually ends it.
#[derive(Debug)]
pub struct LogonScreenWorker {
    process: HANDLE,
    thread: HANDLE,
    pid: u32,
    session: u32,
}

// SAFETY: both handles are ordinary kernel handles, valid process-wide and not
// bound to the thread that created them; moving the value between threads is
// all `Send` promises.
unsafe impl Send for LogonScreenWorker {}

impl LogonScreenWorker {
    /// Launches the worker onto `session`'s `Winlogon` desktop.
    ///
    /// `None` on every failure — no privilege to stamp a token with a session
    /// id, no executable beside this one, `CreateProcessAsUserW` refusing —
    /// because the caller's answer to all of them is the same: there is no
    /// picture, and the guest is told so rather than shown a frozen frame
    /// (§18; ADR 0024).
    #[must_use]
    pub fn start(session: u32) -> Option<Self> {
        let worker = worker_path()?;
        let token = duplicate_own_token_for_session(session)?;
        let spawned = spawn(token, session, &worker);
        // SAFETY: `token` is a live handle from `duplicate_own_token_for_
        // session` and is not used again after the spawn copied what it
        // needed.
        unsafe {
            let _ = CloseHandle(token);
        }
        spawned
    }

    /// The worker's process id, which the host checks against the process at
    /// the far end of the agent channel.
    #[must_use]
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// The session whose logon screen this worker was launched onto.
    #[must_use]
    pub const fn session(&self) -> u32 {
        self.session
    }

    /// Whether the worker is still running.
    ///
    /// A handle that cannot be waited on reads as **not** running, which is
    /// the safe direction: it costs a relaunch, where the other reading costs
    /// a guest a frozen picture it is told is live.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        // SAFETY: `self.process` is a live process handle owned by this value.
        unsafe { WaitForSingleObject(self.process, 0) == WAIT_TIMEOUT }
    }

    /// Whether this worker is still on the session attached to the console.
    ///
    /// A fast user switch moves the console elsewhere while this process keeps
    /// running on a `Winlogon` desktop nobody is looking at; carrying on would
    /// be publishing frames of a screen that is no longer in front of anybody
    /// (ADR 0088 §2).
    #[must_use]
    pub fn serves_the_console(&self) -> bool {
        console_session() == Some(self.session)
    }

    /// Stops the worker, terminating it if it will not leave.
    ///
    /// The caller is expected to have sent
    /// [`AgentCommand::Shutdown`](crate::agent_protocol::AgentCommand::Shutdown)
    /// first. Terminating what will not go is safe here in a way it would not
    /// be for a process holding somebody's session: this one owns a GDI
    /// snapshot and a view of a mapping the host holds, and the host clears
    /// that mapping ([`crate::frame::Writer::clear`]) rather than trusting
    /// whatever was left in it.
    pub fn stop(self) {
        // SAFETY: `self.process` is a live process handle owned by this value.
        let waited = unsafe { WaitForSingleObject(self.process, WORKER_STOP_TIMEOUT_MS) };
        if waited == WAIT_TIMEOUT {
            tracing::warn!(
                pid = self.pid,
                "logon-screen worker did not leave; terminating it"
            );
            // SAFETY: `self.process` is still live; terminating it is always
            // valid.
            unsafe {
                let _ = TerminateProcess(self.process, 1);
            }
        }
    }
}

impl Drop for LogonScreenWorker {
    fn drop(&mut self) {
        // Closing a process handle does not end the process: dropping this
        // value stops watching the worker, and `stop` is the one that ends it.
        //
        // SAFETY: both handles came from a successful create and are not used
        // again after this.
        unsafe {
            let _ = CloseHandle(self.thread);
            let _ = CloseHandle(self.process);
        }
    }
}

/// The worker executable, beside the binary that is launching it.
///
/// `None` rather than a fallback search: a host that cannot find the worker
/// next to itself is an installation somebody has taken apart, and looking
/// elsewhere would be looking for a `LocalSystem` process to run in a place
/// this binary does not control.
fn worker_path() -> Option<String> {
    let Ok(exe) = std::env::current_exe() else {
        tracing::warn!("logon-screen worker: cannot locate this executable");
        return None;
    };
    let Some(worker) = exe.parent().map(|dir| dir.join(WORKER_EXE)) else {
        tracing::warn!("logon-screen worker: this executable has no directory");
        return None;
    };
    if !worker.is_file() {
        tracing::warn!(
            path = %worker.display(),
            "logon-screen worker: no worker beside this binary"
        );
        return None;
    }
    let worker = worker.to_string_lossy().into_owned();
    if worker.contains('"') {
        // A Windows path cannot hold a quote; if one is here the string was
        // built rather than read from the OS, and it is not going onto a
        // command line (`secure_desktop_launch` and `agent_launch` apply the
        // same rule).
        tracing::warn!("logon-screen worker: refusing an executable path that contains a quote");
        return None;
    }
    Some(worker)
}

/// Duplicates this `LocalSystem` process's primary token and stamps the copy
/// with `session`, so a process created with it lands there.
///
/// The same sequence `secure_desktop_launch` runs for its one-frame worker,
/// and deliberately a second copy rather than a shared one: that module lives
/// in the helper's binary and this one is called from the host's, and the two
/// services do not share code for the reason ADR 0087 §1 records. Setting the
/// session id needs `SeTcbPrivilege`, which `LocalSystem` holds; an
/// unprivileged run fails here cleanly rather than reaching for a desktop it
/// could not open anyway.
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

    // SAFETY: `duplicate` is a live primary token; `session` is a 4-byte value
    // that outlives the call, and `TokenSessionId` takes exactly a `u32`.
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

/// `CreateProcessAsUserW`s the worker onto `WinSta0\Winlogon` in `session`.
fn spawn(token: HANDLE, session: u32, worker: &str) -> Option<LogonScreenWorker> {
    let application = wide(worker);
    // The command line is `"exe" --logon-screen-worker`; the quotes keep a
    // `C:\Program Files\...` path from being split into a command plus args,
    // and the tail is a constant — nothing here is supplied by a peer or by a
    // local user.
    let mut command_line = wide(&format!("\"{worker}\" {LOGON_SCREEN_WORKER_ARG}"));
    let mut desktop = wide(WINLOGON_DESKTOP);

    let startup = STARTUPINFOW {
        cb: u32::try_from(size_of::<STARTUPINFOW>()).unwrap_or(0),
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();

    // No environment block, unlike the session agent's launch: that one needs
    // the signed-in user's own `APPDATA` and profile paths, and this one runs
    // as `LocalSystem` and touches no per-user path at all.
    //
    // SAFETY: `token` is a live primary token stamped for `session`;
    // `application`, `command_line` and `desktop` are null-terminated wide
    // buffers that outlive the call; `startup` borrows `desktop` and outlives
    // the call; `process` is written by the call. `binherithandles = false`
    // because the worker reaches the mapping and the channel by name, never by
    // an inherited handle.
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
        tracing::warn!(session, %error, "logon-screen worker: CreateProcessAsUser refused");
        return None;
    }
    tracing::info!(
        session,
        pid = process.dwProcessId,
        "logon-screen worker started on the console session's secure desktop"
    );
    Some(LogonScreenWorker {
        process: process.hProcess,
        thread: process.hThread,
        pid: process.dwProcessId,
        session,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The worker goes onto the secure desktop and nowhere else. A worker on
    /// `Default` would be capturing the desktop of whoever is signed in, which
    /// is the session agent's job and is done as that user rather than as
    /// `LocalSystem` (ADR 0085 §1).
    #[test]
    fn the_worker_is_launched_onto_the_secure_desktop() {
        assert_eq!(WINLOGON_DESKTOP, r"WinSta0\Winlogon");
    }

    /// The worker is this crate's binary, resolved beside whatever launched
    /// it — never a path, and never the desktop application, which is the
    /// agent and has a window and an encoder this one must not.
    #[test]
    fn the_worker_is_this_binary_beside_the_host() {
        assert_eq!(
            std::path::Path::new(WORKER_EXE).extension(),
            Some(std::ffi::OsStr::new("exe"))
        );
        assert!(
            !WORKER_EXE.contains('\\') && !WORKER_EXE.contains('/'),
            "the worker is resolved beside the launching binary, never by a path"
        );
        assert_ne!(WORKER_EXE, "lumepeer-desktop.exe");
    }

    /// The capture rate is a decision, and both halves of it are worth
    /// pinning: slower than a video stream, fast enough to watch somebody
    /// type. Five frames a second is the floor a password field needs.
    #[test]
    fn the_capture_rate_is_deliberate_at_both_ends() {
        const {
            assert!(
                LOGON_SCREEN_CAPTURE_INTERVAL_MS <= 200,
                "slower than five frames a second is a logon screen nobody can type on"
            );
            assert!(
                LOGON_SCREEN_CAPTURE_INTERVAL_MS >= 50,
                "the logon screen is static; there is nothing to spend a video frame rate on"
            );
        }
    }

    /// Starting a worker without the privilege to stamp a token is an answer,
    /// not a panic — which is every unelevated run, including this test.
    #[test]
    fn a_launch_without_the_privilege_is_refused_rather_than_attempted() {
        // `u32::MAX` is the "no session" sentinel: a worker can never belong
        // there, so this asks for the failure path on any machine.
        assert!(LogonScreenWorker::start(u32::MAX).is_none());
    }
}
