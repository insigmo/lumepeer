//! Where a host that belongs to the machine keeps its things (ADR 0085).
//!
//! Every path here comes from [`lumepeer_service::machine_store`] and none of
//! it from a user profile. That is a decision with a reason, not a detail: a
//! `LocalSystem` service writing into whichever profile happened to be first
//! is how one user's address book quietly becomes the machine's policy, and
//! how a second user signing in finds a host configured by somebody else.
//!
//! **The keystore is the interesting one.** `crates/net`'s native backends are
//! all per-user — Credential Manager, Secret Service, Keychain — and a
//! `LocalSystem` service has no meaningful user keyring to put a device
//! password hash into. So the host uses the encrypted [`FileKeystore`] that
//! already exists for headless environments, under the machine directory whose
//! access list admits `LocalSystem` and administrators and nobody else.
//!
//! What protects it is that list. ADR 0085 says so in as many words, and this
//! module does not pretend otherwise: the file key is derived from a random
//! secret that lives in the same protected directory, so anybody who can read
//! the keystore can read the secret too. It is worth having anyway, for the
//! one thing it does buy — a keystore file carried off on its own, by a backup
//! tool or a support bundle, is not a device password — and it is worth
//! *saying*, because a reader who assumed this was a defence against a local
//! administrator would be relying on something that is not there.

use std::path::{Path, PathBuf};

use lumepeer_net::keystore::{FileKeystore, Keystore};
use lumepeer_runtime::network::ActorStores;
use rand::Rng as _;

/// File the machine keystore's encryption secret lives in.
///
/// Beside the keystore, inside the same protected directory. Generated once
/// on first run and never regenerated: a new secret would not "rotate" the
/// keystore, it would make every slot in it unreadable, which on this store
/// means the machine's identity key and its device password at once.
const SECRET_FILE: &str = "keystore.secret";

/// Bytes of that secret.
///
/// Thirty-two because [`FileKeystore`] runs it through a key-derivation step
/// whose output is thirty-two, so more would be thrown away and less would be
/// the only weak link in a chain that is otherwise not the weak link at all.
const SECRET_BYTES: usize = 32;

/// The machine-wide address book (§8; ADR 0034).
fn address_book_path(directory: &Path) -> PathBuf {
    directory.join("address_book.json")
}

/// The live invite, so it survives a restart (ADR 0062).
fn invite_path(directory: &Path) -> PathBuf {
    directory.join("invite.json")
}

/// The remembered-hosts list (ADR 0016).
///
/// A host service is a poor guest and mostly will not have one, but the actor
/// is one actor and this is one of its stores; leaving it `None` would make a
/// service host the only node whose history silently never persists.
fn history_path(directory: &Path) -> PathBuf {
    directory.join("connection_history.json")
}

/// The audit log (§15; ADR 0041).
fn audit_path(directory: &Path) -> PathBuf {
    directory.join("audit.db")
}

/// Reads the keystore secret, creating it on first run.
///
/// `None` when it can be neither read nor written, which the caller must treat
/// as "there is no keystore": deriving a key from a fallback constant would
/// mean every machine that hit this path shared one, and a host that cannot
/// keep a secret should not be holding a device password.
fn keystore_secret(directory: &Path) -> Option<Vec<u8>> {
    let path = directory.join(SECRET_FILE);
    match std::fs::read(&path) {
        // A short file is a truncated one — a crash between create and write,
        // a disk that filled. Refusing beats padding it out to length, which
        // would silently weaken every key derived from it afterwards.
        Ok(secret) if secret.len() == SECRET_BYTES => return Some(secret),
        Ok(secret) => {
            tracing::error!(
                path = %path.display(),
                len = secret.len(),
                "the machine keystore secret is the wrong length; refusing to use it"
            );
            return None;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::error!(path = %path.display(), %error, "cannot read the machine keystore secret");
            return None;
        }
    }

    let mut secret = vec![0u8; SECRET_BYTES];
    rand::rng().fill_bytes(&mut secret);
    if let Err(error) = std::fs::write(&path, &secret) {
        tracing::error!(path = %path.display(), %error, "cannot write the machine keystore secret");
        return None;
    }
    tracing::info!(path = %path.display(), "minted this machine's keystore secret");
    Some(secret)
}

