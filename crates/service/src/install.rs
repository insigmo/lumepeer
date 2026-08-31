//! Registering and removing the service with the service control manager
//! (ADR 0043).
//!
//! This lives in the service binary rather than in the client for one reason:
//! creating a service needs administrator rights, so *something* has to run
//! elevated, and the smallest, most auditable thing to elevate is this binary
//! with one flag. The alternative — the client shelling out to `sc.exe`
//! through an elevated shell — means building a command line out of a path and
//! handing it to a shell, which is a quoting bug away from running whatever
//! the path says.
//!
//! One elevation prompt per action, and the elevated code is ours.

#![allow(
    unsafe_code,
    reason = "the service control manager has no safe bindings; same \
              justification standard as SendInput (ADR 0012)"
)]

use windows::Win32::Foundation::{ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_EXISTS};
use windows::Win32::System::Services::{
    CloseServiceHandle, ControlService, CreateServiceW, DeleteService, OpenSCManagerW,
    OpenServiceW, SC_HANDLE, SC_MANAGER_CONNECT, SC_MANAGER_CREATE_SERVICE, SERVICE_ALL_ACCESS,
    SERVICE_AUTO_START, SERVICE_CONTROL_STOP, SERVICE_ERROR_NORMAL, SERVICE_START, SERVICE_STATUS,
    SERVICE_WIN32_OWN_PROCESS, StartServiceW,
};
use windows::core::{Error, HRESULT, PCWSTR};

use lumepeer_service::SERVICE_NAME;

/// Shown in `services.msc`, so it has to say what it is without a manual.
const DISPLAY_NAME: &str = "Lumepeer helper";

/// Shown as the service's description, for the same reason.
const DESCRIPTION: &str = "Delivers Ctrl+Alt+Del to this computer's screen when Lumepeer's remote session asks for it. \
     Stopping this service only disables that one button.";

/// Registers the service and starts it. Requires administrator rights.
///
/// Registering a service that is already registered succeeds: the
/// post-condition is "this machine has a running lumepeer helper service",
/// not "this call created it". An app update runs the installer hook again
/// over a machine the previous version already set up, and that must not
/// fail (`docs/bugs/12-service-lifecycle.md` #2).
///
/// # Errors
/// A description of what the service control manager refused.
pub fn install() -> Result<(), String> {
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
    let name = wide(SERVICE_NAME);
    let display = wide(DISPLAY_NAME);

    // SAFETY: null-terminated wide strings that outlive the call; the handle
    // is closed on every path out.
    let manager = unsafe { OpenSCManagerW(None, None, SC_MANAGER_CREATE_SERVICE) }
        .map_err(|error| format!("cannot reach the service manager: {error}"))?;

    // SAFETY: every pointer argument is a live, null-terminated wide string
    // owned by this frame.
    let created = unsafe {
        CreateServiceW(
            manager,
            PCWSTR(name.as_ptr()),
            PCWSTR(display.as_ptr()),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            PCWSTR(quoted.as_ptr()),
            None,
            None,
            None,
            // `None` for the account means `LocalSystem`, which is what
            // session 0 SAS delivery requires. It is also the reason this
            // service does exactly one thing.
            None,
            None,
        )
    };
    let result = match created {
        Ok(service) => {
            set_description(service);
            // SAFETY: `service` is live and owned here.
            let started = unsafe { StartServiceW(service, None) };
            // SAFETY: closing a handle this function opened, once.
            unsafe {
                let _ = CloseServiceHandle(service);
            }
            started.map_err(|error| format!("the service was created but would not start: {error}"))
        }
        // Already registered, most likely by an earlier run of this same
        // installer hook. The post-condition is "running", so start it rather
        // than treating "it already exists" as a reason to fail.
        Err(error) if is_already_exists(&error) => start_existing(manager),
        Err(error) => Err(format!("cannot create the service: {error}")),
    };
    // SAFETY: closing a handle this function opened, once.
    unsafe {
        let _ = CloseServiceHandle(manager);
    }
    result
}

/// Starts a service that `CreateServiceW` refused to (re-)create because it
/// is already registered.
///
/// # Errors
/// A description of what the service control manager refused.
fn start_existing(manager: SC_HANDLE) -> Result<(), String> {
    let name = wide(SERVICE_NAME);
    // SAFETY: `name` is a live null-terminated wide string; `manager` is the
    // live handle this function was called with.
    let service = unsafe { OpenServiceW(manager, PCWSTR(name.as_ptr()), SERVICE_START) }.map_err(
        |error| format!("the service is already registered but cannot be reopened: {error}"),
    )?;
    // SAFETY: `service` is live and owned here.
    let started = unsafe { StartServiceW(service, None) };
    // SAFETY: closing a handle this function opened, once.
    unsafe {
        let _ = CloseServiceHandle(service);
    }
    match started {
        Ok(()) => Ok(()),
        // Already running is what "installed and running" looks like; it is
        // not a failure to report.
        Err(error) if is_already_running(&error) => Ok(()),
        Err(error) => Err(format!(
            "the service is registered but would not start: {error}"
        )),
    }
}

