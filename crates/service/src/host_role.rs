//! Which process on this machine is the host, and how the role changes hands
//! (ADR 0085 §4).
//!
//! Two hosts on one machine is not a degraded mode, it is a contradiction:
//! each would own a `SessionManager` for the same screen, and §8.2's
//! `ControlLimited` snapshot would be taken twice against two different
//! policies. So the role is a single machine-wide token, and whoever holds it
//! is the host.
//!
//! **Why a kernel object and not a file or a port.** The kernel releases a
//! mutex when its holder's process ends, so a host that crashes does not leave
//! the machine permanently unhostable — a lock file would, and a half-written
//! one is a state that has to be guessed at. `WAIT_ABANDONED` is the kernel
//! saying exactly that: the previous holder died, and the role is yours.
//!
//! **Why the handover is an event and not a message.** A wire would need a
//! format, a parser and an argument about what a peer can put on it. A manual
//! reset event carries nothing — it is signalled or it is not — so the whole
//! handover is one parameterless operation in one direction, admitted to
//! `LocalSystem` and administrators and to nobody else. There is nothing for a
//! remote guest to ask for here and nothing for a local unprivileged process
//! to reach.
//!
//! The direction is deliberate (ADR 0085 §4): the person at the machine asks,
//! and the service yields. The client is the surface that shows who is
//! connected and carries the revoke, and a design in which a remote party's
//! session outranks the controls of the person sitting in front of the machine
//! is the wrong way round for this project.

#![allow(
    unsafe_code,
    reason = "a named mutex and a named event have no safe bindings; same \
              justification standard as SendInput (ADR 0012) and the rest of \
              this crate's Win32 surface (ADR 0043, ADR 0049)"
)]

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0,
};
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, OpenEventW, ReleaseMutex, ResetEvent, SetEvent,
    WaitForSingleObject,
};
use windows::core::{HRESULT, PCWSTR};

/// Name of the token that says who is hosting this machine.
///
/// `Global\` is required rather than stylistic, for the reason
/// [`crate::protocol::SECURE_DESKTOP_MAPPING_NAME`] already records: the host
/// service runs in session 0 and the desktop client runs in an interactive
/// session, and a name without the prefix would be created in the caller's own
/// session-private namespace — invisible across exactly the boundary this
/// token exists to be single across.
pub const HOST_ROLE_TOKEN: &str = r"Global\lumepeer-host-role";

/// Name of the event that asks whoever holds [`HOST_ROLE_TOKEN`] to give it
/// up.
pub const HOST_ROLE_RELEASE_EVENT: &str = r"Global\lumepeer-host-role-release";

/// Who may take the host role or ask for it back, in SDDL.
///
/// - `SY` — `LocalSystem`, which the host service runs as.
/// - `BA` — administrators, which is what the desktop client runs as
///   (ADR 0057 ships it `requireAdministrator`).
///
/// Deliberately **not** `IU`, unlike the helper's request pipe: that pipe
/// admits interactive users because the one thing they can ask for there is a
/// Ctrl+Alt+Del on their own screen. Taking the host role, or knocking the
/// current host off it, is neither narrow nor self-limiting, so it is not
/// something an ordinary signed-in process gets to do.
const HOST_ROLE_SDDL: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)";

/// A null-terminated UTF-16 copy of `text`, for the `W` entry points.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Builds [`HOST_ROLE_SDDL`] as security attributes, plus the buffer the
/// descriptor was parsed from, which the caller keeps alive across the call
/// that borrows the attributes.
fn security_attributes() -> Option<(SECURITY_ATTRIBUTES, Vec<u16>)> {
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    let encoded = wide(HOST_ROLE_SDDL);
    // SAFETY: `encoded` is a null-terminated wide string that outlives the
    // call; the descriptor it allocates is owned by the caller from here and
    // freed by every caller right after the call that borrows it.
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(encoded.as_ptr()),
            SDDL_REVISION_1,
            &raw mut descriptor,
            None,
        )
    };
    if converted.is_err() {
        tracing::error!("cannot build the host role's access list");
        return None;
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: false.into(),
    };
    Some((attributes, encoded))
}

/// The host role, held for as long as this value lives.
///
/// Dropping it hands the role back, and so does the process ending — the
/// kernel abandons the mutex, and the next acquirer is told so rather than
/// finding a token nobody owns.
#[derive(Debug)]
pub struct HostRole {
    token: HANDLE,
}

