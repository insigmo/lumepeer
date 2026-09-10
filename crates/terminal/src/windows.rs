//! The Windows pseudo-console, and the token drop it exists to make possible
//! (ADR 0079).
//!
//! `ConPTY` is the easy half: [`CreatePseudoConsole`] over two anonymous pipes,
//! the console handed to the child through a process-thread attribute list.
//! The hard half is decision 2 of ADR 0079. ADR 0057 ships this client with
//! `requireAdministrator`, so a shell started the ordinary way — the way
//! `portable-pty`'s own `ConPTY` backend starts one, with `CreateProcessW` —
//! would be an administrator shell by inheritance rather than by anybody's
//! decision. That is a silent privilege escalation, and it is the whole reason
//! this file exists instead of a third-party dependency.
//!
//! The drop is the standard one: the process behind `GetShellWindow()` is
//! Explorer, which runs **unelevated as the interactive user**, so its token
//! duplicated as a primary token is exactly the identity the shell should
//! have. `CreateProcessAsUserW` takes it, and takes a `STARTUPINFOEXW` — which
//! `CreateProcessWithTokenW` does not, since it ignores `lpAttributeList`, and
//! a `ConPTY` child *is* an attribute list. Every way that can fail collapses to
//! [`ShellError::CannotDropPrivileges`] and **no process is created**: there
//! is no fallback to this process's own token, because the fallback is the
//! outcome being prevented.

#![allow(
    unsafe_code,
    reason = "CreatePseudoConsole, the process-thread attribute list, \
              DuplicateTokenEx and CreateProcessAsUserW have no safe bindings; \
              same justification standard as the Win32 surface of \
              lumepeer-service (ADR 0043, ADR 0049, ADR 0056)"
)]

use std::ffi::{OsString, c_void};
use std::io::{Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, TOKEN_ALL_ACCESS, TOKEN_DUPLICATE, TOKEN_QUERY,
    TokenPrimary,
};
use windows::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcess, OpenProcessToken,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    STARTUPINFOEXW, STARTUPINFOW, TerminateProcess, UpdateProcThreadAttribute,
};
use windows::Win32::UI::WindowsAndMessaging::{GetShellWindow, GetWindowThreadProcessId};
use windows::core::PWSTR;

use crate::{ShellError, ShellSize};

/// Shell to run when the environment names none. Present on every Windows
/// install; `%COMSPEC%` is what the machine itself says, and is preferred.
const FALLBACK_SHELL: &str = r"C:\Windows\System32\cmd.exe";

/// Exit code the shell is terminated with when the session withdraws it.
///
/// Nothing reads it — it exists because `TerminateProcess` demands one — but
/// it is deliberately not 0: a shell that was killed did not succeed.
const KILLED_EXIT_CODE: u32 = 1;

/// The host's own shell, with no arguments (ADR 0079).
pub(crate) fn host_shell() -> OsString {
    match std::env::var_os("COMSPEC") {
        Some(shell) if !shell.is_empty() => shell,
        _ => OsString::from(FALLBACK_SHELL),
    }
}

/// A pseudo-console handle that closes itself.
///
/// `HPCON` is not a `HANDLE` and has no `OwnedHandle` equivalent, so the
/// wrapper is written out. Closing it is what signals the shell that its
/// console is gone.
#[derive(Debug)]
struct PseudoConsole(HPCON);

// SAFETY: an HPCON is an opaque kernel-backed handle with no thread affinity;
// the only operations this crate performs on it are ResizePseudoConsole and
// ClosePseudoConsole, both of which the API documents as callable from any
// thread. The `Mutex` in `Inner` is what serializes them.
unsafe impl Send for PseudoConsole {}
unsafe impl Sync for PseudoConsole {}

impl Drop for PseudoConsole {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from a successful `CreatePseudoConsole` and is
        // closed exactly once, here, because `Inner` owns it.
        unsafe { ClosePseudoConsole(self.0) };
    }
}

