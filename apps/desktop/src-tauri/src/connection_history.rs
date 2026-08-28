//! Guest-side "hosts I have connected to" list (§21 punch-list item 5, ADR
//! 0015) — not `lumepeer_core::audit`'s §15/§16.1 audit log, which is a
//! separate, later-phase, retention/export-governed security feature keyed by
//! a one-way peer hash.
//!
//! This list belongs to the side that *dialed*. A host learns nothing durable
//! from having been connected to: it decided once, that decision ended with
//! the session, and remembering the guest would only build a record it never
//! asked for. The guest, on the other hand, chose that host on purpose and
//! wants to get back to it, so each row keeps the invite code it used and can
//! replay it — the host still decides again, every time (§2.3).
//!
//! Kept entirely in `apps/desktop/src-tauri` rather than `crates/core`: the
//! TCB stays storage-agnostic, exactly why `crates/core::audit::AuditSink`
//! is a trait rather than a concrete file/database writer.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use lumepeer_core::consent::Role;
use serde::{Deserialize, Serialize};

/// How many remembered hosts the list keeps. Older entries fall off as new
/// ones arrive; this is a convenience list, not the audit trail, so a bound
/// this small is fine.
const MAX_ENTRIES: usize = 50;

/// One host this node has connected to, and how to get back to it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Pseudonymized host label — never a raw `NodeId` (§15). Unlike the
    /// per-run labels `session_status` hands out, this one is stable across
    /// restarts, which is the whole point of a remembered-hosts list.
    pub peer_label: String,
    /// Role the host last granted.
    pub role: Role,
    /// Unix seconds this row was last written — a connect or a disconnect,
    /// whichever happened most recently (docs/bugs/03-connection-list.md,
    /// task 4). Named for what it actually means now that a row is written
    /// at connect time as well as at disconnect: a value from a session still
    /// in progress is not an "end" and calling it one would be a lie the
    /// sidebar tells. `#[serde(alias)]` keeps a history file written before
    /// this rename loading correctly.
    #[serde(alias = "ended_at")]
    pub last_seen_at: u64,
    /// Invite code this node used to reach the host, replayed when the row is
    /// clicked. Stays in the Rust side: the webview asks to reconnect by label
    /// and never receives the code back (§13).
    #[serde(default)]
    pub code: String,
}

/// In-memory list backed by a best-effort-persisted JSON file.
#[derive(Debug, Default)]
pub struct ConnectionHistory {
    path: Option<PathBuf>,
    /// Newest first.
    entries: Vec<HistoryEntry>,
}

impl ConnectionHistory {
    /// Loads existing history from `path`. `path` is `None` in tests and
    /// whenever the app data directory cannot be resolved: the feature
    /// degrades to in-memory-only for that run rather than failing startup
    /// (§18's "degrade, never fail" spirit).
    #[must_use]
    pub fn open(path: Option<PathBuf>) -> Self {
        let entries = path.as_deref().map(Self::load).unwrap_or_default();
        Self { path, entries }
    }

