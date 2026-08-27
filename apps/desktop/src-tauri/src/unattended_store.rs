//! Persistence for the unattended-access credentials of §8 (ADR 0033).
//!
//! `lumepeer_core::unattended::UnattendedAccess` holds the credentials in
//! memory and makes every decision about them; this is only where they are
//! kept between runs. The store is the OS keystore
//! (`lumepeer_net::keystore`), never `config/*.toml` — `CLAUDE.md` forbids
//! secrets in config files, and both of these are secret material: the
//! password slot holds an Argon2id PHC string, the TOTP slot holds the shared
//! secret an authenticator app was provisioned with.
//!
//! Nothing here decides anything. It loads bytes into the core type and writes
//! the core type's bytes back out; the verdict on a login is
//! `UnattendedAccess::admit`'s alone (§2.1).

use lumepeer_core::consent::Role;
use lumepeer_core::unattended::UnattendedAccess;
use lumepeer_net::keystore::{
    Keystore, UNATTENDED_PASSWORD_ENTRY, UNATTENDED_ROLE_ENTRY, UNATTENDED_TOTP_ENTRY,
};

/// Bytes of a TOTP shared secret, as `UnattendedAccess` stores it.
const TOTP_SECRET_BYTES: usize = 20;

/// Keystore-backed persistence of the unattended credentials.
#[derive(Debug)]
pub struct UnattendedStore {
    store: Box<dyn Keystore>,
}

impl UnattendedStore {
    /// Wraps an open keystore.
    #[must_use]
    pub const fn new(store: Box<dyn Keystore>) -> Self {
        Self { store }
    }

