//! `lumepeer-terminal-worker` — the pseudo-console, on the far side of the
//! privilege drop (ADR 0102).
//!
//! ## Why there is a second process at all
//!
//! ADR 0079 says the shell a guest asks for runs as the person sitting at the
//! host, never as the elevated client. On Windows that meant
//! `CreateProcessAsUserW` with Explorer's token — and ADR 0057 then shipped
//! the client with `requireAdministrator`, which is exactly the configuration
//! that call refuses. An elevated administrator token has
//! `SE_INCREASE_QUOTA_NAME` disabled and no `SE_ASSIGNPRIMARYTOKEN_NAME` at
//! all, so the call comes back `ERROR_PRIVILEGE_NOT_HELD` and, correctly, no
//! shell is started. Enabling the one privilege that *is* present does not
//! help; the other cannot be enabled because it is not in the token.
//!
//! `CreateProcessWithTokenW` wants only `SE_IMPERSONATE_NAME`, which an
//! elevated client does hold, and it starts a process as the interactive user
//! at medium integrity. What it will not do is carry a
//! `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`, because it ignores
//! `lpAttributeList` — and a `ConPTY` child *is* an attribute list. So the
//! client uses it to start **this** binary, and this binary, already being the
//! right person, creates the pseudo-console and starts the shell with an
//! ordinary `CreateProcessW`. No token is juggled here at all.
//!
//! ## What this is not
//!
//! Not a privileged helper, and not a way in. It holds nothing the person who
//! runs it does not already hold: run it by hand and you get your own shell,
//! which you could have had from the Start menu. The decision about whether a
//! guest may have a shell is made before this process exists, by
//! `terminal_allows(&peer)` on the actor loop, and is re-read for every shell
//! (ADR 0079).
//!
//! Nothing that crosses this process is logged or written anywhere. A
//! transcript is what §15 spends its time not keeping, and this is the one
//! place every byte of one passes through.
//!
//! ## What it talks
//!
//! Two inherited anonymous pipes on its own standard handles. In come the
//! frames of [`lumepeer_terminal::worker_protocol`] — keystrokes, and the
//! geometry of the window they are being typed into. Out goes whatever the
//! shell said, unframed, because that is the only thing travelling that way.
//! Either pipe closing ends the process, and the shell goes with it: it is in
//! a job object whose closing kills it, so the orphan ADR 0079 forbids cannot
//! outlive even a `TerminateProcess` of this worker.

#![cfg_attr(not(target_os = "windows"), forbid(unsafe_code))]
#![cfg_attr(
    target_os = "windows",
    allow(
        unsafe_code,
        reason = "CreatePseudoConsole, the process-thread attribute list and the \
                  job object have no safe bindings; same justification standard \
                  as crates/terminal's own Win32 surface (ADR 0079, ADR 0102)"
    )
)]
#![allow(
    unreachable_pub,
    reason = "binary crate: `pub` marks this worker's own surface, not an API"
)]

fn main() -> std::process::ExitCode {
    #[cfg(not(target_os = "windows"))]
    {
        // Staged for every target because Tauri's `externalBin` is not
        // per-platform, and inert everywhere but Windows: Unix needs no drop
        // at all, because the client there already is the interactive user
        // (ADR 0079, `crates/terminal/src/unix.rs`).
        eprintln!(
            "lumepeer-terminal-worker exists only on Windows: every other \
             platform runs the shell in the client's own process"
        );
        std::process::ExitCode::FAILURE
    }

    #[cfg(target_os = "windows")]
    windows_main::run()
}

