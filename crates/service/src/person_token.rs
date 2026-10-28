//! The signed-in person's tokens, for the work the session host does in their
//! name (ADR 0169 §6).
//!
//! The session host is `LocalSystem`. Three kinds of work it does are not its
//! own but the person's, and must run with exactly the person's rights:
//!
//! - **the shell** — started with the person's own token
//!   ([`SignedInPerson::own_token`]), the one `WTSQueryUserToken` hands out:
//!   the limited half of a UAC administrator's, medium integrity, as
//!   ADR 0079 and ADR 0102 decided. Never the elevated half;
//! - **a guest's file work** — listing, reading, writing, renaming, deleting,
//!   receiving — run while impersonating [`SignedInPerson::files_token`]: the
//!   elevated half where the account has one, which is the token the elevated
//!   client acted with until now, so a guest sees exactly what it saw before;
//!   the person's own token otherwise. Never `LocalSystem`'s, which would hand
//!   a guest with `file_browse` more of this machine than the person at it has;
//! - **a recording export** the person's own client asks for — written with
//!   their own token into their own profile.
//!
//! Both tokens are asked for at the moment of use, not kept: the session host
//! lives for one person's console session, and a token asked for again is one
//! that follows the console — at the logon screen there is nobody's token to
//! be had, and every one of those operations is refused (`WTSQueryUserToken`
//! answers `ERROR_NO_TOKEN`), as it is refused there today. A token for
//! anybody but the person whose stores this host serves is refused too.
//!
//! Impersonation never reaches a thread anything else runs on.
//! [`run_impersonating`] takes the token on a thread of its own, gives it up
//! in a guard's `Drop` on that same thread, and the thread ends there: an
//! async runtime's pool thread that went on to run somebody else's task as
//! the person — or a `LocalSystem` task as them by mistake — cannot happen.

#![allow(
    unsafe_code,
    reason = "WTSQueryUserToken, ImpersonateLoggedOnUser, RevertToSelf, \
              CreateEnvironmentBlock and the token queries have no safe \
              bindings; same justification standard as the rest of this \
              crate's Win32 surface (ADR 0043, ADR 0085, ADR 0126)"
)]

use core::ffi::c_void;
use std::ffi::OsString;
use std::io;
use std::marker::PhantomData;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
use std::path::PathBuf;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Security::{
    GetTokenInformation, ImpersonateLoggedOnUser, RevertToSelf, TOKEN_ELEVATION_TYPE,
    TOKEN_LINKED_TOKEN, TokenElevationType, TokenElevationTypeLimited, TokenLinkedToken,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};

use crate::agent_launch::{query_user_token, sid_of_token};

/// The environment variable a profile's local application data lives under.
const LOCAL_APP_DATA: &str = "LOCALAPPDATA";

/// The person signed in at one session, whose stores the session host serves.
#[derive(Debug, Clone)]
pub struct SignedInPerson {
    session: u32,
    sid: String,
}

impl SignedInPerson {
    /// The person signed in at `session`, who must be `sid` whenever one of
    /// their tokens is asked for.
    #[must_use]
    pub const fn new(session: u32, sid: String) -> Self {
        Self { session, sid }
    }

    /// Their SID, as a string.
    #[must_use]
    pub fn sid(&self) -> &str {
        &self.sid
    }

