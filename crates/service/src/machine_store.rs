//! Where a host running as a service keeps its state (ADR 0085).
//!
//! A host inside somebody's desktop session puts its files in that person's
//! profile, which is right: they are that person's address book, that
//! person's invite, that person's keys. A host running as `LocalSystem` has no
//! such person. Writing into whichever profile happened to be signed in first
//! is how one user's address book quietly becomes the machine's policy, and
//! `LocalSystem`'s own profile is not a place any of this belongs either.
//!
//! So the host service's state is machine-wide, under `%ProgramData%`, the
//! same root `log.rs` already uses and for the same reason: it is the one path
//! session 0 and the console session both resolve to the same place.
//!
//! **The access list is the protection, and it is the only one.** What lives
//! here includes the endpoint identity and the Argon2id hash of the device
//! password — the two things that decide whether a guest gets in at all — so
//! the directory admits `LocalSystem` and administrators and nobody else,
//! protected from inheritance so a permissive parent cannot widen it. That is
//! a real boundary against an ordinary signed-in user and, as §3.1 and
//! ADR 0043 both already say of everything else in this crate, **not** a
//! defence against a local administrator. Nothing here pretends otherwise.
//!
//! ADR 0085 records why this is a file with an access list rather than DPAPI
//! with `CRYPTPROTECT_LOCAL_MACHINE`: a machine-key DPAPI blob can be
//! unprotected by any process on the machine, so it would buy no boundary this
//! directory does not already draw, in exchange for FFI in a crate that
//! forbids it.

#![allow(
    unsafe_code,
    reason = "creating a directory with a DACL, and re-applying one to a \
              directory that already exists, have no safe bindings; same \
              justification standard as the rest of this crate's Win32 \
              surface (ADR 0043, ADR 0049)"
)]

use std::path::{Path, PathBuf};

/// Directory under `%ProgramData%` the host service's state lives in.
///
/// A sibling of `log.rs`'s `Lumepeer\logs` rather than a child of it: the log
/// is diagnostics anybody debugging this machine may want to read, and this is
/// not.
const DIRECTORY: &str = r"Lumepeer\host";

/// Who may read or write anything in the store, in SDDL.
///
/// - `D:P` — **protected**: inherited entries from `%ProgramData%`, which is
///   writable by ordinary users, are dropped rather than merged. Without the
///   `P` this list would be an addition to a permissive one instead of a
///   replacement for it, which is the difference between an access list and a
///   suggestion.
/// - `OICI` — object- and container-inherit, so every file and directory
///   created inside gets the same list rather than only the top level being
///   protected.
/// - `SY`, `BA` — `LocalSystem` and administrators, in full. Deliberately no
///   `IU` and no `BU`: the whole point is that the signed-in user cannot read
///   the device password's hash or the endpoint identity out of it.
const STORE_SDDL: &str = "D:P(A;OICI;GA;;;SY)(A;OICI;GA;;;BA)";

/// File name of the machine keystore's identity slot.
///
/// `FileKeystore` keeps one file per named slot and derives the siblings from
/// the path it was constructed with, so this is the whole of what the host
/// front end has to be told: everything else lands beside it, inside the
/// directory this module protects.
const KEYSTORE_FILE: &str = "identity.key";

/// The machine-wide store directory, whether or not it exists yet.
///
/// The literal fallback covers a service environment with no `ProgramData`
/// variable, which should not happen and is not a reason to have no answer.
#[must_use]
pub fn directory() -> PathBuf {
    std::env::var_os("ProgramData")
        .map_or_else(|| PathBuf::from(r"C:\ProgramData"), PathBuf::from)
        .join(DIRECTORY)
}

/// Where the host service's keystore lives.
///
/// Inside [`directory`], so it is covered by the access list above. The
/// caller supplies the encryption secret; this module deliberately does not
/// invent one, because a secret derived from something guessable would read as
/// protection the directory's own list is actually providing.
#[must_use]
pub fn keystore_path() -> PathBuf {
    directory().join(KEYSTORE_FILE)
}

/// Creates the store directory with [`STORE_SDDL`], or re-applies that list to
/// one that already exists.
///
/// Both halves matter. Creating it with the list closes the window in which a
/// fresh directory is readable by anyone; re-applying it to an existing one is
/// what makes an upgrade — or a directory somebody's backup tool recreated —
/// end up protected rather than trusted for being there already.
///
/// `None` when the directory cannot be created or the list cannot be applied.
/// The caller's only safe answer to that is to run without a machine store at
/// all: a host that wrote a device-password hash into a directory it could not
/// protect would be worse than one that did not start.
#[must_use]
pub fn ensure_directory() -> Option<PathBuf> {
    let path = directory();
    if let Some(parent) = path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        tracing::error!(path = %parent.display(), %error, "cannot create the machine store's parent");
        return None;
    }
    if path.is_dir() {
        // Already there. Re-apply rather than assume: the list is the whole
        // protection, and a directory that exists is not evidence of how.
        return apply_access_list(&path).then_some(path);
    }
    if create_protected_directory(&path) {
        return Some(path);
    }
    // A race with another host starting at the same moment is the one failure
    // worth a second look: the directory now exists, so protect it.
    if path.is_dir() && apply_access_list(&path) {
        return Some(path);
    }
    tracing::error!(path = %path.display(), "cannot create the machine store directory");
    None
}

/// A null-terminated UTF-16 copy of `text`, for the `W` entry points.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A null-terminated UTF-16 copy of a path, for the `W` entry points.
fn wide_path(path: &Path) -> Vec<u16> {
    wide(&path.to_string_lossy())
}