/// Opens a keystore over the machine store's identity slot.
///
/// Two of these are opened per run, which costs nothing: a `FileKeystore` is a
/// path and a derived key, and it holds no connection between calls — the same
/// reason `ActorStores` takes two independent handles rather than one behind
/// an `Arc`. Every other slot lands beside the path this is given, which is
/// why the path is the whole of what it has to be told.
fn open_keystore(secret: &[u8]) -> FileKeystore {
    FileKeystore::new(lumepeer_service::machine_store::keystore_path(), secret)
}

/// Everything the host's actor persists, or `None` when the machine store
/// cannot be protected.
///
/// The `None` is not a degraded mode to be worked around. A host that wrote a
/// device-password hash into a directory whose access list it could not apply
/// would be offering exactly the protection it failed to establish, so the
/// caller's only correct answer is to not host.
pub async fn open() -> Option<(ActorStores, PathBuf)> {
    let directory = lumepeer_service::machine_store::ensure_directory()?;
    let secret = keystore_secret(&directory)?;

    let keystore = open_keystore(&secret);
    let audit = open_audit_log(&audit_path(&directory), &keystore).await;

    Some((
        ActorStores {
            history_path: Some(history_path(&directory)),
            address_book_path: Some(address_book_path(&directory)),
            invite_path: Some(invite_path(&directory)),
            keystore: Box::new(keystore),
            remembered_password_keystore: Box::new(open_keystore(&secret)),
            audit,
        },
        directory,
    ))
}

/// Opens the audit log and starts its daily retention sweep (§15; ADR 0041).
///
/// Every failure is a warning and a `None`, never a refusal to start — §18's
/// rule that a storage fault degrades the feature that needs the storage. It
/// matters more here than in the desktop client, not less: this host's *only*
/// way in with nobody present is a credential admission, and every one of
/// those is audited (ADR 0085 §3d), so a log that will not open is worth
/// saying loudly. It is still not worth refusing to run over, because that
/// would hand anyone who can break the database a way to take the machine
/// offline.
async fn open_audit_log(
    path: &Path,
    keystore: &dyn Keystore,
) -> Option<lumepeer_runtime::audit_store::AuditStore> {
    let store =
        match lumepeer_runtime::audit_store::AuditStore::open(path.to_path_buf(), keystore).await {
            Ok(store) => store,
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "audit log unavailable; this host runs without an audit trail"
                );
                return None;
            }
        };

    // Once a day, not once per record: the sweep is a table scan and the
    // cutoff moves by seconds. `AuditStore::open` already swept once.
    let daily = store.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(
            lumepeer_core::constants::AUDIT_RETENTION_SWEEP_SECS,
        ));
        ticker.tick().await; // fires immediately; the open already pruned
        loop {
            ticker.tick().await;
            if let Err(error) = daily.prune().await {
                tracing::warn!(%error, "audit log: retention sweep failed");
            }
        }
    });
    Some(store)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every path the host persists to is under the machine store, never under
    /// a profile. A service that wrote one of these into `AppData` would be
    /// writing into `LocalSystem`'s own profile, which no user owns and no
    /// user can read — and would make the machine's policy depend on which
    /// account the service happened to run as.
    #[test]
    fn nothing_is_written_under_a_user_profile() {
        let directory = lumepeer_service::machine_store::directory();
        for path in [
            address_book_path(&directory),
            invite_path(&directory),
            history_path(&directory),
            audit_path(&directory),
            directory.join(SECRET_FILE),
        ] {
            let shown = path.to_string_lossy().to_lowercase();
            assert!(
                !shown.contains("appdata") && !shown.contains(r"\users\"),
                "a machine store path landed in a profile: {}",
                path.display()
            );
            assert!(
                path.starts_with(&directory),
                "a machine store path landed outside the protected directory: {}",
                path.display()
            );
        }
    }

    /// The keystore file itself is inside the protected directory, which is
    /// the only thing actually defending it (see the module header).
    #[test]
    fn the_keystore_sits_inside_the_protected_directory() {
        let directory = lumepeer_service::machine_store::directory();
        assert!(lumepeer_service::machine_store::keystore_path().starts_with(&directory));
    }
}