    fn load(path: &Path) -> Vec<HistoryEntry> {
        let Ok(bytes) = fs::read(path) else {
            // Missing on first run, or the directory doesn't exist yet;
            // either way there is nothing to recover.
            return Vec::new();
        };
        serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            tracing::warn!(
                %error,
                path = %path.display(),
                "connection history file is unreadable; starting fresh"
            );
            Vec::new()
        })
    }

    /// Every remembered host, most recently connected first.
    #[must_use]
    pub fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }

    /// Invite code remembered for `peer_label`, if that host is still listed.
    #[must_use]
    pub fn code_of(&self, peer_label: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|entry| entry.peer_label == peer_label)
            .map(|entry| entry.code.as_str())
    }

    /// Records one host visit — at connect time or at disconnect, both call
    /// this — and persists the list.
    ///
    /// One row per host, not per session: this is a list of places to go back
    /// to, so connecting to the same host ten times leaves one row that moves
    /// to the front, carrying the code and role of the most recent visit.
    pub fn record(&mut self, peer_label: String, role: Role, code: String) {
        let last_seen_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        self.entries.retain(|entry| entry.peer_label != peer_label);
        self.entries.insert(
            0,
            HistoryEntry {
                peer_label,
                role,
                last_seen_at,
                code,
            },
        );
        self.entries.truncate(MAX_ENTRIES);
        self.save();
    }

    /// Removes the row for `peer_label`, if there is one, and persists the
    /// list (docs/bugs/03-connection-list.md, task 5).
    ///
    /// Returns whether a row was actually removed, so the caller can tell a
    /// real deletion from a no-op on a label that was never there.
    pub fn remove(&mut self, peer_label: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.peer_label != peer_label);
        let removed = self.entries.len() != before;
        if removed {
            self.save();
        }
        removed
    }

    /// Best-effort: a write failure here must never take the session down
    /// with it, only leave the sidebar's history stale until the next
    /// successful save.
    fn save(&self) {
        let Some(path) = &self.path else { return };
        if let Some(parent) = path.parent()
            && let Err(error) = fs::create_dir_all(parent)
        {
            tracing::warn!(%error, "cannot create the connection history directory");
            return;
        }
        match serde_json::to_vec(&self.entries) {
            Ok(bytes) => {
                if let Err(error) = fs::write(path, bytes) {
                    tracing::warn!(%error, "cannot persist connection history");
                }
            }
            Err(error) => tracing::warn!(%error, "cannot serialize connection history"),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_history_with_no_path_stays_in_memory_only() {
        let mut history = ConnectionHistory::open(None);
        history.record("host-ab12".to_owned(), Role::ViewOnly, "code-1".to_owned());
        assert_eq!(history.entries().len(), 1);
    }

    #[test]
    fn newest_entries_come_first_and_old_ones_fall_off_the_cap() {
        let mut history = ConnectionHistory::open(None);
        for n in 0..MAX_ENTRIES + 5 {
            history.record(format!("host-{n}"), Role::ViewOnly, format!("code-{n}"));
        }
        assert_eq!(history.entries().len(), MAX_ENTRIES);
        assert_eq!(
            history.entries()[0].peer_label,
            format!("host-{}", MAX_ENTRIES + 4)
        );
    }

    /// A list of places to go back to, not a session log: reconnecting to the
    /// same host must not grow a second row, and the row that stays has to
    /// carry the code of the most recent visit or the reconnect replays a
    /// stale invite.
    #[test]
    fn connecting_to_the_same_host_twice_keeps_one_row_with_the_newer_code() {
        let mut history = ConnectionHistory::open(None);
        history.record("host-ab12".to_owned(), Role::ViewOnly, "code-1".to_owned());
        history.record("host-cd34".to_owned(), Role::ViewOnly, "code-2".to_owned());
        history.record(
            "host-ab12".to_owned(),
            Role::FullControl,
            "code-3".to_owned(),
        );

        assert_eq!(history.entries().len(), 2);
        assert_eq!(history.entries()[0].peer_label, "host-ab12");
        assert_eq!(history.entries()[0].role, Role::FullControl);
        assert_eq!(history.code_of("host-ab12"), Some("code-3"));
        assert_eq!(history.code_of("host-zz99"), None);
    }

    /// docs/bugs/03-connection-list.md, task 5.
    #[test]
    fn removing_a_row_drops_it_and_leaves_the_rest_alone() {
        let mut history = ConnectionHistory::open(None);
        history.record("host-ab12".to_owned(), Role::ViewOnly, "code-1".to_owned());
        history.record("host-cd34".to_owned(), Role::ViewOnly, "code-2".to_owned());

        assert!(history.remove("host-ab12"));
        assert_eq!(history.entries().len(), 1);
        assert_eq!(history.entries()[0].peer_label, "host-cd34");
        assert_eq!(history.code_of("host-ab12"), None);
    }

    #[test]
    fn removing_a_label_that_was_never_there_changes_nothing() {
        let mut history = ConnectionHistory::open(None);
        history.record("host-ab12".to_owned(), Role::ViewOnly, "code-1".to_owned());

        assert!(!history.remove("host-never-connected"));
        assert_eq!(history.entries().len(), 1);
    }

    #[test]
    fn a_removal_persists_across_a_reload() {
        let dir =
            std::env::temp_dir().join(format!("lumepeer-history-remove-{}", std::process::id()));
        let path = dir.join("connection_history.json");

        let mut history = ConnectionHistory::open(Some(path.clone()));
        history.record("host-ab12".to_owned(), Role::ViewOnly, "code-1".to_owned());
        history.record("host-cd34".to_owned(), Role::ViewOnly, "code-2".to_owned());
        assert!(history.remove("host-ab12"));

        let reloaded = ConnectionHistory::open(Some(path));
        assert_eq!(reloaded.entries().len(), 1);
        assert_eq!(reloaded.entries()[0].peer_label, "host-cd34");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_saved_history_reloads_from_disk() {
        let dir =
            std::env::temp_dir().join(format!("lumepeer-history-test-{}", std::process::id()));
        let path = dir.join("connection_history.json");

        let mut history = ConnectionHistory::open(Some(path.clone()));
        history.record(
            "host-ab12".to_owned(),
            Role::FullControl,
            "code-1".to_owned(),
        );
        history.record("host-cd34".to_owned(), Role::ViewOnly, "code-2".to_owned());

        let reloaded = ConnectionHistory::open(Some(path));
        assert_eq!(reloaded.entries().len(), 2);
        assert_eq!(reloaded.entries()[0].peer_label, "host-cd34");
        assert_eq!(reloaded.entries()[1].role, Role::FullControl);
        // The code has to survive the round trip, or a remembered host is only
        // a label and the row cannot dial anything.
        assert_eq!(reloaded.code_of("host-ab12"), Some("code-1"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_corrupt_history_file_is_treated_as_empty_rather_than_failing() {
        let dir =
            std::env::temp_dir().join(format!("lumepeer-history-corrupt-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("connection_history.json");
        fs::write(&path, b"not json").unwrap();

        let history = ConnectionHistory::open(Some(path));
        assert!(history.entries().is_empty());

        let _ = fs::remove_dir_all(dir);
    }

    /// docs/bugs/03-connection-list.md, task 4: `ended_at` became
    /// `last_seen_at`, but a history file an older build already wrote still
    /// has the old key, and must not be discarded.
    #[test]
    fn a_history_file_from_before_the_rename_still_loads() {
        let dir =
            std::env::temp_dir().join(format!("lumepeer-history-old-key-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("connection_history.json");
        fs::write(
            &path,
            br#"[{"peer_label":"host-ab12","role":"ViewOnly","ended_at":1234,"code":"code-1"}]"#,
        )
        .unwrap();

        let history = ConnectionHistory::open(Some(path));
        assert_eq!(history.entries().len(), 1);
        assert_eq!(history.entries()[0].last_seen_at, 1234);
        assert_eq!(history.code_of("host-ab12"), Some("code-1"));

        let _ = fs::remove_dir_all(dir);
    }
}
