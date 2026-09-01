//! Host-side audit log storage (design doc §15, §16.1; ADR 0041).
//!
//! `lumepeer_core::audit` decides *what* an audit record is and refuses to
//! know where it goes — `AuditSink` is a trait precisely so the TCB stays
//! storage-agnostic. This module is the one real implementation: an
//! append-only SQLite table in the app's own data directory, with the §15
//! retention, an export and a purge behind IPC commands.
//!
//! Three properties the trait's contract asks for, and how they are kept:
//!
//! - **`append` never blocks the consent path.** It is a `try_send` onto a
//!   bounded queue; a full queue drops the record and bumps a counter, exactly
//!   the shape [`crate::recorder`] uses for media. The writer is a task rather
//!   than an OS thread only because `sqlx` is async — a thread would have to
//!   carry a runtime of its own to say the same thing.
//! - **A broken database is not a broken host.** Opening the log is allowed to
//!   fail: the caller logs it and runs on `NullAuditSink` (§18). An audit log
//!   is evidence, and evidence that cannot be written is not a reason to
//!   refuse someone their own machine.
//! - **Nothing identifying is stored.** Rows hold a peer *hash*, a wall-clock
//!   second and a fixed vocabulary of event tags. Never a raw `NodeId`, a
//!   ticket, a token, an address, a file name, clipboard content or chat text
//!   (§15) — which is why [`event_columns`] maps `AuditEvent` by hand instead
//!   of serializing it.
//!
//! Wall-clock time is deliberate. Audit records are evidence, not an
//! authorization input, so the clock-rollback defence of §12.3 does not apply
//! and must not be bolted on here: a record has to say when it claims to have
//! happened, even if the machine was lying about the date.

use std::path::{Path, PathBuf};

use lumepeer_core::audit::{AuditEvent, AuditRecord, AuditSink};
use lumepeer_core::consent::{IndependentGrant, Role};
use lumepeer_core::constants::AUDIT_RETENTION_DAYS;
use lumepeer_net::keystore::{AUDIT_SALT_ENTRY, Keystore};
use serde::Serialize;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row as _, SqlitePool};
use tokio::sync::mpsc;

/// Bounded queue between the consent path and the writer.
///
/// Audit records are tiny and arrive at human speed — a burst is a queue
/// overflowing with consent requests, which is itself the thing worth
/// surviving. Deep enough to absorb one, shallow enough that a wedged disk
/// costs bounded memory.
const QUEUE_CAPACITY: usize = 512;

/// Seconds in a day, for turning [`AUDIT_RETENTION_DAYS`] into a cutoff.
const SECS_PER_DAY: u64 = 24 * 60 * 60;

/// Largest number of rows one `list` call returns.
///
/// The UI filters by date and by kind; this is the backstop that keeps a
/// thirty-day log from being handed to the webview in one message.
const MAX_LIST_ROWS: i64 = 500;

/// Errors this module reports to its caller.
#[derive(Debug, thiserror::Error)]
pub enum AuditStoreError {
    /// The database could not be opened, migrated or queried.
    #[error("audit database: {0}")]
    Database(String),
    /// The keystore refused to hand over or mint the install salt.
    #[error("audit salt: {0}")]
    Salt(String),
    /// The salt is gone but the log is not empty, so every existing row would
    /// become uncorrelatable with everything written from now on.
    #[error(
        "the audit install salt is missing but the log holds {rows} records: \
         minting a new salt would silently split every peer in two"
    )]
    SaltLostWithRecords {
        /// How many records are already stored.
        rows: i64,
    },
}

/// One row as the UI sees it.
#[derive(Debug, Clone, Serialize)]
pub struct AuditRow {
    /// Wall-clock second the event was recorded at.
    pub at_unix_secs: i64,
    /// Short pseudonymized peer label, the same hex prefix shape the session
    /// UI uses — never the raw hash, and never a `NodeId`.
    pub peer: String,
    /// Event tag, from the fixed vocabulary of [`event_columns`].
    pub kind: String,
    /// Extra detail of the same fixed vocabulary, or empty.
    pub detail: String,
}

