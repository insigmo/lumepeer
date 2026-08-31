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

use windows::Win32::Foundation::{
    ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_EXISTS, ERROR_SERVICE_MARKED_FOR_DELETE,
};
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
/// Idempotent: a service already registered under this name is a
/// post-condition already met, not a failure (docs/bugs/
/// 12-service-lifecycle.md task 2) — the NSIS installer hook calls this on
/// every install *and* every upgrade-reinstall, and the settings panel's own
/// Install button can be clicked again after it already succeeded.
///
/// # Errors
/// A description of what the service control manager refused.
pub fn install() -> Result<(), String> {
    install_named(SERVICE_NAME)
}

/// [`install`], against an arbitrary service name.
///
/// Split out so a test can exercise the exact same code path against a
/// throwaway name instead of the real `SERVICE_NAME` — this machine may
/// already have that one installed and running, and a test must never touch
/// it.
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

    // `SC_MANAGER_CONNECT` in addition to `_CREATE_SERVICE`: the idempotent
    // path below re-opens an already-registered service on this same handle
    // rather than creating a second one.
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
            let started = start(service);
            // SAFETY: closing a handle this function opened, once.
            unsafe {
                let _ = CloseServiceHandle(service);
            }
            started
        }
        // Already registered — by a previous run of this same hook, or a
        // second click of the settings panel's button. The post-condition
        // `install()` promises ("a Lumepeer helper service is registered and
        // running") already holds on the registration half; make sure it
        // holds on the running half too instead of reporting failure.
        Err(error) if is_already_registered(&error) => start_registered(manager, &wide_name),
        Err(error) => Err(format!("cannot create the service: {error}")),
    };
    // SAFETY: closing a handle this function opened, once.
    unsafe {
        let _ = CloseServiceHandle(manager);
    }
    result
}

/// Starts a freshly created service, treating "already running" as success
/// rather than a fault — it cannot normally happen right after creation, but
/// there is no reason for this call to know that and every reason for it to
/// share the same rule [`start_registered`] needs.
fn start(service: windows::Win32::System::Services::SC_HANDLE) -> Result<(), String> {
    // SAFETY: `service` is live and owned by the caller for the duration of
    // this call.
    let started = unsafe { StartServiceW(service, None) };
    match started {
        Ok(()) => Ok(()),
        Err(error) if is_already_running(&error) => Ok(()),
        Err(error) => Err(format!(
            "the service was created but would not start: {error}"
        )),
    }
}

/// Starts a service `install_named` found already registered under `name`.
///
/// This is the idempotent half of `install()`: a second `--install` must
/// succeed, and "succeed" means the service ends up running, not merely that
/// the call did not error.
fn start_registered(manager: SC_HANDLE, name: &[u16]) -> Result<(), String> {
    // SAFETY: `manager` is live and was opened with `SC_MANAGER_CONNECT`;
    // `name` is a live null-terminated wide string.
    let service = unsafe { OpenServiceW(manager, PCWSTR(name.as_ptr()), SERVICE_START) }
        .map_err(|error| format!("the service exists but cannot be reached: {error}"))?;
    let result = start(service);
    // SAFETY: closing a handle this function opened, once.
    unsafe {
        let _ = CloseServiceHandle(service);
    }
    result
}

/// Stops and removes the service. Requires administrator rights.
///
/// Removing a service that is not installed succeeds: the post-condition is
/// "this machine has no lumepeer helper service". Idempotent for the same
/// reason `install()` is (docs/bugs/12-service-lifecycle.md task 2): calling
/// this twice in a row — an uninstall hook running on a machine that never
/// had the service, or a second click of the settings panel's Remove button
/// while the first delete is still draining — must succeed both times.
///
/// # Errors
/// A description of what the service control manager refused.
pub fn uninstall() -> Result<(), String> {
    uninstall_named(SERVICE_NAME)
}