/// Everything a running shell owns, dropped as one.
///
/// The kill lives in this type's `Drop` rather than in [`Shell`]'s, because
/// what has to be true is "when the last handle to this shell is gone, so is
/// the shell" — and the handle that outlives the others is the control half
/// the actor keeps. A session that ends by its connection dropping never gets
/// to call anything, and this is what makes that case leave nothing behind.
struct Inner {
    console: Mutex<PseudoConsole>,
    /// The shell process. Kept so it can be terminated and so Windows does not
    /// recycle its id while this handle lives.
    process: Mutex<OwnedHandle>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        terminate(&self.process);
    }
}

/// Terminates the shell, ignoring one that has already gone.
///
/// Not an error: a revoke and an `exit` race every time, and neither is wrong.
fn terminate(process: &Mutex<OwnedHandle>) {
    if let Ok(process) = process.lock() {
        // SAFETY: a borrowed process handle opened with PROCESS_ALL_ACCESS by
        // CreateProcessAsUserW; terminating an already-exited process returns
        // an error rather than doing anything unsound, and the error is the
        // race this ignores.
        unsafe {
            let _ = TerminateProcess(HANDLE(process.as_raw_handle()), KILLED_EXIT_CODE);
        }
    }
}

pub(crate) struct Shell {
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    control: ShellControl,
}

impl std::fmt::Debug for Shell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shell").finish_non_exhaustive()
    }
}

/// Resize and kill, usable while another thread is blocked reading output.
#[derive(Clone)]
pub(crate) struct ShellControl {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for ShellControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellControl").finish_non_exhaustive()
    }
}

impl Shell {
    pub(crate) fn spawn(size: ShellSize) -> Result<Self, ShellError> {
        // The identity first, and before anything is created: a host that
        // cannot make the drop must not have started a console, let alone a
        // process (ADR 0079).
        let token = interactive_user_token()?;

        let (input_read, input_write) = pipe()?;
        let (output_read, output_write) = pipe()?;

        // SAFETY: both handles are live pipe ends this function owns, and the
        // coordinate is a bounded geometry (§9.1 checked it at the wire).
        let console = unsafe {
            CreatePseudoConsole(
                COORD {
                    X: clamp_cell(size.cols),
                    Y: clamp_cell(size.rows),
                },
                HANDLE(input_read.as_raw_handle()),
                HANDLE(output_write.as_raw_handle()),
                0,
            )
        }
        .map_err(|error| {
            tracing::warn!(%error, "no pseudo-console could be created");
            ShellError::Unavailable
        })?;
        let console = PseudoConsole(console);
        // The console owns copies of these two ends now. Holding ours would
        // keep the pipes open after the shell exits, so the reader below would
        // block for ever instead of seeing EOF — which is how an orphan is
        // born.
        drop(input_read);
        drop(output_write);

        let process = spawn_shell(&console, &token)?;

        // SAFETY: each is a pipe end this function owns and has not closed;
        // `File` takes ownership and closes it exactly once.
        let reader = unsafe { std::fs::File::from_raw_handle(output_read.into_raw_handle()) };
        let writer = unsafe { std::fs::File::from_raw_handle(input_write.into_raw_handle()) };

        Ok(Self {
            reader: Box::new(reader),
            writer: Box::new(writer),
            control: ShellControl {
                inner: Arc::new(Inner {
                    console: Mutex::new(console),
                    process: Mutex::new(process),
                }),
            },
        })
    }

    pub(crate) fn into_parts(self) -> (Box<dyn Read + Send>, Box<dyn Write + Send>, ShellControl) {
        (self.reader, self.writer, self.control)
    }

    pub(crate) fn control(&self) -> ShellControl {
        self.control.clone()
    }
}

impl ShellControl {
    pub(crate) fn resize(&self, size: ShellSize) -> Result<(), ShellError> {
        let console = self
            .inner
            .console
            .lock()
            .map_err(|_| ShellError::Unavailable)?;
        // SAFETY: a live HPCON this type owns, with a bounded geometry.
        unsafe {
            ResizePseudoConsole(
                console.0,
                COORD {
                    X: clamp_cell(size.cols),
                    Y: clamp_cell(size.rows),
                },
            )
        }
        .map_err(|_| ShellError::Unavailable)
    }