/// Whether `error` is `CreateServiceW` saying a service under this name is
/// already registered, as opposed to any other reason it refused.
fn is_already_exists(error: &Error) -> bool {
    error.code() == HRESULT::from_win32(ERROR_SERVICE_EXISTS.0)
}

/// Whether `error` is `StartServiceW` saying the service is already running.
fn is_already_running(error: &Error) -> bool {
    error.code() == HRESULT::from_win32(ERROR_SERVICE_ALREADY_RUNNING.0)
}

/// Stops and removes the service. Requires administrator rights.
///
/// Removing a service that is not installed succeeds: the post-condition is
/// "this machine has no lumepeer helper service".
///
/// # Errors
/// A description of what the service control manager refused.
pub fn uninstall() -> Result<(), String> {
    let name = wide(SERVICE_NAME);
    // SAFETY: no string arguments; the handle is closed below.
    let manager = unsafe { OpenSCManagerW(None, None, SC_MANAGER_CONNECT) }
        .map_err(|error| format!("cannot reach the service manager: {error}"))?;

    // SAFETY: `name` is a live null-terminated wide string.
    let service = unsafe { OpenServiceW(manager, PCWSTR(name.as_ptr()), SERVICE_ALL_ACCESS) };
    let result = match service {
        Ok(service) => {
            let mut status = SERVICE_STATUS::default();
            // Stopping a service that is already stopped is an error we do not
            // care about: what matters is that it is not running when it is
            // deleted.
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
            deleted.map_err(|error| format!("cannot remove the service: {error}"))
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

/// Sets the description shown in `services.msc`.
///
/// Best effort: a service without a description still works, and failing the
/// install over a cosmetic field would be the wrong trade.
fn set_description(service: windows::Win32::System::Services::SC_HANDLE) {
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

    /// The classification `install` relies on to treat "already registered"
    /// as success rather than failure. Built from a synthetic `HRESULT`, not
    /// a real service control manager call: this must hold with no
    /// administrator rights and without touching this machine's real
    /// service, whatever that is.
    #[test]
    fn recognises_already_exists_and_nothing_else() {
        let already_exists = Error::from(HRESULT::from_win32(ERROR_SERVICE_EXISTS.0));
        assert!(is_already_exists(&already_exists));

        // Access denied (5) is a different refusal and must not be treated
        // as "it's fine, it's already there" — that would hide a real
        // permission problem behind a false success.
        let access_denied = Error::from(HRESULT::from_win32(5));
        assert!(!is_already_exists(&access_denied));
    }

    /// Same shape, for the "already running" refusal `start_existing` treats
    /// as success rather than failure.
    #[test]
    fn recognises_already_running_and_nothing_else() {
        let already_running = Error::from(HRESULT::from_win32(ERROR_SERVICE_ALREADY_RUNNING.0));
        assert!(is_already_running(&already_running));

        let access_denied = Error::from(HRESULT::from_win32(5));
        assert!(!is_already_running(&access_denied));
    }

    /// `install`/`uninstall`, end to end, against the real service control
    /// manager: called twice each, which is what
    /// `docs/bugs/12-service-lifecycle.md` #2 requires ("install on an
    /// already-installed service and uninstall on an absent one must both
    /// succeed").
    ///
    /// Deliberately **not** run by default. This registers and removes a
    /// real `LocalSystem` service under administrator rights, which
    /// `cargo test --workspace` must never do to a machine that already
    /// depends on the real `LumepeerHelper` for Ctrl+Alt+Del — including a
    /// contributor's own dev machine. Opt in with
    /// `LUMEPEER_TEST_SERVICE_INSTALL=1`, run elevated, on a disposable VM;
    /// the same convention `LUMEPEER_TEST_XTEST` uses for the X11 tests that
    /// need a real display.
    #[test]
    #[ignore = "registers/removes the real Windows service; opt in explicitly, never on a dev machine"]
    fn install_and_uninstall_are_each_idempotent() {
        if std::env::var_os("LUMEPEER_TEST_SERVICE_INSTALL").is_none() {
            return;
        }
        assert!(install().is_ok(), "installing a fresh service must succeed");
        assert!(
            install().is_ok(),
            "installing an already-installed service must succeed"
        );
        assert!(
            uninstall().is_ok(),
            "removing an installed service must succeed"
        );
        assert!(
            uninstall().is_ok(),
            "removing an already-absent service must succeed"
        );
    }
}
