//! Host-side persistence of the address book (§8; ADR 0034).
//!
//! Not to be confused with `connection_history`, which is the *guest* side's
//! "hosts I have connected to" list (ADR 0016). This one belongs to the host
//! and is about trust: which devices are even allowed to try the unattended
//! password of §8.
//!
//! Same shape as `ConnectionHistory` because the same three questions come up
//! and have the same answers: `None` for the path means in-memory only rather
//! than a failed start, an unreadable file means a warning and an empty book
//! rather than a panic, and the serialization is `lumepeer_core`'s own
//! (`AddressBook::to_json`/`from_json`) rather than a second one written here.
//!
//! The file holds public keys and the labels, tags and notes a human typed.
//! There are no secrets in it by construction — a `NodeId` is a public key —
//! and nothing may be added to it that changes that.

use std::fs;
use std::path::{Path, PathBuf};

use lumepeer_core::NodeId;
use lumepeer_core::address_book::{AddressBook, AddressEntry};

/// In-memory book backed by a best-effort-persisted JSON file.
#[derive(Debug, Default)]
pub struct AddressBookStore {
    path: Option<PathBuf>,
    book: AddressBook,
}

impl AddressBookStore {
    /// Loads the book from `path`.
    ///
    /// `path` is `None` in tests and whenever the config directory cannot be
    /// resolved: the feature degrades to in-memory-only for that run rather
    /// than failing startup (§18). Deny-by-default survives the degradation —
    /// an empty book trusts nobody.
    #[must_use]
    pub fn open(path: Option<PathBuf>) -> Self {
        let book = path.as_deref().map(Self::load).unwrap_or_default();
        Self { path, book }
    }

    fn load(path: &Path) -> AddressBook {
        let Ok(text) = fs::read_to_string(path) else {
            // Missing on first run, or the directory does not exist yet.
            return AddressBook::new();
        };
        AddressBook::from_json(&text).unwrap_or_else(|error| {
            // A corrupt book is an empty book, never a partially-trusting one:
            // `from_json` refuses the whole file for exactly this reason, and
            // guessing at half of it is how a stale `trusted` flag would
            // survive an edit that was meant to remove it.
            tracing::warn!(
                %error,
                path = %path.display(),
                "the address book file is unreadable; starting with an empty book"
            );
            AddressBook::new()
        })
    }

    /// The book, for the read-only questions (`is_trusted`, listing).
    #[must_use]
    pub const fn book(&self) -> &AddressBook {
        &self.book
    }

    /// Saves or replaces one device entry and persists the book.
    pub fn upsert(&mut self, peer: &NodeId, entry: AddressEntry) {
        self.book.upsert(peer, entry);
        self.save();
    }

    /// Removes one device; returns whether it was there.
    pub fn remove(&mut self, peer: &NodeId) -> bool {
        let removed = self.book.remove(peer);
        if removed {
            self.save();
        }
        removed
    }

    /// Marks `peer` trusted or untrusted, keeping the rest of its entry.
    ///
    /// Returns whether the device is trusted afterwards; `None` if it is not
    /// in the book at all. Trust is never created for an unknown device here:
    /// the host adds a device first and decides about it second, which keeps
    /// "a peer connected once" from being a path to a trust flag (§2.1).
    pub fn set_trusted(&mut self, peer: &NodeId, trusted: bool) -> Option<bool> {
        let mut entry = self.book.get(peer).cloned()?;
        entry.trusted = trusted;
        self.book.upsert(peer, entry);
        self.save();
        Some(trusted)
    }

    /// Best-effort persistence: a write failure leaves the in-memory book
    /// authoritative for this run and says so, rather than taking a session
    /// down over a file (§18).
    fn save(&self) {
        let Some(path) = &self.path else { return };
        if let Some(parent) = path.parent()
            && let Err(error) = fs::create_dir_all(parent)
        {
            tracing::warn!(%error, "cannot create the address book directory");
            return;
        }
        match self.book.to_json() {
            Ok(text) => {
                if let Err(error) = fs::write(path, text) {
                    tracing::warn!(%error, "cannot persist the address book");
                }
            }
            Err(error) => tracing::warn!(%error, "cannot serialize the address book"),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn peer(n: u8) -> NodeId {
        iroh::SecretKey::from_bytes(&[n; 32]).public()
    }

    fn entry(label: &str, trusted: bool) -> AddressEntry {
        AddressEntry {
            label: label.to_owned(),
            tags: vec!["work".to_owned()],
            notes: "note".to_owned(),
            trusted,
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "lumepeer-address-book-{name}-{}",
            std::process::id()
        ))
    }

    #[test]
    fn a_book_with_no_path_stays_in_memory_only() {
        let mut store = AddressBookStore::open(None);
        store.upsert(&peer(1), entry("office", true));
        assert!(store.book().is_trusted(&peer(1)));
    }

    #[test]
    fn entries_and_trust_survive_a_restart() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("address_book.json");

        let mut store = AddressBookStore::open(Some(path.clone()));
        store.upsert(&peer(1), entry("office", true));
        store.upsert(&peer(2), entry("home", false));

        let reloaded = AddressBookStore::open(Some(path));
        assert!(reloaded.book().is_trusted(&peer(1)));
        assert!(!reloaded.book().is_trusted(&peer(2)));
        assert_eq!(
            reloaded.book().get(&peer(2)).map(|e| e.label.as_str()),
            Some("home")
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_corrupt_file_is_an_empty_book_rather_than_a_panic() {
        let dir = temp_dir("corrupt");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("address_book.json");
        fs::write(&path, b"{ not json").unwrap();

        let store = AddressBookStore::open(Some(path));
        assert!(store.book().is_empty());
        // And the deny-by-default answer survives the corruption.
        assert!(!store.book().is_trusted(&peer(1)));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn trust_can_be_withdrawn_without_losing_the_rest_of_the_entry() {
        let mut store = AddressBookStore::open(None);
        store.upsert(&peer(1), entry("office", true));

        assert_eq!(store.set_trusted(&peer(1), false), Some(false));
        let saved = store.book().get(&peer(1)).unwrap();
        assert!(!saved.trusted);
        assert_eq!(saved.label, "office");
        assert_eq!(saved.tags, vec!["work".to_owned()]);
        assert_eq!(saved.notes, "note");
    }

    #[test]
    fn an_unknown_device_cannot_be_trusted_into_existence() {
        let mut store = AddressBookStore::open(None);
        assert_eq!(store.set_trusted(&peer(9), true), None);
        assert!(store.book().is_empty());
        assert!(!store.book().is_trusted(&peer(9)));
    }

    #[test]
    fn removing_an_absent_device_changes_nothing() {
        let mut store = AddressBookStore::open(None);
        assert!(!store.remove(&peer(3)));
        store.upsert(&peer(3), entry("gone soon", true));
        assert!(store.remove(&peer(3)));
        assert!(!store.remove(&peer(3)));
        assert!(!store.book().is_trusted(&peer(3)));
    }
}
