//! Registering and removing `LumepeerHost` with the service control manager
//! (ADR 0085 §1).
//!
//! The same shape as `crates/service/src/install.rs`, and a deliberate second
//! copy rather than a shared module. `crates/service`'s **library** is what
//! the unprivileged desktop client links, and two properties of it are worth
//! more than two hundred lines of shared boilerplate: its header's claim that
//! nothing privileged lives there, and ADR 0049 §2's rule that its dependency
//! list carries no lumepeer crate at all. Putting `CreateServiceW` in that
//! library would weaken the first; putting it in a crate both service binaries
//! depend on would break the second. So each service binary registers itself,
//! and what the two copies can drift on — a display name, a description, a
//! start type — is visible in `services.msc` rather than silent.
//!
//! Administrator rights are needed, which is why this is a flag on this binary
//! rather than a command line somebody builds for `sc.exe`: the elevated code
//! is ours, and there is no path to quote wrongly.

#![allow(
    unsafe_code,
    reason = "the service control manager has no safe bindings; same \
              justification standard as SendInput (ADR 0012) and \
              crates/service's own installer (ADR 0043)"
)]

use windows::Win32::Foundation::{
    ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_EXISTS, ERROR_SERVICE_MARKED_FOR_DELETE,
};
use windows::Win32::System::Services::{
    ChangeServiceConfigW, CloseServiceHandle, ControlService, CreateServiceW, DeleteService,
    ENUM_SERVICE_TYPE, OpenSCManagerW, OpenServiceW, QueryServiceStatus, SC_HANDLE,
    SC_MANAGER_CONNECT, SC_MANAGER_CREATE_SERVICE, SERVICE_ALL_ACCESS, SERVICE_AUTO_START,
    SERVICE_CHANGE_CONFIG, SERVICE_CONTROL_STOP, SERVICE_ERROR, SERVICE_ERROR_NORMAL,
    SERVICE_NO_CHANGE, SERVICE_QUERY_STATUS, SERVICE_START, SERVICE_START_TYPE, SERVICE_STATUS,
    SERVICE_STOP, SERVICE_STOPPED, SERVICE_WIN32_OWN_PROCESS, StartServiceW,
};
use windows::core::{Error, HRESULT, PCWSTR};

use crate::service::SERVICE_NAME;

/// Shown in `services.msc`, so it has to say what it is without a manual.
const DISPLAY_NAME: &str = "Lumepeer host";

/// Shown as the service's description, for the same reason.
///
/// Says the two things an administrator reading this list needs: that it can
/// answer before anybody signs in, and that stopping it does not take remote
/// access away — the desktop client hosts exactly as it did before (ADR 0085
/// §4).
const DESCRIPTION: &str = "Lets this computer answer a Lumepeer connection while nobody is signed in, using the device \
     password configured in Lumepeer. Stopping this service does not disable remote access: the \
     Lumepeer window hosts as usual whenever somebody is signed in.";

/// How long [`stop_and_wait`] waits for a running service to drain before
/// starting the new image anyway, in milliseconds.
const STOP_TIMEOUT_MS: u64 = 10_000;

/// How often [`stop_and_wait`] re-reads the service's state while waiting.
const STOP_POLL_MS: u64 = 250;

/// Registers the service and starts it. Requires administrator rights.
///
/// Idempotent, for the reason `crates/service`'s installer is: an installer
/// hook runs this on every install and every upgrade-reinstall, and a service
/// already registered under this name is a post-condition already met.
///
/// **Starting it here is the point of running this, not a side effect.** A
/// host that only came up once somebody signed in would be a host that cannot
/// answer while nobody is — which is the entire capability. It is safe to
/// start unasked because of ADR 0085 §2: a host with no unattended credentials
/// configured admits nobody, so a machine whose owner has not set a device
/// password gains a listening service and no new way in.
///
/// # Errors
/// A description of what the service control manager refused.
pub fn install() -> Result<(), String> {
    install_named(SERVICE_NAME)
}