#[cfg(target_os = "windows")]
mod windows_main {
    use std::ffi::{OsString, c_void};
    use std::io::{Read, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use std::process::ExitCode;
    use std::sync::{Arc, Mutex};

    use lumepeer_terminal::worker_protocol::{
        FRAME_HEADER_BYTES, FRAME_INPUT, FRAME_PAYLOAD_MAX_BYTES, FRAME_RESIZE, WORKER_READY,
        decode_resize,
    };
    use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows::Win32::System::Console::{
        COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
    };
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows::Win32::System::Pipes::CreatePipe;
    use windows::Win32::System::Threading::{
        CREATE_UNICODE_ENVIRONMENT, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
        InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
        PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, STARTF_USESTDHANDLES,
        STARTUPINFOEXW, STARTUPINFOW, UpdateProcThreadAttribute,
    };
    use windows::core::PWSTR;

    /// Geometry to start at when the client named none, in character cells.
    ///
    /// Only reachable by running this binary by hand; the client always says.
    const DEFAULT_COLS: u16 = 80;
    /// Companion of [`DEFAULT_COLS`].
    const DEFAULT_ROWS: u16 = 24;

    /// Biggest chunk of shell output moved in one go, in bytes.
    const OUTPUT_CHUNK_BYTES: usize = 16 * 1024;

    pub(crate) fn run() -> ExitCode {
        let (cols, rows) = geometry();
        match start(cols, rows) {
            Ok(()) => ExitCode::SUCCESS,
            // No `tracing`, and deliberately nothing about what was typed:
            // this is the process every byte of a transcript passes through
            // (§15). The client turns a worker that died into the same
            // refusal it would have shown anyway.
            Err(what) => {
                eprintln!("lumepeer-terminal-worker: {what}");
                ExitCode::FAILURE
            }
        }
    }

    /// The geometry the client asked for, as two positional arguments.
    fn geometry() -> (u16, u16) {
        let mut args = std::env::args().skip(1);
        let cols = args.next().and_then(|a| a.parse().ok());
        let rows = args.next().and_then(|a| a.parse().ok());
        (
            cols.filter(|c| *c > 0).unwrap_or(DEFAULT_COLS),
            rows.filter(|r| *r > 0).unwrap_or(DEFAULT_ROWS),
        )
    }

    /// The shell this host runs, from this host's own environment (ADR 0079).
    ///
    /// Read here rather than passed in: a command line the client composed is
    /// a command line something upstream of the client could have influenced,
    /// and "may start a shell" is not "may start any program". The guest never
    /// names the program, and neither does the client.
    fn host_shell() -> OsString {
        lumepeer_terminal::host_shell()
    }

    /// Creates the console, starts the shell under it and relays until one of
    /// the pipes closes.
    fn start(cols: u16, rows: u16) -> Result<(), String> {
        let (input_read, input_write) = pipe()?;
        let (output_read, output_write) = pipe()?;

        // SAFETY: both handles are live pipe ends this function owns, and the
        // geometry is bounded by construction.
        let console = unsafe {
            CreatePseudoConsole(
                COORD {
                    X: clamp_cell(cols),
                    Y: clamp_cell(rows),
                },
                HANDLE(input_read.as_raw()),
                HANDLE(output_write.as_raw()),
                0,
            )
        }
        .map_err(|error| format!("no pseudo-console could be created: {error}"))?;
        let console = Arc::new(PseudoConsole(Mutex::new(console)));
        // The console owns copies of these two ends now. Holding ours would
        // keep the pipes open after the shell exits, so the reader below would
        // block for ever instead of seeing EOF — which is how an orphan is
        // born.
        drop(input_read);
        drop(output_write);

        // The job first, so there is no window in which a started shell is not
        // yet covered by the thing that guarantees it dies with this process.
        let job = Job::create()?;
        let shell = spawn_shell(&console)?;
        job.adopt(&shell)?;

        // SAFETY: each is a pipe end this function owns and has not closed;
        // `File` takes ownership and closes it exactly once.
        let mut from_shell = unsafe { std::fs::File::from_raw_handle(output_read.into_raw()) };
        let mut to_shell = unsafe { std::fs::File::from_raw_handle(input_write.into_raw()) };

        // Only now is there a shell, so only now is the client told there is
        // one. Everything above this line that went wrong reached the client
        // as a closed pipe instead, and became a refusal rather than a
        // terminal that opened and shut (ADR 0102).
        {
            let mut stdout = std::io::stdout();
            stdout
                .write_all(&[WORKER_READY])
                .and_then(|()| stdout.flush())
                .map_err(|error| format!("the client stopped listening: {error}"))?;
        }

        // Output back to the client on this thread's twin: a blocking read on
        // the console's pipe, straight onto stdout, with no buffer of its own
        // kept anywhere.
        std::thread::spawn(move || {
            let mut stdout = std::io::stdout();
            let mut buffer = vec![0u8; OUTPUT_CHUNK_BYTES];
            loop {
                let read = match from_shell.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => read,
                };
                if stdout.write_all(&buffer[..read]).is_err() || stdout.flush().is_err() {
                    break;
                }
            }
            // The shell's console reached EOF, which means the shell is gone.
            // Leaving the process is what closes the job and the console, so
            // the client sees its own end of the pipe close and reports the
            // shell as ended (ADR 0079).
            std::process::exit(0);
        });

        relay(&mut to_shell, &console);
        // Falling out of here drops the job, which kills the shell.
        Ok(())
    }

