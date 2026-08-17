//! Windows `AppContainer` confinement for the decoder worker (§11.3).
//!
//! Unlike Linux seccomp, `AppContainer` cannot be applied by an
//! already-running process to itself: Windows only offers it as a
//! process-*creation*-time restriction, via a "low-box" token built from a
//! `SECURITY_CAPABILITIES` structure passed to `CreateProcessW`. So on this
//! platform the confinement cannot happen the way
//! `crates/decoder-worker/src/main.rs`'s `sandbox::linux_seccomp` does it
//! (a running process fencing itself in); it has to happen here, in the
//! parent, at the moment the worker is spawned. [`spawn_confined`] is that
//! spawn. `crates/decoder-worker`'s own `sandbox::apply` still runs after
//! that, but on Windows it can only *verify* the confinement already took
//! effect ([`verify_confined`]) — by the time the worker's `main` is
//! running, applying AppContainer to it is no longer possible.
//!
//! What zero `SECURITY_CAPABILITIES` capabilities buys the worker:
//! - No network access at all: the Windows Filtering Platform blocks every
//!   socket for an AppContainer token that was not granted a networking
//!   capability (`internetClient` etc.), which this one never is.
//! - No filesystem access beyond handles it already holds. The ring buffer
//!   and the worker's stdin/stdout are all opened here, in this unconfined
//!   parent, and handed to the child only as already-open handles that
//!   `CreateProcessW` duplicates across the boundary. The worker never
//!   opens anything by path itself — verified empirically (§ the
//!   implementation report): an AppContainer token without an explicit ACL
//!   grant cannot even re-open a file it was already granted access to by
//!   path, because it does not carry the "bypass traverse checking"
//!   privilege other processes get by default, so path-based access grants
//!   would have to cover every ancestor directory. Handle inheritance
//!   sidesteps that entirely and matches the Linux ordering of "map ring,
//!   THEN confine" even though the underlying mechanism is process-creation
//!   time rather than self-applied.
//!
//! A job object is layered on top for defence in depth (kill-on-close, plus
//! blocking the coarse desktop/clipboard/global-atom UI surface). It is not
//! the security boundary — the AppContainer token is — it just guarantees a
//! confined worker cannot outlive its parent or fiddle with the desktop.

#![allow(
    unsafe_code,
    reason = "driving CreateProcessW's extended attribute list and the AppContainer/job-object APIs cannot be expressed in safe Rust; every block below carries a SAFETY note, per §21"
)]

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation,
};
use windows::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows::Win32::Security::{
    FreeSid, GetTokenInformation, PSID, SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES, TOKEN_QUERY,
    TokenIsAppContainer,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_UILIMIT_DESKTOP, JOB_OBJECT_UILIMIT_DISPLAYSETTINGS, JOB_OBJECT_UILIMIT_EXITWINDOWS,
    JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_HANDLES, JOB_OBJECT_UILIMIT_READCLIPBOARD,
    JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS, JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
    JOBOBJECT_BASIC_LIMIT_INFORMATION, JOBOBJECT_BASIC_UI_RESTRICTIONS,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicUIRestrictions,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, InitializeProcThreadAttributeList,
    LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcessToken, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
    PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};

use crate::error::{MediaError, Result};

fn to_wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn win_err(context: &str, e: &windows::core::Error) -> MediaError {
    MediaError::SandboxUnavailable(format!("{context}: {e}"))
}

/// Owning wrapper around an `AppContainer` SID, freed with `FreeSid`.
struct AppContainerSid(PSID);

impl Drop for AppContainerSid {
    fn drop(&mut self) {
        // SAFETY: `self.0` was allocated by `CreateAppContainerProfile` or
        // `DeriveAppContainerSidFromAppContainerName`, both of which
        // document `FreeSid` as the matching deallocator; it is not used
        // again after this call.
        unsafe {
            let _ = FreeSid(self.0);
        }
    }
}