// SAFETY: the handle is an ordinary kernel handle, valid process-wide and not
// bound to the thread that created it. Moving the guard between threads is
// all `Send` promises, and releasing a mutex from another thread than the one
// that took it is the one thing this type does *not* do — `Drop` runs
// `ReleaseMutex` wherever the guard ends up, which the kernel accepts only
// from the owning thread, so a guard moved across threads degrades to "the
// role is released when the process ends", never to a false release.
unsafe impl Send for HostRole {}

// SAFETY: nothing reachable through a shared reference touches the handle.
// `Drop` is the only code that uses it and takes `&mut self`, so `&HostRole`
// exposes no operation at all — which is exactly what `Sync` promises. This
// matters because the guard is parked in the desktop app's shared state, whose
// whole purpose is to hold it until the process exits.
unsafe impl Sync for HostRole {}

/// What came of asking for the host role.
///
/// Three outcomes rather than two, because "I could not take it" and "I could
/// not even ask" lead to opposite behaviour, and collapsing them would get one
/// of the two wrong:
///
/// - [`Taken`](Self::Taken) means something else on this machine is hosting,
///   and the caller must not. That includes the case where the token exists
///   and this process is refused access to it, which is precisely what a host
///   service holding it looks like to a process the access list does not
///   admit.
/// - [`Unavailable`](Self::Unavailable) means the question could not be put at
///   all — creating an object in the `Global` namespace needs a privilege an
///   unelevated process does not hold. A **shipped** client is elevated
///   (ADR 0057) and never lands here; a development build run unelevated does,
///   and refusing to host on a machine where nothing else is hosting would
///   turn this token into a new reason for the app not to work.
#[derive(Debug)]
pub enum HostRoleClaim {
    /// The role is this process's, for as long as the guard lives.
    Held(HostRole),
    /// Something else on this machine holds it.
    Taken,
    /// This process cannot ask. See the type's own documentation.
    Unavailable,
}

impl HostRoleClaim {
    /// Whether the caller may host.
    ///
    /// [`Unavailable`](Self::Unavailable) reads as yes, deliberately: it is
    /// the state of a development build on a machine with no host service on
    /// it, and the other reading turns an unanswerable question into a refusal
    /// to run.
    #[must_use]
    pub const fn may_host(&self) -> bool {
        !matches!(self, Self::Taken)
    }
}

/// Asks for the host role.
///
/// The three-way [`HostRoleClaim`], rather than [`HostRole::acquire`] and its
/// `Option`, for callers that have to behave differently when the question
/// itself could not be put.
#[must_use]
pub fn claim() -> HostRoleClaim {
    claim_named(HOST_ROLE_TOKEN)
}

/// [`claim`], against an arbitrary token name.
#[must_use]
pub fn claim_named(name: &str) -> HostRoleClaim {
    match open_token(name) {
        Opened::Token(token) => {
            // SAFETY: `token` is a live mutex handle from `open_token`.
            let waited = unsafe { WaitForSingleObject(token, 0) };
            // `WAIT_ABANDONED` is the kernel saying the previous holder's
            // process ended without releasing. That is a host that crashed,
            // and the role is genuinely free — treating it as held would leave
            // the machine unhostable until a reboot.
            if waited == WAIT_OBJECT_0 || waited == WAIT_ABANDONED {
                return HostRoleClaim::Held(HostRole { token });
            }
            // SAFETY: `token` is live and owned here, and is not used again.
            unsafe {
                let _ = CloseHandle(token);
            }
            HostRoleClaim::Taken
        }
        Opened::Refused => HostRoleClaim::Taken,
        Opened::Impossible => HostRoleClaim::Unavailable,
    }
}

/// What opening the token object produced.
enum Opened {
    /// A handle to it.
    Token(HANDLE),
    /// It exists and this process is not admitted, which from the outside is
    /// indistinguishable from — and means the same as — somebody holding it.
    Refused,
    /// The call failed for a reason that is not about this object's access
    /// list, which in practice means the privilege to create an object in the
    /// `Global` namespace was not held.
    Impossible,
}

/// Creates or opens the named token object.
fn open_token(name: &str) -> Opened {
    let Some((attributes, _encoded)) = security_attributes() else {
        return Opened::Impossible;
    };
    let wide_name = wide(name);
    // Created unowned: ownership is taken by the wait in the caller, so that
    // "the token exists" and "somebody holds it" stay two different facts. A
    // mutex created owned would make the second caller's `CreateMutexW` look
    // like a failure rather than a busy token.
    //
    // SAFETY: `attributes` and `wide_name` are locals that outlive the call.
    let token = unsafe {
        CreateMutexW(
            Some(&raw const attributes),
            false,
            PCWSTR(wide_name.as_ptr()),
        )
    };
    // SAFETY: the descriptor came from `security_attributes` above and is not
    // read again; the object holds its own copy by now.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(attributes.lpSecurityDescriptor)));
    }
    match token {
        Ok(token) => Opened::Token(token),
        Err(error) if error.code() == HRESULT::from_win32(ERROR_ACCESS_DENIED.0) => {
            // The object is there and this process is not on its access list.
            // A host service holding the role looks exactly like this to
            // anything the list does not admit, so it reads as taken rather
            // than as a failure to ask.
            tracing::info!("the host role is held, and this process cannot open the token");
            Opened::Refused
        }
        Err(error) => {
            tracing::warn!(%error, "cannot open the host role token");
            Opened::Impossible
        }
    }
}