/// Handle on the audit log: an [`AuditSink`] for the actor, plus the queries
/// the IPC commands need.
///
/// Cloning gives another handle on the *same* log — the queue sender and the
/// connection pool are both shared. That is what lets the actor hold it as a
/// `Box<dyn AuditSink>` while the IPC commands read and export from it without
/// a round trip through the actor loop: a `SELECT` over a local file is not a
/// decision, and routing it through the loop would only put disk latency in
/// front of consent.
#[derive(Debug, Clone)]
pub struct AuditStore {
    tx: mpsc::Sender<AuditRecord>,
    pool: SqlitePool,
    salt: [u8; 32],
    path: PathBuf,
    /// Records the writer never saw because the queue was full.
    ///
    /// Counted rather than logged and forgotten: a log with holes has to be
    /// able to say so (§24.5).
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl AuditStore {
    /// Opens the log at `path`, taking its install salt from `keystore`.
    ///
    /// Minting the salt is a first-run act. If the slot is empty while the
    /// table already holds rows, this refuses rather than starting a second
    /// pseudonym space over the first one: the caller falls back to
    /// `NullAuditSink` and says so, which loses new records but keeps the
    /// existing log meaningful.
    ///
    /// # Errors
    /// [`AuditStoreError`] when the database cannot be opened or prepared, the
    /// keystore refuses the salt, or the salt is missing under a non-empty log.
    pub async fn open(path: PathBuf, keystore: &dyn Keystore) -> Result<Self, AuditStoreError> {
        let pool = open_pool(&path).await?;
        prepare_schema(&pool).await?;
        let salt = load_or_mint_salt(&pool, keystore).await?;

        let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
        tokio::spawn(run_writer(rx, pool.clone()));

        let store = Self {
            tx,
            pool,
            salt,
            path,
            dropped: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };
        store.prune().await?;
        Ok(store)
    }

    /// The install salt every peer hash in this log is mixed with.
    #[must_use]
    pub const fn salt(&self) -> &[u8; 32] {
        &self.salt
    }

    /// Where the log lives, for the export dialog's default name.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// How many records the queue dropped because the writer fell behind.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Deletes everything past the §15 retention.
    ///
    /// Run at startup and once a day, never per append: the sweep is a table
    /// scan and the cutoff moves by seconds, not by records.
    ///
    /// # Errors
    /// [`AuditStoreError::Database`] when the delete is refused.
    pub async fn prune(&self) -> Result<(), AuditStoreError> {
        let cutoff = now_secs().saturating_sub(AUDIT_RETENTION_DAYS * SECS_PER_DAY);
        let removed = sqlx::query("DELETE FROM audit WHERE at_unix_secs < ?1")
            .bind(i64::try_from(cutoff).unwrap_or(i64::MAX))
            .execute(&self.pool)
            .await
            .map_err(|error| AuditStoreError::Database(error.to_string()))?
            .rows_affected();
        if removed > 0 {
            tracing::info!(removed, "audit log: records past the retention removed");
        }
        Ok(())
    }

    /// Rows matching an optional time window and an optional event kind,
    /// newest first.
    ///
    /// # Errors
    /// [`AuditStoreError::Database`] when the query fails.
    pub async fn list(
        &self,
        since: Option<i64>,
        until: Option<i64>,
        kind: Option<&str>,
    ) -> Result<Vec<AuditRow>, AuditStoreError> {
        let rows = sqlx::query(
            "SELECT at_unix_secs, peer_hash, kind, detail FROM audit \
             WHERE (?1 IS NULL OR at_unix_secs >= ?1) \
               AND (?2 IS NULL OR at_unix_secs <= ?2) \
               AND (?3 IS NULL OR kind = ?3) \
             ORDER BY at_unix_secs DESC, id DESC LIMIT ?4",
        )
        .bind(since)
        .bind(until)
        .bind(kind)
        .bind(MAX_LIST_ROWS)
        .fetch_all(&self.pool)
        .await
        .map_err(|error| AuditStoreError::Database(error.to_string()))?;

        Ok(rows
            .into_iter()
            .map(|row| AuditRow {
                at_unix_secs: row.get::<i64, _>("at_unix_secs"),
                peer: hex_prefix(&row.get::<Vec<u8>, _>("peer_hash")),
                kind: row.get::<String, _>("kind"),
                detail: row.get::<String, _>("detail"),
            })
            .collect())
    }

    /// The whole log as CSV, for the export dialog.
    ///
    /// CSV rather than the database file itself: what leaves the machine
    /// should be the pseudonymized rows, not a file that also carries SQLite's
    /// own free pages and whatever they still hold.
    ///
    /// # Errors
    /// [`AuditStoreError::Database`] when the query fails.
    pub async fn export_csv(&self) -> Result<String, AuditStoreError> {
        let rows = sqlx::query(
            "SELECT at_unix_secs, peer_hash, kind, detail FROM audit \
             ORDER BY at_unix_secs ASC, id ASC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|error| AuditStoreError::Database(error.to_string()))?;

        let mut out = String::from("at_unix_secs,peer,kind,detail\n");
        for row in rows {
            use std::fmt::Write as _;
            let _ = writeln!(
                out,
                "{},{},{},{}",
                row.get::<i64, _>("at_unix_secs"),
                hex_prefix(&row.get::<Vec<u8>, _>("peer_hash")),
                row.get::<String, _>("kind"),
                row.get::<String, _>("detail"),
            );
        }
        Ok(out)
    }

    /// Deletes every record. §15 requires the user be able to erase the log,
    /// not only to read it.
    ///
    /// # Errors
    /// [`AuditStoreError::Database`] when the delete is refused.
    pub async fn purge(&self) -> Result<u64, AuditStoreError> {
        let removed = sqlx::query("DELETE FROM audit")
            .execute(&self.pool)
            .await
            .map_err(|error| AuditStoreError::Database(error.to_string()))?
            .rows_affected();
        tracing::info!(removed, "audit log: cleared on the user's request");
        Ok(removed)
    }
}

impl AuditSink for AuditStore {
    /// Queues one record. Never blocks, never fails the caller: a full queue
    /// costs the record and a counter, and the consent path continues.
    fn append(&mut self, record: AuditRecord) {
        if self.tx.try_send(record).is_err() {
            let dropped = self
                .dropped
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            tracing::warn!(dropped, "audit log: queue full, record dropped");
        }
    }
}

/// Opens the pool, creating the file on first use.
async fn open_pool(path: &Path) -> Result<SqlitePool, AuditStoreError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| AuditStoreError::Database(error.to_string()))?;
    }
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    // One connection: every write goes through the single writer task, and the
    // reads are UI-driven and rare. A pool wider than the writer would only
    // buy SQLITE_BUSY.
    SqlitePoolOptions::new()
        .max_connections(2)
        .connect_with(options)
        .await
        .map_err(|error| AuditStoreError::Database(error.to_string()))
}

