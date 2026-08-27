//! Address book: saved devices, tags, notes and the trusted-device whitelist
//! (design doc §8; ADR 0023 §3, ADR 0034).
//!
//! Deny-by-default applies here too: a device is trusted only after the host
//! user marked it, and trust is per-`NodeId`, never per-name. The book is
//! plain JSON on disk next to the other config files; it holds no secrets
//! (§15) — a `NodeId` is a public key.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::NodeId;

/// One saved device of the address book (§8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddressEntry {
    /// Human label shown in the UI ("Office workstation").
    pub label: String,
    /// Free-form grouping tags ("work", "family").
    #[serde(default)]
    pub tags: Vec<String>,
    /// Optional free-text note.
    #[serde(default)]
    pub notes: String,
    /// Whether this device may connect without answering consent dialogs
    /// (still subject to the unattended password of `crate::unattended`).
    ///
    /// Trust narrows *who may try* the password; it is never a way past it
    /// (ADR 0034).
    #[serde(default)]
    pub trusted: bool,
}

/// The address book itself (§8).
///
/// Keyed by base32 of the `NodeId` so the on-disk form is stable across
/// platforms and does not depend on `NodeId`'s own serialization.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddressBook {
    /// Saved devices keyed by their public-key id.
    #[serde(flatten)]
    entries: BTreeMap<String, AddressEntry>,
}

