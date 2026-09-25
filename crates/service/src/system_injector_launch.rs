//! Starting and watching the `LocalSystem` desktop injector (ADR 0114).
//!
//! The third worker shape this crate launches, and exactly the combination the
//! other two leave open:
//!
//! | | secure-desktop worker (ADR 0056/0057) | session agent (ADR 0085) | **desktop injector (ADR 0114)** |
//! | --- | --- | --- | --- |
//! | Runs as | `LocalSystem` | the signed-in user | **`LocalSystem`** |
//! | Desktop | `WinSta0\Winlogon` | `WinSta0\Default` | **`WinSta0\Default`** |
//! | Lives for | one event | the whole session | **the whole session** |
//!
//! It is `LocalSystem` for the reason the secure-desktop worker is — nothing
//! below that integrity level can put input in front of a System-integrity
//! foreground window, which is what a `VMware` guest's `MKSEmbedded` window is
//! (docs/bugs/17-remote-hotkeys.md) — and it is on `Default` for the reason the
//! agent is: that is the desktop the signed-in person actually uses. The agent
//! deliberately drops to the user's token because a `LocalSystem` process that
//! *captures screens* is a machine-wide risk (ADR 0085 §1); this one keeps
//! `LocalSystem` because it does not capture — it only injects, the one thing
//! ADR 0114 argues is worth that token, behind an authenticated channel and the
//! same authorization the in-process injector already passed.
//!
//! The launch is the mirror of [`crate::agent_launch::SessionAgent::start`],
//! with the token swapped: the agent asks [`WTSQueryUserToken`] for the user's,
//! this asks [`crate::secure_desktop_launch::duplicate_own_token_for_session`]
//! for a copy of the service's own `LocalSystem` token stamped into the console
//! session. Everything the privileged side keeps for itself — the path is this
//! binary's own directory, the argument is a constant — is unchanged, because
//! the elevation still runs *our* binary and never a shell (ADR 0043 §4).
//!
//! [`WTSQueryUserToken`]: windows::Win32::System::RemoteDesktop::WTSQueryUserToken

#![allow(
    unsafe_code,
    reason = "CreateProcessAsUserW and the process queries around it have no \
              safe bindings; same justification standard as the rest of this \
              crate's Win32 surface (ADR 0043, ADR 0056, ADR 0085, ADR 0114)"
)]

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateProcessAsUserW, GetExitCodeProcess, PROCESS_INFORMATION, STARTUPINFOW,
    TerminateProcess, WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};

use lumepeer_service::SYSTEM_INPUT_WORKER_ARG;
use lumepeer_service::agent_launch::console_session;

/// The ordinary interactive desktop, as a path for `STARTUPINFOW.lpDesktop`.
///
/// The same desktop the session agent serves, never `Winlogon`: this injector
/// performs the input the person at the machine is watching, on the desktop
/// they are using. The secure desktop stays the short-lived worker's business
/// (ADR 0056, ADR 0057).
const DEFAULT_DESKTOP: &str = r"WinSta0\Default";

/// File name of the injector, resolved beside this binary.
///
/// The desktop application in injector mode, not a binary of its own, for the
/// reason `agent_launch::AGENT_EXE` is the same: the injection logic lives in
/// `crates/media`, which this privileged binary does not link, so the one
/// implementation is launched rather than a second one kept in step by review.
const INJECTOR_EXE: &str = "lumepeer-desktop.exe";

/// How long [`SystemInjector::stop`] waits for the injector to leave on its own
/// before it is terminated, in milliseconds.
///
/// It has no window and no capture backend to release, so it leaves faster than
/// the agent does; two seconds is generous and short enough that a service
/// stopping does not look wedged.
const INJECTOR_STOP_TIMEOUT_MS: u32 = 2_000;

/// A null-terminated UTF-16 copy of `text`, for the `W` entry points.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A running desktop injector.
///
/// Holding one is what "there is a `LocalSystem` injector on the console
/// session right now" means to the service. Dropping it stops watching, not the
/// process — [`stop`](Self::stop) is the one that ends it — so a value falling
/// out of scope cannot silently kill the path the whole session's input is
/// travelling on.
#[derive(Debug)]
pub struct SystemInjector {
    process: HANDLE,
    thread: HANDLE,
    pid: u32,
    session: u32,
}

// SAFETY: both handles are ordinary kernel handles, valid process-wide and not
// bound to the thread that created them; moving the value between threads is all
// `Send` promises.
unsafe impl Send for SystemInjector {}

impl SystemInjector {
    /// Launches the injector into `session` as `LocalSystem`.
    ///
    /// `None` on every failure — the token could not be stamped for the session
    /// (an unprivileged `--console` run), the executable is not beside this one,
    /// `CreateProcessAsUserW` refused — because the caller's answer to all of
    /// them is the same: there is no `LocalSystem` injector, so fall back to the
    /// host's in-process one (ADR 0114 §3).
    #[must_use]
    pub fn start(session: u32) -> Option<Self> {
        let Ok(exe) = std::env::current_exe() else {
            tracing::warn!("desktop injector: cannot locate this executable");
            return None;
        };
        let Some(injector) = exe.parent().map(|dir| dir.join(INJECTOR_EXE)) else {
            tracing::warn!("desktop injector: this executable has no directory");
            return None;
        };
        if !injector.is_file() {
            tracing::warn!(
                path = %injector.display(),
                "desktop injector: no desktop binary beside this one"
            );
            return None;
        }
        let injector = injector.to_string_lossy().into_owned();
        if injector.contains('"') {
            // A Windows path cannot hold a quote; if one is here the string was
            // built rather than read from the OS, and it is not going onto a
            // command line (the rule `secure_desktop_launch` and `agent_launch`
            // both keep).
            tracing::warn!("desktop injector: refusing an executable path that contains a quote");
            return None;
        }

        let token = crate::secure_desktop_launch::duplicate_own_token_for_session(session)?;
        let spawned = spawn_injector(token, session, &injector);
        // SAFETY: `token` is a live handle from `duplicate_own_token_for_session`
        // and is not used again after the spawn above copied what it needed.
        unsafe {
            let _ = CloseHandle(token);
        }
        spawned
    }