/// Creates (or, if a previous worker registered it and never cleaned up,
/// looks up) a per-worker `AppContainer` profile with zero capabilities: no
/// network, no filesystem beyond what handle inheritance grants explicitly.
///
/// The profile is named after `unique_suffix` (see [`spawn_confined`]) so
/// concurrent workers — one per decode session — get distinct identities and
/// cannot reach each other's ring buffers even by guessing a path.
fn derive_or_create_profile(unique_suffix: &str) -> Result<AppContainerSid> {
    let name = format!("Lumepeer.DecoderWorker.{unique_suffix}");
    let wide_name = to_wide(&name);
    let wide_display = to_wide("Lumepeer decoder worker");
    let wide_desc = to_wide("Sandboxed H.264 decoder for lumepeer, design doc \u{a7}11.3");
    // SAFETY: the three name buffers are owned locals that outlive this
    // call. On success both APIs return an owned PSID that is immediately
    // wrapped in `AppContainerSid`, which frees it exactly once.
    unsafe {
        match CreateAppContainerProfile(
            PCWSTR(wide_name.as_ptr()),
            PCWSTR(wide_display.as_ptr()),
            PCWSTR(wide_desc.as_ptr()),
            None,
        ) {
            Ok(sid) => Ok(AppContainerSid(sid)),
            // A previous worker registered this exact profile name and
            // exited before `delete_profile` ran (e.g. it was killed).
            // Reuse the existing profile rather than failing the session
            // over it; see the "best effort" note on `delete_profile`.
            Err(e) if e.code() == windows::core::HRESULT::from_win32(ERROR_ALREADY_EXISTS.0) => {
                DeriveAppContainerSidFromAppContainerName(PCWSTR(wide_name.as_ptr()))
                    .map(AppContainerSid)
                    .map_err(|e| win_err("cannot derive the existing AppContainer SID", &e))
            }
            Err(e) => Err(win_err("cannot create the AppContainer profile", &e)),
        }
    }
}

/// Deletes the named `AppContainer` profile. Best-effort and non-blocking:
/// called only after the worker process has exited, and a failure here just
/// leaves the profile registered for `derive_or_create_profile` to reuse
/// next time, which is not a confinement problem — it only ever grants the
/// identity of one already-dead worker to the next one.
fn delete_profile(unique_suffix: &str) {
    let name = format!("Lumepeer.DecoderWorker.{unique_suffix}");
    let wide_name = to_wide(&name);
    // SAFETY: `wide_name` is a valid, NUL-terminated wide string alive for
    // the duration of this call.
    unsafe {
        let _ = DeleteAppContainerProfile(PCWSTR(wide_name.as_ptr()));
    }
}

/// Opens the ring buffer file with a handle inheritable by the worker. The
/// parent is unconfined, so it can open by path freely; the worker never
/// does (see the module doc comment).
fn open_inheritable_rw(path: &Path) -> Result<OwnedHandle> {
    let display = path.to_string_lossy().into_owned();
    let wide = to_wide(&display);
    let sa = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: true.into(),
    };
    // SAFETY: `wide` and `sa` are owned locals alive for the call; on
    // success `CreateFileW` returns a fresh, uniquely-owned handle that
    // `OwnedHandle` takes ownership of.
    unsafe {
        let handle = CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            Some(&raw const sa),
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
        .map_err(|e| win_err("cannot open the ring file for the worker", &e))?;
        Ok(OwnedHandle::from_raw_handle(handle.0))
    }
}

/// The two anonymous pipes the worker's stdin/stdout wire protocol needs,
/// split into "the end the child inherits" and "the end the parent keeps".
struct ChildPipes {
    child_stdin_read: OwnedHandle,
    parent_stdin_write: OwnedHandle,
    child_stdout_write: OwnedHandle,
    parent_stdout_read: OwnedHandle,
}

fn create_child_pipes() -> Result<ChildPipes> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: true.into(),
    };
    // SAFETY: `CreatePipe` populates both out-parameters with fresh,
    // uniquely-owned handles on success; `SetHandleInformation` only clears
    // the inherit flag on the copy this process keeps for itself, so the
    // child's copy (already duplicated at `CreateProcessW` time) is
    // unaffected.
    unsafe {
        let mut stdin_read = HANDLE::default();
        let mut stdin_write = HANDLE::default();
        CreatePipe(
            &raw mut stdin_read,
            &raw mut stdin_write,
            Some(&raw const sa),
            0,
        )
        .map_err(|e| win_err("cannot create the worker's stdin pipe", &e))?;
        SetHandleInformation(stdin_write, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0))
            .map_err(|e| win_err("cannot restrict stdin pipe inheritance", &e))?;

        let mut stdout_read = HANDLE::default();
        let mut stdout_write = HANDLE::default();
        CreatePipe(
            &raw mut stdout_read,
            &raw mut stdout_write,
            Some(&raw const sa),
            0,
        )
        .map_err(|e| win_err("cannot create the worker's stdout pipe", &e))?;
        SetHandleInformation(stdout_read, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0))
            .map_err(|e| win_err("cannot restrict stdout pipe inheritance", &e))?;

        Ok(ChildPipes {
            child_stdin_read: OwnedHandle::from_raw_handle(stdin_read.0),
            parent_stdin_write: OwnedHandle::from_raw_handle(stdin_write.0),
            child_stdout_write: OwnedHandle::from_raw_handle(stdout_write.0),
            parent_stdout_read: OwnedHandle::from_raw_handle(stdout_read.0),
        })
    }
}