/// Creates the table and the append-only guard.
///
/// The `UPDATE` guard is a database trigger, so it holds against anything that
/// opens the file, this process included. `DELETE` cannot be guarded the same
/// way — retention and the user's own purge are both deletes — so the rule
/// "never a single row" is kept by this module offering exactly two delete
/// statements and no third (ADR 0041).
async fn prepare_schema(pool: &SqlitePool) -> Result<(), AuditStoreError> {
    for statement in [
        "CREATE TABLE IF NOT EXISTS audit (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            at_unix_secs INTEGER NOT NULL, \
            peer_hash BLOB NOT NULL, \
            kind TEXT NOT NULL, \
            detail TEXT NOT NULL DEFAULT '')",
        "CREATE INDEX IF NOT EXISTS audit_at ON audit(at_unix_secs)",
        "CREATE TRIGGER IF NOT EXISTS audit_is_append_only \
         BEFORE UPDATE ON audit BEGIN \
            SELECT RAISE(ABORT, 'the audit log is append-only'); \
         END",
    ] {
        sqlx::query(statement)
            .execute(pool)
            .await
            .map_err(|error| AuditStoreError::Database(error.to_string()))?;
    }
    Ok(())
}

/// Reads the install salt, minting it only when the log is empty.
async fn load_or_mint_salt(
    pool: &SqlitePool,
    keystore: &dyn Keystore,
) -> Result<[u8; 32], AuditStoreError> {
    use rand::Rng as _;

    let stored = keystore
        .load_secret(AUDIT_SALT_ENTRY)
        .map_err(|error| AuditStoreError::Salt(error.to_string()))?;

    if let Some(bytes) = stored {
        return <[u8; 32]>::try_from(bytes.as_slice())
            .map_err(|_| AuditStoreError::Salt("the stored salt is not 32 bytes".to_owned()));
    }

    let rows: i64 = sqlx::query("SELECT COUNT(*) AS n FROM audit")
        .fetch_one(pool)
        .await
        .map_err(|error| AuditStoreError::Database(error.to_string()))?
        .get("n");
    if rows > 0 {
        return Err(AuditStoreError::SaltLostWithRecords { rows });
    }

    let mut salt = [0u8; 32];
    rand::rng().fill_bytes(&mut salt);
    keystore
        .store_secret(AUDIT_SALT_ENTRY, &salt)
        .map_err(|error| AuditStoreError::Salt(error.to_string()))?;
    tracing::info!("audit log: install salt minted on first use");
    Ok(salt)
}

