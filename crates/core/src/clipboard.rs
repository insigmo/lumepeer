//! Clipboard text sync over the control channel (design doc §9.2; ADR 0023).
//!
//! The `ClipboardSync` message and its `CLIPBOARD_MAX_BYTES` limit already
//! exist in the protocol; this module owns the *policy* around them:
//!
//! - Only plain UTF-8 text is ever exchanged — no images, no file lists
//!   (§9.2 scope for v1).
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

use crate::constants::CLIPBOARD_MAX_BYTES;
use crate::error::{CoreError, Result};

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