impl AddressBook {
    /// An empty book.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    fn key_of(peer: &NodeId) -> String {
        data_encoding::BASE32_NOPAD.encode(peer.as_ref())
    }

    /// Decodes a book key back into the `NodeId` it names.
    ///
    /// The inverse of the keying scheme this book is built on, needed by any
    /// caller that has to turn a stored entry back into a peer — the desktop
    /// shell does, to pseudonymize it for the UI (§15).
    ///
    /// # Errors
    /// [`crate::CoreError::Malformed`] if `key` is not base32 of a 32-byte
    /// public key. A hand-edited file is untrusted input like any other: this
    /// returns an error, never a panic.
    pub fn peer_of_key(key: &str) -> crate::Result<NodeId> {
        let bytes = data_encoding::BASE32_NOPAD
            .decode(key.as_bytes())
            .map_err(|_| crate::CoreError::Malformed)?;
        let bytes: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| crate::CoreError::Malformed)?;
        NodeId::from_bytes(&bytes).map_err(|_| crate::CoreError::Malformed)
    }

    /// All entries whose key still decodes to a peer, in stable id order.
    ///
    /// A key that does not decode is skipped rather than fatal: the file is
    /// editable by the host user, and one bad line must not hide the rest of
    /// the book.
    pub fn peers(&self) -> impl Iterator<Item = (NodeId, &AddressEntry)> {
        self.entries
            .iter()
            .filter_map(|(key, entry)| Self::peer_of_key(key).ok().map(|peer| (peer, entry)))
    }

    /// Saves or updates a device entry.
    pub fn upsert(&mut self, peer: &NodeId, entry: AddressEntry) {
        self.entries.insert(Self::key_of(peer), entry);
    }

    /// Removes a device; returns whether it was present.
    pub fn remove(&mut self, peer: &NodeId) -> bool {
        self.entries.remove(&Self::key_of(peer)).is_some()
    }

    /// Looks a device up.
    #[must_use]
    pub fn get(&self, peer: &NodeId) -> Option<&AddressEntry> {
        self.entries.get(&Self::key_of(peer))
    }

    /// All entries in stable id order (for UI listing).
    pub fn iter(&self) -> impl Iterator<Item = (&String, &AddressEntry)> {
        self.entries.iter()
    }

    /// Number of saved devices.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no device is saved.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether `peer` is on the trusted whitelist (§8).
    ///
    /// A device absent from the book is never trusted, whatever its label.
    #[must_use]
    pub fn is_trusted(&self, peer: &NodeId) -> bool {
        self.get(peer).is_some_and(|e| e.trusted)
    }

    /// Serializes to pretty JSON for persistence.
    ///
    /// # Errors
    /// [`crate::CoreError::Malformed`] if serialization fails — practically
    /// impossible for in-memory data, handled rather than unwrapped anyway.
    pub fn to_json(&self) -> crate::Result<String> {
        serde_json::to_string_pretty(self).map_err(|_| crate::CoreError::Malformed)
    }

    /// Restores from JSON produced by [`Self::to_json`].
    ///
    /// # Errors
    /// [`crate::CoreError::Malformed`] on any parse failure; a corrupt file
    /// never becomes a partially-trusting book.
    pub fn from_json(text: &str) -> crate::Result<Self> {
        serde_json::from_str(text).map_err(|_| crate::CoreError::Malformed)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn peer(n: u8) -> NodeId {
        iroh_base::SecretKey::from_bytes(&[n; 32]).public()
    }

    fn entry(label: &str, trusted: bool) -> AddressEntry {
        AddressEntry {
            label: label.to_owned(),
            tags: Vec::new(),
            notes: String::new(),
            trusted,
        }
    }

    #[test]
    fn unknown_device_is_never_trusted() {
        let book = AddressBook::new();
        assert!(!book.is_trusted(&peer(1)));
        assert!(book.is_empty());
    }

    #[test]
    fn saved_but_untrusted_device_stays_untrusted() {
        let mut book = AddressBook::new();
        book.upsert(&peer(1), entry("office", false));
        assert_eq!(book.get(&peer(1)).map(|e| e.label.as_str()), Some("office"));
        assert!(!book.is_trusted(&peer(1)));
    }

    #[test]
    fn trust_is_per_node_not_per_label() {
        let mut book = AddressBook::new();
        book.upsert(&peer(1), entry("office", true));
        assert!(book.is_trusted(&peer(1)));
        // Same label on another node means nothing.
        book.upsert(&peer(2), entry("office", false));
        assert!(!book.is_trusted(&peer(2)));
    }

    #[test]
    fn remove_drops_the_entry() {
        let mut book = AddressBook::new();
        book.upsert(&peer(3), entry("gone soon", true));
        assert!(book.remove(&peer(3)));
        assert!(!book.remove(&peer(3)));
        assert!(!book.is_trusted(&peer(3)));
    }

    #[test]
    fn json_roundtrip_preserves_entries_and_trust() {
        let mut book = AddressBook::new();
        book.upsert(&peer(1), entry("office", true));
        book.upsert(&peer(2), entry("home", false));
        let text = book.to_json().unwrap();

        let restored = AddressBook::from_json(&text).unwrap();
        assert_eq!(restored, book);
        assert!(restored.is_trusted(&peer(1)));
        assert!(!restored.is_trusted(&peer(2)));
    }

    #[test]
    fn a_key_round_trips_back_into_the_peer_it_names() {
        let mut book = AddressBook::new();
        book.upsert(&peer(1), entry("office", true));
        book.upsert(&peer(2), entry("home", false));

        let listed: Vec<_> = book.peers().collect();
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().any(|(p, e)| *p == peer(1) && e.trusted));
        assert!(listed.iter().any(|(p, e)| *p == peer(2) && !e.trusted));
    }

    #[test]
    fn a_hand_edited_key_is_skipped_rather_than_fatal() {
        assert!(AddressBook::peer_of_key("not base32!").is_err());
        // Well-formed base32 of the wrong length is not a public key either.
        assert!(AddressBook::peer_of_key("AAAA").is_err());

        let book = AddressBook::from_json(r#"{ "AAAA": { "label": "junk" } }"#).unwrap();
        assert_eq!(book.len(), 1, "the entry is still stored");
        assert_eq!(book.peers().count(), 0, "but it names no peer");
    }

    #[test]
    fn corrupt_json_is_an_error_never_a_partial_book() {
        assert!(AddressBook::from_json("{ not json").is_err());
        // Valid JSON of the wrong shape must not panic either.
        assert!(AddressBook::from_json("[1, 2, 3]").is_err());
    }
}