/// Drains the queue into the table until the sink is dropped.
async fn run_writer(mut rx: mpsc::Receiver<AuditRecord>, pool: SqlitePool) {
    while let Some(record) = rx.recv().await {
        let (kind, detail) = event_columns(&record.event);
        let insert = sqlx::query(
            "INSERT INTO audit (at_unix_secs, peer_hash, kind, detail) VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(i64::try_from(record.at_unix_secs).unwrap_or(i64::MAX))
        .bind(record.peer_hash.to_vec())
        .bind(kind)
        .bind(detail)
        .execute(&pool)
        .await;
        if let Err(error) = insert {
            // One failed insert is one lost record, not a reason to abandon
            // the log: the next one may well land (§18).
            tracing::warn!(%error, "audit log: record not written");
        }
    }
}

/// Maps an event onto the two stored columns.
///
/// By hand, not by `Serialize`: the point of the mapping is that the
/// vocabulary is closed. A derive would happily carry a future variant's free
/// text — a file name, a chat line — into the log the moment someone added
/// one (§15).
fn event_columns(event: &AuditEvent) -> (&'static str, String) {
    match *event {
        AuditEvent::ConsentRequested { role } => ("consent_requested", role_tag(role).to_owned()),
        AuditEvent::ConsentGranted { role } => ("consent_granted", role_tag(role).to_owned()),
        AuditEvent::ConsentRevoked => ("consent_revoked", String::new()),
        AuditEvent::ConsentRejectedQueueFull => ("consent_rejected_queue_full", String::new()),
        AuditEvent::ConsentRejectedGuestLimit { limit } => {
            ("consent_rejected_guest_limit", limit.to_string())
        }
        AuditEvent::InputToggled { enabled } => ("input_toggled", on_off(enabled).to_owned()),
        AuditEvent::RecordingToggled { enabled } => {
            ("recording_toggled", on_off(enabled).to_owned())
        }
        AuditEvent::FileAction { action } => ("file_action", action.to_owned()),
        AuditEvent::ProtocolViolation { code } => ("protocol_violation", code.to_owned()),
        AuditEvent::GrantChanged { grant, enabled } => (
            "grant_changed",
            format!("{}:{}", grant_tag(grant), on_off(enabled)),
        ),
        AuditEvent::UnattendedLogin { accepted } => (
            "unattended_login",
            if accepted { "accepted" } else { "rejected" }.to_owned(),
        ),
        AuditEvent::DeviceTrustChanged { trusted } => (
            "device_trust_changed",
            if trusted { "trusted" } else { "untrusted" }.to_owned(),
        ),
    }
}

/// Every `kind` [`event_columns`] can produce, for the UI's filter.
pub const EVENT_KINDS: [&str; 12] = [
    "consent_requested",
    "consent_granted",
    "consent_revoked",
    "consent_rejected_queue_full",
    "consent_rejected_guest_limit",
    "input_toggled",
    "recording_toggled",
    "file_action",
    "protocol_violation",
    "grant_changed",
    "unattended_login",
    "device_trust_changed",
];

const fn role_tag(role: Role) -> &'static str {
    match role {
        Role::ViewOnly => "view_only",
        Role::ControlLimited => "control_limited",
        Role::FullControl => "full_control",
    }
}