    /// Fills `access` from the keystore.
    ///
    /// Every failure mode here degrades to "unattended access is off" with a
    /// warning rather than taking the app down (§18): a keyring that cannot be
    /// unlocked must leave the host asking a human for consent, which is the
    /// safe direction. The one thing it must never do is leave the gate
    /// enabled with credentials it could not read.
    pub fn restore(&self, access: &mut UnattendedAccess) {
        let phc = match self.store.load_secret(UNATTENDED_PASSWORD_ENTRY) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(%error, "cannot read the unattended password; unattended access stays off");
                return;
            }
        };
        let Ok(phc) = String::from_utf8(phc) else {
            tracing::warn!(
                "the stored unattended password hash is not text; unattended access stays off"
            );
            return;
        };
        access.restore_password_hash(&phc);

        match self.store.load_secret(UNATTENDED_TOTP_ENTRY) {
            Ok(Some(bytes)) => {
                if let Ok(secret) = <[u8; TOTP_SECRET_BYTES]>::try_from(bytes.as_slice()) {
                    access.set_totp_secret(secret);
                } else {
                    // Refusing to start rather than dropping the factor: a
                    // second factor that silently disappears is exactly the
                    // silent downgrade §18 forbids.
                    tracing::warn!(
                        "the stored TOTP secret has the wrong length; unattended access stays off"
                    );
                    access.disable();
                    return;
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%error, "cannot read the TOTP secret; unattended access stays off");
                access.disable();
                return;
            }
        }

        // The role is the least dangerous of the three to lose, and the safe
        // direction is down: an unreadable slot leaves `UnattendedAccess`'s own
        // `ViewOnly` default in place rather than guessing at what the host
        // last chose.
        match self.store.load_secret(UNATTENDED_ROLE_ENTRY) {
            Ok(Some(bytes)) => {
                if let Ok(role) = postcard::from_bytes::<Role>(&bytes) {
                    access.set_role(role);
                } else {
                    tracing::warn!(
                        "the stored unattended role is unreadable; falling back to view-only"
                    );
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%error, "cannot read the unattended role; falling back to view-only");
            }
        }
    }

    /// Writes `access`'s password hash to the keystore.
    ///
    /// # Errors
    /// [`lumepeer_net::NetError`] if the keystore refuses the write. The
    /// caller surfaces it: a password the host thinks it set but which did not
    /// persist would be discovered at the worst possible moment.
    pub fn save_password(&self, access: &UnattendedAccess) -> Result<(), lumepeer_net::NetError> {
        match access.stored_secret() {
            Some(phc) => self
                .store
                .store_secret(UNATTENDED_PASSWORD_ENTRY, phc.as_bytes()),
            None => self.store.delete_secret(UNATTENDED_PASSWORD_ENTRY),
        }
    }

    /// Writes `access`'s TOTP secret to the keystore, or clears the slot.
    ///
    /// # Errors
    /// [`lumepeer_net::NetError`] if the keystore refuses the write.
    pub fn save_totp(&self, access: &UnattendedAccess) -> Result<(), lumepeer_net::NetError> {
        match access.stored_totp_secret() {
            Some(secret) => self.store.store_secret(UNATTENDED_TOTP_ENTRY, secret),
            None => self.store.delete_secret(UNATTENDED_TOTP_ENTRY),
        }
    }

    /// Writes the role a successful admission is granted.
    ///
    /// # Errors
    /// [`lumepeer_net::NetError`] if the keystore refuses the write, or if the
    /// role cannot be encoded — impossible for a fieldless enum, handled
    /// rather than unwrapped anyway.
    pub fn save_role(&self, access: &UnattendedAccess) -> Result<(), lumepeer_net::NetError> {
        let bytes = postcard::to_allocvec(&access.role()).map_err(|_| {
            lumepeer_net::NetError::Keystore("cannot encode the unattended role".to_owned())
        })?;
        self.store.store_secret(UNATTENDED_ROLE_ENTRY, &bytes)
    }

    /// Clears every slot, for turning unattended access off.
    ///
    /// # Errors
    /// [`lumepeer_net::NetError`] if either delete is refused. The TOTP slot is
    /// cleared first, so a failure halfway through can never leave a host with
    /// a live second factor and no password to go with it.
    pub fn clear(&self) -> Result<(), lumepeer_net::NetError> {
        self.store.delete_secret(UNATTENDED_TOTP_ENTRY)?;
        self.store.delete_secret(UNATTENDED_ROLE_ENTRY)?;
        self.store.delete_secret(UNATTENDED_PASSWORD_ENTRY)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use lumepeer_net::keystore::MemoryKeystore;

    use super::*;

    fn store() -> UnattendedStore {
        UnattendedStore::new(Box::new(MemoryKeystore::new()))
    }

    #[test]
    fn nothing_stored_leaves_the_gate_disabled() {
        let store = store();
        let mut access = UnattendedAccess::new();
        store.restore(&mut access);
        assert!(!access.enabled());
    }

    #[test]
    fn a_password_survives_a_restart() {
        let store = store();
        let mut saved = UnattendedAccess::new();
        saved.set_password("correct horse battery staple").unwrap();
        store.save_password(&saved).unwrap();

        let mut restored = UnattendedAccess::new();
        store.restore(&mut restored);
        assert!(restored.enabled());
        assert!(!restored.code_required());
        assert_eq!(
            restored
                .admit(Some("correct horse battery staple"), None)
                .unwrap(),
            Role::ViewOnly
        );
    }

    #[test]
    fn the_second_factor_survives_a_restart_too() {
        let store = store();
        let mut saved = UnattendedAccess::new();
        saved.set_password("passphrase").unwrap();
        saved.set_totp_secret([9u8; 20]);
        store.save_password(&saved).unwrap();
        store.save_totp(&saved).unwrap();

        let mut restored = UnattendedAccess::new();
        store.restore(&mut restored);
        assert!(restored.code_required());
        assert_eq!(restored.stored_totp_secret(), Some(&[9u8; 20]));
    }

    /// A stored second factor that cannot be read back must not quietly become
    /// a password-only gate: the host set up two factors and would never be
    /// told it now has one (§18).
    #[test]
    fn a_corrupt_totp_secret_turns_unattended_access_off_rather_than_downgrading_it() {
        let keystore = MemoryKeystore::new();
        let mut saved = UnattendedAccess::new();
        saved.set_password("passphrase").unwrap();
        keystore
            .store_secret(
                UNATTENDED_PASSWORD_ENTRY,
                saved.stored_secret().unwrap().as_bytes(),
            )
            .unwrap();
        keystore
            .store_secret(UNATTENDED_TOTP_ENTRY, b"too short")
            .unwrap();

        let mut restored = UnattendedAccess::new();
        UnattendedStore::new(Box::new(keystore)).restore(&mut restored);
        assert!(!restored.enabled());
    }

    #[test]
    fn the_role_survives_a_restart_and_defaults_to_view_only() {
        let store = store();
        let mut saved = UnattendedAccess::new();
        saved.set_password("passphrase").unwrap();
        store.save_password(&saved).unwrap();

        let mut restored = UnattendedAccess::new();
        store.restore(&mut restored);
        assert_eq!(
            restored.role(),
            Role::ViewOnly,
            "nothing stored means the least"
        );

        saved.set_role(Role::FullControl);
        store.save_role(&saved).unwrap();
        let mut restored = UnattendedAccess::new();
        store.restore(&mut restored);
        assert_eq!(restored.role(), Role::FullControl);
    }

    /// The role must not outlive the password it was chosen for: a host that
    /// turns unattended access off and later sets a new password starts from
    /// view-only again, not from whatever the old setup granted.
    #[test]
    fn clearing_drops_the_role_with_the_credentials() {
        let store = store();
        let mut saved = UnattendedAccess::new();
        saved.set_password("passphrase").unwrap();
        saved.set_role(Role::FullControl);
        store.save_password(&saved).unwrap();
        store.save_role(&saved).unwrap();

        store.clear().unwrap();
        let mut restored = UnattendedAccess::new();
        store.restore(&mut restored);
        assert_eq!(restored.role(), Role::ViewOnly);
    }

    #[test]
    fn clearing_removes_both_slots() {
        let store = store();
        let mut saved = UnattendedAccess::new();
        saved.set_password("passphrase").unwrap();
        saved.set_totp_secret([1u8; 20]);
        store.save_password(&saved).unwrap();
        store.save_totp(&saved).unwrap();

        store.clear().unwrap();
        let mut restored = UnattendedAccess::new();
        store.restore(&mut restored);
        assert!(!restored.enabled());
        assert!(!restored.code_required());
    }
}