/// Creates a job object that kills its member processes when its last
/// handle closes, and blocks the coarse desktop/clipboard/global-atom UI
/// surface. Defence in depth (module doc comment): not the security
/// boundary, the AppContainer token is.
fn create_restricted_job() -> Result<OwnedHandle> {
    // SAFETY: `job` is a fresh, uniquely-owned handle on success, taken over
    // by `OwnedHandle`; the two `SetInformationJobObject` calls each pass a
    // pointer to a same-scope local of exactly the size given.
    unsafe {
        let job = CreateJobObjectW(None, PCWSTR::null())
            .map_err(|e| win_err("cannot create the job object", &e))?;

        let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
            UIRestrictionsClass: JOB_OBJECT_UILIMIT_HANDLES
                | JOB_OBJECT_UILIMIT_READCLIPBOARD
                | JOB_OBJECT_UILIMIT_WRITECLIPBOARD
                | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS
                | JOB_OBJECT_UILIMIT_DESKTOP
                | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
                | JOB_OBJECT_UILIMIT_GLOBALATOMS
                | JOB_OBJECT_UILIMIT_EXITWINDOWS,
        };
        SetInformationJobObject(
            job,
            JobObjectBasicUIRestrictions,
            ptr::from_ref(&ui).cast(),
            u32::try_from(size_of_val(&ui)).unwrap_or(0),
        )
        .map_err(|e| win_err("cannot set the job's UI restrictions", &e))?;

        let ext = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
            BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
                LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                ..Default::default()
            },
            ..Default::default()
        };
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            ptr::from_ref(&ext).cast(),
            u32::try_from(size_of_val(&ext)).unwrap_or(0),
        )
        .map_err(|e| win_err("cannot set the job's kill-on-close limit", &e))?;

        Ok(OwnedHandle::from_raw_handle(job.0))
    }
}

/// Owns the buffer behind `CreateProcessW`'s extended attribute list and the
/// `SECURITY_CAPABILITIES` value it points to, so both stay alive from
/// `InitializeProcThreadAttributeList` through the `CreateProcessW` call
/// that consumes them.
struct AttributeList {
    buffer: Vec<u8>,
    _capabilities: Box<SECURITY_CAPABILITIES>,
}

impl AttributeList {
    fn security_capabilities(sid: PSID) -> Result<Self> {
        let mut capabilities = Box::new(SECURITY_CAPABILITIES {
            AppContainerSid: sid,
            Capabilities: ptr::null_mut(),
            CapabilityCount: 0,
            Reserved: 0,
        });
        // SAFETY: the first `InitializeProcThreadAttributeList` call only
        // measures the required size (`None` list, per its own contract);
        // `buffer` is then sized exactly to that and kept alive as long as
        // `Self` is, which is what the second call and `UpdateProcThreadAttribute`
        // require. `capabilities` is heap-allocated so its address is
        // stable even though `Self` itself moves.
        unsafe {
            let mut size: usize = 0;
            let _ = InitializeProcThreadAttributeList(None, 1, None, &raw mut size);
            let mut buffer = vec![0u8; size];
            let list = LPPROC_THREAD_ATTRIBUTE_LIST(buffer.as_mut_ptr().cast());
            InitializeProcThreadAttributeList(Some(list), 1, None, &raw mut size)
                .map_err(|e| win_err("cannot initialize the process attribute list", &e))?;
            UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
                Some(ptr::from_mut(capabilities.as_mut()).cast()),
                size_of::<SECURITY_CAPABILITIES>(),
                None,
                None,
            )
            .map_err(|e| win_err("cannot set the security-capabilities attribute", &e))?;
            Ok(Self {
                buffer,
                _capabilities: capabilities,
            })
        }
    }

    fn as_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        LPPROC_THREAD_ATTRIBUTE_LIST(self.buffer.as_mut_ptr().cast())
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        let list = self.as_ptr();
        // SAFETY: `list` was produced by a successful
        // `InitializeProcThreadAttributeList` call in `security_capabilities`
        // and is only ever deleted once, here.
        unsafe {
            DeleteProcThreadAttributeList(list);
        }
    }
}

/// The confined worker process, plus enough state to clean up its
/// `AppContainer` profile once it has exited.
#[derive(Debug)]
pub(crate) struct ConfinedProcess {
    process: OwnedHandle,
    // Closing the last handle to this job terminates the worker
    // (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`), which is why it is kept alive
    // for as long as `ConfinedProcess` is.
    _job: OwnedHandle,
    pid: u32,
    profile_suffix: String,
    profile_deleted: bool,
}