/// [`install`], against an arbitrary service name.
///
/// Split out for the reason `host_role::acquire_named` is: a test must never
/// touch the service this machine actually runs.
fn install_named(name: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|error| format!("cannot locate myself: {error}"))?;
    let path = exe.to_string_lossy().into_owned();
    if path.contains('"') {
        // A Windows path cannot hold a quote; if one is here, something built
        // this string rather than the OS, and it is not going into a service's
        // binary path.
        return Err("this executable's path is not one the service manager can take".to_owned());
    }
    // The quoted form is what keeps `C:\Program Files\...` from being read as
    // a command plus arguments.
    let quoted = wide(&format!("\"{path}\""));
    let wide_name = wide(name);
    let display = wide(DISPLAY_NAME);

    // SAFETY: null-terminated wide strings that outlive the call; the handle
    // is closed on every path out.
    let manager =
        unsafe { OpenSCManagerW(None, None, SC_MANAGER_CREATE_SERVICE | SC_MANAGER_CONNECT) }
            .map_err(|error| format!("cannot reach the service manager: {error}"))?;

    // SAFETY: every pointer argument is a live, null-terminated wide string
    // owned by this frame.
    let created = unsafe {
        CreateServiceW(
            manager,
            PCWSTR(wide_name.as_ptr()),
            PCWSTR(display.as_ptr()),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            // Auto-start, because the capability is "answers before anybody
            // signs in" and a demand-started one has nobody to demand it.
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            PCWSTR(quoted.as_ptr()),
            None,
            None,
            None,
            // `None` for the account means `LocalSystem`. Required rather than
            // convenient: `WTSQueryUserToken`, which is how the agent gets the
            // signed-in user's own token, needs `SeTcbPrivilege`, and nothing
            // short of `LocalSystem` holds it (ADR 0085 §1).
            None,
            None,
        )
    };
    let result = match created {
        Ok(service) => {
            set_description(service);
            let started = start(service);
            // SAFETY: closing a handle this function opened, once.
            unsafe {
                let _ = CloseServiceHandle(service);
            }
            started
        }
        Err(error) if is_already_registered(&error) => {
            adopt_registered(manager, &wide_name, &quoted)
        }
        Err(error) => Err(format!("cannot create the service: {error}")),
    };
    // SAFETY: closing a handle this function opened, once.
    unsafe {
        let _ = CloseServiceHandle(manager);
    }
    result
}

/// Starts a freshly created service, treating "already running" as success.
fn start(service: SC_HANDLE) -> Result<(), String> {
    // SAFETY: `service` is live and owned by the caller for this call.
    let started = unsafe { StartServiceW(service, None) };
    match started {
        Ok(()) => Ok(()),
        Err(error) if is_already_running(&error) => Ok(()),
        Err(error) => Err(format!(
            "the service was created but would not start: {error}"
        )),
    }
}

/// Takes over a service already registered under `name`, and leaves it running
/// *this* binary.
///
/// The idempotent half of [`install_named`], and it rewrites the image path
/// and restarts for the two reasons `crates/service`'s own installer learned
/// the hard way: a service first registered from a development build keeps
/// launching that other executable forever, and a running service holds the
/// image it started, so replacing the file on disk changes nothing until it is
/// restarted.
///
/// The restart costs whatever a live session costs, which is more than it
/// costs the helper. It is still the right trade: a host answering the network
/// with an executable nobody installed is worse than a session that has to be
/// redialled after an upgrade.
fn adopt_registered(manager: SC_HANDLE, name: &[u16], binary_path: &[u16]) -> Result<(), String> {
    let access = SERVICE_CHANGE_CONFIG | SERVICE_QUERY_STATUS | SERVICE_START | SERVICE_STOP;
    // SAFETY: `manager` is live and was opened with `SC_MANAGER_CONNECT`;
    // `name` is a live null-terminated wide string.
    let service = unsafe { OpenServiceW(manager, PCWSTR(name.as_ptr()), access) }
        .map_err(|error| format!("the service exists but cannot be reached: {error}"))?;

    // Every field but the binary path is `SERVICE_NO_CHANGE`: this call
    // corrects where the service manager looks for the executable, it does not
    // re-decide how the service is configured.
    // SAFETY: `service` is live; `binary_path` is a null-terminated wide
    // string that outlives the call; every other pointer argument is null.
    let repointed = unsafe {
        ChangeServiceConfigW(
            service,
            ENUM_SERVICE_TYPE(SERVICE_NO_CHANGE),
            SERVICE_START_TYPE(SERVICE_NO_CHANGE),
            SERVICE_ERROR(SERVICE_NO_CHANGE),
            PCWSTR(binary_path.as_ptr()),
            PCWSTR::null(),
            None,
            PCWSTR::null(),
            PCWSTR::null(),
            PCWSTR::null(),
            PCWSTR::null(),
        )
    };
    if let Err(error) = repointed {
        tracing::warn!(%error, "cannot correct the service's registered binary path");
    }

    stop_and_wait(service);
    let result = start(service);
    // SAFETY: closing a handle this function opened, once.
    unsafe {
        let _ = CloseServiceHandle(service);
    }
    result
}

/// Asks `service` to stop and waits for it to actually be stopped, bounded by
/// [`STOP_TIMEOUT_MS`].
fn stop_and_wait(service: SC_HANDLE) {
    let mut status = SERVICE_STATUS::default();
    // SAFETY: `service` is live and `status` is an owned struct that outlives
    // the call.
    if unsafe { ControlService(service, SERVICE_CONTROL_STOP, &raw mut status) }.is_err() {
        // Already stopped, or not stoppable. Nothing to wait for.
        return;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(STOP_TIMEOUT_MS);
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(STOP_POLL_MS));
        // SAFETY: as above.
        if unsafe { QueryServiceStatus(service, &raw mut status) }.is_err() {
            return;
        }
        if status.dwCurrentState == SERVICE_STOPPED {
            return;
        }
    }
    tracing::warn!("the host service did not stop in time; starting it anyway");
}

