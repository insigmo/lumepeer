//! The `--run` controller and the `--service` body (throwaway).
//!
//! `--run` (elevated, the owner's one UAC click) installs a `LocalSystem`
//! service pointing at this binary with `--service`, starts it, waits for it
//! to stop, then removes it. The service — now `LocalSystem` in session 0 —
//! launches `--measure` into the console session on `WinSta0\Default` and
//! waits for it, exactly the path `system_injector_launch.rs` uses.

#![allow(unsafe_code)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::ERROR_SERVICE_EXISTS;
use windows::Win32::System::Services::{
    CloseServiceHandle, ControlService, CreateServiceW, DeleteService, OpenSCManagerW,
    OpenServiceW, QueryServiceStatus, RegisterServiceCtrlHandlerW, SC_HANDLE, SC_MANAGER_CONNECT,
    SC_MANAGER_CREATE_SERVICE, SERVICE_ACCEPT_STOP, SERVICE_ALL_ACCESS, SERVICE_CONTROL_SHUTDOWN,
    SERVICE_CONTROL_STOP, SERVICE_DEMAND_START, SERVICE_ERROR_NORMAL, SERVICE_RUNNING,
    SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_HANDLE,
    SERVICE_STOPPED, SERVICE_STOP_PENDING, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
    SetServiceStatus, StartServiceCtrlDispatcherW,
};
use windows::core::{Error, PCWSTR, PWSTR};

use crate::win;

/// Service name — distinct from `LumepeerHelper`, its own pipe-less service.
pub const SERVICE_NAME: &str = "LumepeerSessionProbe";
const DISPLAY_NAME: &str = "Lumepeer session probe (temporary, pack 01)";

/// The arg the service re-launches this binary with, into the user's session.
const MEASURE_ARG: &str = "--measure";

/// How long the service waits for the measure child (the owner interacts).
const MEASURE_TIMEOUT_MS: u32 = 20 * 60 * 1000;

/// How long `--run` waits for the service to stop before forcing cleanup.
const RUN_POLL_TIMEOUT: Duration = Duration::from_secs(22 * 60);

static STOPPING: AtomicBool = AtomicBool::new(false);
static STATUS_HANDLE: AtomicUsize = AtomicUsize::new(0);

fn log(line: &str) {
    println!("{line}");
    win::append(&win::service_log_path(), line);
}

// ---- `--run` controller (elevated admin) -----------------------------------

/// Installs, starts, waits for, and removes the probe service. Returns the
/// measure report path on success.
pub fn run_controller() -> Result<(), String> {
    win::append(
        &win::service_log_path(),
        &format!("=== --run started {} ===", now()),
    );
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate myself: {e}"))?;
    let path = exe.to_string_lossy().into_owned();
    if path.contains('"') {
        return Err("my own path contains a quote; refusing".into());
    }
    let bin_path = format!("\"{path}\" --service");

    // SAFETY: SCM handles are closed on every path out.
    let manager = unsafe { OpenSCManagerW(None, None, SC_MANAGER_CREATE_SERVICE | SC_MANAGER_CONNECT) }
        .map_err(|e| format!("cannot reach the service manager (run elevated): {e}"))?;

    let service = create_or_open(manager, &bin_path)?;
    log("service registered; starting it");

    // SAFETY: `service` is a live handle; no start arguments.
    if let Err(e) = unsafe { windows::Win32::System::Services::StartServiceW(service, None) } {
        // ERROR_SERVICE_ALREADY_RUNNING would be fine, but a fresh demand-start
        // should not be running yet; report anything else.
        log(&format!("StartServiceW: {e}"));
    } else {
        log("service started; the measure process is running in your session");
    }

    let outcome = wait_until_stopped(service);
    cleanup(manager, service);
    log(&format!("=== --run finished {} ({outcome}) ===", now()));
    Ok(())
}

fn create_or_open(manager: SC_HANDLE, bin_path: &str) -> Result<SC_HANDLE, String> {
    let name = win::wide(SERVICE_NAME);
    let display = win::wide(DISPLAY_NAME);
    let quoted = win::wide(bin_path);
    // SAFETY: every pointer is a live null-terminated wide string of this frame.
    let created = unsafe {
        CreateServiceW(
            manager,
            PCWSTR(name.as_ptr()),
            PCWSTR(display.as_ptr()),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_DEMAND_START,
            SERVICE_ERROR_NORMAL,
            PCWSTR(quoted.as_ptr()),
            None,
            None,
            None,
            None, // account None => LocalSystem
            None,
        )
    };
    match created {
        Ok(h) => Ok(h),
        Err(e) if e.code() == Error::from(ERROR_SERVICE_EXISTS).code() => {
            log("a probe service was already registered; reusing it");
            // SAFETY: live manager handle; `name` outlives the call.
            unsafe { OpenServiceW(manager, PCWSTR(name.as_ptr()), SERVICE_ALL_ACCESS) }
                .map_err(|e| format!("cannot open the existing service: {e}"))
        }
        Err(e) => Err(format!("cannot register the service: {e}")),
    }
}

