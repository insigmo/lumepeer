//! The Windows side of the token drop: finding the interactive user, and
//! starting the worker that owns the pseudo-console (ADR 0079, ADR 0102).
//!
//! ## Why the console is not in this process
//!
//! It was, and it could not work. ADR 0057 ships this client with
//! `requireAdministrator`, and an elevated administrator token may not call
//! `CreateProcessAsUserW`: it has `SE_INCREASE_QUOTA_NAME` disabled and does
//! not contain `SE_ASSIGNPRIMARYTOKEN_NAME` at all, so the call comes back
//! `ERROR_PRIVILEGE_NOT_HELD` and no shell is started. Enabling the privilege
//! that is present does not help, because the one that is missing cannot be
//! enabled — measured rather than assumed, in
//! `docs/bugs/18-terminal-close-and-shell-spawn.md`.
//!
//! `CreateProcessWithTokenW` wants only `SE_IMPERSONATE_NAME`, which an
//! elevated client does hold. What it will not do is carry a
//! `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`, because it ignores
//! `lpAttributeList` — and a `ConPTY` child *is* an attribute list. So this
//! file starts `crates/terminal-worker` with that call, and the worker, being
//! already the right person, creates the console with a plain
//! `CreateProcessW`. The console lives there and only there: one
//! implementation, taken by a dev run and an installed client alike, rather
//! than a second copy on a path that only ever runs unelevated and therefore
//! only ever breaks in the field.
//!
//! ## What this file still decides
//!
//! Who the shell belongs to, and nothing else. The token comes from the
//! process behind `GetShellWindow()` — Explorer, running **unelevated as the
//! interactive user** — and the worker is started with it. When this process
//! may not use that token at all, there is exactly one case in which starting
//! the worker plainly is still the same answer: when this process already *is*
//! that user and is not elevated, which is the Unix rule of ADR 0079 written
//! out for Windows. Anything else is [`ShellError::CannotDropPrivileges`] with
//! **no process created**. There is no fallback to this process's own
//! identity, because that fallback is the outcome being prevented.

#![allow(
    unsafe_code,
    reason = "DuplicateTokenEx, CreateProcessWithTokenW and the token queries \
              have no safe bindings; same justification standard as the Win32 \
              surface of lumepeer-service (ADR 0043, ADR 0049, ADR 0056)"
)]

use std::ffi::{OsString, c_void};
use std::io::{Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_PRIVILEGE_NOT_HELD, HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS,
    SetHandleInformation,
};
use windows::Win32::Security::{
    DuplicateTokenEx, EqualSid, GetTokenInformation, SECURITY_ATTRIBUTES, SecurityImpersonation,
    TOKEN_ALL_ACCESS, TOKEN_DUPLICATE, TOKEN_ELEVATION, TOKEN_QUERY, TOKEN_USER, TokenElevation,
    TokenPrimary, TokenUser,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_PROCESS_LOGON_FLAGS, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
    CreateProcessWithTokenW, GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW, TerminateProcess,
};
use windows::Win32::UI::WindowsAndMessaging::{GetShellWindow, GetWindowThreadProcessId};
use windows::core::PWSTR;

use crate::worker_protocol::{WORKER_READY, encode_input, encode_resize};
use crate::{ShellError, ShellSize};

/// Shell to run when the environment names none. Present on every Windows
/// install; `%COMSPEC%` is what the machine itself says, and is preferred.
///
/// Read here as well as in the worker so that [`crate::host_shell`] answers
/// the same question on both sides of the drop. The worker reads its own
/// rather than being told, because a command line this process composed is one
/// more thing that would have to be trusted.
const FALLBACK_SHELL: &str = r"C:\Windows\System32\cmd.exe";

/// Exit code the worker is terminated with when the session withdraws it.
///
/// Nothing reads it — it exists because `TerminateProcess` demands one — but
/// it is deliberately not 0: a shell that was killed did not succeed.
const KILLED_EXIT_CODE: u32 = 1;