    /// Their own token, as `WTSQueryUserToken` hands it out: the limited half
    /// of a UAC administrator's. A primary token, for `CreateProcessAsUserW`
    /// and for impersonation alike.
    ///
    /// # Errors
    /// When nobody is signed in at the session (the logon screen), when this
    /// process lacks `SeTcbPrivilege` (anything but `LocalSystem`), or when
    /// whoever is signed in is not this person.
    pub fn own_token(&self) -> io::Result<OwnedHandle> {
        let token = query_user_token(self.session).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "nobody's token at this session",
            )
        })?;
        // SAFETY: a token handle `WTSQueryUserToken` just opened for this
        // process and nothing else owns.
        let token = unsafe { OwnedHandle::from_raw_handle(token.0) };
        match sid_of_token(raw(&token)) {
            Some(sid) if sid == self.sid => Ok(token),
            _ => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the token at this session is not the person this host serves",
            )),
        }
    }

    /// The token a guest's file work runs with: the elevated half of a split
    /// UAC token, or the person's own token where it is not split.
    ///
    /// # Errors
    /// As [`Self::own_token`], and when the elevated half cannot be read.
    pub fn files_token(&self) -> io::Result<OwnedHandle> {
        let own = self.own_token()?;
        if !is_limited_half(&own)? {
            return Ok(own);
        }
        let mut linked = TOKEN_LINKED_TOKEN::default();
        let mut written = 0u32;
        // SAFETY: a live token handle; `linked` is a local of exactly the size
        // passed, which the call writes. `LocalSystem` holds `SeTcbPrivilege`,
        // so the handle written is a primary token, not an
        // identification-level copy.
        unsafe {
            GetTokenInformation(
                raw(&own),
                TokenLinkedToken,
                Some((&raw mut linked).cast::<c_void>()),
                u32::try_from(size_of::<TOKEN_LINKED_TOKEN>()).unwrap_or(0),
                &raw mut written,
            )
        }
        .map_err(io::Error::other)?;
        // SAFETY: a token handle the call above opened for this process and
        // nothing else owns.
        Ok(unsafe { OwnedHandle::from_raw_handle(linked.LinkedToken.0) })
    }

    /// Their profile's local application data directory, as their own
    /// environment names it — where their client keeps what it keeps.
    ///
    /// # Errors
    /// As [`Self::own_token`], and when their environment cannot be built or
    /// names no such directory.
    pub fn local_app_data(&self) -> io::Result<PathBuf> {
        let token = self.own_token()?;
        environment_of(&token)?
            .and_then(|block| variable_in(&block, LOCAL_APP_DATA))
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no local application data"))
    }
}

/// Whether `token` is the limited half of a split UAC token — the one with an
/// elevated half linked to it.
fn is_limited_half(token: &OwnedHandle) -> io::Result<bool> {
    let mut kind = TOKEN_ELEVATION_TYPE::default();
    let mut written = 0u32;
    // SAFETY: a live token handle; `kind` is a local of exactly the size
    // passed, which the call writes.
    unsafe {
        GetTokenInformation(
            raw(token),
            TokenElevationType,
            Some((&raw mut kind).cast::<c_void>()),
            u32::try_from(size_of::<TOKEN_ELEVATION_TYPE>()).unwrap_or(0),
            &raw mut written,
        )
    }
    .map_err(io::Error::other)?;
    Ok(kind == TokenElevationTypeLimited)
}

/// The environment block `token`'s user would start with, as UTF-16 units;
/// `None` for an empty block.
fn environment_of(token: &OwnedHandle) -> io::Result<Option<Vec<u16>>> {
    let mut block: *mut c_void = std::ptr::null_mut();
    // SAFETY: `block` is a local the call writes an allocation into; `token`
    // is a live token. `false`: nothing of this process's own environment.
    unsafe { CreateEnvironmentBlock(&raw mut block, Some(raw(token)), false) }
        .map_err(io::Error::other)?;
    if block.is_null() {
        return Ok(None);
    }
    // The block is `NAME=value\0` strings ending in an empty one. Read up to
    // and including that terminator, never past it.
    let units = block.cast::<u16>();
    let mut copied = Vec::new();
    let mut at = 0usize;
    loop {
        // SAFETY: `at` never passes the double terminator the documented
        // block ends with, and the loop stops at it.
        let unit = unsafe { units.add(at).read() };
        copied.push(unit);
        if unit == 0 && (at == 0 || copied.get(at.wrapping_sub(1)) == Some(&0)) {
            break;
        }
        at += 1;
    }
    // SAFETY: `block` came from `CreateEnvironmentBlock` above and is not read
    // again.
    unsafe {
        let _ = DestroyEnvironmentBlock(block);
    }
    Ok(Some(copied))
}