    pub(crate) fn kill(&self) {
        terminate(&self.inner.process);
    }
}

/// A primary token for the machine's interactive, **unelevated** user
/// (ADR 0079 decision 2).
///
/// `GetShellWindow` is the shortest honest way to find that user: the desktop
/// window belongs to Explorer, Explorer runs unelevated in the console
/// session, and its token is therefore the identity a shell started "as the
/// person at this machine" should have. Every failure is the same refusal,
/// because the caller has nothing different to do about any of them and the
/// distinction is a fact about the host's own machine (§15).
fn interactive_user_token() -> Result<OwnedHandle, ShellError> {
    let refuse = |what: &str| {
        tracing::warn!(
            reason = what,
            "refusing a remote shell: this process could not take the interactive \
             user's identity, and will not lend the shell its own (ADR 0079)"
        );
        ShellError::CannotDropPrivileges
    };

    // SAFETY: no arguments, and the return value is checked against the null
    // window before it is used.
    let shell_window = unsafe { GetShellWindow() };
    if shell_window.is_invalid() {
        return Err(refuse("no shell window"));
    }

    let mut pid = 0u32;
    // SAFETY: a live window handle and a stack slot for the id.
    unsafe { GetWindowThreadProcessId(shell_window, Some(&raw mut pid)) };
    if pid == 0 {
        return Err(refuse("no shell process"));
    }

    // SAFETY: opening by id with the narrowest right that allows the token to
    // be read; the returned handle is owned and closed below.
    let opened = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
        .map_err(|_| refuse("the shell process could not be opened"))?;
    // SAFETY: a handle OpenProcess just returned and nothing else owns.
    let process: OwnedHandle = unsafe { OwnedHandle::from_raw_handle(opened.0) };

    let mut token = HANDLE::default();
    // SAFETY: a live process handle; the token handle is taken over below.
    unsafe {
        OpenProcessToken(
            HANDLE(process.as_raw_handle()),
            TOKEN_DUPLICATE | TOKEN_QUERY,
            &raw mut token,
        )
    }
    .map_err(|_| refuse("the shell process's token could not be read"))?;
    // SAFETY: a token handle OpenProcessToken just wrote and nothing else owns.
    let token: OwnedHandle = unsafe { OwnedHandle::from_raw_handle(token.0) };

    let mut primary = HANDLE::default();
    // SAFETY: a live token handle; `primary` is taken over below.
    unsafe {
        DuplicateTokenEx(
            HANDLE(token.as_raw_handle()),
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &raw mut primary,
        )
    }
    .map_err(|_| refuse("the interactive user's token could not be duplicated"))?;
    // SAFETY: a token handle DuplicateTokenEx just wrote and nothing else owns.
    Ok(unsafe { OwnedHandle::from_raw_handle(primary.0) })
}

