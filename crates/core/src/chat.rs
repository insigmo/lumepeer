//! In-session text chat (design doc §9.2; ADR 0023).
//!
//! The `Chat` message already exists in the protocol (`PROTOCOL_MINOR` 1).
//! This module owns the session-side bookkeeping the wire type deliberately
//! does not: a bounded transcript per peer, so a guest cannot grow a host's
//! memory by chatting forever, plus the same validation the decoder enforces.
//!
//! Chat is part of every granted session — it needs no separate grant
//! because it carries no control over the host; it is content between the
//! two humans, and revoking the session removes it.

use std::collections::BTreeMap;

use crate::NodeId;
use crate::constants::CHAT_MAX_BYTES;
use crate::error::{CoreError, Result};

/// Validates one chat message against §9.2 before it is stored or sent.
///
/// # Errors
/// [`CoreError::Malformed`] when the text is empty, over
/// [`CHAT_MAX_BYTES`] UTF-8 bytes, or not valid UTF-8 (a `String` is always
/// valid, but this also guards the raw decode path).
pub fn validate_text(text: &str) -> Result<()> {
    if text.is_empty() || text.len() > CHAT_MAX_BYTES {
        return Err(CoreError::Malformed);
    }
    Ok(())
}

/// One chat message of one session's transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatEntry {
    /// Direction: `true` when the local user sent it.
    pub outgoing: bool,
    /// Message text (already validated).
    pub text: String,
    /// Local wall-clock arrival time in Unix seconds. Display-only (§15):
    /// never used for any decision.
    pub at_unix: u64,
}

/// Bounded per-peer transcripts (§3.2 memory bound).
///
/// Every active session keeps at most [`MAX_TRANSCRIPT_ENTRIES`] entries;
/// older ones fall off the front. A session that ends drops its transcript
/// entirely — chat is ephemeral by design (§15), never persisted.
#[derive(Debug, Default)]
pub struct ChatLog {
    transcripts: BTreeMap<NodeId, Vec<ChatEntry>>,
}

/// Transcript ceiling per peer (§14): a UI shows a window, not an archive;
/// everything past this scrolls out of memory as well as off screen.
pub const MAX_TRANSCRIPT_ENTRIES: usize = 200;

impl ChatLog {
    /// An empty log.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            transcripts: BTreeMap::new(),
        }
    }

    /// Records a message in `peer`'s transcript and returns a reference to
    /// the stored entry.
    ///
    /// # Errors
    /// [`CoreError::Malformed`] for text violating §9.2 — nothing is stored.
    pub fn record(
        &mut self,
        peer: NodeId,
        outgoing: bool,
        text: &str,
        at_unix: u64,
    ) -> Result<&ChatEntry> {
        validate_text(text)?;
        let transcript = self.transcripts.entry(peer).or_default();
        if transcript.len() >= MAX_TRANSCRIPT_ENTRIES {
            transcript.remove(0);
        }
        let entry = ChatEntry {
            outgoing,
            text: text.to_owned(),
            at_unix,
        };
        transcript.push(entry);
        // The entry we just pushed is the last one; `push` cannot fail and
        // `last` is `Some` for a non-empty vector, so this is total.
        match transcript.last() {
            Some(entry) => Ok(entry),
            None => Err(CoreError::Malformed),
        }
    }

    /// The transcript of one peer, oldest first; empty when none.
    #[must_use]
    pub fn transcript(&self, peer: &NodeId) -> &[ChatEntry] {
        self.transcripts.get(peer).map_or(&[], |t| t.as_slice())
    }

    /// Forgets a peer's transcript (session end).
    pub fn drop_transcript(&mut self, peer: &NodeId) {
        self.transcripts.remove(peer);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use iroh_base::SecretKey;

    fn peer(n: u8) -> NodeId {
        SecretKey::from_bytes(&[n; 32]).public()
    }

    #[test]
    fn empty_and_oversized_texts_are_malformed() {
        assert!(matches!(validate_text(""), Err(CoreError::Malformed)));
        let big = "x".repeat(CHAT_MAX_BYTES + 1);
        assert!(matches!(validate_text(&big), Err(CoreError::Malformed)));
        assert!(validate_text(&"x".repeat(CHAT_MAX_BYTES)).is_ok());
        // Multi-byte UTF-8 counts bytes, not chars: 2 049 two-byte chars are
        // one byte over the limit even though 2 049 ≤ 4 096.
        let wide = "ж".repeat(CHAT_MAX_BYTES / 2 + 1);
        assert!(matches!(validate_text(&wide), Err(CoreError::Malformed)));
    }

    #[test]
    fn transcript_is_bounded_and_ordered() {
        let mut log = ChatLog::new();
        let p = peer(1);
        for n in 0..(MAX_TRANSCRIPT_ENTRIES + 25) {
            log.record(p, true, &n.to_string(), n as u64).unwrap();
        }
        let transcript = log.transcript(&p);
        assert_eq!(transcript.len(), MAX_TRANSCRIPT_ENTRIES);
        // The oldest 25 fell off; what remains starts at message 25.
        assert_eq!(transcript[0].text, "25");
        assert_eq!(
            transcript.last().unwrap().text,
            (MAX_TRANSCRIPT_ENTRIES + 24).to_string()
        );
    }

    #[test]
    fn invalid_message_is_never_stored() {
        let mut log = ChatLog::new();
        let p = peer(2);
        assert!(log.record(p, false, "", 0).is_err());
        assert!(log.transcript(&p).is_empty());
        log.record(p, false, "hello", 7).unwrap();
        assert_eq!(log.transcript(&p).len(), 1);
    }

    #[test]
    fn peers_are_isolated_and_dropped_together() {
        let mut log = ChatLog::new();
        let (a, b) = (peer(3), peer(4));
        log.record(a, true, "to a", 0).unwrap();
        log.record(b, false, "from b", 1).unwrap();
        assert_eq!(log.transcript(&a).len(), 1);
        assert!(!log.transcript(&b)[0].outgoing);

        log.drop_transcript(&a);
        assert!(log.transcript(&a).is_empty());
        assert_eq!(log.transcript(&b).len(), 1);
    }
}
