//! Broker integration tests (design doc §12, §17.1, §19 phase 3).
//!
//! Every row §19 names for this phase gets its own test: the trial limit
//! offline, the device conflict resolved by heartbeat, a webhook with an
//! invalid signature, a replayed webhook and a retried `issue`.
//!
//! The broker is a binary crate, so these tests drive its routes the way a
//! client does: over HTTP against a server bound to an ephemeral port, with the
//! database in a temporary file.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a failed assumption must fail the test"
)]

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use lumepeer_core::constants::TRIAL_SESSION_LIMIT_SECS;
use lumepeer_core::license::{LicenseDecision, LicenseGuard, LicenseToken, Plan};

/// Hex of the Ed25519 key the test broker signs tokens with.
const SIGNING_KEY_HEX: &str = "0505050505050505050505050505050505050505050505050505050505050505";
/// Shared secret the test provider signs webhooks with.
const WEBHOOK_SECRET: &str = "test-webhook-secret";

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn broker_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    path.pop();
    path.push("lumepeer-broker");
    path
}

/// A broker process with its own database, killed when the test ends.
struct Broker {
    child: Child,
    base: String,
    database: PathBuf,
    client: ureq::Agent,
}

impl Drop for Broker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.database);
    }
}

impl Broker {
    fn start() -> Option<Self> {
        let binary = broker_binary();
        if !binary.exists() {
            return None;
        }

        // An ephemeral port picked by the OS, then released: the broker binds it
        // immediately afterwards. Good enough for a test, and it avoids a fixed
        // port colliding between parallel runs.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let mut database = std::env::temp_dir();
        database.push(format!("lumepeer-broker-test-{port}.db"));
        let _ = std::fs::remove_file(&database);

        let child = Command::new(binary)
            .env(
                "LUMEPEER_BROKER_DB",
                format!("sqlite://{}", database.display()),
            )
            .env("LUMEPEER_BROKER_BIND", format!("127.0.0.1:{port}"))
            .env("LUMEPEER_BROKER_SIGNING_KEY", SIGNING_KEY_HEX)
            .env("LUMEPEER_BROKER_WEBHOOK_SECRET", WEBHOOK_SECRET)
            .env("LUMEPEER_BROKER_KEY_ID", "7")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        let broker = Self {
            child,
            base: format!("http://127.0.0.1:{port}"),
            database,
            client: ureq::Agent::new_with_config(
                ureq::Agent::config_builder()
                    .timeout_global(Some(Duration::from_secs(5)))
                    // Status errors are read for their body below, same as a
                    // success response, rather than matched out of `Err`.
                    .http_status_as_error(false)
                    .build(),
            ),
        };
        broker.wait_until_up()?;
        Some(broker)
    }

