//! Where this run keeps each of its stores (ADR 0123).
//!
//! The stores that decide who gets into this machine — the device password,
//! its role and second factor, which devices are trusted — and the ones that
//! are this node's own secrets — its endpoint identity, the passwords it
//! remembers for other hosts — used to live in the signed-in user's profile:
//! the Credential Manager and `%APPDATA%`. Any program that account runs can
//! write both, elevated or not. So a program with no rights at all could set a
//! device password and trust a device of its own, and then drive this
//! always-elevated client (ADR 0057) from the network, UAC prompts included:
//! a quiet way in, and a way up.
//!
//! On Windows, an elevated run therefore keeps all of them in its own
//! directory under `%ProgramData%\Lumepeer\users`, which only administrators
//! can write ([`lumepeer_service::program_data::user_directory`]). The first
//! such run moves what the profile held there and deletes it from the profile.
//! An elevated run that cannot establish that directory runs with unattended
//! access off and no device trusted, rather than on stores anybody could have
//! written.
//!
//! Everything else — an unelevated development run, `LUMEPEER_KEYSTORE=file`
//! for the headless end-to-end rigs, Linux and macOS — keeps the profile, as
//! before: there is no account boundary below this process to put anything
//! behind.

use std::path::PathBuf;

use lumepeer_net::NetError;
use lumepeer_net::keystore::Keystore;

/// Where each store of this run lives.
pub struct Placement {
    /// The endpoint identity, the unattended credentials and the audit salt.
    pub keystore: Box<dyn Keystore>,
    /// Passwords remembered for other hosts.
    pub remembered: Box<dyn Keystore>,
    /// The address book, trusted devices included.
    pub address_book: Option<PathBuf>,
    /// The directory the live invite is kept in.
    pub invite_dir: Option<PathBuf>,
    /// The remembered-hosts list.
    pub history: Option<PathBuf>,
    /// The audit log.
    pub audit: Option<PathBuf>,
}

impl std::fmt::Debug for Placement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Placement")
            .field("address_book", &self.address_book)
            .field("invite_dir", &self.invite_dir)
            .finish_non_exhaustive()
    }
}

/// The profile's own locations, where every store lived before ADR 0123.
#[derive(Debug, Clone)]
pub struct Profile {
    /// `%APPDATA%\…\address_book.json`.
    pub address_book: Option<PathBuf>,
    /// `%APPDATA%\…`, where `invite-<key>.json` is.
    pub invite_dir: Option<PathBuf>,
    /// `%LOCALAPPDATA%\…\connection_history.json`.
    pub history: Option<PathBuf>,
    /// `%LOCALAPPDATA%\…\audit.db`.
    pub audit: Option<PathBuf>,
}

/// Chooses where this run keeps its stores; `open_profile_keystore` opens the
/// profile's own keystore, as every run did before ADR 0123.
///
/// # Errors
/// Whatever opening the profile keystore returns, where the profile is used.
pub fn choose(
    profile: Profile,
    open_profile_keystore: &dyn Fn() -> Result<Box<dyn Keystore>, NetError>,
) -> Result<Placement, NetError> {
    #[cfg(target_os = "windows")]
    if std::env::var("LUMEPEER_KEYSTORE").as_deref() != Ok("file")
        && let Some((sid, true)) = lumepeer_service::program_data::this_process()
    {
        if let Some(placement) = protected::open(&sid, &profile, open_profile_keystore) {
            tracing::info!(
                directory = ?placement.invite_dir,
                "keeping the device password, trusted devices and identity where only administrators can write"
            );
            return Ok(placement);
        }
        tracing::error!(
            "cannot secure this account's store under %ProgramData%\\Lumepeer\\users: \
             running with unattended access off and no device trusted"
        );
        return Ok(Placement {
            keystore: Box::new(WithoutPolicy(open_profile_keystore()?)),
            remembered: open_profile_keystore()?,
            address_book: None,
            invite_dir: profile.invite_dir,
            history: profile.history,
            audit: profile.audit,
        });
    }
    Ok(Placement {
        keystore: open_profile_keystore()?,
        remembered: open_profile_keystore()?,
        address_book: profile.address_book,
        invite_dir: profile.invite_dir,
        history: profile.history,
        audit: profile.audit,
    })
}

/// The account whose protected stores this run keeps, or `None` when it keeps
/// the profile (ADR 0123) — the same conditions [`choose`] reads.
#[cfg(target_os = "windows")]
pub fn protected_account() -> Option<String> {
    if std::env::var("LUMEPEER_KEYSTORE").as_deref() == Ok("file") {
        return None;
    }
    match lumepeer_service::program_data::this_process()? {
        (sid, true) => Some(sid),
        (_, false) => None,
    }
}

