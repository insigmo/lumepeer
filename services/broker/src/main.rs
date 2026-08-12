//! Lumepeer license broker (design doc §4, §12).
//!
//! Its own trusted server zone. Compromising or losing it must never hand a
//! guest unauthorized access: the client falls back to cached tokens inside the
//! offline policy of §12.4, and every authorization decision still happens on
//! the host.

#![forbid(unsafe_code)]
#![allow(
    unreachable_pub,
    reason = "binary crate: `pub` marks the HTTP surface of §12.2, not a library API"
)]
#![allow(
    dead_code,
    reason = "phase 0 skeleton: the request/response fields of §12.2 are consumed in phase 3"
)]

mod db;
mod routes {
    pub mod license;
    pub mod webhook;
}

use axum::Router;
use sqlx::SqlitePool;

/// Shared state of the broker.
#[derive(Debug, Clone)]
pub struct AppState {
    /// SQLite pool in WAL mode.
    pub pool: SqlitePool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().json().init();

    let database_url =
        std::env::var("LUMEPEER_BROKER_DB").unwrap_or_else(|_| "sqlite://broker.db".to_owned());
    let pool = db::connect(&database_url).await?;
    let state = AppState { pool };

    // tower middleware stays limited to rate limiting and request ids (§5.1).
    let app = Router::new()
        .merge(routes::license::router())
        .merge(routes::webhook::router())
        .with_state(state);

    let bind =
        std::env::var("LUMEPEER_BROKER_BIND").unwrap_or_else(|_| "127.0.0.1:8080".to_owned());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(%bind, "broker listening");
    axum::serve(listener, app).await?;
    Ok(())
}
