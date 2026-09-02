//! Clipboard text sync over the control channel (design doc §9.2; ADR 0023).
//!
//! The `ClipboardSync` message and its `CLIPBOARD_MAX_BYTES` limit already
//! exist in the protocol; this module owns the *policy* around them:
//!
//! - Only plain UTF-8 text is ever exchanged through this module — no
//!   images, no file lists (§9.2 scope for v1). Files copied onto a
//!   clipboard now reach the peer too, but through a different door
//!   entirely: `MessageKind::ClipboardFileOffer` and the `file_transfer`
//!   grant [`permits_files`] checks below, never `clipboard_read`/
//!   `clipboard_write` (docs/bugs/14-clipboard-files.md; ADR 0047). A
//!   clipboard holding a file list is not text this module's `ClipboardSync`
//!   state machine ever sees.
//! - The payload limit is enforced on both sides before anything is stored
//!   or forwarded, so a peer cannot smuggle an oversized paste through a
//!   hand-built frame.
//! - Loop suppression: the side that *applies* a remote clipboard update
//!   marks that content as "just applied", so its own clipboard watcher
//!   does not echo it back as a local change. Without this two synced peers
//!   ping-pong one clipboard value forever.
//! - Grant gating lives with the caller: this module decides nothing about
//!   who may sync (`crates/core` stays the only authorization surface); it
//!   refuses payloads that violate §9.2 regardless of grants.

use crate::consent::{Grants, IndependentGrant};
use crate::constants::CLIPBOARD_MAX_BYTES;
use crate::error::{CoreError, Result};

/// Which way one clipboard payload travels (§8.2).
///
/// The two grants of §2.2 are named for what the *guest* gets, and each of
/// them covers one direction of travel. Both ends of a flow have to agree on
/// which grant that is: the sender decides whether to put a payload on the
/// wire and the receiver decides whether to act on one, and if those two
/// decisions read different flags the clipboard silently half-works — it
/// travels and is then dropped, with a grant switched on and nothing to show
/// for it.
///
/// So the mapping lives here, in the one crate allowed to decide it, and
/// both sides ask [`permits`] rather than each spelling the rule out again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardFlow {
    /// The host's clipboard goes to the guest: the guest is *reading* it.
    HostToGuest,
    /// The guest's clipboard goes to the host: the guest is *writing* it.
    GuestToHost,
}

impl ClipboardFlow {
    /// The one grant this direction of travel needs (§8.2).
    #[must_use]
    pub const fn grant(self) -> IndependentGrant {
        match self {
            Self::HostToGuest => IndependentGrant::ClipboardRead,
            Self::GuestToHost => IndependentGrant::ClipboardWrite,
        }
    }
}

/// Whether `grants` permit a clipboard payload to travel in `flow` (§8.2).
///
/// `grants` are the *host's* session grants, which are the only copy that
/// decides anything: a guest holds no clipboard grants of its own (ADR 0029,
/// ADR 0030). The host asks this twice per session — before putting its own
/// clipboard on the wire, and before acting on one that arrived.
#[must_use]
pub const fn permits(grants: Grants, flow: ClipboardFlow) -> bool {
    grants.get(flow.grant())
}

/// Whether `grants` allow files to be offered through a clipboard file list
/// at all (docs/bugs/14-clipboard-files.md #4; ADR 0047).
///
/// Deliberately not part of [`ClipboardFlow`]: a file list on the clipboard
/// is a file transfer with a different entry point, not an extension of
/// text sync, so it is gated on `file_transfer` and never on
/// `clipboard_read`/`clipboard_write` — a host that turns clipboard sync off
/// but leaves `file_transfer` on must still be able to paste a file, and a
/// host that turns `file_transfer` off must not leak files through the
/// clipboard just because a text grant happens to be live.
#[must_use]
pub const fn permits_files(grants: Grants) -> bool {
    grants.get(IndependentGrant::FileTransfer)
}