fn wait_until_stopped(service: SC_HANDLE) -> &'static str {
    let start = Instant::now();
    let mut announced_running = false;
    loop {
        match query_state(service) {
            Some(s) if s == SERVICE_STOPPED => return "service stopped",
            Some(s) if s == SERVICE_RUNNING && !announced_running => {
                announced_running = true;
                log("service is running; waiting for the measurements to finish…");
            }
            _ => {}
        }
        if start.elapsed() > RUN_POLL_TIMEOUT {
            return "timed out waiting for the service to stop";
        }
        sleep(Duration::from_secs(2));
    }
}

fn query_state(service: SC_HANDLE) -> Option<SERVICE_STATUS_CURRENT_STATE> {
    let mut status = SERVICE_STATUS::default();
    // SAFETY: `service` live, `status` outlives the call.
    unsafe { QueryServiceStatus(service, &raw mut status) }
        .ok()
        .map(|()| status.dwCurrentState)
}

fn cleanup(manager: SC_HANDLE, service: SC_HANDLE) {
    if matches!(query_state(service), Some(s) if s != SERVICE_STOPPED) {
        let mut status = SERVICE_STATUS::default();
        // SAFETY: live handle; `status` outlives the call.
        let _ = unsafe { ControlService(service, SERVICE_CONTROL_STOP, &raw mut status) };
        for _ in 0..40 {
            if matches!(query_state(service), Some(s) if s == SERVICE_STOPPED) {
                break;
            }
            sleep(Duration::from_millis(250));
        }
    }
    // SAFETY: deleting and closing handles this function owns.
    unsafe {
        let _ = DeleteService(service);
        let _ = CloseServiceHandle(service);
        let _ = CloseServiceHandle(manager);
    }
    log("service removed");
}

// ---- `--service` body (LocalSystem, session 0) -----------------------------

/// Hands this process to the SCM; `service_main` does the work.
pub fn dispatch() {
    let mut name = win::wide(SERVICE_NAME);
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(name.as_mut_ptr()),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    // SAFETY: null-terminated table that outlives the blocking dispatcher call.
    if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) }.is_err() {
        eprintln!("not started by the SCM; run with --run (elevated) instead");
        std::process::exit(1);
    }
}

extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let mut name = win::wide(SERVICE_NAME);
    // SAFETY: `name` outlives the call; `handler` is a plain extern fn.
    let Ok(handle) = (unsafe { RegisterServiceCtrlHandlerW(PCWSTR(name.as_mut_ptr()), Some(handler)) })
    else {
        return;
    };
    STATUS_HANDLE.store(handle.0 as usize, Ordering::SeqCst);
    report(handle, SERVICE_START_PENDING, 0);
    report(handle, SERVICE_RUNNING, SERVICE_ACCEPT_STOP);
    win::append(
        &win::service_log_path(),
        &format!("service_main running {}", now()),
    );

    launch_measure();

    STOPPING.store(true, Ordering::SeqCst);
    report(handle, SERVICE_STOPPED, 0);
}

/// Launches `--measure` into the console session on `WinSta0\Default` and
/// waits for it.
fn launch_measure() {
    let log = win::service_log_path();
    let Some(session) = win::console_session() else {
        win::append(&log, "no active console session; cannot launch measure");
        return;
    };
    let Ok(exe) = std::env::current_exe() else {
        win::append(&log, "cannot locate myself");
        return;
    };
    let path = exe.to_string_lossy().into_owned();
    if path.contains('"') {
        win::append(&log, "my path contains a quote; refusing");
        return;
    }
    let Some(token) = win::duplicate_own_token_for_session(session) else {
        win::append(&log, "cannot duplicate my LocalSystem token into the session");
        return;
    };
    let cmd = format!("\"{path}\" {MEASURE_ARG}");
    let Some(process) = win::create_process_as_user(token, &cmd, r"WinSta0\Default") else {
        // SAFETY: closing the token this scope owns.
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(token);
        }
        win::append(&log, "CreateProcessAsUser refused the measure launch");
        return;
    };
    // SAFETY: closing the token after the spawn copied what it needed.
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(token);
    }
    win::append(
        &log,
        &format!("measure launched into session {session} on WinSta0\\Default"),
    );
    let code = win::wait_and_exit_code(process, MEASURE_TIMEOUT_MS);
    win::append(&log, &format!("measure exited: {code:?}"));
}

extern "system" fn handler(control: u32) {
    if control == SERVICE_CONTROL_STOP || control == SERVICE_CONTROL_SHUTDOWN {
        STOPPING.store(true, Ordering::SeqCst);
        let raw = STATUS_HANDLE.load(Ordering::SeqCst);
        if raw != 0 {
            report(
                SERVICE_STATUS_HANDLE(raw as *mut core::ffi::c_void),
                SERVICE_STOP_PENDING,
                0,
            );
        }
    }
}

fn report(handle: SERVICE_STATUS_HANDLE, state: SERVICE_STATUS_CURRENT_STATE, accepted: u32) {
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: accepted,
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: 0,
    };
    // SAFETY: `handle` from RegisterServiceCtrlHandlerW; `status` outlives call.
    unsafe {
        let _ = SetServiceStatus(handle, &raw const status);
    }
}

fn now() -> String {
    // Coarse wall clock without a date dependency: seconds since the epoch.
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| format!("t+{}s", d.as_secs()))
        .unwrap_or_else(|_| "t?".into())
}
