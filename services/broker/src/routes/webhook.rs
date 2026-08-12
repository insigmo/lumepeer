//! `/v1/webhook/payment` (design doc §12.2, §18).
//!
//! Three checks before anything changes: provider signature, a 5 minute
//! timestamp window, and a unique `event_id` inserted in the same SQLite
//! transaction as the entitlement change. An invalid signature answers 400, a
//! replay answers 409, and neither touches entitlement.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::db::{self, EntitlementChange};

/// Accepted clock skew for webhook timestamps (§12.2).
pub const WEBHOOK_TIMESTAMP_WINDOW_SECS: u64 = 5 * 60;

/// Header carrying the provider's hex HMAC over the raw body.
pub const SIGNATURE_HEADER: &str = "x-lumepeer-signature";
/// Header carrying the provider's Unix timestamp, signed together with the body.
pub const TIMESTAMP_HEADER: &str = "x-lumepeer-timestamp";

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
    /// Provider event kind: `subscription.active` or `subscription.cancelled`.
    pub kind: String,
    /// License the event grants, for the granting kinds.
    pub license_id: Option<String>,
    /// Plan byte for the granting kinds (§12.1).
    pub plan: Option<u8>,
    /// Unix seconds the entitlement runs to, for the granting kinds.
    pub expires_at: Option<u64>,
}

/// Result of a webhook call.
#[derive(Debug, Serialize)]
pub struct WebhookResponse {
    /// Stable machine-readable status.
    pub status: &'static str,
}

async fn payment(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, Json<WebhookResponse>) {
    let now = crate::now_secs();

    let Some(timestamp) = headers
        .get(TIMESTAMP_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
    else {
        return refuse(StatusCode::BAD_REQUEST, "missing_timestamp");
    };
    let Some(signature) = headers
        .get(SIGNATURE_HEADER)
        .and_then(|value| value.to_str().ok())
    else {
        return refuse(StatusCode::BAD_REQUEST, "missing_signature");
    };

    // Window first: an old-but-correctly-signed event is still a replay
    // candidate, and checking it before the body is parsed keeps the work small.
    if now.abs_diff(timestamp) > WEBHOOK_TIMESTAMP_WINDOW_SECS {
        return refuse(StatusCode::BAD_REQUEST, "timestamp_outside_window");
    }
    if !state.verify_webhook(timestamp, &body, signature) {
        return refuse(StatusCode::BAD_REQUEST, "invalid_signature");
    }

    let Ok(event) = serde_json::from_slice::<PaymentEvent>(&body) else {
        return refuse(StatusCode::BAD_REQUEST, "malformed_event");
    };
    if event.event_ts.abs_diff(timestamp) > WEBHOOK_TIMESTAMP_WINDOW_SECS {
        return refuse(StatusCode::BAD_REQUEST, "event_timestamp_outside_window");
    }

    let change = match event.kind.as_str() {
        "subscription.active" => {
            let (Some(license_id), Some(plan), Some(expires_at)) =
                (event.license_id, event.plan, event.expires_at)
            else {
                return refuse(StatusCode::BAD_REQUEST, "incomplete_event");
            };
            EntitlementChange::Grant {
                license_id,
                plan,
                expires_at,
            }
        }
        "subscription.cancelled" => EntitlementChange::RevokeAll,
        // An unknown kind changes nothing, and says so rather than guessing.
        _ => return refuse(StatusCode::BAD_REQUEST, "unknown_event_kind"),
    };

    match db::apply_webhook(
        &state.pool,
        &event.event_id,
        "payment",
        event.event_ts,
        &event.account_id,
        change,
        now,
    )
    .await
    {
        // The event id was already recorded: replay, entitlement untouched.
        Ok(false) => refuse(StatusCode::CONFLICT, "replay"),
        Ok(true) => (StatusCode::OK, Json(WebhookResponse { status: "applied" })),
        Err(error) => {
            tracing::error!(%error, "webhook transaction failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(WebhookResponse { status: "internal" }),
            )
        }
    }
}

/// Refuses the event without changing entitlement, and says why in a way that
/// carries no secret and no body content (§15).
fn refuse(status: StatusCode, reason: &'static str) -> (StatusCode, Json<WebhookResponse>) {
    tracing::warn!(reason, "refusing a payment webhook");
    (status, Json(WebhookResponse { status: reason }))
}