    /// Reads frames from the client until the pipe closes.
    ///
    /// A frame this build cannot make sense of is skipped rather than fatal,
    /// for the same reason the guest's own decoder does it: somebody is typing
    /// into this shell, and one bad frame is not a reason to take it away.
    fn relay(to_shell: &mut std::fs::File, console: &PseudoConsole) {
        let mut stdin = std::io::stdin().lock();
        let mut header = [0u8; FRAME_HEADER_BYTES];
        loop {
            if stdin.read_exact(&mut header).is_err() {
                return;
            }
            let kind = header[0];
            let length = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
            if length > FRAME_PAYLOAD_MAX_BYTES {
                // A length nothing legitimate produces. Stopping rather than
                // skipping, because the stream is no longer framed and every
                // byte after it would be read as a header.
                return;
            }
            let mut payload = vec![0u8; length];
            if stdin.read_exact(&mut payload).is_err() {
                return;
            }
            match kind {
                FRAME_INPUT => {
                    if to_shell.write_all(&payload).is_err() || to_shell.flush().is_err() {
                        return;
                    }
                }
                FRAME_RESIZE => {
                    if let Some((cols, rows)) = decode_resize(&payload) {
                        console.resize(cols, rows);
                    }
                }
                _ => {}
            }
        }
    }

