//! The machine-wide `%ProgramData%\Lumepeer` tree, and how it is kept out of
//! an unprivileged user's hands (ADR 0122).
//!
//! Both services write here as `LocalSystem`: the helper and the host their
//! logs (`log.rs`), the host its keystore, address book and invite
//! (`machine_store.rs`). `%ProgramData%` itself lets every ordinary user
//! create a folder in it and become that folder's owner, so "the directory is
//! there" says nothing about who made it. Three things follow, and this module
//! is where each of them is enforced:
//!
//! - **A directory somebody else made is not ours by being there.** A folder
//!   an ordinary user created before this service ever ran keeps that user as
//!   its owner, and an owner can rewrite the access list at any time — so
//!   re-applying our list over it protects nothing. Whoever owns it is read
//!   from the directory itself: `LocalSystem` or administrators, or it is
//!   either taken over (a directory holding nothing we trust, like the log
//!   folder) or refused (the host's store, whose contents that user could
//!   have planted).
//! - **A reparse point is not a directory.** A junction in the place of one
//!   of these folders would send `LocalSystem`'s writes wherever the user
//!   pointed it. Every directory is opened without following reparse points
//!   and refused if it is one, and its owner and access list are read and set
//!   through that same handle, so nothing can be swapped in between.
//! - **Nobody but administrators writes here.** The root's own list lets
//!   ordinary users read — the logs are for whoever is debugging this
//!   machine — and nothing more, so no ordinary user can put a file or a
//!   folder anywhere beneath it once it has been secured.

#![allow(
    unsafe_code,
    reason = "opening a directory without following reparse points, and reading \
              and setting its owner and access list through that handle, have \
              no safe bindings; same justification standard as the rest of \
              this crate's Win32 surface (ADR 0043, ADR 0049)"
)]

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
    SE_FILE_OBJECT, SetSecurityInfo,
};
use windows::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner,
    IsWellKnownSid, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, WinBuiltinAdministratorsSid,
    WinLocalSystemSid,
};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateDirectoryW, CreateFileW, FILE_ACCESS_RIGHTS,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GetFileInformationByHandle, OPEN_EXISTING, READ_CONTROL, WRITE_DAC, WRITE_OWNER,
};
use windows::core::PCWSTR;

/// The tree's root, under `%ProgramData%`.
const ROOT: &str = "Lumepeer";

/// The root's access list, and the log folder's: `LocalSystem` and
/// administrators in full, ordinary users read and list only.
///
/// - `D:P` — protected, so `%ProgramData%`'s own entries, which let every user
///   create files and folders, are dropped rather than merged.
/// - `OICI` — inherited by everything created beneath, so a log file gets the
///   same list as the folder it is in.
/// - `GRGX;;;BU` — read and traverse for built-in users, which is what lets a
///   person debugging this machine read the logs without being able to write
///   a byte into the tree.
pub const TREE_SDDL: &str = "D:P(A;OICI;GA;;;SY)(A;OICI;GA;;;BA)(A;OICI;GRGX;;;BU)";

/// Every directory secured here is given to the administrators group, which
/// `LocalSystem` may name as an owner (it is a group-owner member of it)
/// without any privilege beyond the ones it already holds.
const OWNER_SDDL: &str = "O:BA";

/// `%ProgramData%\Lumepeer`, whether or not it exists yet.
///
/// The literal fallback covers a service environment with no `ProgramData`
/// variable, which should not happen and is not a reason to have no answer.
#[must_use]
pub fn root() -> PathBuf {
    std::env::var_os("ProgramData")
        .map_or_else(|| PathBuf::from(r"C:\ProgramData"), PathBuf::from)
        .join(ROOT)
}

/// What to do with a directory whose owner is neither `LocalSystem` nor
/// administrators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Foreign {
    /// Take it over: its contents are nothing anybody reads as a decision
    /// (the root, which only holds folders; the log folder).
    Adopt,
    /// Refuse it: its contents decide who gets into this machine, and a user
    /// who owned the folder could have written any of them.
    Refuse,
}