impl ConfinedProcess {
    pub(crate) fn id(&self) -> u32 {
        self.pid
    }

    pub(crate) fn kill(&mut self) {
        // SAFETY: `self.process` is a valid, open process handle for as
        // long as `self` exists.
        unsafe {
            let _ = TerminateProcess(HANDLE(self.process.as_raw_handle()), 1);
        }
    }

    pub(crate) fn wait(&mut self) {
        // SAFETY: as above.
        unsafe {
            let _ = WaitForSingleObject(HANDLE(self.process.as_raw_handle()), u32::MAX);
        }
        self.cleanup_profile();
    }

    fn cleanup_profile(&mut self) {
        if !self.profile_deleted {
            delete_profile(&self.profile_suffix);
            self.profile_deleted = true;
        }
    }
}

impl Drop for ConfinedProcess {
    fn drop(&mut self) {
        // Best-effort: `DecoderHandle`'s own `Drop`/`shutdown` always call
        // `kill()` then `wait()` first (mirroring the non-Windows path), so
        // by the time this runs the profile is normally already cleaned up;
        // this is only a backstop.
        self.cleanup_profile();
    }
}

/// The confined worker process plus its stdin/stdout ends of the pipes
/// `spawn_confined` wired up.
pub(crate) type ConfinedWorker = (
    ConfinedProcess,
    Box<dyn super::DebugWrite>,
    Box<dyn super::DebugRead>,
);

/// Spawns `program` (the decoder worker binary) already confined inside a
/// fresh `AppContainer` with no capabilities, its stdio wired to pipes and
/// `ring_path`'s file handed over as an inherited, already-open handle.
///
/// # Errors
/// [`MediaError::SandboxUnavailable`] if any step of building or launching
/// the confined process fails — per §11.3 that must abort the spawn
/// entirely rather than fall back to an unconfined worker.
pub(crate) fn spawn_confined(program: &Path, ring_path: &Path) -> Result<ConfinedWorker> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    // `CreateProcessW`'s `lpApplicationName`, unlike `std::process::Command`
    // (which this replaces on Windows), does not infer a missing `.exe`
    // extension on an already-qualified path - it just fails the lookup. Add
    // it here so `spawn_with` keeps behaving the same regardless of whether
    // the caller remembered the extension, matching `Command`'s own leniency.
    let program_buf;
    let program = if program.extension().is_none() {
        program_buf = program.with_extension("exe");
        program_buf.as_path()
    } else {
        program
    };

    let profile_suffix = format!(
        "{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );

    let sid = derive_or_create_profile(&profile_suffix)?;
    let ring_handle = open_inheritable_rw(ring_path)?;
    let pipes = create_child_pipes()?;
    let job = create_restricted_job()?;
    let mut attrs = AttributeList::security_capabilities(sid.0)?;

    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = u32::try_from(size_of::<STARTUPINFOEXW>()).unwrap_or(0);
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = HANDLE(pipes.child_stdin_read.as_raw_handle());
    startup.StartupInfo.hStdOutput = HANDLE(pipes.child_stdout_write.as_raw_handle());
    // Best-effort: if the parent has no inheritable stderr (e.g. it is not
    // attached to a console), the worker just loses its tracing output —
    // a diagnostics gap, not a §11.3 safety gap, so a failure here is not
    // fatal to the spawn.
    // SAFETY: no preconditions beyond the process having a standard error
    // handle at all, which every host process here does.
    startup.StartupInfo.hStdError =
        unsafe { GetStdHandle(STD_ERROR_HANDLE) }.unwrap_or(HANDLE::default());
    startup.lpAttributeList = attrs.as_ptr();

    let program_str = program
        .to_str()
        .ok_or_else(|| MediaError::DecoderWorker("worker path is not valid UTF-16".to_owned()))?;
    let ring_str = ring_path
        .to_str()
        .ok_or_else(|| MediaError::DecoderWorker("ring path is not valid UTF-16".to_owned()))?;
    // The worker never reopens the ring by path (module doc comment); the
    // path is passed only for logging on the worker side. The handle value
    // is what `SharedRing::from_raw_handle` actually maps.
    let cmd_line = format!(
        "\"{program_str}\" \"{ring_str}\" {}",
        ring_handle.as_raw_handle() as isize
    );
    let mut cmdline_wide = to_wide(&cmd_line);
    let program_wide = to_wide(program_str);
    let cwd = program.parent().unwrap_or_else(|| Path::new("."));
    let cwd_wide = to_wide(&cwd.to_string_lossy());

    let mut pi = PROCESS_INFORMATION::default();
    // SAFETY: `program_wide`/`cmdline_wide`/`cwd_wide` are owned locals
    // that outlive this call; `startup` (and the attribute list it points
    // to via `attrs`) is valid until `attrs` drops, after this call
    // returns; `pi` is zero-initialized and only read below once
    // `CreateProcessW` has reported success. `bInheritHandles: true` is
    // required so the ring/pipe handles prepared above cross into the
    // child; every *other* inheritable handle in this process crosses too
    // (a residual risk noted in the implementation report), which the
    // AppContainer's own denial of filesystem/network access bounds.
    let spawn_result = unsafe {
        CreateProcessW(
            PCWSTR(program_wide.as_ptr()),
            Some(PWSTR(cmdline_wide.as_mut_ptr())),
            None,
            None,
            true,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT,
            None,
            PCWSTR(cwd_wide.as_ptr()),
            &raw const startup.StartupInfo,
            &raw mut pi,
        )
    };

    // The child's own copies of these were duplicated at CreateProcessW
    // time (or the call failed and none of this matters); either way this
    // process does not need them anymore.
    drop(attrs);
    drop(pipes.child_stdin_read);
    drop(pipes.child_stdout_write);
    drop(ring_handle);
    spawn_result.map_err(|e| win_err("cannot spawn the confined decoder worker", &e))?;

    // SAFETY: `pi.hProcess`/`pi.hThread` are fresh, uniquely-owned handles
    // from the `CreateProcessW` success just checked above.
    let process = unsafe { OwnedHandle::from_raw_handle(pi.hProcess.0) };
    let thread = unsafe { OwnedHandle::from_raw_handle(pi.hThread.0) };

    // Assign to the job, THEN resume: the worker never runs a single
    // instruction outside the job's restrictions (it is still suspended
    // from `CREATE_SUSPENDED` above).
    // SAFETY: `job`/`process`/`thread` are the handles created/received
    // above and are all still valid at this point.
    unsafe {
        AssignProcessToJobObject(HANDLE(job.as_raw_handle()), HANDLE(process.as_raw_handle()))
            .map_err(|e| win_err("cannot assign the worker to its job object", &e))?;
        ResumeThread(HANDLE(thread.as_raw_handle()));
    }
    drop(thread);

    let confined = ConfinedProcess {
        process,
        _job: job,
        pid: pi.dwProcessId,
        profile_suffix,
        profile_deleted: false,
    };

    let stdin: Box<dyn super::DebugWrite> = Box::new(std::fs::File::from(pipes.parent_stdin_write));
    let stdout: Box<dyn super::DebugRead> = Box::new(std::fs::File::from(pipes.parent_stdout_read));

    Ok((confined, stdin, stdout))
}

