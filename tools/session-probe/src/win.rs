//! Shared Win32 helpers for the pack-01 probe (throwaway).
//!
//! The token-duplication is a copy of
//! `crates/service/src/secure_desktop_launch.rs::duplicate_own_token_for_session`
//! (that one is `pub(crate)`, out of reach of this standalone crate), and the
//! launch onto `WinSta0\Default` mirrors `system_injector_launch.rs`.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::PathBuf;

use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree, WAIT_OBJECT_0};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{
    DuplicateTokenEx, GetTokenInformation, SecurityIdentification, SetTokenInformation, TOKEN_QUERY,
    TOKEN_ACCESS_MASK, TOKEN_ALL_ACCESS, TOKEN_DUPLICATE, TOKEN_INFORMATION_CLASS, TOKEN_USER,
    TokenPrimary, TokenSessionId, TokenUser,
};
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess,
    OpenProcessToken, PROCESS_INFORMATION, STARTUPINFOW, WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};

/// Where every artefact lands, beside the service's own logs.
pub fn logs_dir() -> PathBuf {
    let base = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".to_owned());
    let dir = PathBuf::from(base).join("Lumepeer").join("logs");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// The owner-facing report (`--measure` owns it).
pub fn report_path() -> PathBuf {
    logs_dir().join("session-probe.txt")
}

/// The controller + service diagnostics log.
pub fn service_log_path() -> PathBuf {
    logs_dir().join("session-probe-service.txt")
}

/// What the medium-integrity poke writes for `--measure` to read back.
pub fn poke_path() -> PathBuf {
    logs_dir().join("session-probe-poke.txt")
}

/// The 2.2 proof frame.
pub fn capture_png_path() -> PathBuf {
    logs_dir().join("session-probe-capture.png")
}

/// Where 2.6's `cmd whoami /all` output is captured. NOT the admin-only logs
/// dir: the shell runs with the user's limited token, which cannot write
/// there — the `C:\ProgramData` root is writable by the Users group.
pub fn whoami_path() -> PathBuf {
    let base = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".to_owned());
    PathBuf::from(base).join("session-probe-user.txt")
}

/// Appends one line to a log (shared-access create+append), best effort.
pub fn append(path: &std::path::Path, line: &str) {
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}

/// A null-terminated UTF-16 copy of `text`.
pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The session attached to the physical console, or `None`.
pub fn console_session() -> Option<u32> {
    // SAFETY: a plain kernel32 export; `0xFFFFFFFF` means none attached.
    let s = unsafe { WTSGetActiveConsoleSessionId() };
    (s != u32::MAX).then_some(s)
}

/// This process's token user SID as a string (e.g. `S-1-5-18`).
pub fn current_user_sid() -> Option<String> {
    let mut token = HANDLE::default();
    // SAFETY: pseudo-handle needs no close; `token` outlives the call.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) }.ok()?;
    let sid = token_info_sid(token, TokenUser);
    // SAFETY: closing the token this function opened, once.
    unsafe {
        let _ = CloseHandle(token);
    }
    sid
}

/// Reads a SID-shaped token-information class and returns its string form.
fn token_info_sid(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Option<String> {
    let mut len = 0u32;
    // SAFETY: first call sizes the buffer; a zero length with ERROR_INSUFFICIENT
    // _BUFFER is expected and not treated as success.
    let _ = unsafe { GetTokenInformation(token, class, None, 0, &raw mut len) };
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u8; len as usize];
    // SAFETY: `buf` is exactly `len` bytes and outlives the call.
    unsafe {
        GetTokenInformation(
            token,
            class,
            Some(buf.as_mut_ptr().cast::<c_void>()),
            len,
            &raw mut len,
        )
    }
    .ok()?;
    // TOKEN_USER begins with a SID_AND_ATTRIBUTES whose first field is the PSID.
    let tu = buf.as_ptr().cast::<TOKEN_USER>();
    // SAFETY: `buf` holds a TOKEN_USER the OS just wrote.
    let psid = unsafe { (*tu).User.Sid };
    sid_to_string(psid)
}

/// `ConvertSidToStringSidW`, owned-string result.
pub fn sid_to_string(psid: windows::Win32::Security::PSID) -> Option<String> {
    let mut out = PWSTR::null();
    // SAFETY: `out` receives a LocalAlloc'd string freed below.
    unsafe { ConvertSidToStringSidW(psid, &raw mut out) }.ok()?;
    if out.is_null() {
        return None;
    }
    // SAFETY: `out` is a null-terminated wide string the call allocated.
    let s = unsafe { out.to_string().ok() };
    // SAFETY: freeing the LocalAlloc'd buffer, once.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(out.0.cast())));
    }
    s
}