/// Makes `path` a real directory, owned by administrators and carrying
/// `dacl_sddl` as a protected list, creating it if it is missing.
///
/// `false` when that cannot be established — the path is a reparse point, the
/// directory belongs to somebody else and `foreign` says to refuse it, or the
/// owner and list cannot be written. The caller's only safe answer is not to
/// use the directory at all.
#[must_use]
pub fn secure_directory(path: &Path, dacl_sddl: &str, foreign: Foreign) -> bool {
    let Some(descriptor) = Descriptor::parse(&format!("{OWNER_SDDL}{dacl_sddl}")) else {
        tracing::error!("cannot build an access list for {}", path.display());
        return false;
    };
    // Created with the list already on it, so a fresh directory is never
    // readable by anyone else even for a moment. A failure here is fine as
    // long as the directory now exists: that is either a race with another
    // service starting, or a directory that was already there, and the
    // checks below decide about both.
    let name = wide(&path.to_string_lossy());
    let attributes = descriptor.attributes();
    // SAFETY: `name` and `attributes` are locals that outlive the call, and
    // `attributes` borrows `descriptor`, which is still alive.
    let _ = unsafe { CreateDirectoryW(PCWSTR(name.as_ptr()), Some(&raw const attributes)) };

    let Some(handle) = open_without_following(path, READ_CONTROL | WRITE_DAC | WRITE_OWNER) else {
        tracing::error!(path = %path.display(), "cannot open a machine directory to secure it");
        return false;
    };
    let secured = secure_handle(&handle, path, &descriptor, foreign);
    drop(handle);
    secured
}

/// Removes `path` if it is anything but a plain file owned by `LocalSystem`
/// or administrators with a single name.
///
/// For a file that is about to be opened for writing by `LocalSystem` inside
/// a directory that has just been secured: whatever an ordinary user left
/// there *before* it was secured — a link to somewhere else, a file they own
/// and could still rewrite — is gone before the first write. `false` only when
/// such a file is there and cannot be removed.
#[must_use]
pub fn discard_untrusted_file(path: &Path) -> bool {
    if std::fs::symlink_metadata(path).is_err() {
        // Nothing there: the file this service creates next is its own.
        return true;
    }
    // Something is there. Whatever cannot even be read is treated as
    // somebody else's, like one that can and turns out to be.
    let trusted = open_without_following(path, READ_CONTROL).is_some_and(|handle| {
        let plain_single_file = handle.information().is_some_and(|info| {
            info.dwFileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT.0 | FILE_ATTRIBUTE_DIRECTORY.0)
                == 0
                && info.nNumberOfLinks <= 1
        });
        plain_single_file && handle.owner_is_trusted() == Some(true)
    });
    if trusted {
        return true;
    }
    tracing::warn!(path = %path.display(), "removing a file in a machine directory that this service did not write");
    std::fs::remove_file(path).is_ok()
}