    /// Starts the host's shell attached to `console`.
    ///
    /// An ordinary `CreateProcessW`, with no token anywhere: this process is
    /// already the person the shell is meant to belong to, which is the entire
    /// point of there being a second process (ADR 0102).
    fn spawn_shell(console: &PseudoConsole) -> Result<OwnedHandle, String> {
        let mut attributes = AttributeList::with_pseudo_console(console)?;
        let startup = STARTUPINFOEXW {
            StartupInfo: STARTUPINFOW {
                cb: u32::try_from(size_of::<STARTUPINFOEXW>()).unwrap_or(0),
                // Named, and named invalid. This process's *own* standard
                // handles are the client's two pipes, and a child started
                // without this takes those instead of the pseudo-console's:
                // the shell then writes its banner straight down the wire and
                // reads keystrokes out of the same pipe this worker is reading
                // frames from, so nothing it is typed ever reaches it and
                // nothing it says is ever a console. `portable-pty` carries
                // the same three lines for the same reason.
                dwFlags: STARTF_USESTDHANDLES,
                hStdInput: INVALID_HANDLE_VALUE,
                hStdOutput: INVALID_HANDLE_VALUE,
                hStdError: INVALID_HANDLE_VALUE,
                ..STARTUPINFOW::default()
            },
            lpAttributeList: attributes.as_mut(),
        };

        let mut command: Vec<u16> = host_shell()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut info = PROCESS_INFORMATION::default();

        // SAFETY: a NUL-terminated mutable command line, an initialized
        // attribute list carrying the pseudo-console, and an output slot for
        // the process ids. A failure writes nothing to `info`.
        unsafe {
            windows::Win32::System::Threading::CreateProcessW(
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
        }
        .map_err(|error| format!("the shell could not be started: {error}"))?;

        // SAFETY: both handles were just written by a successful spawn; the
        // thread handle is taken only to be closed, which stops it leaking.
        let process = unsafe { OwnedHandle::from_raw_handle(info.hProcess.0) };
        // SAFETY: same, and closing it does not affect the running process.
        unsafe {
            let _ = CloseHandle(info.hThread);
        }
        Ok(process)
    }

    /// One anonymous pipe, as two owned ends.
    fn pipe() -> Result<(OwnedHandle, OwnedHandle), String> {
        let mut read = HANDLE::default();
        let mut write = HANDLE::default();
        // SAFETY: two stack slots the call fills in; default attributes and
        // the system's own buffer size.
        unsafe { CreatePipe(&raw mut read, &raw mut write, None, 0) }
            .map_err(|error| format!("no pipe could be created: {error}"))?;
        // SAFETY: two handles CreatePipe just wrote and nothing else owns.
        unsafe {
            Ok((
                OwnedHandle::from_raw_handle(read.0),
                OwnedHandle::from_raw_handle(write.0),
            ))
        }
    }

    /// The pseudo-console, closed when this process ends.
    struct PseudoConsole(Mutex<HPCON>);

    // SAFETY: an HPCON is an opaque kernel-backed handle with no thread
    // affinity; the only calls made on it here are ResizePseudoConsole and
    // ClosePseudoConsole, both documented as callable from any thread, and the
    // `Mutex` is what serializes them.
    unsafe impl Send for PseudoConsole {}
    unsafe impl Sync for PseudoConsole {}

    impl PseudoConsole {
        /// Tells the shell its window changed size, ignoring a refusal: the
        /// shell is still running and still usable at the old geometry.
        fn resize(&self, cols: u16, rows: u16) {
            if let Ok(console) = self.0.lock() {
                // SAFETY: a live HPCON this type owns, with a bounded geometry.
                unsafe {
                    let _ = ResizePseudoConsole(
                        *console,
                        COORD {
                            X: clamp_cell(cols),
                            Y: clamp_cell(rows),
                        },
                    );
                }
            }
        }
    }

    impl Drop for PseudoConsole {
        fn drop(&mut self) {
            if let Ok(console) = self.0.lock() {
                // SAFETY: an HPCON from a successful CreatePseudoConsole,
                // closed exactly once because this type owns it.
                unsafe { ClosePseudoConsole(*console) };
            }
        }
    }

    /// A job object the shell is put in, so that it cannot outlive this
    /// process however this process ends.
    ///
    /// Closing the console would usually be enough — a shell whose console is
    /// gone exits. "Usually" is not the standard ADR 0079 sets for a process
    /// on somebody else's machine, and a `TerminateProcess` of this worker
    /// runs no destructor at all. The kernel closing the last job handle does
    /// not depend on this process getting to do anything.
    struct Job(OwnedHandle);

    impl Job {
        fn create() -> Result<Self, String> {
            // SAFETY: no name and default attributes; the handle is owned below.
            let job = unsafe { CreateJobObjectW(None, None) }
                .map_err(|error| format!("no job object could be created: {error}"))?;
            // SAFETY: a handle CreateJobObjectW just returned and nothing else owns.
            let job: OwnedHandle = unsafe { OwnedHandle::from_raw_handle(job.0) };

            let limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
                BasicLimitInformation:
                    windows::Win32::System::JobObjects::JOBOBJECT_BASIC_LIMIT_INFORMATION {
                        LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                        ..Default::default()
                    },
                ..Default::default()
            };
            // SAFETY: a live job handle and a fully initialized limit struct
            // of exactly the size named.
            unsafe {
                SetInformationJobObject(
                    HANDLE(job.as_raw()),
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_ref(&limits).cast::<c_void>(),
                    u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).unwrap_or(0),
                )
            }
            .map_err(|error| format!("the job object would not take its limits: {error}"))?;
            Ok(Self(job))
        }