/// Duplicates this (`LocalSystem`) process's primary token and stamps it with
/// `session`, so a process created with it lands in that session. Copy of the
/// service's own `duplicate_own_token_for_session`.
pub fn duplicate_own_token_for_session(session: u32) -> Option<HANDLE> {
    let mut process_token = HANDLE::default();
    // SAFETY: pseudo-handle needs no close; `process_token` outlives the call.
    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ACCESS_MASK(TOKEN_DUPLICATE.0 | TOKEN_QUERY.0),
            &raw mut process_token,
        )
    }
    .ok()?;
    let mut duplicate = HANDLE::default();
    // SAFETY: `process_token` opened with TOKEN_DUPLICATE; `duplicate` outlives.
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
    // SAFETY: closing the source token, once.
    unsafe {
        let _ = CloseHandle(process_token);
    }
    duplicated.ok()?;
    // SAFETY: `duplicate` is a live primary token; `session` is a 4-byte value.
    let stamped = unsafe {
        SetTokenInformation(
            duplicate,
            TokenSessionId,
            (&raw const session).cast::<c_void>(),
            u32::try_from(size_of::<u32>()).unwrap_or(4),
        )
    };
    if stamped.is_err() {
        // SAFETY: `duplicate` is live and owned here.
        unsafe {
            let _ = CloseHandle(duplicate);
        }
        return None;
    }
    Some(duplicate)
}

/// The console user's token (`WTSQueryUserToken`), limited half on a UAC
/// account. Caller closes it.
pub fn console_user_token() -> Option<HANDLE> {
    let session = console_session()?;
    let mut token = HANDLE::default();
    // SAFETY: `token` outlives the call; needs SeTcbPrivilege (LocalSystem has it).
    unsafe { WTSQueryUserToken(session, &raw mut token) }.ok()?;
    Some(token)
}

/// `CreateProcessAsUserW` of `command_line` with `token` onto `desktop`
/// (e.g. `WinSta0\Default`). Returns the process handle; caller waits/closes.
pub fn create_process_as_user(
    token: HANDLE,
    command_line: &str,
    desktop: &str,
) -> Option<HANDLE> {
    let mut cmd = wide(command_line);
    let mut desk = wide(desktop);
    let startup = STARTUPINFOW {
        cb: u32::try_from(size_of::<STARTUPINFOW>()).unwrap_or(0),
        lpDesktop: PWSTR(desk.as_mut_ptr()),
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    // SAFETY: `token` is a live primary token; the wide buffers outlive the
    // call; `startup` borrows `desk`; `process` is written by the call.
    let created = unsafe {
        CreateProcessAsUserW(
            Some(token),
            PCWSTR::null(),
            Some(PWSTR(cmd.as_mut_ptr())),
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
    if created.is_err() {
        return None;
    }
    // SAFETY: thread handle from the successful create, not used again.
    unsafe {
        let _ = CloseHandle(process.hThread);
    }
    Some(process.hProcess)
}

/// Waits up to `timeout_ms` for `process` and returns its exit code; closes
/// the handle.
pub fn wait_and_exit_code(process: HANDLE, timeout_ms: u32) -> Option<u32> {
    // SAFETY: `process` is a live process handle.
    let waited = unsafe { WaitForSingleObject(process, timeout_ms) };
    let code = if waited == WAIT_OBJECT_0 {
        let mut c = 0u32;
        // SAFETY: `process` live, `c` outlives the call.
        if unsafe { GetExitCodeProcess(process, &raw mut c) }.is_ok() {
            Some(c)
        } else {
            None
        }
    } else {
        None
    };
    // SAFETY: closing the process handle this value owned, once.
    unsafe {
        let _ = CloseHandle(process);
    }
    code
}