/// Validates an outbound/inbound clipboard payload against §9.2.
///
/// # Errors
/// [`CoreError::Malformed`] when the payload is empty, over
/// [`CLIPBOARD_MAX_BYTES`], or not valid UTF-8.
pub fn validate_payload(data: &[u8]) -> Result<()> {
    if data.is_empty() || data.len() > CLIPBOARD_MAX_BYTES {
        return Err(CoreError::Malformed);
    }
    match std::str::from_utf8(data) {
        Ok(_) => Ok(()),
        // A non-UTF-8 payload is malformed input, never silently mangled:
        // mangling could turn a pasted path into a different path (§18).
        Err(_) => Err(CoreError::Malformed),
    }
}

/// One direction of the clipboard sync state machine for a session.
///
/// `outgoing` produces frames to send when the local clipboard changes;
/// [`Self::note_applied`] feeds back what arrived from the peer so the
/// local watcher can skip its own echo. Everything here is in-memory
/// per-session state: nothing about the clipboard persists (§15).
#[derive(Debug, Default)]
pub struct ClipboardSync {
    /// Content most recently applied from the remote side, if any.
    last_applied: Option<Vec<u8>>,
}

impl ClipboardSync {
    /// A fresh, empty sync state.
    #[must_use]
    pub const fn new() -> Self {
        Self { last_applied: None }
    }

    /// Local clipboard changed to `data`: returns the bytes to send, or
    /// `None` when this change is our own echo of a remote update and must
    /// not be reflected back (loop suppression).
    ///
    /// # Errors
    /// [`CoreError::Malformed`] when the payload violates §9.2; the caller
    /// sends nothing.
    pub fn local_changed(&mut self, data: &[u8]) -> Result<Option<Vec<u8>>> {
        validate_payload(data)?;
        if self.last_applied.as_deref() == Some(data) {
            // This is the echo of what we just applied remotely. Consume it:
            // a later genuine re-copy of the same text must still be sent,
            // so forget the marker after one match.
            self.last_applied = None;
            return Ok(None);
        }
        Ok(Some(data.to_vec()))
    }

