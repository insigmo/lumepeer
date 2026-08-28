//! The Windows half of the helper service (ADR 0043).
//!
//! Every `unsafe` block here is a raw Win32 entry point with no safe binding,
//! and each carries the justification ADR 0012 set for `SendInput`. There is
//! no safe way to become a Windows service — `StartServiceCtrlDispatcherW` is
//! the mechanism — and no safe way to create a named pipe with a DACL.
//!
//! The listener is deliberately dull: one connection at a time, a fixed-size
//! read, one match, a fixed-size write, disconnect. Nothing here allocates on
//! behalf of the caller and nothing here parses anything.

#![allow(
    unsafe_code,
    reason = "a Windows service and a DACL'd named pipe have no safe bindings; \
              same justification standard as SendInput (ADR 0012)"
)]

use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authentication::Identity::SendSAS;
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAGS_AND_ATTRIBUTES, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Services::{
    RegisterServiceCtrlHandlerW, SERVICE_ACCEPT_STOP, SERVICE_CONTROL_SHUTDOWN,
    SERVICE_CONTROL_STOP, SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATUS,
    SERVICE_STATUS_HANDLE, SERVICE_STOP_PENDING, SERVICE_STOPPED, SERVICE_TABLE_ENTRYW,
    SERVICE_WIN32_OWN_PROCESS, SetServiceStatus, StartServiceCtrlDispatcherW,
};
use windows::core::{PCWSTR, PWSTR};

use lumepeer_service::SERVICE_NAME;
use lumepeer_service::protocol::{
    ENDPOINT, FRAME_LEN, OP_DELIVER_SAS, STATUS_OK, STATUS_REFUSED, parse_request, response,
};

/// Who may open the pipe, in SDDL.
///
/// - `SY` — `LocalSystem`, which is what the service itself runs as.
/// - `BA` — the built-in administrators group.
/// - `IU` — **interactive** users: somebody signed in at this machine. This is
///   the grant that matters, and its narrowness is the point. A network logon,
///   a service account and a scheduled task running as another user are all
///   outside it, so the only thing that can ask for a Ctrl+Alt+Del is a
///   process belonging to the person sitting in front of the screen that would
///   receive it.
///
/// `0x0012019b` is `FILE_GENERIC_READ | FILE_GENERIC_WRITE` for a pipe: read
/// and write the two frames, and nothing else — no `WRITE_DAC`, so a client
/// cannot widen this after the fact.
const PIPE_SDDL: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x0012019b;;;IU)";

/// Buffer sizes of the pipe. Two bytes is the whole protocol; the rest is the
/// kernel's minimum granularity, not headroom anyone asked for.
const PIPE_BUFFER_BYTES: u32 = 64;

/// How long a client may leave a connection half-finished before the service
/// gives up on it and goes back to waiting, in milliseconds.
///
/// Nothing legitimate takes this long: the client writes two bytes as soon as
/// it connects. It exists so one stuck client cannot hold the single-instance
/// pipe forever.
const PIPE_TIMEOUT_MS: u32 = 5_000;

/// Set by the SCM control handler; read by the accept loop between clients.
static STOPPING: AtomicBool = AtomicBool::new(false);

/// Handle the control handler reports status through, as a raw pointer-sized
/// value because `SERVICE_STATUS_HANDLE` is not `Sync`.
static STATUS_HANDLE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

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
            "lumepeer-service was not started by the service control manager. \
             Install it with the client's own settings panel, or run it with \
             --console to exercise the endpoint without registering anything."
        );
        std::process::exit(1);
    }
}

/// The SCM's entry point for this service.
extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let mut name = wide(SERVICE_NAME);
    // SAFETY: `name` is a null-terminated wide string that outlives the call;
    // the handler is a plain `extern "system"` function with no state of its
    // own beyond the two statics above.
    let handle = unsafe { RegisterServiceCtrlHandlerW(PCWSTR(name.as_mut_ptr()), Some(handler)) };
    let Ok(handle) = handle else {
        return;
    };
    STATUS_HANDLE.store(handle.0 as usize, Ordering::SeqCst);

    report(handle, SERVICE_START_PENDING, 0);
    report(handle, SERVICE_RUNNING, SERVICE_ACCEPT_STOP);
    tracing::info!("lumepeer helper service running");

    serve_until_stopped(&STOPPING);

    report(handle, SERVICE_STOPPED, 0);
    tracing::info!("lumepeer helper service stopped");
}

/// The SCM's control callback. Stop and shutdown are the only controls
/// accepted, and both mean the same thing.
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
        // The accept loop is blocked inside `ConnectNamedPipe`, which no flag
        // can interrupt. Connecting to our own pipe wakes it; it then sees the
        // flag and returns instead of serving the connection. This is an
        // ordinary `CreateFileW` through the standard library, so it needs no
        // `unsafe` of its own.
        let _ = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(ENDPOINT);
    }
}

/// Tells the SCM where the service is in its lifecycle.
fn report(
    handle: SERVICE_STATUS_HANDLE,
    state: windows::Win32::System::Services::SERVICE_STATUS_CURRENT_STATE,
    accepted: u32,
) {
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: accepted,
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: 0,
    };
    // SAFETY: `handle` came from `RegisterServiceCtrlHandlerW` and `status` is
    // a fully initialized owned struct that outlives the call.
    unsafe {
        let _ = SetServiceStatus(handle, &raw const status);
    }
}