/// Confirms this process is actually running inside an `AppContainer`.
///
/// The confinement itself was applied by the parent at `CreateProcessW`
/// time ([`spawn_confined`]); by the time the worker's own `main` runs, it
/// is too late to *apply* AppContainer, only to check it. This is that
/// check — called from `crates/decoder-worker`'s `sandbox::apply` — so that
/// running the worker binary directly (bypassing `DecoderHandle::spawn`,
/// which is the only caller of `spawn_confined`) fails closed instead of
/// decoding unconfined.
///
/// # Errors
/// [`MediaError::SandboxUnavailable`] if the token cannot be queried, or if
/// it can and says this process is not in an `AppContainer`.
pub fn verify_confined() -> Result<()> {
    // SAFETY: `token` is populated by `OpenProcessToken` on success and
    // owned by the `OwnedHandle` wrapper from that point on, closed exactly
    // once when it drops. `is_app_container` is a plain `u32` out-buffer
    // whose address and length are passed consistently to
    // `GetTokenInformation`.
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token)
            .map_err(|e| win_err("cannot open this process's own token", &e))?;
        let token = OwnedHandle::from_raw_handle(token.0);

        let mut is_app_container: u32 = 0;
        let mut returned = 0u32;
        GetTokenInformation(
            HANDLE(token.as_raw_handle()),
            TokenIsAppContainer,
            Some(ptr::from_mut(&mut is_app_container).cast()),
            u32::try_from(size_of::<u32>()).unwrap_or(0),
            &raw mut returned,
        )
        .map_err(|e| win_err("cannot query TokenIsAppContainer", &e))?;

        if is_app_container == 0 {
            return Err(MediaError::SandboxUnavailable(
                "this process is not running inside an AppContainer; refusing to decode unconfined"
                    .to_owned(),
            ));
        }
    }
    Ok(())
}