/// Parses [`STORE_SDDL`] into a security descriptor the caller must
/// `LocalFree`.
fn store_descriptor() -> Option<windows::Win32::Security::PSECURITY_DESCRIPTOR> {
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::PSECURITY_DESCRIPTOR;
    use windows::core::PCWSTR;

    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    let encoded = wide(STORE_SDDL);
    // SAFETY: `encoded` is a null-terminated wide string that outlives the
    // call; the descriptor it allocates is the caller's to free.
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(encoded.as_ptr()),
            SDDL_REVISION_1,
            &raw mut descriptor,
            None,
        )
    };
    if converted.is_err() {
        tracing::error!("cannot build the machine store's access list");
        return None;
    }
    Some(descriptor)
}

/// `CreateDirectoryW` with [`STORE_SDDL`] on it.
fn create_protected_directory(path: &Path) -> bool {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::SECURITY_ATTRIBUTES;
    use windows::Win32::Storage::FileSystem::CreateDirectoryW;
    use windows::core::PCWSTR;

    let Some(descriptor) = store_descriptor() else {
        return false;
    };
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: false.into(),
    };
    let name = wide_path(path);
    // SAFETY: `name` and `attributes` are locals that outlive the call, and
    // `attributes` borrows the descriptor freed immediately below.
    let created =
        unsafe { CreateDirectoryW(PCWSTR(name.as_ptr()), Some(&raw const attributes)) }.is_ok();
    // SAFETY: the descriptor came from `store_descriptor` and is not read
    // again; the directory holds its own copy by now.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
    }
    created
}

/// Replaces `path`'s access list with [`STORE_SDDL`], protected from
/// inheritance.
fn apply_access_list(path: &Path) -> bool {
    use windows::Win32::Foundation::{ERROR_SUCCESS, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{SE_FILE_OBJECT, SetNamedSecurityInfoW};
    use windows::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl,
        PROTECTED_DACL_SECURITY_INFORMATION,
    };
    use windows::core::PWSTR;

    let Some(descriptor) = store_descriptor() else {
        return false;
    };
    let mut present = windows::core::BOOL::default();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut defaulted = windows::core::BOOL::default();
    // SAFETY: `descriptor` came from the SDDL conversion above; the three
    // outputs are locals that outlive the call and are only written.
    let read = unsafe {
        GetSecurityDescriptorDacl(
            descriptor,
            &raw mut present,
            &raw mut dacl,
            &raw mut defaulted,
        )
    };
    let applied = if read.is_ok() && present.as_bool() && !dacl.is_null() {
        let mut name = wide_path(path);
        // `PROTECTED_DACL_SECURITY_INFORMATION` is the half that matters:
        // without it the list below is merged with whatever `%ProgramData%`
        // hands down, which includes entries for ordinary users.
        //
        // SAFETY: `name` is a null-terminated wide buffer that outlives the
        // call, and `dacl` points inside the descriptor, which is still alive
        // here and freed only afterwards.
        let status = unsafe {
            SetNamedSecurityInfoW(
                PWSTR(name.as_mut_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(dacl),
                None,
            )
        };
        if status != ERROR_SUCCESS {
            tracing::error!(
                path = %path.display(),
                code = status.0,
                "cannot protect the machine store directory"
            );
        }
        status == ERROR_SUCCESS
    } else {
        tracing::error!("the machine store's access list has no DACL to apply");
        false
    };
    // SAFETY: the descriptor came from `store_descriptor` and nothing reads it
    // after this point.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
    }
    applied
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Machine-wide, not per-user. A privileged service writing into whichever
    /// profile happened to be signed in first is how one person's address book
    /// becomes the machine's policy.
    #[test]
    fn the_store_is_machine_wide() {
        let dir = directory();
        assert!(dir.ends_with(DIRECTORY), "unexpected directory: {dir:?}");
        let text = dir.to_string_lossy().to_lowercase();
        assert!(!text.contains("appdata"));
        assert!(!text.contains(r"\users\"));
    }

    /// The keystore is inside the protected directory, not beside it. What is
    /// in it decides whether a guest gets in at all.
    #[test]
    fn the_keystore_lives_inside_the_protected_directory() {
        assert_eq!(keystore_path().parent(), Some(directory().as_path()));
    }

    /// The access list admits `LocalSystem` and administrators, inherits into
    /// everything created inside, and is protected — a list that merged with
    /// `%ProgramData%`'s own would admit ordinary users to the device
    /// password's hash.
    #[test]
    fn the_access_list_is_protected_and_admits_nobody_else() {
        assert!(
            STORE_SDDL.starts_with("D:P"),
            "an unprotected list inherits %ProgramData%'s permissive entries"
        );
        assert!(STORE_SDDL.contains("(A;OICI;GA;;;SY)"));
        assert!(STORE_SDDL.contains("(A;OICI;GA;;;BA)"));
        for trustee in [";;;IU)", ";;;BU)", ";;;WD)", ";;;AU)", ";;;IN)"] {
            assert!(
                !STORE_SDDL.contains(trustee),
                "{trustee} must not be admitted to the machine store"
            );
        }
    }

    /// Inheritance is on every entry, or a file created inside the directory
    /// would carry a different list from the directory itself.
    #[test]
    fn every_entry_inherits() {
        let entries: Vec<&str> = STORE_SDDL
            .split("(A;")
            .skip(1)
            .map(|entry| entry.trim_end_matches(')'))
            .collect();
        assert_eq!(entries.len(), 2, "unexpected entry count in {STORE_SDDL}");
        for entry in entries {
            assert!(
                entry.starts_with("OICI;"),
                "{entry} does not inherit into the files inside"
            );
        }
    }
}
