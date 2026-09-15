//! Guest-side "remember this device's password" (§8; ADR 0033;
//! docs/bugs/02-connect-form.md, task 6; docs/bugs/DECISIONS.md, D2).
//!
//! A remembered password is kept in the OS keystore, one entry per host, and
//! is never returned to the webview: `invite-view.ts`'s own rule ("nothing in
//! this module keeps a copy") still holds for the TypeScript side. On the
//! next connect to the same host the Rust side substitutes it itself.
//!
//! Kept entirely in `apps/desktop/src-tauri`, alongside `unattended_store.rs`
//! and `connection_history.rs` — the TCB in `crates/core` decides nothing
//! about *this* node's own outgoing credentials, and has no reason to.

use lumepeer_net::NetError;
use lumepeer_net::keystore::Keystore;

/// Keystore entry name of the remembered password for one host.
///
/// One entry per `host_tag` — the same stable, install-salt-free label
/// `connection_history.rs` already keys its rows on — so the entry survives
/// restarts and does not collide with the unattended-access slots of
/// `crates/net::keystore`, which name *this* host's own credentials, not a
/// remote one's.
fn entry_name(host_tag: &str) -> String {
    format!("lumepeer.guest.password.{host_tag}")
}

/// Keystore-backed storage of remembered device passwords, one per host this
/// node has dialed with unattended credentials.
#[derive(Debug)]
pub struct RememberedPasswordStore {
    store: Box<dyn Keystore>,
}

impl RememberedPasswordStore {
    /// Wraps an open keystore.
    #[must_use]
    pub const fn new(store: Box<dyn Keystore>) -> Self {
        Self { store }
    }

    /// The password remembered for `host_tag`, if any.
    ///
    /// # Errors
    /// [`NetError::Keystore`] if the platform store refuses the read, or the
    /// stored bytes are not valid UTF-8 — treated as an error rather than
    /// silently forgotten, since a keystore that can be read but not decoded
    /// is worth a log line, not a quiet fall-through to the login modal.
    pub fn load(&self, host_tag: &str) -> Result<Option<String>, NetError> {
        let Some(bytes) = self.store.load_secret(&entry_name(host_tag))? else {
            return Ok(None);
        };
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| NetError::Keystore("the remembered password is not text".to_owned()))
    }

    /// Remembers `password` for `host_tag`, replacing any previous value.
    ///
    /// # Errors
    /// [`NetError::Keystore`] if the write is refused.
    pub fn save(&self, host_tag: &str, password: &str) -> Result<(), NetError> {
        self.store
            .store_secret(&entry_name(host_tag), password.as_bytes())
    }

    /// Forgets the password remembered for `host_tag`, if any. Forgetting an
    /// entry that was never stored still succeeds.
    ///
    /// # Errors
    /// [`NetError::Keystore`] if the delete is refused.
    pub fn forget(&self, host_tag: &str) -> Result<(), NetError> {
        self.store.delete_secret(&entry_name(host_tag))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use lumepeer_net::keystore::MemoryKeystore;

    use super::*;

    fn store() -> RememberedPasswordStore {
        RememberedPasswordStore::new(Box::new(MemoryKeystore::new()))
    }

    #[test]
    fn nothing_remembered_is_none_rather_than_an_error() {
        let store = store();
        assert_eq!(store.load("host-ab12").unwrap(), None);
    }

    #[test]
    fn a_saved_password_round_trips() {
        let store = store();
        store
            .save("host-ab12", "correct horse battery staple")
            .unwrap();
        assert_eq!(
            store.load("host-ab12").unwrap().as_deref(),
            Some("correct horse battery staple")
        );
    }

    #[test]
    fn hosts_are_kept_apart() {
        let store = store();
        store.save("host-ab12", "first").unwrap();
        store.save("host-cd34", "second").unwrap();
        assert_eq!(store.load("host-ab12").unwrap().as_deref(), Some("first"));
        assert_eq!(store.load("host-cd34").unwrap().as_deref(), Some("second"));
    }

    #[test]
    fn saving_again_replaces_the_previous_password() {
        let store = store();
        store.save("host-ab12", "old").unwrap();
        store.save("host-ab12", "new").unwrap();
        assert_eq!(store.load("host-ab12").unwrap().as_deref(), Some("new"));
    }

    #[test]
    fn forgetting_removes_it_and_does_not_disturb_another_host() {
        let store = store();
        store.save("host-ab12", "first").unwrap();
        store.save("host-cd34", "second").unwrap();
        store.forget("host-ab12").unwrap();
        assert_eq!(store.load("host-ab12").unwrap(), None);
        assert_eq!(store.load("host-cd34").unwrap().as_deref(), Some("second"));
    }

    #[test]
    fn forgetting_an_absent_entry_still_succeeds() {
        let store = store();
        store.forget("host-never-connected").unwrap();
    }
}