/// [`uninstall`], against an arbitrary service name — see [`install_named`]
/// for why tests need this split.
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
            match deleted {
                Ok(()) => Ok(()),
                // A previous call already asked the SCM to delete this
                // service and it has not finished draining yet (every open
                // handle has to close first). That deletion is exactly what
                // this call is also asking for, so it is success, not a
                // second failure.
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

/// Whether `error` is the specific answer `CreateServiceW` gives when a
/// service by the requested name is already registered.
fn is_already_registered(error: &Error) -> bool {
    error.code() == HRESULT::from_win32(ERROR_SERVICE_EXISTS.0)
}

/// Whether `error` is the specific answer `DeleteService` gives for a
/// service that is already marked for deletion.
fn is_already_removing(error: &Error) -> bool {
    error.code() == HRESULT::from_win32(ERROR_SERVICE_MARKED_FOR_DELETE.0)
}

/// Whether `error` is the specific answer `StartServiceW` gives for a service
/// that is already running.
fn is_already_running(error: &Error) -> bool {
    error.code() == HRESULT::from_win32(ERROR_SERVICE_ALREADY_RUNNING.0)
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
    use windows::Win32::Foundation::ERROR_ACCESS_DENIED;

    /// Builds the exact `Error` the real Win32 call would produce for `code`,
    /// with no service, no elevation and no live system involved: these
    /// three predicates are the whole idempotency fix (docs/bugs/
    /// 12-service-lifecycle.md task 2), and they must be verifiable by every
    /// `cargo test --workspace`, not only from an elevated shell against a
    /// disposable service (see `install_and_uninstall_are_idempotent` below
    /// for that one).
    fn win32_error(code: windows::Win32::Foundation::WIN32_ERROR) -> Error {
        Error::from_hresult(HRESULT::from_win32(code.0))
    }

    #[test]
    fn already_registered_is_recognized_and_nothing_else_is() {
        assert!(is_already_registered(&win32_error(ERROR_SERVICE_EXISTS)));
        assert!(!is_already_registered(&win32_error(
            ERROR_SERVICE_MARKED_FOR_DELETE
        )));
        assert!(!is_already_registered(&win32_error(ERROR_ACCESS_DENIED)));
    }

    #[test]
    fn already_removing_is_recognized_and_nothing_else_is() {
        assert!(is_already_removing(&win32_error(
            ERROR_SERVICE_MARKED_FOR_DELETE
        )));
        assert!(!is_already_removing(&win32_error(ERROR_SERVICE_EXISTS)));
        assert!(!is_already_removing(&win32_error(ERROR_ACCESS_DENIED)));
    }

    #[test]
    fn already_running_is_recognized_and_nothing_else_is() {
        assert!(is_already_running(&win32_error(
            ERROR_SERVICE_ALREADY_RUNNING
        )));
        assert!(!is_already_running(&win32_error(ERROR_SERVICE_EXISTS)));
        assert!(!is_already_running(&win32_error(ERROR_ACCESS_DENIED)));
    }

    /// The real thing, end to end, against a disposable service name that is
    /// never `SERVICE_NAME` — this machine may have the real `LumepeerHelper`
    /// installed and running from prior work, and this test must never touch
    /// it.
    ///
    /// Registering a service needs administrator rights the same way
    /// `install()`/`uninstall()` always have, so this is opt-in the same way
    /// `lumepeer-media`'s `LUMEPEER_TEST_XTEST` is: a plain `cargo test` must
    /// not need an elevated shell, and without one this would fail on
    /// `OpenSCManagerW`'s `SC_MANAGER_CREATE_SERVICE` request rather than
    /// testing anything about idempotency.
    ///
    /// The service this creates points at the *test harness's own*
    /// executable, which never calls `StartServiceCtrlDispatcherW`, so
    /// `StartServiceW` cannot succeed here — that half of `install_named` is
    /// exercised for real by `crates/service/tests/endpoint.rs` against the
    /// actual `lumepeer-service` binary instead. What this checks is the
    /// half task 2 is actually about: a second `install_named` must not fail
    /// with "cannot create the service" (`ERROR_SERVICE_EXISTS`, unhandled),
    /// and a second `uninstall_named` must not fail with "cannot remove the
    /// service" (`ERROR_SERVICE_MARKED_FOR_DELETE`, unhandled) — the two
    /// messages this file's `Err` arms produced before this fix.
    #[test]
    fn install_and_uninstall_are_idempotent() {
        if std::env::var_os("LUMEPEER_TEST_SERVICE_INSTALL").is_none() {
            eprintln!(
                "skipping install_and_uninstall_are_idempotent: set \
                 LUMEPEER_TEST_SERVICE_INSTALL=1 in an elevated shell to run it"
            );
            return;
        }
        let name = format!("LumepeerHelperTest{}", std::process::id());

        let first_install = install_named(&name);
        let second_install = install_named(&name);
        // Cleanup first so a failed assertion below does not leave the test
        // service registered.
        let first_uninstall = uninstall_named(&name);
        let second_uninstall = uninstall_named(&name);

        for (label, result) in [
            ("first install", &first_install),
            ("second install", &second_install),
        ] {
            if let Err(message) = result {
                assert!(
                    !message.contains("cannot create the service"),
                    "{label} hit the pre-fix failure mode: {message}"
                );
            }
        }
        for (label, result) in [
            ("first uninstall", &first_uninstall),
            ("second uninstall", &second_uninstall),
        ] {
            assert!(result.is_ok(), "{label} must succeed: {result:?}");
        }
    }
}