/// Serves one client at a time until `stopping` is set.
///
/// Shared by the service path and `--console`, so the two cannot drift: the
/// only difference between them is which privileges `SendSAS` runs with.
pub fn serve_until_stopped(stopping: &AtomicBool) {
    loop {
        let Some(pipe) = create_pipe() else {
            tracing::error!("cannot create the service endpoint; giving up");
            return;
        };
        let served = accept_and_serve(pipe, stopping);
        // SAFETY: `pipe` came from `CreateNamedPipeW` above and is not used
        // again after this call.
        unsafe {
            let _ = CloseHandle(pipe);
        }
        if !served || stopping.load(Ordering::SeqCst) {
            return;
        }
    }
}

/// Creates the single-instance pipe with [`PIPE_SDDL`] on it.
fn create_pipe() -> Option<HANDLE> {
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    let sddl = wide(PIPE_SDDL);
    // SAFETY: `sddl` is a null-terminated wide string that outlives the call;
    // the descriptor it allocates is released with `LocalFree` below, on every
    // path out of this function.
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &raw mut descriptor,
            None,
        )
    };
    if converted.is_err() {
        tracing::error!("cannot build the endpoint's access list");
        return None;
    }

    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: false.into(),
    };
    let name = wide(ENDPOINT);
    // SAFETY: `name` and `attributes` outlive the call. `PIPE_REJECT_REMOTE
    // _CLIENTS` is what keeps this endpoint off the network entirely; one
    // instance means a second listener cannot be squatted alongside it.
    let pipe = unsafe {
        CreateNamedPipeW(
            PCWSTR(name.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(PIPE_ACCESS_DUPLEX.0),
            PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            PIPE_BUFFER_BYTES,
            PIPE_BUFFER_BYTES,
            PIPE_TIMEOUT_MS,
            Some(&raw const attributes),
        )
    };
    // SAFETY: `descriptor.0` came from the conversion above and is not used
    // again; the pipe holds its own copy of the descriptor by this point.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
    }

    if pipe.is_invalid() {
        tracing::error!("cannot create the service endpoint");
        return None;
    }
    Some(pipe)
}

/// Waits for one client, answers it, and disconnects.
///
/// Returns `false` when the endpoint itself failed, which ends the loop; a
/// client that misbehaves only ends its own connection.
fn accept_and_serve(pipe: HANDLE, stopping: &AtomicBool) -> bool {
    // SAFETY: `pipe` is a live named-pipe handle from `create_pipe`.
    let connected = unsafe { ConnectNamedPipe(pipe, None) };
    if connected.is_err() {
        // `ERROR_PIPE_CONNECTED` means a client arrived before the wait
        // started, which is a connection, not a failure. Every other error
        // ends the loop.
        let error = windows::core::Error::from_win32();
        if error.code().0 != -0x7FF8_FEDB {
            tracing::warn!(?error, "the service endpoint stopped accepting");
            return false;
        }
    }
    if stopping.load(Ordering::SeqCst) {
        // The wake-up connection from `handler`. Nothing to serve.
        // SAFETY: `pipe` is still live and connected.
        unsafe {
            let _ = DisconnectNamedPipe(pipe);
        }
        return true;
    }

    let mut frame = [0u8; FRAME_LEN];
    let mut read = 0u32;
    // SAFETY: `frame` is a live buffer of exactly the length passed, and
    // `read` outlives the call.
    let ok = unsafe { ReadFile(pipe, Some(&mut frame), Some(&raw mut read), None) };
    let status = if ok.is_ok() && read as usize == FRAME_LEN {
        serve(frame)
    } else {
        // A short or failed read is not an operation. Answering `refused`
        // rather than staying silent keeps the client from waiting out its
        // own timeout on a question that was never understood.
        STATUS_REFUSED
    };

    let reply = response(status);
    let mut written = 0u32;
    // SAFETY: `reply` and `written` are live and owned for the call.
    unsafe {
        let _ = WriteFile(pipe, Some(&reply), Some(&raw mut written), None);
        let _ = DisconnectNamedPipe(pipe);
    }
    true
}

/// Carries out one request. The whole authorization story is the pipe's DACL:
/// anything that got this far is an interactive user or an administrator.
fn serve(frame: [u8; FRAME_LEN]) -> u8 {
    if parse_request(&frame) == Some(OP_DELIVER_SAS) {
        tracing::info!("delivering the secure attention sequence");
        // SAFETY: documented Win32 entry point of `sas.dll` with no
        // invariants beyond its argument. `FALSE` names the caller's own
        // session, which for a service is session 0 — the case the
        // `SoftwareSASGeneration` policy grants services.
        unsafe {
            SendSAS(false);
        }
        return STATUS_OK;
    }
    tracing::warn!("refusing an unknown request");
    STATUS_REFUSED
}

/// A null-terminated UTF-16 copy of `text`, for the `W` entry points.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The access list names exactly three trustees, and grants interactive
    /// users read/write only — never `WRITE_DAC`, which would let a client
    /// widen the pipe from under the service.
    #[test]
    fn the_endpoint_admits_only_local_interactive_callers() {
        assert!(
            PIPE_SDDL.contains(";;;SY)"),
            "LocalSystem must be able to own it"
        );
        assert!(
            PIPE_SDDL.contains(";;;BA)"),
            "administrators must be able to manage it"
        );
        assert!(
            PIPE_SDDL.contains("0x0012019b;;;IU)"),
            "interactive users get read/write only"
        );
        assert!(
            !PIPE_SDDL.contains(";;;WD)") && !PIPE_SDDL.contains(";;;AU)"),
            "everyone and authenticated-users are exactly who must not be admitted"
        );
    }

    /// A wide string is null-terminated, or every `W` call reads past it.
    #[test]
    fn wide_strings_are_terminated() {
        let encoded = wide("ab");
        assert_eq!(encoded, vec![u16::from(b'a'), u16::from(b'b'), 0]);
    }
}
