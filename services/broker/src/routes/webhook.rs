//! `/v1/webhook/payment` (design doc §12.2, §18).
//!
//! Three checks before anything changes: provider signature, a 5 minute
//! timestamp window, and a unique `event_id` inserted in the same SQLite
//! transaction as the entitlement change. An invalid signature answers 400, a
//! replay answers 409, and neither touches entitlement.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;

use crate::AppState;

/// Accepted clock skew for webhook timestamps (§12.2).
pub const WEBHOOK_TIMESTAMP_WINDOW_SECS: u64 = 5 * 60;

/// Mounts the webhook routes.
pub fn router() -> Router<AppState> {
    Router::new().route("/v1/webhook/payment", post(payment))
}

/// Payment provider event.
#[derive(Debug, Deserialize)]
pub struct PaymentEvent {
    /// Unique per event; a duplicate is a replay (§12.2).
    pub event_id: String,
    /// Provider-side Unix timestamp.
    pub event_ts: u64,
    /// Account whose entitlement changes.
    pub account_id: String,
    /// Provider event kind.
    pub kind: String,
}

async fn payment(
    State(_state): State<AppState>,
    Json(_event): Json<PaymentEvent>,
) -> (StatusCode, &'static str) {
    (StatusCode::NOT_IMPLEMENTED, "phase 3: payment webhook")
}