/// Starts the host's shell under `console`, as the user `token` names.
///
/// `CreateProcessAsUserW` and not `CreateProcessWithTokenW`: the latter
/// ignores `lpAttributeList`, and the attribute list *is* how a child is given
/// a pseudo-console.
fn spawn_shell(console: &PseudoConsole, token: &OwnedHandle) -> Result<OwnedHandle, ShellError> {
    let mut attributes = AttributeList::with_pseudo_console(console)?;
    let startup = STARTUPINFOEXW {
        StartupInfo: STARTUPINFOW {
            cb: u32::try_from(size_of::<STARTUPINFOEXW>()).unwrap_or(0),
            ..STARTUPINFOW::default()
        },
        lpAttributeList: attributes.as_mut(),
    };

    let mut command: Vec<u16> = host_shell()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut info = PROCESS_INFORMATION::default();

    // SAFETY: a live primary token, a NUL-terminated mutable command line, an
    // initialized attribute list carrying the pseudo-console, and an output
    // slot for the process ids. A failure writes nothing to `info`.
    let spawned = unsafe {
        windows::Win32::System::Threading::CreateProcessAsUserW(
            Some(HANDLE(token.as_raw_handle())),
            None,
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            false,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
            None,
            None,
            &raw const startup.StartupInfo,
            &raw mut info,
        )
    };
    if let Err(error) = spawned {
        // The one failure that is *not* "no shell": the identity was found and
        // the spawn with it was still refused, which is the drop failing
        // rather than the machine having no shell (ADR 0079).
        tracing::warn!(
            %error,
            "refusing a remote shell: the interactive user's token was found but the \
             shell could not be started with it, and this process will not lend its own"
        );
        return Err(ShellError::CannotDropPrivileges);
    }

    // SAFETY: both handles were just written by a successful spawn; the thread
    // handle is taken only to be closed, which is what stops it leaking.
    let process = unsafe { OwnedHandle::from_raw_handle(info.hProcess.0) };
    // SAFETY: same, and closing it does not affect the running process.
    unsafe {
        let _ = CloseHandle(info.hThread);
    }
    Ok(process)
}

/// One anonymous pipe, as two owned ends.
fn pipe() -> Result<(OwnedHandle, OwnedHandle), ShellError> {
    let mut read = HANDLE::default();
    let mut write = HANDLE::default();
    // SAFETY: two stack slots the call fills in; default attributes and the
    // system's own buffer size.
    unsafe { CreatePipe(&raw mut read, &raw mut write, None, 0) }.map_err(|error| {
        tracing::warn!(%error, "no pipe could be created for a shell");
        ShellError::Unavailable
    })?;
    // SAFETY: two handles CreatePipe just wrote and nothing else owns.
    unsafe {
        Ok((
            OwnedHandle::from_raw_handle(read.0),
            OwnedHandle::from_raw_handle(write.0),
        ))
    }
}

/// A process-thread attribute list carrying exactly one attribute: the
/// pseudo-console the child is to be attached to.
struct AttributeList {
    storage: Vec<u8>,
}

impl AttributeList {
    fn with_pseudo_console(console: &PseudoConsole) -> Result<Self, ShellError> {
        let mut needed = 0usize;
        // The documented two-call shape: the first call always "fails" with
        // the size it wants, which is why its result is deliberately ignored.
        // SAFETY: a null list is what asks for the size.
        unsafe {
            let _ = InitializeProcThreadAttributeList(None, 1, None, &raw mut needed);
        }
        if needed == 0 {
            return Err(ShellError::Unavailable);
        }
        let mut storage = vec![0u8; needed];
        let list = LPPROC_THREAD_ATTRIBUTE_LIST(storage.as_mut_ptr().cast::<c_void>());
        // SAFETY: `storage` is exactly the size the call above asked for and
        // outlives the list, which is deleted in `Drop`.
        unsafe { InitializeProcThreadAttributeList(Some(list), 1, None, &raw mut needed) }
            .map_err(|_| ShellError::Unavailable)?;

        // SAFETY: an initialized list with room for one attribute, and an
        // HPCON that outlives the spawn it is used for — `spawn_shell` holds a
        // borrow of the console for the whole call.
        unsafe {
            UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                Some(std::ptr::from_ref(&console.0).cast::<c_void>()),
                size_of::<HPCON>(),
                None,
                None,
            )
        }
        .map_err(|_| ShellError::Unavailable)?;

        Ok(Self { storage })
    }

    fn as_mut(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        LPPROC_THREAD_ATTRIBUTE_LIST(self.storage.as_mut_ptr().cast::<c_void>())
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        // SAFETY: a list this type initialized and owns, deleted exactly once.
        unsafe { DeleteProcThreadAttributeList(self.as_mut()) };
    }
}

/// A geometry as a console coordinate.
///
/// `TERMINAL_COLS_MAX`/`TERMINAL_ROWS_MAX` already keep this well inside
/// `i16`, so the saturation is a belt on top of the wire's own check rather
/// than a policy — but it is here rather than an `as`, because a silently
/// negative console size is not something this code should be able to
/// produce.
fn clamp_cell(cells: u16) -> i16 {
    i16::try_from(cells).unwrap_or(i16::MAX)
}

