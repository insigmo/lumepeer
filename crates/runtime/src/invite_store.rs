//! Host-side persistence of the one live invite (ADR 0062).
//!
//! `TicketRegistry` lives in memory, so every restart of the host emptied it
//! and `claim` refused every code the host had ever read out — the guest was
//! told its invite "may be out of date" when nothing about it had changed but
//! the host's uptime. This file is what survives that restart.
//!
//! Only the invite *code* is kept, not a parallel copy of its fields: the code
//! already carries the invite id, the expiry, the role and the host's
//! signature, and `InviteTicket::from_code` verifies all of it on the way back
//! in. Two records of the same facts is how they come to disagree.
//!
//! Same shape as `AddressBookStore` and `ConnectionHistory` next door, for the
//! same reasons: `None` for the path means in-memory-only rather than a failed
//! start, and an unreadable file means a warning and no invite rather than a
//! panic (§18).
//!
//! There is no secret in the file. An invite code is exactly what the host
//! reads out loud to the guest it is inviting, and it authorizes nothing on
//! its own — the host still decides on every connection (§2.3).

use std::fs;
use std::path::{Path, PathBuf};

use ed25519_dalek::VerifyingKey;
use lumepeer_net::ticket::InviteTicket;

/// The one file every host kept its invite in before the name carried the
/// host's key. Still read, so a host upgraded past that change keeps the code
/// its guests are holding.
const LEGACY_FILE_NAME: &str = "invite.json";

/// The file the live invite of the host that signs with `host` is kept in.
///
/// Named after the key because one user profile can hold more than one host
/// identity — a second instance on the file keystore beside the installed app
/// — and with a single shared file each overwrote the other's invite. The
/// installed host then came back from its next restart with nothing to
/// restore, and refused every code its guests had saved.
#[must_use]
pub fn file_name(host: &VerifyingKey) -> String {
    format!(
        "invite-{}.json",
        crate::network::hex_prefix(host.as_bytes())
    )
}

/// The live invite, as it is written to disk.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StoredInvite {
    /// The invite code itself, in the same text form the host shows.
    code: String,
}

/// In-memory live invite backed by a best-effort-persisted JSON file.
#[derive(Debug, Default)]
pub struct InviteStore {
    path: Option<PathBuf>,
    code: Option<String>,
}

impl InviteStore {
    /// Loads the stored invite from `path`, or from the legacy `invite.json`
    /// beside it when `path` holds nothing yet.
    ///
    /// The legacy file may hold another identity's code; the caller checks the
    /// signature before it registers anything, as it always did.
    #[must_use]
    pub fn open(path: Option<PathBuf>) -> Self {
        let code = path.as_deref().and_then(|path| {
            Self::load(path).or_else(|| Self::load(&path.with_file_name(LEGACY_FILE_NAME)))
        });
        Self { path, code }
    }

    fn load(path: &Path) -> Option<String> {
        // Missing on first run, or the directory does not exist yet.
        let text = fs::read_to_string(path).ok()?;
        match serde_json::from_str::<StoredInvite>(&text) {
            Ok(stored) => Some(stored.code),
            Err(error) => {
                tracing::warn!(
                    %error,
                    path = %path.display(),
                    "the stored invite is unreadable; the host will issue a new code"
                );
                None
            }
        }
    }

    /// The stored invite, parsed and checked against `now` (Unix seconds).
    ///
    /// `None` when nothing is stored, when the file did not parse, or when the
    /// invite has expired — every one of which means the same thing to the
    /// caller: there is no invite to put back into the registry.
    #[must_use]
    pub fn live(&self, now: u64) -> Option<(String, InviteTicket)> {
        let code = self.code.as_deref()?;
        let ticket = InviteTicket::from_code(code)
            .inspect_err(|error| tracing::warn!(%error, "the stored invite does not parse"))
            .ok()?;
        if ticket.is_expired_at(now) {
            tracing::info!("the stored invite has expired; the host will issue a new code");
            return None;
        }
        Some((code.to_owned(), ticket))
    }

    /// Replaces the stored invite and persists it.
    pub fn set(&mut self, code: String) {
        self.code = Some(code);
        self.save();
    }