    /// The injector's process id.
    ///
    /// The service compares it against the process at the far end of the
    /// injector channel, so a different process in the same session cannot stand
    /// in for the injector the service launched.
    #[must_use]
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// Whether the injector is still running.
    ///
    /// A handle that cannot be waited on reads as **not** running, the safe
    /// direction: it costs a relaunch, where the other reading costs a guest an
    /// input path that silently went nowhere.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        // SAFETY: `self.process` is a live process handle owned by this value.
        unsafe { WaitForSingleObject(self.process, 0) == WAIT_TIMEOUT }
    }

    /// Whether this injector is still the one serving the console session.
    ///
    /// A fast user switch moves the console to another session while this
    /// injector keeps running in the one it was started for; noticing that is
    /// what keeps the service from feeding input to a desktop nobody is at.
    #[must_use]
    pub fn serves_the_console(&self) -> bool {
        console_session() == Some(self.session)
    }

    /// Stops the injector, terminating it if it will not leave.
    ///
    /// There is no courteous "shut down" message on this one-way channel — the
    /// injector has nothing to say back and exits when the channel closes — so
    /// closing the service's end and then this is the whole teardown. A service
    /// that could not stop its own injector could not hand the host role over.
    pub fn stop(self) {
        // SAFETY: `self.process` is a live process handle owned by this value.
        let waited = unsafe { WaitForSingleObject(self.process, INJECTOR_STOP_TIMEOUT_MS) };
        if waited == WAIT_TIMEOUT {
            tracing::warn!(
                pid = self.pid,
                "desktop injector did not leave; terminating it"
            );
            // SAFETY: `self.process` is still live; terminating it is always
            // valid.
            unsafe {
                let _ = TerminateProcess(self.process, 1);
            }
        }
    }

    /// The injector's exit code, once it has exited. `None` while it is still
    /// running or when the code cannot be read — used for the log, never for a
    /// decision, since every reason it is gone leads to the same next step.
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

impl Drop for SystemInjector {
    fn drop(&mut self) {
        // Closing a process handle does not end the process: dropping this value
        // stops watching, `stop` is the one that ends it.
        //
        // SAFETY: both handles came from a successful create and are not used
        // again after this.
        unsafe {
            let _ = CloseHandle(self.thread);
            let _ = CloseHandle(self.process);
        }
    }
}

/// `CreateProcessAsUserW`s the injector onto the console session's `Default`
/// desktop as `LocalSystem`.
fn spawn_injector(token: HANDLE, session: u32, injector: &str) -> Option<SystemInjector> {
    let application = wide(injector);
    // The command line is `"exe" --system-input-worker`; the quotes keep a
    // `C:\Program Files\...` path from being split, and the tail is a constant —
    // nothing here is supplied by a peer or a local user.
    let mut command_line = wide(&format!("\"{injector}\" {SYSTEM_INPUT_WORKER_ARG}"));
    let mut desktop = wide(DEFAULT_DESKTOP);

    let startup = STARTUPINFOW {
        cb: u32::try_from(size_of::<STARTUPINFOW>()).unwrap_or(0),
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();

    // No user environment block, unlike the agent: this process is `LocalSystem`
    // and loads no user profile — it opens a pipe and calls `SendInput`, neither
    // of which touches a per-user path. `CREATE_NO_WINDOW` because it has no UI
    // at all, the same as the secure-desktop worker.
    //
    // SAFETY: `token` is a live primary `LocalSystem` token stamped for the
    // console session; `application`, `command_line` and `desktop` are
    // null-terminated wide buffers that outlive the call; `startup` borrows
    // `desktop` and outlives the call; `process` is written by the call.
    // `binherithandles = false` because the injector reaches the channel by
    // name, never by an inherited handle.
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
        tracing::warn!(session, %error, "desktop injector: CreateProcessAsUser refused");
        return None;
    }
    tracing::info!(
        session,
        pid = process.dwProcessId,
        "desktop injector started in the console session"
    );
    Some(SystemInjector {
        process: process.hProcess,
        thread: process.hThread,
        pid: process.dwProcessId,
        session,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The injector is launched onto the ordinary desktop, never the secure one
    /// — reaching `Winlogon` is the short-lived worker's business (ADR 0056,
    /// ADR 0057).
    #[test]
    fn the_injector_is_launched_onto_the_ordinary_desktop() {
        assert_eq!(DEFAULT_DESKTOP, r"WinSta0\Default");
        assert!(!DEFAULT_DESKTOP.contains("Winlogon"));
    }

    /// The injector is the desktop application in injector mode, resolved beside
    /// this binary and never by a path.
    #[test]
    fn the_injector_is_the_desktop_binary_beside_this_one() {
        assert_eq!(
            std::path::Path::new(INJECTOR_EXE).extension(),
            Some(std::ffi::OsStr::new("exe"))
        );
        assert!(!INJECTOR_EXE.contains('\\') && !INJECTOR_EXE.contains('/'));
    }
}