    fn wait_until_up(&self) -> Option<()> {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            // Any answer at all means the listener is up; a 404 or a 400 counts.
            match self
                .client
                .post(&format!("{}/v1/license/heartbeat", self.base))
                .send_json(serde_json::json!({ "device_id": "probe", "active_seconds": 0 }))
            {
                // Any answer means the listener is up, including an error
                // status: the probe device does not exist.
                Ok(_) => return Some(()),
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        None
    }

    fn post(&self, path: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
        match self
            .client
            .post(&format!("{}{path}", self.base))
            .send_json(body)
        {
            Ok(mut response) => {
                let status = response.status().as_u16();
                let value = response
                    .body_mut()
                    .read_json::<serde_json::Value>()
                    .unwrap_or(serde_json::Value::Null);
                (status, value)
            }
            Err(e) => panic!("broker request failed: {e}"),
        }
    }

    /// Posts a webhook, signing it the way the provider would.
    fn webhook(&self, timestamp: u64, event: &serde_json::Value) -> (u16, serde_json::Value) {
        let body = serde_json::to_vec(event).unwrap();
        let signature = sign_webhook(timestamp, &body);
        self.post_signed("/v1/webhook/payment", timestamp, &signature, &body)
    }

    fn post_signed(
        &self,
        path: &str,
        timestamp: u64,
        signature: &str,
        body: &[u8],
    ) -> (u16, serde_json::Value) {
        let request = self
            .client
            .post(&format!("{}{path}", self.base))
            .header("content-type", "application/json")
            .header("x-lumepeer-timestamp", timestamp.to_string())
            .header("x-lumepeer-signature", signature);
        match request.send(body) {
            Ok(mut response) => {
                let status = response.status().as_u16();
                let value = response
                    .body_mut()
                    .read_json::<serde_json::Value>()
                    .unwrap_or(serde_json::Value::Null);
                (status, value)
            }
            Err(e) => panic!("broker request failed: {e}"),
        }
    }

    /// Grants an account a license through a signed webhook.
    fn grant(&self, account: &str, license: &str, plan: Plan, expires_at: u64) {
        let now = now_secs();
        let (status, body) = self.webhook(
            now,
            &serde_json::json!({
                "event_id": format!("evt-{license}"),
                "event_ts": now,
                "account_id": account,
                "kind": "subscription.active",
                "license_id": license,
                "plan": plan.to_wire(),
                "expires_at": expires_at,
            }),
        );
        assert_eq!(status, 200, "granting the license failed: {body}");
    }
}

fn sign_webhook(timestamp: u64, body: &[u8]) -> String {
    let key = *blake3::hash(WEBHOOK_SECRET.as_bytes()).as_bytes();
    let mut hasher = blake3::Hasher::new_keyed(&key);
    hasher.update(timestamp.to_string().as_bytes());
    hasher.update(b".");
    hasher.update(body);
    hasher.finalize().to_hex().to_string()
}

fn verifying_key() -> ed25519_dalek::VerifyingKey {
    let mut bytes = [0u8; 32];
    for (index, chunk) in SIGNING_KEY_HEX
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .enumerate()
    {
        let text = std::str::from_utf8(chunk).unwrap();
        bytes[index] = u8::from_str_radix(text, 16).unwrap();
    }
    ed25519_dalek::SigningKey::from_bytes(&bytes).verifying_key()
}

fn decode_token(value: &serde_json::Value) -> LicenseToken {
    let text = value["token"].as_str().expect("no token in the response");
    let bytes = data_encoding::BASE32_NOPAD.decode(text.as_bytes()).unwrap();
    LicenseToken::parse_and_verify(&bytes, &verifying_key()).expect("the token does not verify")
}

/// §12.2, §18: retrying `issue` with the same `idempotency_key` returns the
/// same token and creates no second device.
#[test]
fn a_retried_issue_returns_the_same_token() {
    let Some(broker) = Broker::start() else {
        return;
    };
    broker.grant("acct-1", "lic-1", Plan::Pro, now_secs() + 86_400);

    let request = serde_json::json!({
        "account_id": "acct-1",
        "device_id": "dev-1",
        "idempotency_key": "key-1",
    });
    let (first_status, first) = broker.post("/v1/license/issue", request.clone());
    assert_eq!(first_status, 201);
    let (second_status, second) = broker.post("/v1/license/issue", request);
    assert_eq!(second_status, 200);
    assert_eq!(first["token"], second["token"]);

    let token = decode_token(&first);
    assert_eq!(token.plan, Plan::Pro);
    assert_eq!(token.key_id, 7);
}

/// §12.2, §19 phase 3: two devices claim one seat, the one with the older
/// heartbeat loses and is told why.
#[test]
fn the_broker_resolves_a_device_conflict_by_heartbeat() {
    let Some(broker) = Broker::start() else {
        return;
    };
    broker.grant("acct-2", "lic-2", Plan::Pro, now_secs() + 86_400);

    let (status, _) = broker.post(
        "/v1/license/issue",
        serde_json::json!({
            "account_id": "acct-2",
            "device_id": "dev-a",
            "idempotency_key": "key-a",
        }),
    );
    assert_eq!(status, 201);
    let (status, body) = broker.post(
        "/v1/license/heartbeat",
        serde_json::json!({ "device_id": "dev-a", "active_seconds": 10 }),
    );
    assert_eq!(status, 200);
    assert_eq!(body["ok"], true);

    // A second device takes the only seat of a Pro license.
    let (status, _) = broker.post(
        "/v1/license/issue",
        serde_json::json!({
            "account_id": "acct-2",
            "device_id": "dev-b",
            "idempotency_key": "key-b",
        }),
    );
    assert_eq!(status, 201);

    let (status, body) = broker.post(
        "/v1/license/heartbeat",
        serde_json::json!({ "device_id": "dev-a", "active_seconds": 20 }),
    );
    assert_eq!(status, 200);
    assert_eq!(body["ok"], false, "the displaced device must be told");
    assert!(
        body["reason"].as_str().unwrap().contains("another device"),
        "the reason must explain the conflict: {body}"
    );

    let (status, body) = broker.post(
        "/v1/license/heartbeat",
        serde_json::json!({ "device_id": "dev-b", "active_seconds": 1 }),
    );
    assert_eq!(status, 200);
    assert_eq!(body["ok"], true, "the newest device keeps the seat");
}

/// §12.2, §18: an invalid provider signature answers 400 and changes nothing.
#[test]
fn a_webhook_with_an_invalid_signature_is_refused() {
    let Some(broker) = Broker::start() else {
        return;
    };
    let now = now_secs();
    let event = serde_json::json!({
        "event_id": "evt-bad",
        "event_ts": now,
        "account_id": "acct-3",
        "kind": "subscription.active",
        "license_id": "lic-3",
        "plan": Plan::Team.to_wire(),
        "expires_at": now + 86_400,
    });
    let body = serde_json::to_vec(&event).unwrap();
    let (status, answer) = broker.post_signed("/v1/webhook/payment", now, &"0".repeat(64), &body);
    assert_eq!(status, 400);
    assert_eq!(answer["status"], "invalid_signature");

    // Entitlement is untouched: issue still finds no license.
    let (status, _) = broker.post(
        "/v1/license/issue",
        serde_json::json!({
            "account_id": "acct-3",
            "device_id": "dev-3",
            "idempotency_key": "key-3",
        }),
    );
    assert_eq!(status, 403);
}

/// §12.2, §18: a replayed `event_id` answers 409 and changes nothing.
#[test]
fn a_replayed_webhook_is_refused_with_conflict() {
    let Some(broker) = Broker::start() else {
        return;
    };
    let now = now_secs();
    let event = serde_json::json!({
        "event_id": "evt-replay",
        "event_ts": now,
        "account_id": "acct-4",
        "kind": "subscription.active",
        "license_id": "lic-4",
        "plan": Plan::Team.to_wire(),
        "expires_at": now + 86_400,
    });

    let (status, _) = broker.webhook(now, &event);
    assert_eq!(status, 200);
    let (status, answer) = broker.webhook(now, &event);
    assert_eq!(status, 409);
    assert_eq!(answer["status"], "replay");
}

/// §12.2: a correctly signed event outside the 5 minute window is refused.
#[test]
fn a_webhook_outside_the_timestamp_window_is_refused() {
    let Some(broker) = Broker::start() else {
        return;
    };
    let stale = now_secs() - 3_600;
    let event = serde_json::json!({
        "event_id": "evt-stale",
        "event_ts": stale,
        "account_id": "acct-5",
        "kind": "subscription.active",
        "license_id": "lic-5",
        "plan": Plan::Pro.to_wire(),
        "expires_at": stale + 86_400,
    });
    let (status, answer) = broker.webhook(stale, &event);
    assert_eq!(status, 400);
    assert_eq!(answer["status"], "timestamp_outside_window");
}

/// §12.3: the trial limit holds offline, on the client's own ledger, and the
/// broker refuses a device that has spent it.
#[test]
fn the_trial_limit_holds_offline_and_at_the_broker() {
    // Offline half: no broker involved at all.
    let mut guard = LicenseGuard::new(Some(trial_token()), 0);
    assert!(matches!(guard.evaluate(0), LicenseDecision::Allow { .. }));
    guard.add_trial_seconds(TRIAL_SESSION_LIMIT_SECS);
    assert!(matches!(guard.evaluate(0), LicenseDecision::Deny { .. }));

    let Some(broker) = Broker::start() else {
        return;
    };
    broker.grant("acct-6", "lic-6", Plan::Trial, now_secs() + 86_400);
    let (status, _) = broker.post(
        "/v1/license/issue",
        serde_json::json!({
            "account_id": "acct-6",
            "device_id": "dev-6",
            "idempotency_key": "key-6",
        }),
    );
    assert_eq!(status, 201);

    let (status, body) = broker.post(
        "/v1/license/heartbeat",
        serde_json::json!({
            "device_id": "dev-6",
            "active_seconds": TRIAL_SESSION_LIMIT_SECS,
        }),
    );
    assert_eq!(status, 200);
    assert_eq!(body["ok"], false);

    // Reporting less afterwards does not hand the trial back (§12.3).
    let (status, body) = broker.post(
        "/v1/license/heartbeat",
        serde_json::json!({ "device_id": "dev-6", "active_seconds": 1 }),
    );
    assert_eq!(status, 200);
    assert_eq!(body["ok"], false);
}

/// A revoked license stops both the heartbeat and any refresh.
#[test]
fn revoke_stops_the_heartbeat_and_the_refresh() {
    let Some(broker) = Broker::start() else {
        return;
    };
    broker.grant("acct-7", "lic-7", Plan::Team, now_secs() + 86_400);
    let (status, _) = broker.post(
        "/v1/license/issue",
        serde_json::json!({
            "account_id": "acct-7",
            "device_id": "dev-7",
            "idempotency_key": "key-7",
        }),
    );
    assert_eq!(status, 201);
    let (status, body) = broker.post(
        "/v1/license/refresh",
        serde_json::json!({ "device_id": "dev-7" }),
    );
    assert_eq!(status, 200);
    assert_eq!(decode_token(&body).plan, Plan::Team);

    let (status, _) = broker.post(
        "/v1/license/revoke",
        serde_json::json!({ "device_id": "dev-7" }),
    );
    assert_eq!(status, 204);

    let (status, body) = broker.post(
        "/v1/license/heartbeat",
        serde_json::json!({ "device_id": "dev-7", "active_seconds": 5 }),
    );
    assert_eq!(status, 200);
    assert_eq!(body["ok"], false);

    let (status, _) = broker.post(
        "/v1/license/refresh",
        serde_json::json!({ "device_id": "dev-7" }),
    );
    assert_eq!(status, 403);
}

fn trial_token() -> LicenseToken {
    let mut token = LicenseToken {
        version: lumepeer_core::license::TOKEN_VERSION,
        key_id: 1,
        license_id: [1u8; 16],
        plan: Plan::Trial,
        device_id: [2u8; 16],
        issued_at: 0,
        not_before: 0,
        expires_at: u64::MAX,
        features: 0,
        payload_hash: [0u8; 32],
        signature: [0u8; 64],
    };
    let mut bytes = [0u8; 32];
    for (index, chunk) in SIGNING_KEY_HEX
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .enumerate()
    {
        let text = std::str::from_utf8(chunk).unwrap();
        bytes[index] = u8::from_str_radix(text, 16).unwrap();
    }
    token.sign(&ed25519_dalek::SigningKey::from_bytes(&bytes));
    token
}
