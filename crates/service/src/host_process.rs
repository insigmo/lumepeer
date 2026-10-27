//! A whole host the service runs as `LocalSystem` in the console session: the
//! logon host on `Winlogon` (ADR 0126) and the session host on `Default`
//! (ADR 0169).
//!
//! Both are the desktop binary beside this one with one constant argument,
//! launched with the service's own token stamped for the console session
//! ([`crate::secure_desktop_launch::duplicate_own_token_for_session`]), and
//! stopped the same way: asked to give the host role up, waited for, and
//! terminated if they do not leave. The two supervisors differ only in *when*
//! — nobody signed in, or somebody — so the launch is written once here.

#![allow(
    unsafe_code,
    reason = "CreateProcessAsUserW and the process queries around it have no \
              safe bindings; same justification standard as the rest of this \
              crate's Win32 surface (ADR 0043, ADR 0056, ADR 0114, ADR 0126)"
)]

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateProcessAsUserW, GetExitCodeProcess, PROCESS_INFORMATION, STARTUPINFOW,
    TerminateProcess, WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};

/// File name of both hosts, resolved beside this binary: the desktop
/// application, for the reason `system_injector_launch::INJECTOR_EXE` is.
const HOST_EXE: &str = "lumepeer-desktop.exe";

/// How long [`HostProcess::stop`] waits for a host to give the role up and
/// leave before it is terminated, in milliseconds.
///
/// Longer than the injector's: a host has sessions to close and an endpoint
/// to shut down, and a guest told the session ended is better off than one
/// left to time a dead link out.
const HOST_STOP_TIMEOUT_MS: u32 = 5_000;

/// A null-terminated UTF-16 copy of `text`, for the `W` entry points.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A running host.
#[derive(Debug)]
pub struct HostProcess {
    process: HANDLE,
    thread: HANDLE,
    /// Its process id, for the log.
    pub pid: u32,
    /// The console session it was started into.
    pub session: u32,
    /// Which host this is, for the log: "logon host", "session host".
    what: &'static str,
}

impl HostProcess {
    /// Launches the desktop binary with `argument` onto `desktop` of
    /// `session`, as `LocalSystem`; `None` on every failure, each of which is
    /// logged.
    pub fn start(session: u32, desktop: &str, argument: &str, what: &'static str) -> Option<Self> {
        let exe = std::env::current_exe().ok()?;
        let Some(host) = exe.parent().map(|dir| dir.join(HOST_EXE)) else {
            tracing::warn!("{what}: this executable has no directory");
            return None;
        };
        if !host.is_file() {
            tracing::warn!(path = %host.display(), "{what}: no desktop binary beside this one");
            return None;
        }
        let host = host.to_string_lossy().into_owned();
        if host.contains('"') {
            tracing::warn!("{what}: refusing an executable path that contains a quote");
            return None;
        }
        let token = crate::secure_desktop_launch::duplicate_own_token_for_session(session)?;
        let spawned = spawn(token, session, desktop, &host, argument, what);
        // SAFETY: `token` is a live handle from `duplicate_own_token_for_session`
        // and is not used again after the spawn above copied what it needed.
        unsafe {
            let _ = CloseHandle(token);
        }
        spawned
    }

    /// Whether the host is still running; a handle that cannot be waited on
    /// reads as not running.
    pub fn is_alive(&self) -> bool {
        // SAFETY: `self.process` is a live process handle owned by this value.
        unsafe { WaitForSingleObject(self.process, 0) == WAIT_TIMEOUT }
    }

    /// The exit code, once it has exited.
    pub fn exit_code(&self) -> Option<u32> {
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

    /// Asks the host to give the host role up and leave, and terminates it if
    /// it has not within [`HOST_STOP_TIMEOUT_MS`]; the exit code when it left
    /// on its own.
    pub fn stop(self) -> Option<u32> {
        // The request the desktop client makes too (ADR 0085 §4): the host
        // ends its sessions and releases the role on its own.
        let asked = lumepeer_service::host_role::request_release();
        // SAFETY: `self.process` is a live process handle owned by this value.
        let waited = unsafe {
            WaitForSingleObject(self.process, if asked { HOST_STOP_TIMEOUT_MS } else { 0 })
        };
        if waited == WAIT_TIMEOUT {
            tracing::warn!(
                pid = self.pid,
                "the {} did not leave; terminating it",
                self.what
            );
            // SAFETY: `self.process` is still live; terminating it is always
            // valid.
            unsafe {
                let _ = TerminateProcess(self.process, 1);
            }
            return None;
        }
        self.exit_code()
    }
}

impl Drop for HostProcess {
    fn drop(&mut self) {
        // SAFETY: both handles came from a successful create and are not used
        // again after this.
        unsafe {
            let _ = CloseHandle(self.thread);
            let _ = CloseHandle(self.process);
        }
    }
}

/// `CreateProcessAsUserW`s `host argument` onto `desktop` of `session` as
/// `LocalSystem`.
fn spawn(
    token: HANDLE,
    session: u32,
    desktop: &str,
    host: &str,
    argument: &str,
    what: &'static str,
) -> Option<HostProcess> {
    let application = wide(host);
    // `"exe" --flag`: the quotes keep a `C:\Program Files\...` path whole, and
    // the tail is a constant.
    let mut command_line = wide(&format!("\"{host}\" {argument}"));
    let mut desktop = wide(desktop);
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
        tracing::warn!(session, %error, "{what}: CreateProcessAsUser refused");
        return None;
    }
    tracing::info!(session, pid = process.dwProcessId, "{what} started");
    Some(HostProcess {
        process: process.hProcess,
        thread: process.hThread,
        pid: process.dwProcessId,
        session,
        what,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both hosts are the desktop binary beside this one, never a path.
    #[test]
    fn a_host_is_the_desktop_binary_beside_this_one() {
        assert!(!HOST_EXE.contains('\\') && !HOST_EXE.contains('/'));
        assert_eq!(
            std::path::Path::new(HOST_EXE).extension(),
            Some(std::ffi::OsStr::new("exe"))
        );
    }
}