/// The stores of the account `sid`, where its elevated client keeps them, for
/// the logon host (ADR 0126); `None` when that directory cannot be opened.
///
/// Nothing is moved out of a profile here: the logon host runs as
/// `LocalSystem` and has no profile of the owner's to move from.
#[cfg(target_os = "windows")]
pub fn for_logon_host(sid: &str) -> Option<Placement> {
    protected::existing(sid)
}

/// A profile keystore with the unattended credentials taken out of it: read
/// as absent, and refused on write (ADR 0123).
///
/// What an elevated run falls back to when it cannot protect its own store:
/// anything in the profile may have been written by a program with no rights,
/// and a device password is the one entry that would let such a program in.
#[cfg(any(target_os = "windows", test))]
#[derive(Debug)]
struct WithoutPolicy(Box<dyn Keystore>);

#[cfg(any(target_os = "windows", test))]
impl WithoutPolicy {
    fn is_policy(entry: &str) -> bool {
        use lumepeer_net::keystore::{
            UNATTENDED_PASSWORD_ENTRY, UNATTENDED_ROLE_ENTRY, UNATTENDED_TOTP_ENTRY,
        };
        [
            UNATTENDED_PASSWORD_ENTRY,
            UNATTENDED_ROLE_ENTRY,
            UNATTENDED_TOTP_ENTRY,
        ]
        .contains(&entry)
    }
}

#[cfg(any(target_os = "windows", test))]
impl Keystore for WithoutPolicy {
    fn load_secret(&self, entry: &str) -> lumepeer_net::error::Result<Option<Vec<u8>>> {
        if Self::is_policy(entry) {
            return Ok(None);
        }
        self.0.load_secret(entry)
    }

    fn store_secret(&self, entry: &str, bytes: &[u8]) -> lumepeer_net::error::Result<()> {
        if Self::is_policy(entry) {
            return Err(NetError::Keystore(
                "unattended access needs a store only administrators can write".to_owned(),
            ));
        }
        self.0.store_secret(entry, bytes)
    }

    fn delete_secret(&self, entry: &str) -> lumepeer_net::error::Result<()> {
        if Self::is_policy(entry) {
            return Ok(());
        }
        self.0.delete_secret(entry)
    }
}

#[cfg(target_os = "windows")]
mod protected {
    use std::path::{Path, PathBuf};

    use lumepeer_net::NetError;
    use lumepeer_net::keystore::{
        AUDIT_SALT_ENTRY, FileKeystore, IDENTITY_ENTRY, Keystore, UNATTENDED_PASSWORD_ENTRY,
        UNATTENDED_ROLE_ENTRY, UNATTENDED_TOTP_ENTRY,
    };

    use super::{Placement, Profile};

    /// Written once everything the profile held has been moved, so it is
    /// moved exactly once.
    const MOVED_MARKER: &str = "moved-from-profile";

    /// The protected placement for the account `sid`, moving the profile's
    /// stores into it the first time; `None` when the directory cannot be
    /// established.
    pub(super) fn open(
        sid: &str,
        profile: &Profile,
        open_profile_keystore: &dyn Fn() -> Result<Box<dyn Keystore>, NetError>,
    ) -> Option<Placement> {
        let directory = lumepeer_service::program_data::user_directory(sid)?;
        let placement = placement_in(&directory)?;
        if !directory.join(MOVED_MARKER).exists() {
            move_from_profile(&directory, profile, &placement, open_profile_keystore);
        }
        Some(placement)
    }

    /// The placement for the account `sid` as it stands, moving nothing
    /// (ADR 0126).
    pub(super) fn existing(sid: &str) -> Option<Placement> {
        placement_in(&lumepeer_service::program_data::user_directory(sid)?)
    }

    /// Every store of the protected `directory`.
    fn placement_in(directory: &Path) -> Option<Placement> {
        let secret = lumepeer_service::program_data::keystore_secret(directory)?;
        let keystore = || Box::new(FileKeystore::new(directory.join("identity.key"), &secret));
        Some(Placement {
            keystore: keystore(),
            remembered: keystore(),
            address_book: Some(directory.join("address_book.json")),
            invite_dir: Some(directory.to_path_buf()),
            history: Some(directory.join("connection_history.json")),
            audit: Some(directory.join("audit.db")),
        })
    }