/// Reads who owns the directory behind `handle`, refuses what is not a
/// directory, and writes the owner and list of `descriptor` onto it.
fn secure_handle(
    handle: &OwnedHandle,
    path: &Path,
    descriptor: &Descriptor,
    foreign: Foreign,
) -> bool {
    let Some(info) = handle.information() else {
        tracing::error!(path = %path.display(), "cannot read what a machine directory is");
        return false;
    };
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        tracing::error!(
            path = %path.display(),
            "refusing a machine directory that is a reparse point: it would redirect this service's writes"
        );
        return false;
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0 {
        tracing::error!(path = %path.display(), "refusing a machine directory that is a file");
        return false;
    }
    match handle.owner_is_trusted() {
        Some(true) => {}
        Some(false) if foreign == Foreign::Adopt => {
            tracing::warn!(
                path = %path.display(),
                "a machine directory was created by an ordinary user; taking it over"
            );
        }
        Some(false) => {
            tracing::error!(
                path = %path.display(),
                "refusing a machine directory that an ordinary user owns: anything in it may have \
                 been planted. An administrator should delete this folder; it is recreated on the \
                 next start"
            );
            return false;
        }
        None => {
            tracing::error!(path = %path.display(), "cannot read who owns a machine directory");
            return false;
        }
    }
    let (Some(owner), Some(dacl)) = (descriptor.owner(), descriptor.dacl()) else {
        return false;
    };
    // SAFETY: `handle` is live and was opened with `WRITE_OWNER | WRITE_DAC`;
    // `owner` and `dacl` point inside `descriptor`, which outlives the call.
    let status = unsafe {
        SetSecurityInfo(
            handle.raw(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            Some(owner),
            None,
            Some(dacl),
            None,
        )
    };
    if status != ERROR_SUCCESS {
        tracing::error!(
            path = %path.display(),
            code = status.0,
            "cannot protect a machine directory"
        );
        return false;
    }
    true
}

/// Opens `path` — file or directory — with `access`, without following a
/// reparse point if that is what it is.
fn open_without_following(path: &Path, access: FILE_ACCESS_RIGHTS) -> Option<OwnedHandle> {
    let name = wide(&path.to_string_lossy());
    // SAFETY: `name` is a null-terminated wide string that outlives the call;
    // the handle it returns is owned by the `OwnedHandle` below.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            access.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .ok()?;
    (!handle.is_invalid()).then_some(OwnedHandle(handle))
}

/// A handle closed when it goes out of scope.
struct OwnedHandle(HANDLE);

impl OwnedHandle {
    const fn raw(&self) -> HANDLE {
        self.0
    }

    fn information(&self) -> Option<BY_HANDLE_FILE_INFORMATION> {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: the handle is live; `info` is a local that outlives the call.
        unsafe { GetFileInformationByHandle(self.0, &raw mut info) }.ok()?;
        Some(info)
    }

    /// Whether the object's owner is `LocalSystem` or administrators; `None`
    /// when the owner cannot be read.
    fn owner_is_trusted(&self) -> Option<bool> {
        let mut owner = PSID::default();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: the handle is live and was opened with `READ_CONTROL`; the
        // two outputs are locals, and the descriptor is freed below.
        let status = unsafe {
            GetSecurityInfo(
                self.0,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                Some(&raw mut owner),
                None,
                None,
                None,
                Some(&raw mut descriptor),
            )
        };
        let trusted = (status == ERROR_SUCCESS && !owner.is_invalid()).then(|| {
            // SAFETY: `owner` points inside `descriptor`, which is still alive.
            unsafe {
                IsWellKnownSid(owner, WinLocalSystemSid).as_bool()
                    || IsWellKnownSid(owner, WinBuiltinAdministratorsSid).as_bool()
            }
        });
        if !descriptor.is_invalid() {
            // SAFETY: allocated by `GetSecurityInfo` and not read again.
            unsafe {
                let _ = LocalFree(Some(HLOCAL(descriptor.0)));
            }
        }
        trusted
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from `CreateFileW` and is closed only here.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// A security descriptor parsed from SDDL, freed when it goes out of scope.
struct Descriptor(PSECURITY_DESCRIPTOR);

impl Descriptor {
    fn parse(sddl: &str) -> Option<Self> {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        let encoded = wide(sddl);
        // SAFETY: `encoded` is a null-terminated wide string that outlives the
        // call; the descriptor it allocates is owned by `Self` from here.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(encoded.as_ptr()),
                SDDL_REVISION_1,
                &raw mut descriptor,
                None,
            )
        }
        .ok()?;
        Some(Self(descriptor))
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
            lpSecurityDescriptor: self.0.0,
            bInheritHandle: false.into(),
        }
    }

    fn owner(&self) -> Option<PSID> {
        let mut owner = PSID::default();
        let mut defaulted = windows::core::BOOL::default();
        // SAFETY: the descriptor is live; both outputs are locals.
        unsafe { GetSecurityDescriptorOwner(self.0, &raw mut owner, &raw mut defaulted) }.ok()?;
        (!owner.is_invalid()).then_some(owner)
    }

    fn dacl(&self) -> Option<*const ACL> {
        let mut present = windows::core::BOOL::default();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut defaulted = windows::core::BOOL::default();
        // SAFETY: the descriptor is live; the three outputs are locals.
        unsafe {
            GetSecurityDescriptorDacl(self.0, &raw mut present, &raw mut dacl, &raw mut defaulted)
        }
        .ok()?;
        (present.as_bool() && !dacl.is_null()).then_some(dacl.cast_const())
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
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// The tree is readable by ordinary users, writable by nobody but
    /// `LocalSystem` and administrators, and does not inherit
    /// `%ProgramData%`'s own create-anything entries.
    #[test]
    fn the_tree_admits_writers_only_among_system_and_administrators() {
        assert!(TREE_SDDL.starts_with("D:P"), "the list must be protected");
        assert!(TREE_SDDL.contains("(A;OICI;GA;;;SY)"));
        assert!(TREE_SDDL.contains("(A;OICI;GA;;;BA)"));
        assert!(
            TREE_SDDL.contains("(A;OICI;GRGX;;;BU)"),
            "users read the logs"
        );
        for writer in ["GA;;;BU)", "GW;;;BU)", ";;;IU)", ";;;WD)", ";;;AU)", "CO)"] {
            assert!(!TREE_SDDL.contains(writer), "{writer} must not be admitted");
        }
    }

    /// The root is machine-wide, never a profile.
    #[test]
    fn the_root_is_under_program_data() {
        let root = root();
        assert!(root.ends_with(ROOT));
        let shown = root.to_string_lossy().to_lowercase();
        assert!(!shown.contains("appdata") && !shown.contains(r"\users\"));
    }

    /// Both lists this module writes parse, with an owner, so a typo cannot
    /// reach a machine as a service that silently secures nothing.
    #[test]
    fn every_list_parses_with_an_owner_and_a_dacl() {
        for dacl in [TREE_SDDL, crate::machine_store::STORE_SDDL] {
            let descriptor = Descriptor::parse(&format!("{OWNER_SDDL}{dacl}")).unwrap();
            assert!(descriptor.owner().is_some(), "{dacl} has no owner");
            assert!(descriptor.dacl().is_some(), "{dacl} has no dacl");
        }
    }

    /// A junction in place of a directory is refused rather than followed:
    /// securing it would rewrite the access list of wherever it points, and
    /// using it would send this service's writes there.
    #[test]
    fn a_junction_is_refused_and_its_target_left_alone() {
        let base =
            std::env::temp_dir().join(format!("lumepeer-program-data-test-{}", std::process::id()));
        let target = base.join("target");
        let link = base.join("link");
        std::fs::create_dir_all(&target).unwrap();
        let made = std::process::Command::new(r"C:\Windows\System32\cmd.exe")
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output()
            .is_ok_and(|output| output.status.success());
        if !made {
            eprintln!("skipping: could not create a junction here");
            let _ = std::fs::remove_dir_all(&base);
            return;
        }
        assert!(!secure_directory(&link, TREE_SDDL, Foreign::Adopt));
        assert!(target.is_dir(), "the junction's target must be untouched");
        let _ = std::fs::remove_dir(&link);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Hands `path` to the signed-in user, the owner an ordinary process
    /// would leave on a folder it made under `%ProgramData%`.
    fn give_to_this_user(path: &Path) -> bool {
        let (Ok(domain), Ok(user)) = (std::env::var("USERDOMAIN"), std::env::var("USERNAME"))
        else {
            return false;
        };
        std::process::Command::new(r"C:\Windows\System32\icacls.exe")
            .arg(path)
            .args(["/setowner", &format!("{domain}\\{user}")])
            .output()
            .is_ok_and(|output| output.status.success())
    }

    /// ADR 0122: a folder an ordinary user owns is taken over where nothing in
    /// it is trusted, refused where its contents decide who gets in, and a
    /// file in it this service did not write is removed while one it did
    /// write stays.
    ///
    /// Only runs elevated: giving a directory to administrators is exactly
    /// what an unelevated process cannot do, which is the point of the whole
    /// module.
    #[test]
    fn a_folder_an_ordinary_user_owns_is_adopted_or_refused() {
        let base = std::env::temp_dir().join(format!(
            "lumepeer-program-data-owner-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        if !secure_directory(&base, TREE_SDDL, Foreign::Adopt) {
            eprintln!("skipping: securing a directory needs an elevated run");
            let _ = std::fs::remove_dir_all(&base);
            return;
        }
        let ours = base.join("ours.log");
        let planted = base.join("planted.log");
        std::fs::write(&ours, b"written by this service").unwrap();
        std::fs::write(&planted, b"left by somebody else").unwrap();
        assert!(give_to_this_user(&planted) && give_to_this_user(&base));

        assert!(
            !secure_directory(&base, TREE_SDDL, Foreign::Refuse),
            "a user's folder must not hold the host's store"
        );
        assert!(secure_directory(&base, TREE_SDDL, Foreign::Adopt));
        assert_eq!(
            open_without_following(&base, READ_CONTROL)
                .unwrap()
                .owner_is_trusted(),
            Some(true),
            "an adopted folder belongs to administrators"
        );

        assert!(discard_untrusted_file(&ours));
        assert!(ours.exists(), "a file this service wrote stays");
        assert!(discard_untrusted_file(&planted));
        assert!(!planted.exists(), "a file somebody else left is removed");
        let _ = std::fs::remove_dir_all(&base);
    }
}
