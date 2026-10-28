//! The access policy is changed by the client started again elevated, and the
//! session host is told (ADR 0169 §5).
//!
//! The device password, its second factor, the unattended role, the logon
//! screen and the trusted devices decide who gets into this machine. They live
//! where only `LocalSystem` and administrators can write
//! (`program_data::user_directory`, ADR 0123), and nothing the person's
//! ordinary programs can reach may change them: not the client channel
//! (`host_channel_protocol`, which has no message for it) and not this module.
//! What it gives is the two ends of the one path that does change them:
//!
//! - **The way in.** [`open_elevated`] starts this same desktop binary again
//!   with [`crate::ACCESS_SETTINGS_ARG`] through `ShellExecuteExW` and the
//!   `runas` verb: one UAC prompt, at the moment of the change. On a standard
//!   account that prompt asks for an administrator's credentials, as Windows
//!   does for any machine-wide security setting. The account whose policy is
//!   edited travels as a SID on the command line, because with an
//!   administrator's credentials the elevated process runs as *that*
//!   administrator, and the policy to edit is still the signed-in person's.
//!   Naming any account there gains nothing: whoever passes the prompt is an
//!   administrator, who can open every account's directory anyway.
//! - **The signal.** Once it has written, that process raises
//!   [`policy_changed_event`] for the account, and the session host — which
//!   created the event and waits on it ([`PolicyChanges`]) — reads the policy
//!   again without restarting. The event's list admits `SY` and `BA` only
//!   ([`POLICY_CHANGED_SDDL`]), so an ordinary program can neither raise it
//!   nor wait on it, and it lives in `Global\`, where an ordinary program
//!   cannot create it first either (creating there needs
//!   `SeCreateGlobalPrivilege`, which a medium-integrity token lacks).

#![allow(
    unsafe_code,
    reason = "a named event and ShellExecuteExW have no safe bindings; same \
              justification standard as host_role.rs and the rest of this \
              crate's Win32 surface (ADR 0043, ADR 0049)"
)]

use std::path::Path;

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_CANCELLED, HANDLE, HLOCAL, LocalFree, WAIT_OBJECT_0,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::Threading::{
    CreateEventW, EVENT_MODIFY_STATE, OpenEventW, SetEvent, WaitForSingleObject,
};
use windows::Win32::UI::Shell::{SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HRESULT, PCWSTR};

/// Prefix of the per-account event the session host waits on; the account's
/// SID follows.
const POLICY_CHANGED_PREFIX: &str = r"Global\lumepeer-policy-changed-";

/// Access list of that event: `LocalSystem` (the session host, which waits)
/// and administrators (the `--access-settings` process, which signals), and
/// nobody else — not the person's ordinary programs, which could otherwise
/// make the host re-read at will or swallow the signal.
const POLICY_CHANGED_SDDL: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)";

/// The event that says the access policy of the account `sid` was written;
/// `None` for anything that is not a SID, so a name built from it can never
/// reach outside this prefix.
#[must_use]
pub fn policy_changed_event(sid: &str) -> Option<String> {
    crate::frame::is_sid_string(sid).then(|| format!("{POLICY_CHANGED_PREFIX}{sid}"))
}

/// The account `--access-settings` edits, read from the arguments after
/// [`crate::ACCESS_SETTINGS_ARG`]; `None` when the flag is not there.
///
/// `Some(None)` when the flag is there with no SID after it, or with
/// something that is not one: such a run edits nothing.
#[must_use]
pub fn requested_account(args: &[String]) -> Option<Option<String>> {
    let at = args
        .iter()
        .position(|arg| arg == crate::ACCESS_SETTINGS_ARG)?;
    Some(
        args.get(at + 1)
            .filter(|sid| crate::frame::is_sid_string(sid))
            .cloned(),
    )
}

/// Why [`open_elevated`] did not start the settings window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenError {
    /// The person said no at the UAC prompt, or could not give an
    /// administrator's credentials.
    Cancelled,
    /// `sid` is not a SID.
    NotAnAccount,
    /// Windows refused to start it, with this Win32 code.
    Failed(u32),
}