    /// Remote payload arrived and passed validation: returns the text to
    /// place on the local clipboard.
    ///
    /// # Errors
    /// [`CoreError::Malformed`] when the payload violates §9.2; nothing is
    /// remembered and nothing reaches the OS clipboard.
    pub fn remote_received<'a>(&mut self, data: &'a [u8]) -> Result<&'a [u8]> {
        validate_payload(data)?;
        self.last_applied = Some(data.to_vec());
        Ok(data)
    }

    /// Forgets the echo marker (session end, clipboard cleared locally).
    pub fn reset(&mut self) {
        self.last_applied = None;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn empty_oversized_and_non_utf8_are_malformed() {
        assert!(matches!(validate_payload(b""), Err(CoreError::Malformed)));
        let big = vec![b'a'; CLIPBOARD_MAX_BYTES + 1];
        assert!(matches!(validate_payload(&big), Err(CoreError::Malformed)));
        // Exactly at the limit is fine.
        let at_limit = vec![b'a'; CLIPBOARD_MAX_BYTES];
        assert!(validate_payload(&at_limit).is_ok());
        // Invalid UTF-8 (a lone continuation byte) must be refused, not mangled.
        assert!(matches!(
            validate_payload(&[0x80]),
            Err(CoreError::Malformed)
        ));
        assert!(validate_payload("привет".as_bytes()).is_ok());
    }

    #[test]
    fn a_remote_update_is_not_echoed_back() {
        let mut sync = ClipboardSync::new();
        let text = b"from the guest".to_vec();

        // Remote arrives → we apply it locally → our watcher fires.
        let applied = sync.remote_received(&text).unwrap().to_vec();
        assert_eq!(applied, text);
        assert!(sync.local_changed(&text).unwrap().is_none());
    }

    #[test]
    fn a_genuine_second_copy_of_the_same_text_still_travels() {
        let mut sync = ClipboardSync::new();
        let text = b"same".to_vec();
        let _ = sync.remote_received(&text);

        // First local change after applying: echo, suppressed.
        assert!(sync.local_changed(&text).unwrap().is_none());
        // User copies the same text again later: it must be sent again.
        assert_eq!(sync.local_changed(&text).unwrap(), Some(text));
    }

    #[test]
    fn unrelated_local_changes_pass_through() {
        let mut sync = ClipboardSync::new();
        let _ = sync.remote_received(b"remote");
        assert_eq!(
            sync.local_changed(b"totally different").unwrap(),
            Some(b"totally different".to_vec())
        );
    }

    #[test]
    fn oversized_remote_payload_is_refused_before_applying() {
        let mut sync = ClipboardSync::new();
        let big = vec![b'x'; CLIPBOARD_MAX_BYTES + 1];
        assert!(matches!(
            sync.remote_received(&big),
            Err(CoreError::Malformed)
        ));
        // Nothing was remembered, so a same-content local copy still travels.
        assert_eq!(sync.local_changed(&big).err().map(|_| ()), Some(()));
    }

    #[test]
    fn each_direction_needs_its_own_grant_and_only_its_own() {
        let mut read_only = Grants::from_role(crate::consent::Role::ViewOnly);
        read_only.set(IndependentGrant::ClipboardRead, true);
        assert!(permits(read_only, ClipboardFlow::HostToGuest));
        assert!(!permits(read_only, ClipboardFlow::GuestToHost));

        let mut write_only = Grants::from_role(crate::consent::Role::ViewOnly);
        write_only.set(IndependentGrant::ClipboardWrite, true);
        assert!(permits(write_only, ClipboardFlow::GuestToHost));
        assert!(!permits(write_only, ClipboardFlow::HostToGuest));

        // Full control brings both, and withdrawing one leaves the other
        // (§2.2: still two flags, not one).
        let mut full = Grants::from_role(crate::consent::Role::FullControl);
        assert!(permits(full, ClipboardFlow::HostToGuest));
        assert!(permits(full, ClipboardFlow::GuestToHost));
        full.set(IndependentGrant::ClipboardRead, false);
        assert!(!permits(full, ClipboardFlow::HostToGuest));
        assert!(permits(full, ClipboardFlow::GuestToHost));
    }

    /// docs/bugs/14-clipboard-files.md #4: files through the clipboard run
    /// under `file_transfer` alone. Neither clipboard grant, on its own or
    /// together, substitutes for it, and `file_transfer` alone is
    /// sufficient without either clipboard grant.
    #[test]
    fn clipboard_files_are_gated_on_file_transfer_alone() {
        let mut clipboard_only = Grants::from_role(crate::consent::Role::ViewOnly);
        clipboard_only.set(IndependentGrant::ClipboardRead, true);
        clipboard_only.set(IndependentGrant::ClipboardWrite, true);
        assert!(
            !permits_files(clipboard_only),
            "both clipboard grants without file_transfer must not permit files"
        );

        let mut file_transfer_only = Grants::from_role(crate::consent::Role::ViewOnly);
        file_transfer_only.set(IndependentGrant::FileTransfer, true);
        assert!(
            permits_files(file_transfer_only),
            "file_transfer alone must permit files, with no clipboard grant at all"
        );

        // Full control brings `file_transfer`, and withdrawing it stops
        // clipboard files even though both clipboard grants stay (§2.2).
        let mut full = Grants::from_role(crate::consent::Role::FullControl);
        assert!(permits_files(full));
        full.set(IndependentGrant::FileTransfer, false);
        assert!(!permits_files(full));
        assert!(full.clipboard_read && full.clipboard_write);
    }

    #[test]
    fn reset_clears_the_echo_marker() {
        let mut sync = ClipboardSync::new();
        let _ = sync.remote_received(b"hello");
        sync.reset();
        assert_eq!(
            sync.local_changed(b"hello").unwrap(),
            Some(b"hello".to_vec())
        );
    }
}
