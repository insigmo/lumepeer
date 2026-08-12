//! Broker persistence: SQLite in WAL mode (design doc §5.1, §12).
//!
//! PostgreSQL is a separate task with its own ADR, not a "just in case"
//! abstraction. Nothing here stores session content: only account ids, license
//! and device pseudonyms and token lifecycle (§15).

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

/// Opens the pool and applies migrations.
///
/// # Errors
/// Propagates connection and migration failures.
pub async fn connect(database_url: &str) -> anyhow::Result<SqlitePool> {
    let options = database_url
        .parse::<SqliteConnectOptions>()?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await?;

    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

/// One row of `licenses`, as the queries select it.
#[derive(Debug, Clone, sqlx::FromRow)]
struct LicenseRow {
    id: String,
    plan: i64,
    expires_at: i64,
    revoked_at: Option<i64>,
}

/// One row of `devices`, as the queries select it.
#[derive(Debug, Clone, sqlx::FromRow)]
struct DeviceRow {
    license_id: String,
    trial_seconds_used: i64,
    displaced_at: Option<i64>,
}

/// Newest non-revoked, non-expired license of an account.
///
/// # Errors
/// Propagates query failures.
pub async fn active_license_for_account(
    pool: &SqlitePool,
    account_id: &str,
    now: u64,
) -> sqlx::Result<Option<LicenseView>> {
    let row = sqlx::query_as::<_, LicenseRow>(
        "SELECT id, plan, expires_at, revoked_at
           FROM licenses
          WHERE account_id = ?1 AND revoked_at IS NULL AND expires_at >= ?2
          ORDER BY created_at DESC
          LIMIT 1",
    )
    .bind(account_id)
    .bind(i64::try_from(now).unwrap_or(i64::MAX))
    .fetch_optional(pool)
    .await?;
    Ok(row.map(LicenseView::from))
}

/// A license as the route layer sees it, with unsigned times.
#[derive(Debug, Clone)]
pub struct LicenseView {
    /// License identity.
    pub id: String,
    /// Plan byte, as in §12.1.
    pub plan: u8,
    /// Unix seconds the entitlement ends at.
    pub expires_at: u64,
    /// Unix seconds the license was revoked at, if it was.
    pub revoked_at: Option<u64>,
}

impl From<LicenseRow> for LicenseView {
    fn from(row: LicenseRow) -> Self {
        Self {
            plan: u8::try_from(row.plan).unwrap_or(0),
            expires_at: u64::try_from(row.expires_at).unwrap_or(0),
            revoked_at: row.revoked_at.map(|at| u64::try_from(at).unwrap_or(0)),
            id: row.id,
        }
    }
}

/// One license by id.
///
/// # Errors
/// Propagates query failures.
pub async fn license(pool: &SqlitePool, license_id: &str) -> sqlx::Result<Option<LicenseView>> {
    let row = sqlx::query_as::<_, LicenseRow>(
        "SELECT id, plan, expires_at, revoked_at FROM licenses WHERE id = ?1",
    )
    .bind(license_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(LicenseView::from))
}

/// A device as the route layer sees it.
#[derive(Debug, Clone)]
pub struct DeviceView {
    /// License it belongs to.
    pub license_id: String,
    /// Cumulative trial seconds already recorded.
    pub trial_seconds_used: u64,
    /// Set once another device took the seat (§12.2).
    pub displaced_at: Option<u64>,
}

/// One device by id.
///
/// # Errors
/// Propagates query failures.
pub async fn device(pool: &SqlitePool, device_id: &str) -> sqlx::Result<Option<DeviceView>> {
    let row = sqlx::query_as::<_, DeviceRow>(
        "SELECT license_id, trial_seconds_used, displaced_at FROM devices WHERE id = ?1",
    )
    .bind(device_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| DeviceView {
        license_id: row.license_id,
        trial_seconds_used: u64::try_from(row.trial_seconds_used).unwrap_or(0),
        displaced_at: row.displaced_at.map(|at| u64::try_from(at).unwrap_or(0)),
    }))
}

/// Token previously issued for an idempotency key, if any (§12.2).
///
/// # Errors
/// Propagates query failures.
pub async fn token_for_idempotency_key(
    pool: &SqlitePool,
    key: &str,
) -> sqlx::Result<Option<Vec<u8>>> {
    let row: Option<(Vec<u8>,)> =
        sqlx::query_as("SELECT token FROM issued_tokens WHERE idempotency_key = ?1")
            .bind(key)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(token,)| token))
}

/// Records a device and the token issued for it, in one transaction.
///
/// The device row uses `INSERT OR IGNORE`, so a retry that slipped past the
/// idempotency lookup still cannot create a second device (§12.2, §18).
///
/// # Errors
/// Propagates query failures, including the unique violation when two calls
/// with the same key race.
pub async fn record_issue(
    pool: &SqlitePool,
    license_id: &str,
    device_id: &str,
    idempotency_key: &str,
    token: &[u8],
    now: u64,
) -> sqlx::Result<()> {
    let now = i64::try_from(now).unwrap_or(i64::MAX);
    let mut tx = pool.begin().await?;

    sqlx::query(
        "INSERT INTO devices (id, license_id, last_heartbeat_at, trial_seconds_used, created_at)
         VALUES (?1, ?2, NULL, 0, ?3)
         ON CONFLICT (id) DO UPDATE SET displaced_at = NULL",
    )
    .bind(device_id)
    .bind(license_id)
    .bind(now)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "INSERT INTO issued_tokens (idempotency_key, license_id, device_id, token, issued_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(idempotency_key)
    .bind(license_id)
    .bind(device_id)
    .bind(token)
    .bind(now)
    .execute(&mut *tx)
    .await?;

    tx.commit().await
}