    /// Moves what the profile held into `placement`, once.
    ///
    /// Nothing already in the protected store is overwritten, and a profile
    /// entry is deleted only once its copy is written, so a run interrupted
    /// half-way finishes the move on the next start rather than losing
    /// anything.
    fn move_from_profile(
        directory: &Path,
        profile: &Profile,
        placement: &Placement,
        open_profile_keystore: &dyn Fn() -> Result<Box<dyn Keystore>, NetError>,
    ) {
        let mut files: Vec<(Option<PathBuf>, Option<PathBuf>)> = vec![
            (profile.address_book.clone(), placement.address_book.clone()),
            (profile.history.clone(), placement.history.clone()),
        ];
        for suffix in ["", "-wal", "-shm"] {
            let with = |path: &PathBuf| PathBuf::from(format!("{}{suffix}", path.display()));
            files.push((
                profile.audit.as_ref().map(with),
                placement.audit.as_ref().map(with),
            ));
        }
        if let Some(dir) = &profile.invite_dir
            && let Ok(entries) = std::fs::read_dir(dir)
        {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("invite") && name.ends_with(".json") {
                    files.push((Some(entry.path()), Some(directory.join(&*name))));
                }
            }
        }
        for (from, to) in files {
            if let (Some(from), Some(to)) = (from, to) {
                move_file(&from, &to);
            }
        }