/// Starts `exe` — this desktop binary — elevated with `--access-settings sid`:
/// the UAC prompt, and then the settings window (ADR 0169 §5).
///
/// Returns once the prompt has been answered; the window is its own process
/// from then on.
///
/// # Errors
/// [`OpenError`] when nothing was started.
pub fn open_elevated(exe: &Path, sid: &str) -> Result<(), OpenError> {
    if !crate::frame::is_sid_string(sid) {
        return Err(OpenError::NotAnAccount);
    }
    let verb = wide("runas");
    let file = wide(&exe.to_string_lossy());
    let parameters = wide(&format!("{} {sid}", crate::ACCESS_SETTINGS_ARG));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: u32::try_from(size_of::<SHELLEXECUTEINFOW>()).unwrap_or(0),
        // Synchronous: the caller is a blocking task that wants the answer to
        // the prompt, not a window that pumps messages for an async launch.
        fMask: SEE_MASK_NOASYNC,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(parameters.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: `info` and the three wide strings it points into are locals
    // that outlive the call; no process handle is asked for, so there is
    // nothing to close afterwards.
    match unsafe { ShellExecuteExW(&raw mut info) } {
        Ok(()) => {
            tracing::info!("started the access settings window elevated (ADR 0169 §5)");
            Ok(())
        }
        Err(error) if error.code() == HRESULT::from_win32(ERROR_CANCELLED.0) => {
            tracing::info!(
                "the access settings window was not started: the UAC prompt was declined"
            );
            Err(OpenError::Cancelled)
        }
        Err(error) => {
            tracing::warn!(%error, "cannot start the access settings window elevated");
            Err(OpenError::Failed(win32_code(error.code())))
        }
    }
}

/// The session host's end: the event it created for its account, read once
/// per tick.
#[derive(Debug)]
pub struct PolicyChanges {
    event: HANDLE,
}

// SAFETY: an event handle is a kernel object reference, usable from any
// thread; nothing here is tied to the thread that created it.
unsafe impl Send for PolicyChanges {}

impl PolicyChanges {
    /// Creates the event for the account `sid`; `None`, logged, when it
    /// cannot be — the session host then reads the policy only when it
    /// starts.
    #[must_use]
    pub fn create(sid: &str) -> Option<Self> {
        let name = policy_changed_event(sid)?;
        Self::create_named(&name)
    }

    /// [`Self::create`] under `name`.
    #[must_use]
    pub fn create_named(name: &str) -> Option<Self> {
        let descriptor = Descriptor::parse(POLICY_CHANGED_SDDL)?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
            lpSecurityDescriptor: descriptor.0.0,
            bInheritHandle: false.into(),
        };
        let wide_name = wide(name);
        // Auto-reset: one read consumes one signal. A change written twice
        // before the host looks is read once, which is all it needs — the
        // read takes whatever the stores hold by then.
        //
        // SAFETY: `attributes` points into `descriptor`, which is alive, and
        // `wide_name` is a local; both outlive the call.
        let event = unsafe {
            CreateEventW(
                Some(&raw const attributes),
                false,
                false,
                PCWSTR(wide_name.as_ptr()),
            )
        }
        .inspect_err(|error| tracing::warn!(%error, "cannot create the policy-changed event"))
        .ok()?;
        Some(Self { event })
    }

    /// Whether the policy was written since the last look. Any failure reads
    /// as "no": the policy in force stays in force.
    #[must_use]
    pub fn changed(&self) -> bool {
        // SAFETY: `self.event` is a live event handle owned by this value.
        unsafe { WaitForSingleObject(self.event, 0) == WAIT_OBJECT_0 }
    }

    /// Raises the event through this value's own handle, for the tests: the
    /// production path, [`signal_policy_changed`], goes through the access
    /// check, which an unelevated test run is — correctly — refused by.
    #[cfg(test)]
    fn raise_locally(&self) {
        // SAFETY: `self.event` is a live event handle owned by this value.
        unsafe {
            let _ = SetEvent(self.event);
        }
    }
}