/// Records a heartbeat and the cumulative trial seconds.
///
/// # Errors
/// Propagates query failures.
pub async fn record_heartbeat(
    pool: &SqlitePool,
    device_id: &str,
    trial_seconds_used: u64,
    now: u64,
) -> sqlx::Result<()> {
    sqlx::query("UPDATE devices SET last_heartbeat_at = ?2, trial_seconds_used = ?3 WHERE id = ?1")
        .bind(device_id)
        .bind(i64::try_from(now).unwrap_or(i64::MAX))
        .bind(i64::try_from(trial_seconds_used).unwrap_or(i64::MAX))
        .execute(pool)
        .await?;
    Ok(())
}

/// Marks a license revoked. Idempotent.
///
/// # Errors
/// Propagates query failures.
pub async fn revoke_license(pool: &SqlitePool, license_id: &str, now: u64) -> sqlx::Result<()> {
    sqlx::query("UPDATE licenses SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL")
        .bind(license_id)
        .bind(i64::try_from(now).unwrap_or(i64::MAX))
        .execute(pool)
        .await?;
    Ok(())
}

/// Live devices of a license, ordered by their last heartbeat, most recent
/// first. A device that was already displaced is not in the list.
///
/// # Errors
/// Propagates query failures.
pub async fn live_devices(
    pool: &SqlitePool,
    license_id: &str,
) -> sqlx::Result<Vec<(String, Option<u64>)>> {
    let rows: Vec<(String, Option<i64>)> = sqlx::query_as(
        "SELECT id, last_heartbeat_at FROM devices
          WHERE license_id = ?1 AND displaced_at IS NULL
          ORDER BY COALESCE(last_heartbeat_at, 0) DESC",
    )
    .bind(license_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, at)| (id, at.map(|at| u64::try_from(at).unwrap_or(0))))
        .collect())
}

/// Displaces `device_id`: another device took the seat (§12.2, §19 phase 3).
///
/// # Errors
/// Propagates query failures.
pub async fn displace_device(pool: &SqlitePool, device_id: &str, now: u64) -> sqlx::Result<()> {
    sqlx::query("UPDATE devices SET displaced_at = ?2 WHERE id = ?1 AND displaced_at IS NULL")
        .bind(device_id)
        .bind(i64::try_from(now).unwrap_or(i64::MAX))
        .execute(pool)
        .await?;
    Ok(())
}

/// Inserts a webhook event and applies the entitlement change in the same
/// transaction (§12.2).
///
/// Returns `false` when the `event_id` was already recorded: that is a replay
/// and the caller answers 409 without touching entitlement (§18).
///
/// # Errors
/// Propagates query failures other than the unique violation.
pub async fn apply_webhook(
    pool: &SqlitePool,
    event_id: &str,
    provider: &str,
    event_ts: u64,
    account_id: &str,
    change: EntitlementChange,
    now: u64,
) -> sqlx::Result<bool> {
    let now = i64::try_from(now).unwrap_or(i64::MAX);
    let mut tx = pool.begin().await?;

    let inserted = sqlx::query(
        "INSERT OR IGNORE INTO webhook_events (event_id, provider, received_at, event_ts)
         VALUES (?1, ?2, ?3, ?4)",
    )
    .bind(event_id)
    .bind(provider)
    .bind(now)
    .bind(i64::try_from(event_ts).unwrap_or(i64::MAX))
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() == 0 {
        tx.rollback().await?;
        return Ok(false);
    }

    sqlx::query("INSERT OR IGNORE INTO accounts (id, created_at) VALUES (?1, ?2)")
        .bind(account_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;

    match change {
        EntitlementChange::Grant {
            license_id,
            plan,
            expires_at,
        } => {
            sqlx::query(
                "INSERT INTO licenses (id, account_id, plan, expires_at, revoked_at, created_at)
                 VALUES (?1, ?2, ?3, ?4, NULL, ?5)
                 ON CONFLICT (id) DO UPDATE SET
                     plan = excluded.plan,
                     expires_at = excluded.expires_at,
                     revoked_at = NULL",
            )
            .bind(&license_id)
            .bind(account_id)
            .bind(i64::from(plan))
            .bind(i64::try_from(expires_at).unwrap_or(i64::MAX))
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
        EntitlementChange::RevokeAll => {
            sqlx::query(
                "UPDATE licenses SET revoked_at = ?2 WHERE account_id = ?1 AND revoked_at IS NULL",
            )
            .bind(account_id)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
    }

    tx.commit().await?;
    Ok(true)
}

/// What a payment event does to an account's entitlement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntitlementChange {
    /// Create or extend a license.
    Grant {
        /// License to create or update.
        license_id: String,
        /// Plan byte (§12.1).
        plan: u8,
        /// Unix seconds the entitlement runs to.
        expires_at: u64,
    },
    /// Revoke everything the account holds.
    RevokeAll,
}
