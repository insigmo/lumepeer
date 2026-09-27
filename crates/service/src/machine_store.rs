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
//!
//! The list only protects a directory this service owns, so who owns it is
//! checked too, and the Win32 half of that lives in `program_data.rs`
//! (ADR 0122).

use std::path::PathBuf;

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
pub(crate) const STORE_SDDL: &str = "D:P(A;OICI;GA;;;SY)(A;OICI;GA;;;BA)";

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

/// Makes the store directory a real, administrator-owned directory carrying
/// [`STORE_SDDL`], creating it if it is missing — and the `%ProgramData%`
/// tree above it with it (ADR 0122).
///
/// A directory that is already there is not trusted for being there. Its owner
/// is read from the directory itself, through a handle that does not follow a
/// reparse point: one an ordinary user created before this service first ran
/// is refused rather than re-protected, because that user could have written
/// the keystore secret, the identity or the address book inside it, and would
/// still own it after any list this service applied. The tree's root is
/// secured first, so nobody but an administrator can put a folder in its
/// place afterwards.
///
/// `None` when any of that cannot be established. The caller's only safe
/// answer is to run without a machine store at all: a host that wrote a
/// device-password hash into a directory it could not protect would be worse
/// than one that did not start.
#[must_use]
pub fn ensure_directory() -> Option<PathBuf> {
    use crate::program_data::{self, Foreign, TREE_SDDL};

    if !program_data::secure_directory(&program_data::root(), TREE_SDDL, Foreign::Adopt) {
        tracing::error!(
            "cannot secure the machine directory tree; running without a machine store"
        );
        return None;
    }
    let path = directory();
    program_data::secure_directory(&path, STORE_SDDL, Foreign::Refuse).then_some(path)
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