        /// Puts `process` in the job. A shell that could not be adopted is not
        /// a shell this worker keeps: it would be the orphan the job exists to
        /// prevent, so the caller ends instead.
        fn adopt(&self, process: &OwnedHandle) -> Result<(), String> {
            // SAFETY: two live handles this process owns.
            unsafe { AssignProcessToJobObject(HANDLE(self.0.as_raw()), HANDLE(process.as_raw())) }
                .map_err(|error| format!("the shell could not be put in a job: {error}"))
        }
    }

    /// A process-thread attribute list carrying exactly one attribute: the
    /// pseudo-console the child is to be attached to.
    struct AttributeList {
        storage: Vec<u8>,
    }

    impl AttributeList {
        fn with_pseudo_console(console: &PseudoConsole) -> Result<Self, String> {
            let mut needed = 0usize;
            // The documented two-call shape: the first call always "fails"
            // with the size it wants, which is why its result is ignored.
            // SAFETY: a null list is what asks for the size.
            unsafe {
                let _ = InitializeProcThreadAttributeList(None, 1, None, &raw mut needed);
            }
            if needed == 0 {
                return Err("no attribute list size".to_owned());
            }
            let mut storage = vec![0u8; needed];
            let list = LPPROC_THREAD_ATTRIBUTE_LIST(storage.as_mut_ptr().cast::<c_void>());
            // SAFETY: `storage` is exactly the size the call above asked for
            // and outlives the list, which is deleted in `Drop`.
            unsafe { InitializeProcThreadAttributeList(Some(list), 1, None, &raw mut needed) }
                .map_err(|error| format!("no attribute list: {error}"))?;

            let handle = console
                .0
                .lock()
                .map_err(|_| "a poisoned console".to_owned())?;
            // SAFETY: an initialized list with room for one attribute, and an
            // HPCON that outlives the spawn it is used for — the caller holds
            // the console for the whole call.
            unsafe {
                UpdateProcThreadAttribute(
                    list,
                    0,
                    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                    Some(handle.0 as *const c_void),
                    size_of::<HPCON>(),
                    None,
                    None,
                )
            }
            .map_err(|error| format!("the pseudo-console would not go in the list: {error}"))?;
            drop(handle);

            Ok(Self { storage })
        }

        fn as_mut(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
            LPPROC_THREAD_ATTRIBUTE_LIST(self.storage.as_mut_ptr().cast::<c_void>())
        }
    }

    impl Drop for AttributeList {
        fn drop(&mut self) {
            // SAFETY: a list this type initialized and owns, deleted once.
            unsafe { DeleteProcThreadAttributeList(self.as_mut()) };
        }
    }

    /// A geometry as a console coordinate, saturating rather than wrapping:
    /// a silently negative console size is not something this should produce.
    fn clamp_cell(cells: u16) -> i16 {
        i16::try_from(cells).unwrap_or(i16::MAX)
    }

    /// A `HANDLE` as the raw pointer `OwnedHandle` speaks, and back.
    trait RawHandleExt {
        fn as_raw(&self) -> *mut c_void;
        fn into_raw(self) -> *mut c_void;
    }

    impl RawHandleExt for OwnedHandle {
        fn as_raw(&self) -> *mut c_void {
            std::os::windows::io::AsRawHandle::as_raw_handle(self)
        }

        fn into_raw(self) -> *mut c_void {
            std::os::windows::io::IntoRawHandle::into_raw_handle(self)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The geometry is the client's, and a missing or nonsensical one
        /// still leaves a usable console rather than a zero-sized one.
        #[test]
        fn a_geometry_is_never_zero() {
            assert_eq!(clamp_cell(0), 0);
            assert_eq!(clamp_cell(u16::MAX), i16::MAX);
            let (cols, rows) = geometry();
            assert!(cols > 0 && rows > 0);
        }

        /// ADR 0079: the program is the machine's own, never anything that
        /// travelled. This worker reads it from its own environment for the
        /// same reason the client did.
        #[test]
        fn the_shell_is_the_machines_own() {
            assert!(!host_shell().is_empty());
        }
    }
}
