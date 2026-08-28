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

use windows::Win32::System::Services::{
    CloseServiceHandle, ControlService, CreateServiceW, DeleteService, OpenSCManagerW,
    OpenServiceW, SC_MANAGER_CONNECT, SC_MANAGER_CREATE_SERVICE, SERVICE_ALL_ACCESS,
    SERVICE_AUTO_START, SERVICE_CONTROL_STOP, SERVICE_ERROR_NORMAL, SERVICE_STATUS,
    SERVICE_WIN32_OWN_PROCESS, StartServiceW,
};
use windows::core::PCWSTR;

use lumepeer_service::SERVICE_NAME;

/// Shown in `services.msc`, so it has to say what it is without a manual.
const DISPLAY_NAME: &str = "Lumepeer helper";

/// Shown as the service's description, for the same reason.
const DESCRIPTION: &str = "Delivers Ctrl+Alt+Del to this computer's screen when Lumepeer's remote session asks for it. \
     Stopping this service only disables that one button.";

/// Registers the service and starts it. Requires administrator rights.
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
        Err(error) => Err(format!("cannot create the service: {error}")),
    };
    // SAFETY: closing a handle this function opened, once.
    unsafe {
        let _ = CloseServiceHandle(manager);
    }
    result
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