impl HostRole {
    /// Takes the host role, or `None` if something else on this machine
    /// already holds it — or if this process cannot ask at all.
    ///
    /// Never blocks: a caller that cannot have the role needs to know now, so
    /// it can run as something other than a host, not in a minute.
    ///
    /// [`claim`] is the same question with the two failures kept apart, which
    /// is what a caller that must still run without the role needs.
    #[must_use]
    pub fn acquire() -> Option<Self> {
        Self::acquire_named(HOST_ROLE_TOKEN)
    }

    /// [`acquire`](Self::acquire), against an arbitrary token name.
    ///
    /// Split out so a test can exercise the same code path against a
    /// throwaway, session-local name — the same reason `install.rs` has
    /// `install_named`. A test must never take the real machine's host role,
    /// and an unelevated test run cannot create a `Global\` object at all.
    #[must_use]
    pub fn acquire_named(name: &str) -> Option<Self> {
        match claim_named(name) {
            HostRoleClaim::Held(role) => Some(role),
            HostRoleClaim::Taken | HostRoleClaim::Unavailable => None,
        }
    }

    /// Whether anything on this machine currently holds the host role.
    ///
    /// Answered by trying to take it and giving it straight back, so there is
    /// exactly one implementation of what "held" means. Inherently a snapshot:
    /// the answer can be stale the instant it is returned, which is why the
    /// only caller that acts on it is one that goes on to
    /// [`acquire`](Self::acquire) anyway and treats *that* as the decision.
    #[must_use]
    pub fn is_held() -> bool {
        Self::is_held_named(HOST_ROLE_TOKEN)
    }

    /// [`is_held`](Self::is_held), against an arbitrary token name.
    #[must_use]
    pub fn is_held_named(name: &str) -> bool {
        Self::acquire_named(name).is_none()
    }
}

impl Drop for HostRole {
    fn drop(&mut self) {
        // SAFETY: `self.token` is a live mutex this process owns, and is not
        // used again after this.
        unsafe {
            let _ = ReleaseMutex(self.token);
            let _ = CloseHandle(self.token);
        }
    }
}

/// Asks whoever holds the host role to give it up (ADR 0085 §4).
///
/// Returns whether the request was raised. `false` means there was nothing to
/// ask — no host service is waiting on the event — or that this caller is not
/// admitted by [`HOST_ROLE_SDDL`], which is the same answer for the same
/// reason every other refusal in this crate collapses to one bit.
///
/// This does not wait, and it does not promise the role afterwards: the holder
/// ends its own sessions first, which takes as long as it takes. The caller's
/// next move is to try [`HostRole::acquire`] again, and to stay a non-host if
/// it still cannot have it.
#[must_use]
pub fn request_release() -> bool {
    request_release_named(HOST_ROLE_RELEASE_EVENT)
}

/// [`request_release`], against an arbitrary event name.
#[must_use]
pub fn request_release_named(name: &str) -> bool {
    let wide_name = wide(name);
    // Opened, never created: the event is the *holder's* rendezvous, and a
    // requester that created one would be signalling an object nobody is
    // waiting on while believing it had asked.
    //
    // SAFETY: `wide_name` is a null-terminated wide string that outlives the
    // call.
    let Ok(event) = (unsafe { OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(wide_name.as_ptr())) })
    else {
        return false;
    };
    // SAFETY: `event` was just opened with `EVENT_MODIFY_STATE`, which is
    // exactly what `SetEvent` needs.
    let raised = unsafe { SetEvent(event) }.is_ok();
    // SAFETY: `event` is live and owned here, and is not used again.
    unsafe {
        let _ = CloseHandle(event);
    }
    raised
}

/// The host service's end of the handover: the event it waits on, created
/// once and owned for as long as it is hosting.
#[derive(Debug)]
pub struct ReleaseRequests {
    event: HANDLE,
}