    fn save(&self) {
        let (Some(path), Some(code)) = (self.path.as_deref(), self.code.as_deref()) else {
            return;
        };
        let Ok(text) = serde_json::to_string(&StoredInvite {
            code: code.to_owned(),
        }) else {
            return;
        };
        if let Some(parent) = path.parent()
            && let Err(error) = fs::create_dir_all(parent)
        {
            tracing::warn!(%error, path = %parent.display(), "cannot create the invite directory");
            return;
        }
        if let Err(error) = fs::write(path, text) {
            // Best effort, like the other stores: an invite that cannot be
            // written still works for this run, it just will not survive a
            // restart — which is the behaviour this file was added to improve,
            // not a behaviour anything depends on.
            tracing::warn!(%error, path = %path.display(), "cannot persist the invite");
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use ed25519_dalek::SigningKey;
    use lumepeer_core::consent::Role;

    use super::*;

    fn code_valid_at(now: u64) -> String {
        let key = SigningKey::from_bytes(&[3u8; 32]);
        let secret = iroh::SecretKey::from_bytes(&[4u8; 32]);
        let addr = iroh::EndpointAddr::from(secret.public());
        InviteTicket::issue(&key, &addr, Role::ViewOnly, now, None, None)
            .unwrap()
            .to_code()
            .unwrap()
    }

    #[test]
    fn nothing_stored_is_no_invite() {
        assert!(InviteStore::open(None).live(0).is_none());
    }

    #[test]
    fn a_stored_invite_comes_back_and_survives_a_reopen() {
        let path = std::env::temp_dir().join(format!(
            "lumepeer-invite-store-{}-{}.json",
            std::process::id(),
            line!()
        ));
        let _ = fs::remove_file(&path);
        let code = code_valid_at(1_000);

        let mut store = InviteStore::open(Some(path.clone()));
        store.set(code.clone());
        assert_eq!(store.live(1_000).unwrap().0, code);

        // The whole point: a second process finds the same invite.
        let reopened = InviteStore::open(Some(path.clone()));
        assert_eq!(reopened.live(1_000).unwrap().0, code);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn an_expired_invite_is_not_offered() {
        let mut store = InviteStore::open(None);
        store.set(code_valid_at(1_000));
        let past_its_ttl = 1_000 + lumepeer_core::constants::INVITE_TICKET_TTL_SECS + 1;
        assert!(store.live(past_its_ttl).is_none());
    }

    #[test]
    fn each_host_key_gets_its_own_file() {
        let one = SigningKey::from_bytes(&[1u8; 32]).verifying_key();
        let two = SigningKey::from_bytes(&[2u8; 32]).verifying_key();
        assert_ne!(file_name(&one), file_name(&two));
        assert_eq!(file_name(&one), file_name(&one));
        assert_ne!(file_name(&one), LEGACY_FILE_NAME);
    }

    /// The legacy file is a fallback only: a keyed file, once there, wins.
    #[test]
    fn the_legacy_file_is_read_only_while_the_keyed_one_is_missing() {
        let dir = std::env::temp_dir().join(format!(
            "lumepeer-invite-store-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let keyed = dir.join("invite-0000000000000000.json");
        let legacy_code = code_valid_at(1_000);
        InviteStore::open(Some(dir.join(LEGACY_FILE_NAME))).set(legacy_code.clone());

        assert_eq!(
            InviteStore::open(Some(keyed.clone()))
                .live(1_000)
                .unwrap()
                .0,
            legacy_code
        );

        let keyed_code = code_valid_at(2_000);
        InviteStore::open(Some(keyed.clone())).set(keyed_code.clone());
        assert_eq!(
            InviteStore::open(Some(keyed)).live(2_000).unwrap().0,
            keyed_code
        );
        // Writing the keyed file left the legacy one as it was.
        assert_eq!(
            InviteStore::open(Some(dir.join(LEGACY_FILE_NAME)))
                .live(1_000)
                .unwrap()
                .0,
            legacy_code
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_code_is_no_invite_rather_than_a_panic() {
        let mut store = InviteStore::open(None);
        store.set("lumepeer1:not-a-real-code".to_owned());
        assert!(store.live(0).is_none());
    }
}