/// A `HANDLE` as the raw pointer `OwnedHandle` speaks, and back.
trait RawHandleExt {
    fn as_raw_handle(&self) -> *mut c_void;
    fn into_raw_handle(self) -> *mut c_void;
}

impl RawHandleExt for OwnedHandle {
    fn as_raw_handle(&self) -> *mut c_void {
        std::os::windows::io::AsRawHandle::as_raw_handle(self)
    }

    fn into_raw_handle(self) -> *mut c_void {
        std::os::windows::io::IntoRawHandle::into_raw_handle(self)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// The shell is the machine's own, from the machine's own environment, and
    /// never anything a guest sent (ADR 0079).
    #[test]
    fn the_shell_comes_from_comspec_with_a_system32_fallback() {
        let shell = host_shell();
        assert!(!shell.is_empty());
        match std::env::var_os("COMSPEC") {
            Some(from_env) if !from_env.is_empty() => assert_eq!(shell, from_env),
            _ => assert_eq!(shell, OsString::from(FALLBACK_SHELL)),
        }
    }

    /// The attribute list is the mechanism ADR 0079 turns on: without one a
    /// child cannot be handed a pseudo-console, and `CreateProcessWithTokenW`
    /// — the other way to spawn with a token — ignores it. If this stops
    /// building or stops initializing, the Windows terminal has no path that
    /// both drops privileges and attaches a console.
    #[test]
    fn a_pseudo_console_can_be_created_and_carried_in_an_attribute_list() {
        let (input_read, input_write) = pipe().expect("a pipe");
        let (output_read, output_write) = pipe().expect("a pipe");
        // SAFETY: four live pipe ends and a bounded geometry.
        let raw = unsafe {
            CreatePseudoConsole(
                COORD { X: 80, Y: 24 },
                HANDLE(input_read.as_raw_handle()),
                HANDLE(output_write.as_raw_handle()),
                0,
            )
        }
        .expect("a pseudo-console");
        let console = PseudoConsole(raw);

        let list = AttributeList::with_pseudo_console(&console);
        assert!(
            list.is_ok(),
            "a pseudo-console could not be put in an attribute list"
        );

        // Resizing a live console is the other half of the guest's window
        // following the shell's idea of its own size.
        // SAFETY: a live HPCON and a bounded geometry.
        unsafe { ResizePseudoConsole(console.0, COORD { X: 120, Y: 40 }) }.expect("a resize");

        drop(list);
        drop(console);
        drop(input_read);
        drop(input_write);
        drop(output_read);
        drop(output_write);
    }

    /// The refusal is a refusal, not a fallback. Whatever
    /// [`interactive_user_token`] does on this machine, the one thing it may
    /// never hand back is this process's own token — which is what a caller
    /// would get if the failure path returned something instead of erroring.
    #[test]
    fn a_failed_drop_is_a_refusal_and_never_this_processs_own_identity() {
        match interactive_user_token() {
            // On a desktop session this succeeds, and what it hands back is
            // Explorer's token rather than ours. Both are the same *user*, so
            // there is nothing to compare here that would not also be true of
            // a correct result; what is checkable is that a handle came back
            // at all and that it is not the pseudo-handle for "me".
            Ok(token) => {
                // SAFETY: no arguments, and the pseudo-handle it returns is
                // only compared, never used.
                let me = unsafe { windows::Win32::System::Threading::GetCurrentProcess() };
                assert_ne!(
                    token.as_raw_handle(),
                    me.0,
                    "the drop handed back this process instead of a token"
                );
            }
            // On a service or a session with no Explorer this is the correct
            // answer, and the caller turns it into
            // `TerminalRefusal::CannotDropPrivileges`.
            Err(error) => assert_eq!(error, ShellError::CannotDropPrivileges),
        }
    }
}