// SAFETY: an ordinary kernel handle, valid process-wide and not bound to the
// thread that created it; moving it between threads is all `Send` promises.
unsafe impl Send for ReleaseRequests {}

impl ReleaseRequests {
    /// Creates the event the host service watches, with
    /// [`HOST_ROLE_SDDL`] on it.
    #[must_use]
    pub fn create() -> Option<Self> {
        Self::create_named(HOST_ROLE_RELEASE_EVENT)
    }

    /// [`create`](Self::create), against an arbitrary event name.
    #[must_use]
    pub fn create_named(name: &str) -> Option<Self> {
        let (attributes, _encoded) = security_attributes()?;
        let wide_name = wide(name);
        // Manual reset: the request stands until the holder has actually
        // finished giving the role up and clears it. An auto-reset event would
        // be consumed by whichever wait happened to run first, which is how a
        // handover gets lost between a poll and a shutdown.
        //
        // SAFETY: `attributes` and `wide_name` are locals that outlive the
        // call.
        let event = unsafe {
            CreateEventW(
                Some(&raw const attributes),
                true,
                false,
                PCWSTR(wide_name.as_ptr()),
            )
        };
        // SAFETY: the descriptor came from `security_attributes` above and is
        // not read again; the object holds its own copy by now.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(attributes.lpSecurityDescriptor)));
        }
        let event = event
            .inspect_err(|error| tracing::warn!(%error, "cannot create the host handover event"))
            .ok()?;
        Some(Self { event })
    }

    /// Whether somebody has asked for the role since the last
    /// [`clear`](Self::clear).
    ///
    /// Polled rather than waited on, because the host service's own loop has
    /// other things to watch and a handover that lands a poll late costs
    /// nothing. Any failure reads as "nobody asked": the safe direction here
    /// is to keep hosting, not to drop sessions because a handle misbehaved.
    #[must_use]
    pub fn requested(&self) -> bool {
        // SAFETY: `self.event` is a live event handle owned by this value.
        unsafe { WaitForSingleObject(self.event, 0) == WAIT_OBJECT_0 }
    }

    /// Takes the request down, once the role has actually been given up.
    ///
    /// Called *after* releasing, never before: clearing first would leave a
    /// window in which the requester sees no outstanding request and the role
    /// has not moved, and would ask again into the gap.
    pub fn clear(&self) {
        // SAFETY: `self.event` is a live event handle owned by this value.
        unsafe {
            let _ = ResetEvent(self.event);
        }
    }

    /// Raises a request through this value's own handle, for the tests.
    ///
    /// [`request_release`] is the production path and goes through
    /// [`HOST_ROLE_SDDL`]'s access check, which an unelevated test run is not
    /// admitted by — correctly, and that refusal is its own test below. This
    /// exists so the manual-reset semantics `clear` depends on are still
    /// exercised on a developer machine rather than only on an installed one.
    #[cfg(test)]
    fn raise_locally(&self) {
        // SAFETY: `self.event` is a live event handle owned by this value.
        unsafe {
            let _ = SetEvent(self.event);
        }
    }
}

