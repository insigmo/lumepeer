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