const fn grant_tag(grant: IndependentGrant) -> &'static str {
    match grant {
        IndependentGrant::ClipboardRead => "clipboard_read",
        IndependentGrant::ClipboardWrite => "clipboard_write",
        IndependentGrant::FileTransfer => "file_transfer",
        IndependentGrant::Recording => "recording",
        IndependentGrant::DisplayMode => "display_mode",
    }
}

const fn on_off(enabled: bool) -> &'static str {
    if enabled { "on" } else { "off" }
}

/// The same eight-byte hex prefix the session UI uses for a peer.
fn hex_prefix(hash: &[u8]) -> String {
    hash.iter().take(8).fold(String::new(), |mut out, byte| {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// Current wall-clock second; 0 if the clock is before the epoch.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failed assumption must fail the test")]

    use super::*;
    use lumepeer_core::audit::peer_hash;
    use lumepeer_net::keystore::MemoryKeystore;

    fn temp_db() -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("lumepeer-audit-test-{}.db", rand::random::<u64>()));
        path
    }

    fn record(at: u64, event: AuditEvent) -> AuditRecord {
        AuditRecord {
            peer_hash: [7u8; 32],
            at_unix_secs: at,
            event,
        }
    }

    /// A fresh log mints a salt, stores what it is given and reads it back.
    #[tokio::test]
    async fn appends_and_lists() {
        let path = temp_db();
        let keystore = MemoryKeystore::default();
        let mut store = AuditStore::open(path.clone(), &keystore).await.unwrap();

        store.append(record(
            now_secs(),
            AuditEvent::ConsentGranted {
                role: Role::FullControl,
            },
        ));
        store.append(record(now_secs(), AuditEvent::ConsentRevoked));

        // The writer is a task; give it the chance to drain.
        for _ in 0..50 {
            if store.list(None, None, None).await.unwrap().len() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let rows = store.list(None, None, None).await.unwrap();
        assert_eq!(rows.len(), 2, "both records reached the table");
        assert!(rows.iter().any(|row| row.kind == "consent_granted"));
        assert!(
            rows.iter()
                .any(|row| row.kind == "consent_granted" && row.detail == "full_control")
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The peer hash is what lands in the row: no raw identity, and the label
    /// is only a prefix of the hash.
    #[tokio::test]
    async fn stores_only_the_peer_hash() {
        let path = temp_db();
        let keystore = MemoryKeystore::default();
        let mut store = AuditStore::open(path.clone(), &keystore).await.unwrap();
        let salt = *store.salt();
        let peer = lumepeer_core::NodeId::from_bytes(&[3u8; 32]).unwrap();
        let hash = peer_hash(&salt, &peer);

        store.append(AuditRecord {
            peer_hash: hash,
            at_unix_secs: now_secs(),
            event: AuditEvent::ConsentRevoked,
        });
        for _ in 0..50 {
            if !store.list(None, None, None).await.unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let rows = store.list(None, None, None).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].peer, hex_prefix(&hash));
        assert!(
            !rows[0].peer.contains("0303"),
            "the stored label is a hash, not the identity"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Records older than the retention go, whatever they say.
    #[tokio::test]
    async fn retention_removes_old_records_including_violations() {
        let path = temp_db();
        let keystore = MemoryKeystore::default();
        let mut store = AuditStore::open(path.clone(), &keystore).await.unwrap();

        let old = now_secs().saturating_sub((AUDIT_RETENTION_DAYS + 1) * SECS_PER_DAY);
        store.append(record(old, AuditEvent::ProtocolViolation { code: "FRAME" }));
        store.append(record(now_secs(), AuditEvent::ConsentRevoked));
        for _ in 0..50 {
            if store.list(None, None, None).await.unwrap().len() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        store.prune().await.unwrap();
        let rows = store.list(None, None, None).await.unwrap();
        assert_eq!(rows.len(), 1, "only the record inside the window survives");
        assert_eq!(rows[0].kind, "consent_revoked");
        let _ = std::fs::remove_file(&path);
    }

    /// The database itself refuses an update, not just this module.
    #[tokio::test]
    async fn the_table_refuses_updates() {
        let path = temp_db();
        let keystore = MemoryKeystore::default();
        let mut store = AuditStore::open(path.clone(), &keystore).await.unwrap();
        store.append(record(now_secs(), AuditEvent::ConsentRevoked));
        for _ in 0..50 {
            if !store.list(None, None, None).await.unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let attempt = sqlx::query("UPDATE audit SET kind = 'rewritten'")
            .execute(&store.pool)
            .await;
        assert!(attempt.is_err(), "the append-only trigger must abort it");
        let rows = store.list(None, None, None).await.unwrap();
        assert_eq!(rows[0].kind, "consent_revoked");
        let _ = std::fs::remove_file(&path);
    }

    /// A missing salt over a non-empty log is an error, never a fresh start.
    #[tokio::test]
    async fn a_lost_salt_over_records_refuses() {
        let path = temp_db();
        {
            let keystore = MemoryKeystore::default();
            let mut store = AuditStore::open(path.clone(), &keystore).await.unwrap();
            store.append(record(now_secs(), AuditEvent::ConsentRevoked));
            for _ in 0..50 {
                if !store.list(None, None, None).await.unwrap().is_empty() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }

        // Same database, a keystore that has forgotten the salt.
        let empty = MemoryKeystore::default();
        let reopened = AuditStore::open(path.clone(), &empty).await;
        assert!(matches!(
            reopened,
            Err(AuditStoreError::SaltLostWithRecords { .. })
        ));
        let _ = std::fs::remove_file(&path);
    }

    /// The purge clears everything; the export carries what was there.
    #[tokio::test]
    async fn exports_then_purges() {
        let path = temp_db();
        let keystore = MemoryKeystore::default();
        let mut store = AuditStore::open(path.clone(), &keystore).await.unwrap();
        store.append(record(
            now_secs(),
            AuditEvent::InputToggled { enabled: true },
        ));
        for _ in 0..50 {
            if !store.list(None, None, None).await.unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let csv = store.export_csv().await.unwrap();
        assert!(csv.starts_with("at_unix_secs,peer,kind,detail\n"));
        assert!(csv.contains("input_toggled,on"));

        assert_eq!(store.purge().await.unwrap(), 1);
        assert!(store.list(None, None, None).await.unwrap().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    /// Every kind the mapping can produce is in the filter vocabulary.
    #[test]
    fn every_event_kind_is_listed() {
        let all = [
            AuditEvent::ConsentRequested {
                role: Role::ViewOnly,
            },
            AuditEvent::ConsentGranted {
                role: Role::ViewOnly,
            },
            AuditEvent::ConsentRevoked,
            AuditEvent::ConsentRejectedQueueFull,
            AuditEvent::ConsentRejectedGuestLimit { limit: 1 },
            AuditEvent::InputToggled { enabled: true },
            AuditEvent::RecordingToggled { enabled: true },
            AuditEvent::FileAction { action: "offer" },
            AuditEvent::ProtocolViolation { code: "FRAME_SIZE" },
            AuditEvent::GrantChanged {
                grant: IndependentGrant::Recording,
                enabled: true,
            },
            AuditEvent::UnattendedLogin { accepted: true },
            AuditEvent::DeviceTrustChanged { trusted: true },
        ];
        for event in &all {
            let (kind, _) = event_columns(event);
            assert!(
                EVENT_KINDS.contains(&kind),
                "{kind} is produced but not offered as a filter"
            );
        }
        assert_eq!(all.len(), EVENT_KINDS.len());
    }
}
