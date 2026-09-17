//! Handing this process to the service control manager (ADR 0085 §1).
//!
//! The same lifecycle `crates/service/src/windows_service.rs` runs for the
//! helper, and a second copy for the reason `install.rs`'s header gives.
//!
//! One thing genuinely differs, and it is worth stating rather than leaving to
//! be noticed: **the helper needs waking and this does not.** The helper's
//! accept loop blocks inside `ConnectNamedPipe`, which no flag can interrupt,
//! so its control handler has to connect to its own pipe to break the wait.
//! This service's loop is a supervision tick that already wakes on its own
//! every [`crate::host::SUPERVISION_TICK`] to notice a session that appeared
//! or an agent that died; reading one more `AtomicBool` on that tick costs
//! nothing and needs no second mechanism. The price is that a stop takes up to
//! one tick to be acted on, which is well inside the SCM's patience.
//!
//! **Session changes are the one control that carries data** (ADR 0088 §2).
//! `SERVICE_CONTROL_SESSIONCHANGE` is only delivered to a handler registered
//! with `RegisterServiceCtrlHandlerExW`, and only to a service that says it
//! accepts it, so this module does both. The handler does no work with what it
//! is told: it parses the event into a
//! [`SessionChange`](lumepeer_service::session_change::SessionChange) and
//! queues it, and the supervision loop drains the queue on its own thread. A
//! control handler that tore down an attachment itself would be doing
//! blocking pipe and process work on the SCM's dispatcher thread.

#![allow(
    unsafe_code,
    reason = "the service control manager has no safe bindings; same \
              justification standard as crates/service's own dispatcher \
              (ADR 0043)"
)]

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use lumepeer_service::session_change::{SessionChange, from_wts_event};
use windows::Win32::System::RemoteDesktop::WTSSESSION_NOTIFICATION;
use windows::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SERVICE_ACCEPT_SESSIONCHANGE, SERVICE_ACCEPT_STOP,
    SERVICE_CONTROL_SESSIONCHANGE, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING,
    SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STATUS_HANDLE, SERVICE_STOP_PENDING,
    SERVICE_STOPPED, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS, SetServiceStatus,
    StartServiceCtrlDispatcherW,
};
use windows::core::{PCWSTR, PWSTR};

/// Name this service is registered under with the service control manager.
///
/// Deliberately not `lumepeer_service::SERVICE_NAME`: ADR 0085 §1 makes these
/// two different services with different threat models, and one name would
/// mean installing either of them repointed the other.
pub const SERVICE_NAME: &str = "LumepeerHost";

/// Set by the SCM control handler; read by the supervision loop on its tick.
static STOPPING: AtomicBool = AtomicBool::new(false);

/// Session changes the SCM reported and the supervision loop has not read yet.
///
/// A queue rather than a latest-value slot: a fast user switch is a disconnect
/// followed by a connect, and a loop that only saw the second would never have
/// ended the attachment the first one was about.
static SESSION_CHANGES: Mutex<Vec<SessionChange>> = Mutex::new(Vec::new());

/// Every session change reported since the last call, oldest first.
///
/// Always empty under `--console`, which has no SCM to report anything: that
/// run falls back to noticing a changed console session on its own tick, as
/// the host did before ADR 0088.
pub fn take_session_changes() -> Vec<SessionChange> {
    std::mem::take(
        &mut *SESSION_CHANGES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

/// Handle the control handler reports status through, as a pointer-sized value
/// because `SERVICE_STATUS_HANDLE` is not `Sync`.
static STATUS_HANDLE: AtomicUsize = AtomicUsize::new(0);

/// How long the SCM is asked to wait between stop checkpoints, in
/// milliseconds.
///
/// A host's stop is not instant: it ends live sessions, tells the agent to go
/// away and lets the endpoint close. This is the hint that keeps the SCM from
/// calling a stop that is progressing a hung one, and it is comfortably longer
/// than one supervision tick.
const STOP_WAIT_HINT_MS: u32 = 15_000;

/// Hands this process to the service control manager.
///
/// Returns only when the service has stopped, or immediately when the process
/// was not started by the SCM at all — which is what happens if somebody runs
/// the binary by hand without `--console`.
pub fn dispatch() {
    let mut name = wide(SERVICE_NAME);
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(name.as_mut_ptr()),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    // SAFETY: the table is a null-terminated array of entries that outlives
    // the call — `StartServiceCtrlDispatcherW` blocks until the service ends.
    // Failure means this process was not started as a service, which is
    // reported rather than treated as an error worth retrying.
    let started = unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) };
    if started.is_err() {
        eprintln!(
            "lumepeer-host was not started by the service control manager. Install it with \
             --install, or run it with --console to exercise everything below the privilege \
             line without registering anything."
        );
        std::process::exit(1);
    }
}