/// Name of the worker binary, which sits next to the main executable.
///
/// Same arrangement as `lumepeer-decoder-worker` (§11.3), for the same reason:
/// a `cargo build` tree drops it beside the debug binary, and an installed
/// build ships it through Tauri's `externalBin` with a target-triple suffix.
const WORKER_BINARY: &str = "lumepeer-terminal-worker";

/// Target triple this build was compiled for, matching the `-$TARGET_TRIPLE`
/// suffix Tauri's sidecar convention puts on the staged binary. Only the two
/// Windows triples are listed, because this file is the Windows one.
const TARGET_TRIPLE: Option<&str> = {
    #[cfg(target_arch = "x86_64")]
    {
        Some("x86_64-pc-windows-msvc")
    }
    #[cfg(target_arch = "aarch64")]
    {
        Some("aarch64-pc-windows-msvc")
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        None
    }
};

/// The host's own shell, with no arguments (ADR 0079).
pub(crate) fn host_shell() -> OsString {
    match std::env::var_os("COMSPEC") {
        Some(shell) if !shell.is_empty() => shell,
        _ => OsString::from(FALLBACK_SHELL),
    }
}

/// Everything one running shell owns, dropped as one.
///
/// The shell itself is not in here and cannot be: it is the worker's child, on
/// the far side of the drop. What stands in for a handle to it is the worker
/// process, which holds it in a job object that kills it when the worker goes
/// — so terminating the worker is exactly as final as terminating the shell
/// was before ADR 0102 put a process between them.
///
/// The kill lives in this type's `Drop` rather than in [`Shell`]'s, because
/// what has to be true is "when the last handle to this shell is gone, so is
/// the shell", and the handle that outlives the others is the control half the
/// actor keeps. A session that ends by its connection dropping never gets to
/// call anything, and this is what makes that case leave nothing behind.
struct Worker {
    /// The worker's standard input, which carries both keystrokes and
    /// geometries. Behind a mutex because the actor's writer thread and a
    /// resize arriving from anywhere else share it.
    stdin: Mutex<std::fs::File>,
    process: Mutex<OwnedHandle>,
}

impl Worker {
    /// Writes one framed message, whole, under the lock.
    fn send(&self, frame: &[u8]) -> std::io::Result<()> {
        let mut stdin = self
            .stdin
            .lock()
            .map_err(|_| std::io::Error::other("a poisoned shell input"))?;
        stdin.write_all(frame)?;
        stdin.flush()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        terminate(&self.process);
    }
}

/// Terminates the worker, ignoring one that has already gone.
///
/// Not an error: a revoke and an `exit` race every time, and neither is wrong.
fn terminate(process: &Mutex<OwnedHandle>) {
    if let Ok(process) = process.lock() {
        // SAFETY: a borrowed process handle opened with full access by the
        // spawn that created it; terminating an already-exited process returns
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
    worker: Arc<Worker>,
}

impl std::fmt::Debug for ShellControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellControl").finish_non_exhaustive()
    }
}