/// The value of `name` in an environment block, compared without regard to
/// case as Windows compares variable names.
fn variable_in(block: &[u16], name: &str) -> Option<OsString> {
    block
        .split(|&unit| unit == 0)
        .take_while(|entry| !entry.is_empty())
        .find_map(|entry| {
            let at = entry.iter().position(|&unit| unit == u16::from(b'='))?;
            let key = String::from_utf16(entry.get(..at)?).ok()?;
            key.eq_ignore_ascii_case(name)
                .then(|| OsString::from_wide(entry.get(at + 1..).unwrap_or_default()))
        })
}

/// A token handle as the `HANDLE` the `windows` crate speaks.
fn raw(token: &OwnedHandle) -> HANDLE {
    HANDLE(token.as_raw_handle())
}

/// The calling thread impersonating a token, until dropped.
///
/// Not `Send`: impersonation belongs to one thread, and is given up on that
/// thread or not at all.
struct Impersonation {
    _thread_bound: PhantomData<*const ()>,
}

impl Impersonation {
    /// Starts impersonating `token` on the calling thread.
    fn begin(token: BorrowedHandle<'_>) -> io::Result<Self> {
        // SAFETY: a live token handle the caller borrows for the call.
        unsafe { ImpersonateLoggedOnUser(HANDLE(token.as_raw_handle())) }
            .map_err(|error| io::Error::new(io::ErrorKind::PermissionDenied, error))?;
        Ok(Self {
            _thread_bound: PhantomData,
        })
    }
}

impl Drop for Impersonation {
    fn drop(&mut self) {
        // SAFETY: no arguments; undoes the impersonation `begin` started on
        // this same thread.
        if let Err(error) = unsafe { RevertToSelf() } {
            // The thread this runs on ends right after (`run_impersonating`),
            // and nothing else is ever run on it; said loudly all the same.
            tracing::error!(%error, "could not stop impersonating the signed-in person");
        }
    }
}