/// The SCM's entry point for this service.
extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let mut name = wide(SERVICE_NAME);
    // SAFETY: `name` is a null-terminated wide string that outlives the call;
    // the handler is a plain `extern "system"` function with no state of its
    // own beyond the statics above, and it reads no context pointer.
    let handle =
        unsafe { RegisterServiceCtrlHandlerExW(PCWSTR(name.as_mut_ptr()), Some(handler), None) };
    let Ok(handle) = handle else {
        return;
    };
    STATUS_HANDLE.store(handle.0 as usize, Ordering::SeqCst);

    report(handle, SERVICE_START_PENDING, 0, STOP_WAIT_HINT_MS);
    report(
        handle,
        SERVICE_RUNNING,
        SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SESSIONCHANGE,
        0,
    );
    tracing::info!("lumepeer host service running");

    crate::host::run(&STOPPING);

    report(handle, SERVICE_STOPPED, 0, 0);
    tracing::info!("lumepeer host service stopped");
}

/// Win32's `NO_ERROR`, the only answer this handler gives.
const NO_ERROR: u32 = 0;

/// The SCM's control callback.
///
/// Stop and shutdown mean the same thing. A session change is parsed and
/// queued, and nothing else — see the module header.
extern "system" fn handler(
    control: u32,
    event_type: u32,
    event_data: *mut core::ffi::c_void,
    _context: *mut core::ffi::c_void,
) -> u32 {
    if control == SERVICE_CONTROL_SESSIONCHANGE {
        if event_data.is_null() {
            return NO_ERROR;
        }
        // SAFETY: for `SERVICE_CONTROL_SESSIONCHANGE` the SCM documents
        // `event_data` as a `WTSSESSION_NOTIFICATION` valid for the duration of
        // this call; it was checked for null above and is copied out, not kept.
        let notification = unsafe {
            event_data
                .cast::<WTSSESSION_NOTIFICATION>()
                .read_unaligned()
        };
        if let Some(change) = from_wts_event(event_type, notification.dwSessionId) {
            SESSION_CHANGES
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(change);
        }
        return NO_ERROR;
    }
    if control == SERVICE_CONTROL_STOP || control == SERVICE_CONTROL_SHUTDOWN {
        STOPPING.store(true, Ordering::SeqCst);
        let raw = STATUS_HANDLE.load(Ordering::SeqCst);
        if raw != 0 {
            report(
                SERVICE_STATUS_HANDLE(raw as *mut core::ffi::c_void),
                SERVICE_STOP_PENDING,
                0,
                STOP_WAIT_HINT_MS,
            );
        }
        // No wake-up call here, unlike the helper's handler: see the module
        // header. The supervision loop reads `STOPPING` on a tick it takes
        // anyway.
    }
    NO_ERROR
}

/// Tells the SCM where the service is in its lifecycle.
fn report(
    handle: SERVICE_STATUS_HANDLE,
    state: windows::Win32::System::Services::SERVICE_STATUS_CURRENT_STATE,
    accepted: u32,
    wait_hint_ms: u32,
) {
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: accepted,
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: wait_hint_ms,
    };
    // SAFETY: `handle` came from `RegisterServiceCtrlHandlerW` and `status` is
    // a fully initialized owned struct that outlives the call.
    unsafe {
        let _ = SetServiceStatus(handle, &raw const status);
    }
}

/// A null-terminated UTF-16 copy of `text`, for the `W` entry points.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}