        let Ok(old) = open_profile_keystore() else {
            let _ = std::fs::write(directory.join(MOVED_MARKER), b"");
            return;
        };
        let mut entries: Vec<String> = [
            IDENTITY_ENTRY,
            UNATTENDED_PASSWORD_ENTRY,
            UNATTENDED_ROLE_ENTRY,
            UNATTENDED_TOTP_ENTRY,
            AUDIT_SALT_ENTRY,
        ]
        .map(str::to_owned)
        .to_vec();
        entries.extend(
            lumepeer_runtime::connection_history::ConnectionHistory::open(
                placement.history.clone(),
            )
            .entries()
            .iter()
            .map(|entry| lumepeer_runtime::remembered_password::entry_name(&entry.peer_label)),
        );
        let mut complete = true;
        for entry in entries {
            match (
                placement.keystore.load_secret(&entry),
                old.load_secret(&entry),
            ) {
                (Ok(None), Ok(Some(bytes))) => {
                    if placement.keystore.store_secret(&entry, &bytes).is_ok() {
                        let _ = old.delete_secret(&entry);
                    } else {
                        complete = false;
                    }
                }
                // Moved by an earlier run that stopped before deleting it.
                (Ok(Some(_)), Ok(Some(_))) => {
                    let _ = old.delete_secret(&entry);
                }
                (Ok(_), Ok(None)) => {}
                _ => complete = false,
            }
        }
        if complete {
            let _ = std::fs::write(directory.join(MOVED_MARKER), b"");
            tracing::info!("moved this account's stores out of its profile");
        }
    }

    /// Moves `from` to `to` unless `to` is already there.
    fn move_file(from: &Path, to: &Path) {
        if to.exists() || !from.is_file() {
            return;
        }
        match std::fs::copy(from, to) {
            Ok(_) => {
                let _ = std::fs::remove_file(from);
            }
            Err(error) => {
                tracing::warn!(from = %from.display(), %error, "could not move a store out of the profile");
            }
        }
    }

    #[cfg(test)]
    mod tests {
        #![allow(clippy::unwrap_used)]

        use std::sync::Arc;

        use lumepeer_core::consent::Role;
        use lumepeer_net::keystore::MemoryKeystore;
        use lumepeer_runtime::connection_history::ConnectionHistory;

        use super::*;

        /// One in-memory keystore behind every handle the code under test
        /// opens, the way every handle on the Credential Manager sees the
        /// same entries.
        #[derive(Debug, Clone, Default)]
        struct Shared(Arc<MemoryKeystore>);

        impl Keystore for Shared {
            fn load_secret(&self, entry: &str) -> lumepeer_net::Result<Option<Vec<u8>>> {
                self.0.load_secret(entry)
            }
            fn store_secret(&self, entry: &str, bytes: &[u8]) -> lumepeer_net::Result<()> {
                self.0.store_secret(entry, bytes)
            }
            fn delete_secret(&self, entry: &str) -> lumepeer_net::Result<()> {
                self.0.delete_secret(entry)
            }
        }

        /// ADR 0123: the first protected run moves the identity, the
        /// unattended credentials, remembered passwords and the store files
        /// out of the profile, deletes what it moved there, and does it once.
        #[test]
        fn the_profile_is_moved_once_and_left_empty() {
            let base =
                std::env::temp_dir().join(format!("lumepeer-placement-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            let (old_dir, new_dir) = (base.join("profile"), base.join("protected"));
            std::fs::create_dir_all(&old_dir).unwrap();
            std::fs::create_dir_all(&new_dir).unwrap();

            let profile = Profile {
                address_book: Some(old_dir.join("address_book.json")),
                invite_dir: Some(old_dir.clone()),
                history: Some(old_dir.join("connection_history.json")),
                audit: Some(old_dir.join("audit.db")),
            };
            std::fs::write(old_dir.join("address_book.json"), b"{}").unwrap();
            std::fs::write(old_dir.join("invite-0011.json"), b"{}").unwrap();
            ConnectionHistory::open(profile.history.clone()).record(
                "host-ab12".to_owned(),
                Role::ViewOnly,
                "lumepeer1:code".to_owned(),
                None,
                Vec::new(),
            );
            let old = Shared::default();
            old.store_secret(IDENTITY_ENTRY, &[7; 32]).unwrap();
            old.store_secret(UNATTENDED_PASSWORD_ENTRY, b"$argon2id$hash")
                .unwrap();
            let remembered = lumepeer_runtime::remembered_password::entry_name("host-ab12");
            old.store_secret(&remembered, b"device password").unwrap();

            let new = Shared::default();
            let placement = Placement {
                keystore: Box::new(new.clone()),
                remembered: Box::new(new.clone()),
                address_book: Some(new_dir.join("address_book.json")),
                invite_dir: Some(new_dir.clone()),
                history: Some(new_dir.join("connection_history.json")),
                audit: Some(new_dir.join("audit.db")),
            };
            let open_old = || -> Result<Box<dyn Keystore>, NetError> { Ok(Box::new(old.clone())) };
            move_from_profile(&new_dir, &profile, &placement, &open_old);

            for entry in [
                IDENTITY_ENTRY,
                UNATTENDED_PASSWORD_ENTRY,
                remembered.as_str(),
            ] {
                assert!(
                    new.load_secret(entry).unwrap().is_some(),
                    "{entry} was not moved"
                );
                assert!(
                    old.load_secret(entry).unwrap().is_none(),
                    "{entry} stayed in the profile"
                );
            }
            for name in [
                "address_book.json",
                "invite-0011.json",
                "connection_history.json",
            ] {
                assert!(new_dir.join(name).is_file(), "{name} was not moved");
                assert!(!old_dir.join(name).exists(), "{name} stayed in the profile");
            }
            assert!(new_dir.join(MOVED_MARKER).is_file());

            // Once: a device password planted in the profile afterwards is
            // not carried over by a later start.
            old.store_secret(UNATTENDED_TOTP_ENTRY, b"planted").unwrap();
            if !new_dir.join(MOVED_MARKER).exists() {
                move_from_profile(&new_dir, &profile, &placement, &open_old);
            }
            assert!(new.load_secret(UNATTENDED_TOTP_ENTRY).unwrap().is_none());
            let _ = std::fs::remove_dir_all(&base);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use lumepeer_net::keystore::{
        AUDIT_SALT_ENTRY, IDENTITY_ENTRY, MemoryKeystore, UNATTENDED_PASSWORD_ENTRY,
    };

    use super::*;

    /// ADR 0123: the fallback of an elevated run that cannot protect its
    /// store reads no device password, stores none, and passes everything
    /// else through.
    #[test]
    fn the_fallback_keystore_has_no_device_password() {
        let inner = MemoryKeystore::new();
        inner
            .store_secret(UNATTENDED_PASSWORD_ENTRY, b"$argon2id$planted")
            .unwrap();
        inner.store_secret(IDENTITY_ENTRY, &[7; 32]).unwrap();
        let guarded = WithoutPolicy(Box::new(inner));
        assert_eq!(
            guarded.load_secret(UNATTENDED_PASSWORD_ENTRY).unwrap(),
            None
        );
        assert!(
            guarded
                .store_secret(UNATTENDED_PASSWORD_ENTRY, b"x")
                .is_err()
        );
        assert_eq!(
            guarded.load_secret(IDENTITY_ENTRY).unwrap(),
            Some(vec![7; 32])
        );
        guarded.store_secret(AUDIT_SALT_ENTRY, b"salt").unwrap();
        assert_eq!(
            guarded.load_secret(AUDIT_SALT_ENTRY).unwrap(),
            Some(b"salt".to_vec())
        );
    }

    /// Off Windows, and in a run that is not elevated, nothing moves.
    #[test]
    fn an_unelevated_or_non_windows_run_keeps_the_profile() {
        #[cfg(target_os = "windows")]
        if lumepeer_service::program_data::this_process().is_some_and(|(_, elevated)| elevated) {
            return;
        }
        let profile = Profile {
            address_book: Some(PathBuf::from("book.json")),
            invite_dir: Some(PathBuf::from("dir")),
            history: None,
            audit: None,
        };
        let placement = choose(profile, &|| Ok(Box::new(MemoryKeystore::new()))).unwrap();
        assert_eq!(placement.address_book, Some(PathBuf::from("book.json")));
        assert_eq!(placement.invite_dir, Some(PathBuf::from("dir")));
    }
}