impl Drop for PolicyChanges {
    fn drop(&mut self) {
        // SAFETY: `self.event` is live and owned here, and is not used again.
        unsafe {
            let _ = CloseHandle(self.event);
        }
    }
}

/// Why [`signal_policy_changed`] did not reach a session host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalError {
    /// No session host waits for this account: it reads the policy when it
    /// next starts, so there is nothing to tell.
    NobodyListening,
    /// The event's list does not admit this process.
    AccessDenied,
    /// Anything else, with its Win32 code.
    Failed(u32),
}

/// Tells the session host of the account `sid` that its policy was written.
///
/// # Errors
/// [`SignalError`] when nobody was told.
pub fn signal_policy_changed(sid: &str) -> Result<(), SignalError> {
    let name = policy_changed_event(sid).ok_or(SignalError::Failed(0))?;
    signal_named(&name)
}

/// [`signal_policy_changed`] on the event `name`.
///
/// # Errors
/// As [`signal_policy_changed`].
pub fn signal_named(name: &str) -> Result<(), SignalError> {
    let wide_name = wide(name);
    // SAFETY: `wide_name` is a null-terminated local that outlives the call.
    let event = unsafe { OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(wide_name.as_ptr())) }
        .map_err(|error| match win32_code(error.code()) {
            // ERROR_FILE_NOT_FOUND: no such event, so no session host.
            2 => SignalError::NobodyListening,
            code if code == ERROR_ACCESS_DENIED.0 => SignalError::AccessDenied,
            code => SignalError::Failed(code),
        })?;
    // SAFETY: `event` was just opened with `EVENT_MODIFY_STATE` and is closed
    // below, after this one use.
    let raised = unsafe { SetEvent(event) };
    // SAFETY: as above; not used again.
    unsafe {
        let _ = CloseHandle(event);
    }
    raised.map_err(|error| SignalError::Failed(win32_code(error.code())))
}

/// The Win32 code inside an `HRESULT` built from one; the `HRESULT` itself
/// otherwise.
#[allow(
    clippy::cast_sign_loss,
    reason = "an HRESULT is a bit pattern; the low word is the Win32 code"
)]
const fn win32_code(code: HRESULT) -> u32 {
    let bits = code.0 as u32;
    if bits & 0xFFFF_0000 == 0x8007_0000 {
        bits & 0xFFFF
    } else {
        bits
    }
}

/// A security descriptor parsed from SDDL, freed when it goes out of scope.
struct Descriptor(PSECURITY_DESCRIPTOR);

impl Descriptor {
    fn parse(sddl: &str) -> Option<Self> {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        let encoded = wide(sddl);
        // SAFETY: `encoded` is a null-terminated local that outlives the
        // call; the descriptor it allocates is owned by `Self` from here.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(encoded.as_ptr()),
                SDDL_REVISION_1,
                &raw mut descriptor,
                None,
            )
        }
        .inspect_err(|error| tracing::error!(%error, "cannot build the policy event's access list"))
        .ok()?;
        Some(Self(descriptor))
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by the SDDL conversion and freed only here.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0.0)));
        }
    }
}