/// Stops and removes the service. Requires administrator rights.
///
/// Removing one that is not installed succeeds: the post-condition is "this
/// machine has no Lumepeer host service".
///
/// # Errors
/// A description of what the service control manager refused.
pub fn uninstall() -> Result<(), String> {
    uninstall_named(SERVICE_NAME)
}

/// [`uninstall`], against an arbitrary service name.
fn uninstall_named(name: &str) -> Result<(), String> {
    let wide_name = wide(name);
    // SAFETY: no string arguments; the handle is closed below.
    let manager = unsafe { OpenSCManagerW(None, None, SC_MANAGER_CONNECT) }
        .map_err(|error| format!("cannot reach the service manager: {error}"))?;

    // SAFETY: `wide_name` is a live null-terminated wide string.
    let service = unsafe { OpenServiceW(manager, PCWSTR(wide_name.as_ptr()), SERVICE_ALL_ACCESS) };
    let result = match service {
        Ok(service) => {
            let mut status = SERVICE_STATUS::default();
            // SAFETY: `service` is live and `status` is an owned struct that
            // outlives the call.
            unsafe {
                let _ = ControlService(service, SERVICE_CONTROL_STOP, &raw mut status);
            }
            // SAFETY: `service` is live.
            let deleted = unsafe { DeleteService(service) };
            // SAFETY: closing a handle this function opened, once.
            unsafe {
                let _ = CloseServiceHandle(service);
            }
            match deleted {
                Ok(()) => Ok(()),
                Err(error) if is_already_removing(&error) => Ok(()),
                Err(error) => Err(format!("cannot remove the service: {error}")),
            }
        }
        // Not installed. Nothing to do, and nothing to complain about.
        Err(_) => Ok(()),
    };
    // SAFETY: closing a handle this function opened, once.
    unsafe {
        let _ = CloseServiceHandle(manager);
    }
    result
}

/// Whether `error` is `CreateServiceW`'s answer for a name already registered.
fn is_already_registered(error: &Error) -> bool {
    error.code() == HRESULT::from_win32(ERROR_SERVICE_EXISTS.0)
}

/// Whether `error` is `DeleteService`'s answer for one already being removed.
fn is_already_removing(error: &Error) -> bool {
    error.code() == HRESULT::from_win32(ERROR_SERVICE_MARKED_FOR_DELETE.0)
}

/// Whether `error` is `StartServiceW`'s answer for one already running.
fn is_already_running(error: &Error) -> bool {
    error.code() == HRESULT::from_win32(ERROR_SERVICE_ALREADY_RUNNING.0)
}

/// Sets the description shown in `services.msc`. Best effort: a service
/// without one still works.
fn set_description(service: SC_HANDLE) {
    use windows::Win32::System::Services::{ChangeServiceConfig2W, SERVICE_CONFIG_DESCRIPTION};

    let mut text = wide(DESCRIPTION);
    let description = windows::Win32::System::Services::SERVICE_DESCRIPTIONW {
        lpDescription: windows::core::PWSTR(text.as_mut_ptr()),
    };
    // SAFETY: `description` borrows `text`, which outlives the call; the API
    // copies the string it is handed.
    unsafe {
        let _ = ChangeServiceConfig2W(
            service,
            SERVICE_CONFIG_DESCRIPTION,
            Some((&raw const description).cast()),
        );
    }
}

/// A null-terminated UTF-16 copy of `text`, for the `W` entry points.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::ERROR_ACCESS_DENIED;

    /// The two services this project ships must not contend for one
    /// registration. A shared name would mean installing one silently
    /// repointed the other's image path at a binary that does something else
    /// entirely — `adopt_registered` would do exactly that, correctly, to the
    /// wrong service.
    #[test]
    fn the_two_services_have_different_names() {
        assert_ne!(SERVICE_NAME, lumepeer_service::SERVICE_NAME);
    }

    /// Removing a service that was never installed is the post-condition
    /// already met, not a failure — an uninstall hook on a machine that never
    /// had one has to succeed.
    ///
    /// Unelevated, `OpenSCManagerW` refuses before any of that is reached,
    /// which is the other correct outcome and the one a developer machine
    /// takes. Saying which happened beats a test that passes for two opposite
    /// reasons without distinguishing them.
    #[test]
    fn removing_what_was_never_installed_is_not_a_failure() {
        let name = format!("LumepeerHostTest{}", std::process::id());
        match uninstall_named(&name) {
            Ok(()) => {}
            Err(message) => {
                let refused = format!("{:?}", Error::from(ERROR_ACCESS_DENIED.to_hresult()));
                assert!(
                    message.contains("cannot reach the service manager"),
                    "unexpected refusal: {message} (an unelevated run says {refused})"
                );
            }
        }
    }
}
