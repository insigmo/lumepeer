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

mod db;
mod routes {
    pub mod license;
    pub mod webhook;
}

use axum::Router;
use ed25519_dalek::SigningKey;
use sqlx::SqlitePool;

/// Environment variable holding the hex Ed25519 token signing key (§12.1).
pub const SIGNING_KEY_ENV: &str = "LUMEPEER_BROKER_SIGNING_KEY";
/// Environment variable holding the shared secret the payment provider signs
/// its webhooks with (§12.2).
pub const WEBHOOK_SECRET_ENV: &str = "LUMEPEER_BROKER_WEBHOOK_SECRET";
/// Environment variable holding the id of the current signing key, so that a
/// rotation can be told apart on the client side (§12.2).
pub const KEY_ID_ENV: &str = "LUMEPEER_BROKER_KEY_ID";

/// Shared state of the broker.
#[derive(Clone)]
pub struct AppState {
    /// SQLite pool in WAL mode.
    pub pool: SqlitePool,
    /// Ed25519 key that signs license tokens (§12.1).
    pub signing_key: SigningKey,
    /// Identifier of that key, carried in every token for rotation (§12.2).
    pub key_id: u32,
    /// Shared secret the payment provider signs webhooks with.
    pub webhook_secret: Vec<u8>,
}

// Neither the signing key nor the webhook secret may ever reach a log (§15).
impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

impl AppState {
    /// Verifies a provider signature over `timestamp || "." || body`.
    ///
    /// The comparison runs through `blake3::Hash`, whose `PartialEq` is
    /// constant time, so a wrong signature does not leak how much of it was
    /// right.
    #[must_use]
    pub fn verify_webhook(&self, timestamp: u64, body: &[u8], signature_hex: &str) -> bool {
        let Ok(provided) = decode_hex32(signature_hex) else {
            return false;
        };
        self.webhook_signature(timestamp, body) == blake3::Hash::from_bytes(provided)
    }

    /// Signature the provider is expected to send.
    #[must_use]
    pub fn webhook_signature(&self, timestamp: u64, body: &[u8]) -> blake3::Hash {
        let mut hasher = blake3::Hasher::new_keyed(&keyed_secret(&self.webhook_secret));
        hasher.update(timestamp.to_string().as_bytes());
        hasher.update(b".");
        hasher.update(body);
        hasher.finalize()
    }
}

/// Stretches an arbitrary-length shared secret into the 32 byte key BLAKE3's
/// keyed mode needs.
fn keyed_secret(secret: &[u8]) -> [u8; 32] {
    *blake3::hash(secret).as_bytes()
}

fn decode_hex32(text: &str) -> Result<[u8; 32], ()> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return Err(());
    }
    let mut out = [0u8; 32];
    for (index, pair) in bytes.chunks_exact(2).enumerate() {
        let hi = (pair[0] as char).to_digit(16).ok_or(())?;
        let lo = (pair[1] as char).to_digit(16).ok_or(())?;
        out[index] = u8::try_from(hi * 16 + lo).map_err(|_| ())?;
    }
    Ok(out)
}

/// Current wall clock in Unix seconds.
#[must_use]
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Builds the router over an existing state. Used by `main` and by the tests.
pub fn app(state: AppState) -> Router {
    // tower middleware stays limited to rate limiting and request ids (§5.1).
    Router::new()
        .merge(routes::license::router())
        .merge(routes::webhook::router())
        .with_state(state)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().json().init();

    let database_url =
        std::env::var("LUMEPEER_BROKER_DB").unwrap_or_else(|_| "sqlite://broker.db".to_owned());
    let pool = db::connect(&database_url).await?;

    let signing_key = {
        let hex = std::env::var(SIGNING_KEY_ENV).map_err(|_| {
            anyhow::anyhow!(
                "{SIGNING_KEY_ENV} is required: the broker refuses to sign with a default key"
            )
        })?;
        let bytes = decode_hex32(&hex)
            .map_err(|()| anyhow::anyhow!("{SIGNING_KEY_ENV} must be 64 hex characters"))?;
        SigningKey::from_bytes(&bytes)
    };
    let webhook_secret = std::env::var(WEBHOOK_SECRET_ENV)
        .map_err(|_| anyhow::anyhow!("{WEBHOOK_SECRET_ENV} is required"))?
        .into_bytes();
    let key_id = std::env::var(KEY_ID_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);

    let state = AppState {
        pool,
        signing_key,
        key_id,
        webhook_secret,
    };

    let bind =
        std::env::var("LUMEPEER_BROKER_BIND").unwrap_or_else(|_| "127.0.0.1:8080".to_owned());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(%bind, key_id, "broker listening");
    axum::serve(listener, app(state)).await?;
    Ok(())
}