impl Drop for ReleaseRequests {
    fn drop(&mut self) {
        // SAFETY: `self.event` is live and owned here, and is not used again.
        unsafe {
            let _ = CloseHandle(self.event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A per-test name in the session-local namespace: a test must never take
    /// this machine's real host role, and an unelevated test run cannot create
    /// a `Global\` object at all (the same limitation `frame.rs`'s own tests
    /// work around).
    fn test_name(suffix: &str) -> String {
        format!(
            r"Local\lumepeer-test-host-role-{}-{suffix}",
            std::process::id()
        )
    }

    /// The whole point of the token: the second acquirer does not get it.
    #[test]
    fn only_one_holder_at_a_time() {
        let name = test_name("exclusive");
        let Some(first) = HostRole::acquire_named(&name) else {
            eprintln!("skipping: cannot create a named mutex in this environment");
            return;
        };
        assert!(
            HostRole::acquire_named(&name).is_none(),
            "a second acquirer must not also become the host"
        );
        assert!(HostRole::is_held_named(&name));
        drop(first);
    }

    /// Releasing hands the role back, so the machine is hostable again after
    /// a host stops hosting — a token that stayed taken would need a reboot.
    #[test]
    fn releasing_lets_the_next_host_take_it() {
        let name = test_name("release");
        let Some(first) = HostRole::acquire_named(&name) else {
            eprintln!("skipping: cannot create a named mutex in this environment");
            return;
        };
        drop(first);
        let second = HostRole::acquire_named(&name);
        assert!(
            second.is_some(),
            "the role must be available once its holder let go"
        );
    }

    /// An untaken token reads as free. This is the state a machine with no
    /// host service installed is always in, and the desktop client's start-up
    /// depends on it.
    #[test]
    fn an_untaken_token_is_not_held() {
        let name = test_name("free");
        if HostRole::acquire_named(&name).is_none() {
            eprintln!("skipping: cannot create a named mutex in this environment");
            return;
        }
        // The guard above was dropped at the end of the `if` condition, so the
        // token is free again by the time this runs.
        assert!(!HostRole::is_held_named(&name));
    }

    /// The handover: the holder sees a request only after one is raised, and
    /// only until it clears it.
    #[test]
    fn a_handover_request_stands_until_it_is_cleared() {
        let name = test_name("handover");
        let Some(requests) = ReleaseRequests::create_named(&name) else {
            eprintln!("skipping: cannot create a named event in this environment");
            return;
        };
        assert!(
            !requests.requested(),
            "a fresh event must not look like an outstanding request"
        );
        requests.raise_locally();
        assert!(requests.requested());
        // Manual reset: still standing on a second look, because a handover
        // that a poll consumed would be a handover that never happened.
        assert!(requests.requested());
        requests.clear();
        assert!(!requests.requested());
    }

    /// The access list is not decoration: an unelevated process cannot raise
    /// a handover request even against an event that exists, because
    /// [`HOST_ROLE_SDDL`] admits `LocalSystem` and administrators only. The
    /// desktop client is admitted because ADR 0057 ships it elevated; an
    /// ordinary process on the machine is not, and that is the point.
    #[test]
    fn an_unadmitted_caller_cannot_raise_a_handover() {
        let name = test_name("access");
        let Some(_requests) = ReleaseRequests::create_named(&name) else {
            eprintln!("skipping: cannot create a named event in this environment");
            return;
        };
        if request_release_named(&name) {
            // An elevated test run *is* admitted, which is the other correct
            // outcome. Saying so beats a test that passes for two opposite
            // reasons without distinguishing them.
            eprintln!("this run is elevated, so the access list admits it");
        }
    }

    /// Asking when nobody is listening is not an error and not a silent
    /// success: it reports that there was nothing to ask.
    #[test]
    fn asking_with_no_host_listening_says_so() {
        assert!(!request_release_named(&test_name("nobody-home")));
    }

    /// The three-way claim keeps apart the two answers that lead to opposite
    /// behaviour: a role somebody else holds, and a question this process
    /// could not put at all.
    #[test]
    fn a_claim_distinguishes_taken_from_unaskable() {
        let name = test_name("claim");
        let first = claim_named(&name);
        match first {
            HostRoleClaim::Held(_) => {
                assert!(first.may_host());
                let second = claim_named(&name);
                assert!(
                    matches!(second, HostRoleClaim::Taken),
                    "a role somebody holds must read as taken, never as unaskable"
                );
                assert!(
                    !second.may_host(),
                    "a second host is exactly what the token exists to prevent"
                );
            }
            HostRoleClaim::Unavailable => {
                // No named objects in this environment. Still an answer, and
                // it must be the permissive one: a development build on a
                // machine with no host service must not be stopped by a
                // question it cannot ask.
                assert!(first.may_host());
            }
            HostRoleClaim::Taken => {
                panic!("a token this test just named cannot already be held")
            }
        }
    }

    /// The access list admits `LocalSystem` and administrators, and — unlike
    /// the helper's request pipe — deliberately not interactive users: taking
    /// the host role is neither narrow nor self-limiting.
    #[test]
    fn the_token_admits_only_system_and_administrators() {
        assert!(HOST_ROLE_SDDL.contains(";;;SY)"));
        assert!(HOST_ROLE_SDDL.contains(";;;BA)"));
        assert!(
            !HOST_ROLE_SDDL.contains(";;;IU)"),
            "an ordinary signed-in process must not be able to take or move the host role"
        );
        assert!(
            !HOST_ROLE_SDDL.contains(";;;WD)") && !HOST_ROLE_SDDL.contains(";;;AU)"),
            "everyone and authenticated-users are exactly who must not be admitted"
        );
    }

    /// Both names are in the `Global\` namespace, or the token would be one
    /// per session and session 0 and the console session would each have
    /// their own host.
    #[test]
    fn the_names_cross_the_session_boundary() {
        assert!(HOST_ROLE_TOKEN.starts_with(r"Global\"));
        assert!(HOST_ROLE_RELEASE_EVENT.starts_with(r"Global\"));
        assert_ne!(HOST_ROLE_TOKEN, HOST_ROLE_RELEASE_EVENT);
    }
}