/// Runs `work` impersonating `token`, on a thread of its own that gives the
/// identity up and ends before this returns.
///
/// # Errors
/// When the impersonation could not be started — `work` has then not run —
/// or the thread did not finish.
pub fn run_impersonating(
    token: BorrowedHandle<'_>,
    work: &mut (dyn FnMut() + Send),
) -> io::Result<()> {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let impersonating = Impersonation::begin(token)?;
                work();
                drop(impersonating);
                Ok(())
            })
            .join()
    })
    .map_err(|_| io::Error::other("the person's thread did not finish"))?
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::os::windows::io::AsHandle;

    use windows::Win32::Foundation::ERROR_NO_TOKEN;
    use windows::Win32::Security::TOKEN_QUERY;
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken,
    };

    use super::*;

    /// This process's own token: something a test can impersonate without
    /// any privilege.
    fn own_process_token() -> OwnedHandle {
        use windows::Win32::Security::{TOKEN_DUPLICATE, TOKEN_IMPERSONATE};
        let mut token = HANDLE::default();
        // SAFETY: the current-process pseudo-handle and a local the call
        // writes.
        unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_IMPERSONATE,
                &raw mut token,
            )
        }
        .unwrap();
        // SAFETY: a token handle the call above just opened.
        unsafe { OwnedHandle::from_raw_handle(token.0) }
    }

    /// Whether the calling thread is impersonating anybody right now.
    fn thread_impersonates() -> bool {
        let mut token = HANDLE::default();
        // SAFETY: the current-thread pseudo-handle and a local the call
        // writes; the handle, if any, is taken over and closed below.
        let opened =
            unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, true, &raw mut token) };
        match opened {
            Ok(()) => {
                // SAFETY: a handle the call above opened.
                drop(unsafe { OwnedHandle::from_raw_handle(token.0) });
                true
            }
            Err(error) => {
                assert_eq!(
                    error.code(),
                    ERROR_NO_TOKEN.to_hresult(),
                    "not a thread without a token: {error}"
                );
                false
            }
        }
    }

    /// The work runs impersonating, on a thread other than the caller's, and
    /// the caller's thread never carries the identity — before, during or
    /// after.
    #[test]
    fn impersonation_stays_on_a_thread_of_its_own() {
        let token = own_process_token();
        let caller = std::thread::current().id();
        let mut seen = None;
        run_impersonating(token.as_handle(), &mut || {
            seen = Some((std::thread::current().id(), thread_impersonates()));
        })
        .unwrap();
        let (thread, impersonated) = seen.expect("the work ran");
        assert_ne!(thread, caller, "the work ran on the caller's thread");
        assert!(impersonated, "the work did not run impersonating");
        assert!(
            !thread_impersonates(),
            "the caller's thread was left impersonating"
        );
    }

    /// Work done impersonating is done as that identity, not as this
    /// process: impersonating the elevated half of this account as an
    /// unelevated process gives only an identification-level token, with
    /// which nothing can be created — where this process itself can.
    #[test]
    fn work_under_impersonation_is_done_as_the_impersonated_identity() {
        let own = own_process_token();
        if !is_limited_half(&own).unwrap() {
            // Not a split UAC token (or an elevated run): there is no other
            // half of this account to tell apart from this process.
            return;
        }
        let mut linked = TOKEN_LINKED_TOKEN::default();
        let mut written = 0u32;
        // SAFETY: as in `files_token`; unelevated, the handle is an
        // identification-level token.
        unsafe {
            GetTokenInformation(
                raw(&own),
                TokenLinkedToken,
                Some((&raw mut linked).cast::<c_void>()),
                u32::try_from(size_of::<TOKEN_LINKED_TOKEN>()).unwrap_or(0),
                &raw mut written,
            )
        }
        .unwrap();
        // SAFETY: a handle the call above opened.
        let linked = unsafe { OwnedHandle::from_raw_handle(linked.LinkedToken.0) };

        let dir = std::env::temp_dir().join(format!("lumepeer-person-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("made-as-the-other-half.txt");
        let _ = std::fs::remove_file(&path);
        let mut created = None;
        let started = run_impersonating(linked.as_handle(), &mut || {
            created = Some(std::fs::write(&path, b"x").is_ok());
        });
        assert!(
            started.is_err() || created == Some(false),
            "a file was created as this process while impersonating somebody else"
        );
        assert!(
            std::fs::write(&path, b"x").is_ok(),
            "this process itself can"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Only `LocalSystem` may ask for a signed-in person's token. Anything
    /// else — this unelevated test — is refused, and there is no fallback to
    /// this process's own token.
    #[test]
    fn without_tcb_there_is_no_persons_token() {
        let session = crate::agent_launch::current_session().unwrap();
        let person = SignedInPerson::new(session, "S-1-5-21-0-0-0-1001".to_owned());
        assert!(person.own_token().is_err());
        assert!(person.files_token().is_err());
        assert!(person.local_app_data().is_err());
    }

    /// Variables are found by name without regard to case, and never past
    /// the block's end.
    #[test]
    fn a_variable_is_read_out_of_an_environment_block() {
        let block: Vec<u16> = "Path=C:\\x\0LocalAppData=C:\\Users\\p\\AppData\\Local\0\0"
            .encode_utf16()
            .collect();
        assert_eq!(
            variable_in(&block, LOCAL_APP_DATA),
            Some(OsString::from(r"C:\Users\p\AppData\Local"))
        );
        assert_eq!(variable_in(&block, "TEMP"), None);
        assert_eq!(variable_in(&[0, 0], LOCAL_APP_DATA), None);
    }

    /// This process's own environment, built from its token, names a local
    /// application data directory — the call works end to end.
    #[test]
    fn a_tokens_environment_names_its_local_app_data() {
        let token = own_process_token();
        let block = environment_of(&token).unwrap().expect("a block");
        let named = variable_in(&block, LOCAL_APP_DATA).expect("LOCALAPPDATA");
        assert!(!named.is_empty());
    }
}