impl Shell {
    pub(crate) fn spawn(size: ShellSize) -> Result<Self, ShellError> {
        // The identity first, and before anything is created: a host that
        // cannot make the drop must not have started a pipe, let alone a
        // process (ADR 0079).
        let token = interactive_user_token()?;
        let program = locate_worker_binary()?;

        // Exactly one end of each pipe crosses into the worker; the ends kept
        // here are made private again, so nothing else this process later
        // starts inherits a handle onto somebody's terminal.
        let (to_worker_read, to_worker_write) = inheritable_pipe()?;
        let (from_worker_read, from_worker_write) = inheritable_pipe()?;
        keep_private(&to_worker_write)?;
        keep_private(&from_worker_read)?;

        let process = start_worker(&program, size, &token, &to_worker_read, &from_worker_write);
        // The worker's ends are the worker's now. Holding them here would keep
        // the pipes open after it exits, and the reader would never see EOF.
        drop(to_worker_read);
        drop(from_worker_write);
        let process = process?;

        // SAFETY: each is a pipe end this function owns and has not closed;
        // `File` takes ownership and closes it exactly once.
        let mut reader =
            unsafe { std::fs::File::from_raw_handle(from_worker_read.into_raw_handle()) };
        let stdin = unsafe { std::fs::File::from_raw_handle(to_worker_write.into_raw_handle()) };

        let worker = Arc::new(Worker {
            stdin: Mutex::new(stdin),
            process: Mutex::new(process),
        });

        // The worker says when the shell is actually running, the way the
        // decoder worker says when it is confined (§11.3). Until this byte
        // arrives nothing has started, so a worker that could not start a
        // shell becomes a refusal here rather than a terminal that opens and
        // instantly closes. Dropping `worker` on the way out is what kills it.
        let mut ready = [0u8; 1];
        if reader.read_exact(&mut ready).is_err() || ready[0] != WORKER_READY {
            tracing::warn!("refusing a remote shell: the shell worker started no shell (ADR 0102)");
            return Err(ShellError::Unavailable);
        }

        Ok(Self {
            reader: Box::new(reader),
            writer: Box::new(WorkerInput {
                worker: Arc::clone(&worker),
            }),
            control: ShellControl { worker },
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
        // The console is the worker's, so a resize travels the same pipe the
        // keystrokes do rather than being a call made here.
        self.worker
            .send(&encode_resize(size.cols, size.rows))
            .map_err(|_| ShellError::Unavailable)
    }

    pub(crate) fn kill(&self) {
        // Killing the worker kills the shell with it: the job object the
        // worker put it in is set to kill on close (ADR 0102).
        terminate(&self.worker.process);
    }
}

/// Starts the worker as the interactive user, by whichever call this process
/// is allowed to make.
///
/// [`CreateProcessWithTokenW`] is the one that matters and the one an elevated
/// client uses. The plain [`CreateProcessW`] underneath it is not a second
/// mechanism but the same one in the case where the drop is a no-op: this
/// process is already that user and holds nothing extra, so a child of it is
/// indistinguishable from a child started with the token. That case is checked
/// rather than assumed, and everything else is a refusal.
fn start_worker(
    program: &Path,
    size: ShellSize,
    token: &OwnedHandle,
    stdin: &OwnedHandle,
    stdout: &OwnedHandle,
) -> Result<OwnedHandle, ShellError> {
    let startup = STARTUPINFOW {
        cb: u32::try_from(size_of::<STARTUPINFOW>()).unwrap_or(0),
        dwFlags: STARTF_USESTDHANDLES,
        hStdInput: HANDLE(stdin.as_raw_handle()),
        hStdOutput: HANDLE(stdout.as_raw_handle()),
        // The worker's own diagnostics share the output pipe on purpose: it
        // writes to it only on a path where no shell ever started, and the
        // ready byte is what tells the two cases apart. A third pipe would
        // exist to carry text that must never be kept anyway (§15).
        hStdError: HANDLE(stdout.as_raw_handle()),
        ..STARTUPINFOW::default()
    };

    let mut command: Vec<u16> = command_line(program, size)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut info = PROCESS_INFORMATION::default();

    // SAFETY: a live primary token, a NUL-terminated mutable command line
    // naming a path this process built, a startup struct borrowing two live
    // pipe ends, and an output slot for the process ids. A failure writes
    // nothing to `info`.
    let spawned = unsafe {
        CreateProcessWithTokenW(
            HANDLE(token.as_raw_handle()),
            CREATE_PROCESS_LOGON_FLAGS(0),
            None,
            Some(PWSTR(command.as_mut_ptr())),
            CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
            None,
            None,
            &raw const startup,
            &raw mut info,
        )
    };

    match spawned {
        Ok(()) => {}
        Err(error) if error.code() == ERROR_PRIVILEGE_NOT_HELD.to_hresult() => {
            // No `SE_IMPERSONATE_NAME`, which is what an ordinary unelevated
            // process has. Starting the worker as ourselves is the same answer
            // only if we already are the person the shell is for.
            if !already_the_interactive_user(token) {
                tracing::warn!(
                    "refusing a remote shell: this process may not start one as the \
                     interactive user and is not that user itself (ADR 0079)"
                );
                return Err(ShellError::CannotDropPrivileges);
            }
            // SAFETY: as above, minus the token; the command line and startup
            // struct are the same live buffers, and `bInheritHandles` is true
            // because the two pipe ends in `startup` are how the worker is
            // reached at all.
            unsafe {
                CreateProcessW(
                    None,
                    Some(PWSTR(command.as_mut_ptr())),
                    None,
                    None,
                    true,
                    CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
                    None,
                    None,
                    &raw const startup,
                    &raw mut info,
                )
            }
            .map_err(|error| {
                tracing::warn!(%error, "no shell worker could be started");
                ShellError::Unavailable
            })?;
        }
        Err(error) => {
            tracing::warn!(
                %error,
                "refusing a remote shell: the interactive user's token was found but the \
                 shell worker could not be started with it, and this process will not \
                 lend its own (ADR 0102)"
            );
            return Err(ShellError::CannotDropPrivileges);
        }
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

/// Whether a child of this process would already be what the drop is for: the
/// interactive user, unelevated.
///
/// Both halves are load-bearing. The same user elevated is the administrator
/// shell this crate exists to prevent; a different user unelevated is a shell
/// belonging to somebody who is not sitting at the machine. Only both together
/// make "start it as ourselves" mean the same thing as "start it as them".
fn already_the_interactive_user(theirs: &OwnedHandle) -> bool {
    let mut ours = HANDLE::default();
    // SAFETY: the current-process pseudo-handle and a stack slot for the
    // token, which is taken over immediately below.
    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut ours) };
    if opened.is_err() {
        return false;
    }
    // SAFETY: a token handle OpenProcessToken just wrote and nothing else owns.
    let ours: OwnedHandle = unsafe { OwnedHandle::from_raw_handle(ours.0) };

    !is_elevated(&ours) && same_user(&ours, theirs)
}

/// Whether `token` carries an elevated administrator's privileges.
///
/// A query that fails is read as "elevated": the question is being asked in
/// order to decide whether skipping the drop is safe, and an unanswered
/// question is not a yes.
fn is_elevated(token: &OwnedHandle) -> bool {
    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0u32;
    // SAFETY: a live token handle and a fully owned struct of exactly the size
    // named, which the call fills in.
    let queried = unsafe {
        GetTokenInformation(
            HANDLE(token.as_raw_handle()),
            TokenElevation,
            Some(std::ptr::from_mut(&mut elevation).cast::<c_void>()),
            u32::try_from(size_of::<TOKEN_ELEVATION>()).unwrap_or(0),
            &raw mut returned,
        )
    };
    queried.is_err() || elevation.TokenIsElevated != 0
}

/// Whether two tokens name the same user.
///
/// A query that fails is read as "no", for the same reason as above.
fn same_user(ours: &OwnedHandle, theirs: &OwnedHandle) -> bool {
    let (Some(ours), Some(theirs)) = (token_user(ours), token_user(theirs)) else {
        return false;
    };
    // SAFETY: two buffers this function owns, each holding a TOKEN_USER the
    // kernel wrote, whose `Sid` points inside its own buffer; both outlive the
    // call.
    unsafe {
        let ours = ours.as_ptr().cast::<TOKEN_USER>();
        let theirs = theirs.as_ptr().cast::<TOKEN_USER>();
        EqualSid((*ours).User.Sid, (*theirs).User.Sid).is_ok()
    }
}

/// A buffer for a variable-length `TOKEN_*` struct, aligned for one.
///
/// `Vec<u8>` is only byte-aligned, and a `TOKEN_USER` read out of one is a
/// misaligned read. `u64` is the alignment of every pointer the struct holds,
/// which on both Windows targets is the strictest alignment it has.
type TokenBuffer = Vec<u64>;

/// Bytes one [`TokenBuffer`] element covers.
const TOKEN_BUFFER_UNIT: usize = size_of::<u64>();

/// One token's `TOKEN_USER`, in a buffer that owns the SID it points at.
///
/// The documented two-call shape: the first asks how much room the variable
/// part needs and is expected to fail, the second fills it.
fn token_user(token: &OwnedHandle) -> Option<TokenBuffer> {
    let mut needed = 0u32;
    // SAFETY: a live token handle, a null buffer to ask the size, and a stack
    // slot for the answer.
    unsafe {
        let _ = GetTokenInformation(
            HANDLE(token.as_raw_handle()),
            TokenUser,
            None,
            0,
            &raw mut needed,
        );
    }
    if needed == 0 {
        return None;
    }
    let mut buffer: TokenBuffer = vec![0; (needed as usize).div_ceil(TOKEN_BUFFER_UNIT).max(1)];
    // SAFETY: a live token handle and a buffer of exactly the size the call
    // above asked for.
    unsafe {
        GetTokenInformation(
            HANDLE(token.as_raw_handle()),
            TokenUser,
            Some(buffer.as_mut_ptr().cast::<c_void>()),
            needed,
            &raw mut needed,
        )
    }
    .ok()?;
    Some(buffer)
}

/// The worker's command line: the program, then the geometry to start at.
///
/// Quoted because an installed client lives under `C:\Program Files`, and a
/// path with a space in it that is not quoted is a different program.
fn command_line(program: &Path, size: ShellSize) -> OsString {
    let mut line = OsString::from("\"");
    line.push(program);
    line.push(format!("\" {} {}", size.cols, size.rows));
    line
}

/// What the actor's writer thread writes into.
///
/// Each `write` becomes exactly one frame and reports the whole buffer
/// consumed, so `write_all` never splits a keystroke across two of them.
struct WorkerInput {
    worker: Arc<Worker>,
}

impl Write for WorkerInput {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.worker.send(&encode_input(buf))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // `send` already flushed; a frame half-written into the pipe would be
        // a frame the worker blocks on for ever.
        Ok(())
    }
}

/// Picks the worker binary sitting next to the running executable.
///
/// Bare name first — what a `cargo build` workspace tree drops beside the
/// debug binary, so nothing changes for a local run — then the
/// target-triple-suffixed sidecar name an installed Tauri build ships. Exactly
/// the arrangement `crates/media`'s decoder worker uses, and it fails the same
/// way: a client whose worker was never staged refuses the shell rather than
/// starting one some other way.
fn locate_worker_binary() -> Result<PathBuf, ShellError> {
    let exe = std::env::current_exe().map_err(|error| {
        tracing::warn!(%error, "no shell worker: this executable has no path");
        ShellError::Unavailable
    })?;
    let mut bare = exe.with_file_name(WORKER_BINARY);
    bare.set_extension("exe");
    if bare.is_file() {
        return Ok(bare);
    }
    if let Some(triple) = TARGET_TRIPLE {
        let mut sidecar = exe.with_file_name(format!("{WORKER_BINARY}-{triple}"));
        sidecar.set_extension("exe");
        if sidecar.is_file() {
            return Ok(sidecar);
        }
    }
    tracing::warn!(
        path = %bare.display(),
        "no shell worker beside this executable; the sidecar was never staged (ADR 0102)"
    );
    Err(ShellError::Unavailable)
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

/// One anonymous pipe whose ends both start inheritable, for a worker that is
/// about to be handed one of them on a standard handle.
///
/// The end kept on this side is taken back out of inheritance by
/// [`keep_private`] immediately afterwards, so the window in which a handle
/// onto somebody's terminal could reach an unrelated child is no window at all
/// — `Shell::spawn` runs on one thread.
fn inheritable_pipe() -> Result<(OwnedHandle, OwnedHandle), ShellError> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: true.into(),
    };
    let mut read = HANDLE::default();
    let mut write = HANDLE::default();
    // SAFETY: two stack slots the call fills in, an attribute struct that
    // outlives the call, and the system's own buffer size.
    unsafe {
        CreatePipe(
            &raw mut read,
            &raw mut write,
            Some(&raw const attributes),
            0,
        )
    }
    .map_err(|error| {
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

/// Takes one handle back out of inheritance.
fn keep_private(handle: &OwnedHandle) -> Result<(), ShellError> {
    // SAFETY: a live handle this process owns; clearing the inherit flag has
    // no effect on anything already started.
    unsafe {
        SetHandleInformation(
            HANDLE(handle.as_raw_handle()),
            HANDLE_FLAG_INHERIT.0,
            HANDLE_FLAGS(0),
        )
    }
    .map_err(|error| {
        tracing::warn!(%error, "a shell pipe could not be made private");
        ShellError::Unavailable
    })
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
                let me = unsafe { GetCurrentProcess() };
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

    /// Skipping the token is only ever allowed when it would change nothing,
    /// and "nothing" is two conditions rather than one. On a desktop session
    /// this test runs as the interactive user, so the answer tracks elevation
    /// exactly: unelevated it may skip, elevated it may not.
    #[test]
    fn the_token_may_only_be_skipped_by_the_interactive_user_unelevated() {
        let Ok(token) = interactive_user_token() else {
            return;
        };
        let mut ours = HANDLE::default();
        // SAFETY: the current-process pseudo-handle and a stack slot.
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut ours) }
            .expect("this process's own token");
        // SAFETY: a token handle OpenProcessToken just wrote.
        let ours: OwnedHandle = unsafe { OwnedHandle::from_raw_handle(ours.0) };

        assert!(
            same_user(&ours, &token),
            "the test is not running as the person at this machine"
        );
        assert_eq!(
            already_the_interactive_user(&token),
            !is_elevated(&ours),
            "an elevated process must not be allowed to skip the drop"
        );
    }

    /// ADR 0102: the end this process keeps must not travel to a child. A
    /// handle onto a terminal reaching some unrelated process is the leak the
    /// clearing exists to prevent, and `SetHandleInformation` silently doing
    /// nothing would not otherwise show up anywhere.
    #[test]
    fn the_end_this_process_keeps_is_taken_back_out_of_inheritance() {
        use windows::Win32::Foundation::GetHandleInformation;

        let (read, write) = inheritable_pipe().expect("a pipe");
        let mut flags = 0u32;
        // SAFETY: a live handle and a stack slot for the flags.
        unsafe { GetHandleInformation(HANDLE(write.as_raw_handle()), &raw mut flags) }
            .expect("the flags");
        assert_eq!(
            flags & HANDLE_FLAG_INHERIT.0,
            HANDLE_FLAG_INHERIT.0,
            "an inheritable pipe did not start inheritable"
        );

        keep_private(&write).expect("the handle to go private");
        // SAFETY: as above.
        unsafe { GetHandleInformation(HANDLE(write.as_raw_handle()), &raw mut flags) }
            .expect("the flags");
        assert_eq!(flags & HANDLE_FLAG_INHERIT.0, 0, "the handle still travels");
        drop(read);
    }

    /// An installed client lives under a path with a space in it, so the
    /// worker's own path has to survive being put on a command line.
    #[test]
    fn the_workers_path_is_quoted_and_carries_the_geometry() {
        let line = command_line(
            Path::new(r"C:\Program Files\Lumepeer\lumepeer-terminal-worker.exe"),
            ShellSize {
                cols: 120,
                rows: 40,
            },
        );
        assert_eq!(
            line.to_string_lossy(),
            "\"C:\\Program Files\\Lumepeer\\lumepeer-terminal-worker.exe\" 120 40"
        );
    }
}