/// A null-terminated UTF-16 copy of `text`, for the `W` entry points.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A per-test name in the session-local namespace: an unelevated run
    /// cannot create a `Global\` object at all, and a test must not touch a
    /// real session host's event.
    fn test_name(suffix: &str) -> String {
        format!(
            r"Local\lumepeer-test-policy-changed-{}-{suffix}",
            std::process::id()
        )
    }

    /// ADR 0169 §5: the event admits `LocalSystem` and administrators and
    /// nobody else — no interactive user, no "everyone", no authenticated
    /// user, no ordinary user.
    #[test]
    fn the_event_admits_only_system_and_administrators() {
        assert!(POLICY_CHANGED_SDDL.contains("(A;;GA;;;SY)"));
        assert!(POLICY_CHANGED_SDDL.contains("(A;;GA;;;BA)"));
        for nobody in [";;;IU)", ";;;WD)", ";;;AU)", ";;;BU)", ";;;S-1-"] {
            assert!(
                !POLICY_CHANGED_SDDL.contains(nobody),
                "{nobody} must not be admitted"
            );
        }
        assert!(
            Descriptor::parse(POLICY_CHANGED_SDDL).is_some(),
            "the list must parse"
        );
    }

    /// The name crosses sessions, carries one SID, and nothing else gets in.
    #[test]
    fn the_name_is_global_and_built_from_a_sid_only() {
        let sid = "S-1-5-21-1111111111-2222222222-3333333333-1001";
        assert_eq!(
            policy_changed_event(sid).as_deref(),
            Some(r"Global\lumepeer-policy-changed-S-1-5-21-1111111111-2222222222-3333333333-1001")
        );
        assert_eq!(policy_changed_event(r"..\..\BaseNamedObjects\x"), None);
        assert_eq!(policy_changed_event("S-1-5-21)(A;;GA;;;WD"), None);
        assert_eq!(policy_changed_event(""), None);
    }

    /// The flag takes the SID after it, and only a SID.
    #[test]
    fn the_account_comes_from_the_argument_after_the_flag() {
        let args = |rest: &[&str]| -> Vec<String> {
            std::iter::once("lumepeer-desktop.exe")
                .chain(rest.iter().copied())
                .map(str::to_owned)
                .collect()
        };
        assert_eq!(requested_account(&args(&[])), None);
        assert_eq!(requested_account(&args(&["--session-host"])), None);
        assert_eq!(
            requested_account(&args(&["--access-settings", "S-1-5-21-1-2-3-1001"])),
            Some(Some("S-1-5-21-1-2-3-1001".to_owned()))
        );
        assert_eq!(requested_account(&args(&["--access-settings"])), Some(None));
        assert_eq!(
            requested_account(&args(&["--access-settings", "../../Windows"])),
            Some(None)
        );
    }

    /// One signal is one read: seen once, then not again until raised again.
    #[test]
    fn a_change_is_read_once() {
        let Some(changes) = PolicyChanges::create_named(&test_name("once")) else {
            eprintln!("skipping: cannot create a named event in this environment");
            return;
        };
        assert!(!changes.changed(), "a fresh event says nothing changed");
        changes.raise_locally();
        assert!(changes.changed());
        assert!(!changes.changed(), "auto-reset: the read consumed it");
    }

    /// ADR 0169 §5, the refusal half: a process the list does not admit
    /// cannot raise the event. Run unelevated, this is the medium-integrity
    /// caller and must be refused with `ERROR_ACCESS_DENIED`; run elevated it
    /// is an administrator and is admitted, which it says.
    #[test]
    fn an_unadmitted_caller_cannot_signal_a_change() {
        let name = test_name("access");
        let Some(changes) = PolicyChanges::create_named(&name) else {
            eprintln!("skipping: cannot create a named event in this environment");
            return;
        };
        let elevated = crate::program_data::this_process().is_some_and(|(_, elevated)| elevated);
        let signalled = signal_named(&name);
        if elevated {
            assert_eq!(signalled, Ok(()));
            assert!(changes.changed());
            eprintln!("this run is elevated, so the access list admits it");
        } else {
            assert_eq!(signalled, Err(SignalError::AccessDenied));
            assert!(!changes.changed());
            eprintln!("a medium-integrity caller was refused: ERROR_ACCESS_DENIED");
        }
    }

    /// Signalling with no session host listening says so, rather than
    /// reporting a change nobody will read.
    #[test]
    fn signalling_with_nobody_listening_says_so() {
        assert_eq!(
            signal_named(&test_name("nobody-home")),
            Err(SignalError::NobodyListening)
        );
    }

    /// The Win32 code is read out of an `HRESULT` that wraps one.
    #[test]
    fn a_win32_code_is_read_out_of_its_hresult() {
        assert_eq!(win32_code(HRESULT::from_win32(ERROR_CANCELLED.0)), 1223);
        assert_eq!(win32_code(HRESULT::from_win32(ERROR_ACCESS_DENIED.0)), 5);
    }
}
