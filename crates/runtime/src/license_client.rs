//! Client half of licensing (design doc §12, ADR 0163).
//!
//! An install with no token runs on [`Plan::Free`]: nothing to expire,
//! nothing to refresh, no network traffic at all. A token comes from the
//! broker once, against the account key the buyer was given, and from then on
//! is kept alive by heartbeat and refresh with nothing but this device's id.
//! Whatever goes wrong with a token — expired, displaced, offline past its
//! grace, clock rolled back — the install falls back to Free; it never stops
//! working.
//!
//! Every call that talks to the broker blocks: callers run it off the async
//! runtime.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ed25519_dalek::VerifyingKey;
use lumepeer_core::license::{LicenseDecision, LicenseGuard, LicenseToken, Plan};
use rand::Rng as _;
use serde::Deserialize;

/// Environment variable naming the broker's base URL, read at run time first
/// and at build time second.
pub const BROKER_URL_ENV: &str = "LUMEPEER_BROKER_URL";
/// Environment variable carrying the hex of the broker's Ed25519 public key.
pub const BROKER_PUBLIC_KEY_ENV: &str = "LUMEPEER_BROKER_PUBLIC_KEY";

/// Refresh a token once it has less than this much validity left. Issued
/// tokens live 24 hours (ADR 0006); at a 3 minute heartbeat this leaves
/// hundreds of chances before the token runs out.
pub const REFRESH_BEFORE_SECS: u64 = 12 * 60 * 60;

/// File the device id is kept in, inside the license directory.
const DEVICE_ID_FILE: &str = "device-id";
/// File the current token is kept in, base32 as the broker sends it.
const TOKEN_FILE: &str = "license.token";
/// Upper bound on any one broker request.
const BROKER_TIMEOUT: Duration = Duration::from_secs(10);

/// Where the broker is and which key it signs with.
#[derive(Debug, Clone, Default)]
pub struct LicenseConfig {
    /// Base URL, without a trailing slash.
    pub broker_url: Option<String>,
    /// Key every token must verify against.
    pub verifying_key: Option<VerifyingKey>,
}

impl LicenseConfig {
    /// Reads both values from the environment, falling back to what the build
    /// was given. A build with neither has licensing switched off: it runs on
    /// Free and says activation is unavailable.
    #[must_use]
    pub fn from_env() -> Self {
        let read = |name: &str, built: Option<&'static str>| {
            std::env::var(name)
                .ok()
                .or_else(|| built.map(str::to_owned))
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        let broker_url = read(BROKER_URL_ENV, option_env!("LUMEPEER_BROKER_URL"))
            .map(|url| url.trim_end_matches('/').to_owned());
        let verifying_key = read(
            BROKER_PUBLIC_KEY_ENV,
            option_env!("LUMEPEER_BROKER_PUBLIC_KEY"),
        )
        .and_then(|hex| parse_public_key(&hex));
        Self {
            broker_url,
            verifying_key,
        }
    }

    /// Whether activation can work at all in this build.
    #[must_use]
    pub const fn is_configured(&self) -> bool {
        self.broker_url.is_some() && self.verifying_key.is_some()
    }
}

fn parse_public_key(hex: &str) -> Option<VerifyingKey> {
    let bytes = data_encoding::HEXLOWER_PERMISSIVE
        .decode(hex.as_bytes())
        .ok()?;
    let bytes: [u8; 32] = bytes.try_into().ok()?;
    VerifyingKey::from_bytes(&bytes).ok()
}

/// Why a license operation failed. Every message is safe for the UI: no
/// token bytes, no account key (§15).
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum LicenseError {
    /// This build has no broker URL or no broker key.
    #[error("licensing is not configured in this build")]
    NotConfigured,
    /// The broker answered and said no.
    #[error("{message}")]
    Rejected {
        /// Stable code from the broker, e.g. `NO_ACTIVE_LICENSE`.
        code: String,
        /// The broker's message.
        message: String,
    },
    /// The broker could not be reached.
    #[error("the license server is unreachable")]
    Unreachable,
    /// The broker sent a token that does not verify or is not for this device.
    #[error("the license server sent an invalid token")]
    InvalidToken,
    /// The token could not be written to or removed from disk.
    #[error("the license could not be saved on this device")]
    Storage,
}

/// Answer to a heartbeat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeartbeatAnswer {
    /// Whether the device keeps its seat.
    pub ok: bool,
    /// Why not, when it does not.
    pub reason: Option<String>,
}

/// The four broker calls the client makes (§12.2). A trait so the tests can
/// stand in for the broker without a server.
pub trait BrokerApi: Send {
    /// `/v1/license/issue`: a token for `device_id` from the account's license.
    ///
    /// # Errors
    /// [`LicenseError::Rejected`] or [`LicenseError::Unreachable`].
    fn issue(
        &self,
        account_id: &str,
        device_id: &str,
        idempotency_key: &str,
    ) -> Result<String, LicenseError>;
    /// `/v1/license/heartbeat`.
    ///
    /// # Errors
    /// [`LicenseError::Rejected`] or [`LicenseError::Unreachable`].
    fn heartbeat(&self, device_id: &str) -> Result<HeartbeatAnswer, LicenseError>;
    /// `/v1/license/refresh`: a fresh token for a device that has one.
    ///
    /// # Errors
    /// [`LicenseError::Rejected`] or [`LicenseError::Unreachable`].
    fn refresh(&self, device_id: &str) -> Result<String, LicenseError>;
    /// `/v1/license/revoke`: gives this device's seat back.
    ///
    /// # Errors
    /// [`LicenseError::Rejected`] or [`LicenseError::Unreachable`].
    fn revoke(&self, device_id: &str) -> Result<(), LicenseError>;
}

/// The broker over HTTPS.
#[derive(Debug)]
pub struct HttpBroker {
    base: String,
    agent: ureq::Agent,
}

impl HttpBroker {
    /// A client for the broker at `base`.
    #[must_use]
    pub fn new(base: String) -> Self {
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .timeout_global(Some(BROKER_TIMEOUT))
                // Error statuses carry a JSON body worth reading.
                .http_status_as_error(false)
                .build(),
        );
        Self { base, agent }
    }

    fn post(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, LicenseError> {
        let mut response = self
            .agent
            .post(&format!("{}{path}", self.base))
            .send_json(body)
            .map_err(|error| {
                tracing::warn!(%error, path, "license server request failed");
                LicenseError::Unreachable
            })?;
        let status = response.status();
        let value = response
            .body_mut()
            .read_json::<serde_json::Value>()
            .unwrap_or(serde_json::Value::Null);
        if status.is_success() {
            return Ok(value);
        }
        let text = |key: &str| {
            value
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        Err(LicenseError::Rejected {
            code: text("code").unwrap_or_else(|| status.as_u16().to_string()),
            message: text("message").unwrap_or_else(|| "the license server refused".to_owned()),
        })
    }
}

#[derive(Deserialize)]
struct TokenBody {
    token: String,
}

#[derive(Deserialize)]
struct HeartbeatBody {
    ok: bool,
    reason: Option<String>,
}

fn token_from(value: serde_json::Value) -> Result<String, LicenseError> {
    serde_json::from_value::<TokenBody>(value)
        .map(|body| body.token)
        .map_err(|_| LicenseError::InvalidToken)
}

impl BrokerApi for HttpBroker {
    fn issue(
        &self,
        account_id: &str,
        device_id: &str,
        idempotency_key: &str,
    ) -> Result<String, LicenseError> {
        token_from(self.post(
            "/v1/license/issue",
            &serde_json::json!({
                "account_id": account_id,
                "device_id": device_id,
                "idempotency_key": idempotency_key,
            }),
        )?)
    }

    fn heartbeat(&self, device_id: &str) -> Result<HeartbeatAnswer, LicenseError> {
        // Active seconds only matter to the trial plan, which no install is put
        // on any more (ADR 0163).
        let value = self.post(
            "/v1/license/heartbeat",
            &serde_json::json!({ "device_id": device_id, "active_seconds": 0 }),
        )?;
        let body: HeartbeatBody =
            serde_json::from_value(value).map_err(|_| LicenseError::Unreachable)?;
        Ok(HeartbeatAnswer {
            ok: body.ok,
            reason: body.reason,
        })
    }

    fn refresh(&self, device_id: &str) -> Result<String, LicenseError> {
        token_from(self.post(
            "/v1/license/refresh",
            &serde_json::json!({ "device_id": device_id }),
        )?)
    }

    fn revoke(&self, device_id: &str) -> Result<(), LicenseError> {
        self.post(
            "/v1/license/revoke",
            &serde_json::json!({ "device_id": device_id }),
        )
        .map(|_| ())
    }
}

/// What the UI is told about the license.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LicenseStatus {
    /// Plan sessions run under right now.
    pub plan: Plan,
    /// Whether a paid token is held and honoured.
    pub licensed: bool,
    /// Seconds left on the token, when one is honoured.
    pub seconds_left: Option<u64>,
    /// A token is held but the last broker contact failed.
    pub offline: bool,
    /// Why a held token is not honoured, or why it was dropped.
    pub reason: Option<String>,
    /// Whether this build can activate a license at all.
    pub activation_available: bool,
}

/// The client: device id, cached token and the guard deciding what it allows.
pub struct LicenseClient {
    key: Option<VerifyingKey>,
    broker: Option<Box<dyn BrokerApi>>,
    dir: Option<PathBuf>,
    device_id: String,
    guard: LicenseGuard,
    /// Last broker contact succeeded.
    online: bool,
    /// Why the last token was dropped, until a new one arrives.
    dropped: Option<String>,
}

impl std::fmt::Debug for LicenseClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LicenseClient")
            .field("dir", &self.dir)
            .field("online", &self.online)
            .finish_non_exhaustive()
    }
}

impl LicenseClient {
    /// Loads the device id and any cached token from `dir`, creating the id
    /// on first run. `dir` of `None` keeps everything in memory: the install
    /// then runs on Free and an activation lasts until the process exits.
    #[must_use]
    pub fn load(
        config: &LicenseConfig,
        broker: Option<Box<dyn BrokerApi>>,
        dir: Option<PathBuf>,
        now: u64,
    ) -> Self {
        let device_id = dir.as_deref().and_then(read_device_id).unwrap_or_else(|| {
            let id = random_hex();
            if let Some(dir) = dir.as_deref()
                && let Err(error) = write_file(dir, DEVICE_ID_FILE, &id)
            {
                tracing::warn!(%error, "cannot save the license device id");
            }
            id
        });
        let token = match (dir.as_deref(), config.verifying_key.as_ref()) {
            (Some(dir), Some(key)) => std::fs::read_to_string(dir.join(TOKEN_FILE))
                .ok()
                .and_then(|text| verify(text.trim(), key, &device_id).ok()),
            _ => None,
        };
        if let Some(token) = token.as_ref() {
            tracing::info!(plan = token.plan.as_str(), "cached license token loaded");
        }
        Self {
            key: config.verifying_key,
            broker,
            dir,
            device_id,
            guard: LicenseGuard::new(token, now),
            online: false,
            dropped: None,
        }
    }

    /// Activates `account_key` on this device.
    ///
    /// # Errors
    /// [`LicenseError::NotConfigured`] in a build without a broker; otherwise
    /// whatever the broker or the token check said. On error the current
    /// state is left as it was.
    pub fn activate(&mut self, account_key: &str, now: u64) -> Result<LicenseStatus, LicenseError> {
        let (broker, key) = self.configured()?;
        let token_text = broker.issue(account_key.trim(), &self.device_id, &random_hex())?;
        let token = verify(&token_text, key, &self.device_id)?;
        self.store(&token_text)?;
        tracing::info!(plan = token.plan.as_str(), "license activated");
        self.guard.on_online_check(Some(token), now);
        self.online = true;
        self.dropped = None;
        Ok(self.status(now))
    }

    /// Gives the seat back and returns this device to Free.
    ///
    /// The revoke is best effort: a broker that cannot be reached still sees
    /// the seat go stale through missed heartbeats.
    ///
    /// # Errors
    /// [`LicenseError::Storage`] if the cached token cannot be removed.
    pub fn deactivate(&mut self, now: u64) -> Result<LicenseStatus, LicenseError> {
        if self.guard.token().is_some()
            && let Some(broker) = self.broker.as_deref()
            && let Err(error) = broker.revoke(&self.device_id)
        {
            tracing::warn!(%error, "license revoke did not reach the server");
        }
        self.forget()?;
        self.dropped = None;
        Ok(self.status(now))
    }

    /// One heartbeat, plus a refresh when the token is getting old. Does
    /// nothing on Free.
    pub fn tick(&mut self, now: u64) -> LicenseStatus {
        let Some(seconds_left) = self.guard.token().map(|token| token.seconds_left(now)) else {
            return self.status(now);
        };
        let Some(broker) = self.broker.as_deref() else {
            return self.status(now);
        };
        match broker.heartbeat(&self.device_id) {
            Ok(HeartbeatAnswer { ok: true, .. }) => {
                self.guard.on_heartbeat(now);
                self.online = true;
                if seconds_left < REFRESH_BEFORE_SECS {
                    self.refresh(now);
                }
            }
            Ok(HeartbeatAnswer { ok: false, reason }) => {
                let reason =
                    reason.unwrap_or_else(|| "the license server ended this seat".to_owned());
                tracing::warn!(%reason, "license seat lost; back to the free plan");
                if let Err(error) = self.forget() {
                    tracing::warn!(%error, "cannot remove the cached license token");
                }
                self.online = true;
                self.dropped = Some(reason);
            }
            Err(LicenseError::Rejected { code, message }) => {
                tracing::warn!(%code, %message, "license heartbeat refused; back to the free plan");
                if let Err(error) = self.forget() {
                    tracing::warn!(%error, "cannot remove the cached license token");
                }
                self.online = true;
                self.dropped = Some(message);
            }
            Err(_) => self.online = false,
        }
        self.status(now)
    }

    /// Current state, evaluated at `now`.
    pub fn status(&mut self, now: u64) -> LicenseStatus {
        let decision = self.guard.evaluate(now);
        let has_token = self.guard.token().is_some();
        let (licensed, seconds_left, reason) = match &decision {
            LicenseDecision::Allow { seconds_left, .. } if has_token => (true, *seconds_left, None),
            LicenseDecision::Allow { .. } => (false, None, self.dropped.clone()),
            LicenseDecision::DenyNewSession { reason, .. } | LicenseDecision::Deny { reason } => {
                (false, None, Some((*reason).to_owned()))
            }
        };
        LicenseStatus {
            plan: decision.plan_in_force(),
            licensed,
            seconds_left,
            offline: has_token && !self.online,
            reason,
            activation_available: self.broker.is_some() && self.key.is_some(),
        }
    }

    fn configured(&self) -> Result<(&dyn BrokerApi, &VerifyingKey), LicenseError> {
        match (self.broker.as_deref(), self.key.as_ref()) {
            (Some(broker), Some(key)) => Ok((broker, key)),
            _ => Err(LicenseError::NotConfigured),
        }
    }

    fn refresh(&mut self, now: u64) {
        let Ok((broker, key)) = self.configured() else {
            return;
        };
        let fresh = broker
            .refresh(&self.device_id)
            .and_then(|text| verify(&text, key, &self.device_id).map(|token| (text, token)));
        match fresh {
            Ok((text, token)) => {
                if let Err(error) = self.store(&text) {
                    tracing::warn!(%error, "cannot save the refreshed license token");
                }
                self.guard.on_online_check(Some(token), now);
            }
            Err(error) => tracing::warn!(%error, "license refresh failed; the current token stays"),
        }
    }

    fn store(&self, token_text: &str) -> Result<(), LicenseError> {
        match self.dir.as_deref() {
            Some(dir) => write_file(dir, TOKEN_FILE, token_text).map_err(|error| {
                tracing::warn!(%error, "cannot save the license token");
                LicenseError::Storage
            }),
            None => Ok(()),
        }
    }

    fn forget(&mut self) -> Result<(), LicenseError> {
        self.guard = LicenseGuard::new(None, 0);
        match self
            .dir
            .as_deref()
            .map(|dir| std::fs::remove_file(dir.join(TOKEN_FILE)))
        {
            Some(Err(error)) if error.kind() != std::io::ErrorKind::NotFound => {
                tracing::warn!(%error, "cannot remove the license token");
                Err(LicenseError::Storage)
            }
            _ => Ok(()),
        }
    }
}

/// Decodes, verifies and checks the device binding of a base32 token.
fn verify(text: &str, key: &VerifyingKey, device_id: &str) -> Result<LicenseToken, LicenseError> {
    let bytes = data_encoding::BASE32_NOPAD
        .decode(text.as_bytes())
        .map_err(|_| LicenseError::InvalidToken)?;
    let token =
        LicenseToken::parse_and_verify(&bytes, key).map_err(|_| LicenseError::InvalidToken)?;
    // A token copied from another install is not this device's seat.
    if token.device_id != device_hash(device_id) {
        return Err(LicenseError::InvalidToken);
    }
    Ok(token)
}

/// The 16 bytes the broker puts in a token for `device_id` (ADR 0006).
fn device_hash(device_id: &str) -> [u8; 16] {
    let mut out = [0u8; 16];
    out.copy_from_slice(&blake3::hash(device_id.as_bytes()).as_bytes()[..16]);
    out
}

fn random_hex() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    data_encoding::HEXLOWER.encode(&bytes)
}

fn read_device_id(dir: &Path) -> Option<String> {
    std::fs::read_to_string(dir.join(DEVICE_ID_FILE))
        .ok()
        .map(|text| text.trim().to_owned())
        .filter(|id| !id.is_empty())
}

fn write_file(dir: &Path, name: &str, contents: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(name);
    let partial = dir.join(format!("{name}.partial"));
    std::fs::write(&partial, contents)?;
    std::fs::rename(partial, path)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::{Arc, Mutex};

    use ed25519_dalek::SigningKey;
    use lumepeer_core::license::TOKEN_VERSION;

    use super::*;

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[5u8; 32])
    }

    fn config() -> LicenseConfig {
        LicenseConfig {
            broker_url: Some("http://broker.test".to_owned()),
            verifying_key: Some(signing_key().verifying_key()),
        }
    }

    fn token_for(device_id: &str, plan: Plan, expires_at: u64) -> String {
        let mut token = LicenseToken {
            version: TOKEN_VERSION,
            key_id: 1,
            license_id: [7u8; 16],
            plan,
            device_id: device_hash(device_id),
            issued_at: 0,
            not_before: 0,
            expires_at,
            features: 0,
            payload_hash: [0u8; 32],
            signature: [0u8; 64],
        };
        token.sign(&signing_key());
        data_encoding::BASE32_NOPAD.encode(&token.encode())
    }

    /// What the fake broker answers and what it was asked.
    #[derive(Default)]
    struct Script {
        plan: Option<Plan>,
        expires_at: u64,
        heartbeat: Option<Result<HeartbeatAnswer, LicenseError>>,
        heartbeats: u32,
        refreshes: u32,
        revokes: u32,
        foreign_device: bool,
    }

    struct FakeBroker(Arc<Mutex<Script>>);

    impl BrokerApi for FakeBroker {
        fn issue(&self, account: &str, device: &str, _: &str) -> Result<String, LicenseError> {
            let script = self.0.lock().unwrap();
            match script.plan {
                Some(plan) if account == "good-key" => {
                    let device = if script.foreign_device {
                        "other"
                    } else {
                        device
                    };
                    Ok(token_for(device, plan, script.expires_at))
                }
                _ => Err(LicenseError::Rejected {
                    code: "NO_ACTIVE_LICENSE".to_owned(),
                    message: "the account has no active license".to_owned(),
                }),
            }
        }

        fn heartbeat(&self, _: &str) -> Result<HeartbeatAnswer, LicenseError> {
            let mut script = self.0.lock().unwrap();
            script.heartbeats += 1;
            script.heartbeat.clone().unwrap_or(Ok(HeartbeatAnswer {
                ok: true,
                reason: None,
            }))
        }

        fn refresh(&self, device: &str) -> Result<String, LicenseError> {
            let mut script = self.0.lock().unwrap();
            script.refreshes += 1;
            script.expires_at += 24 * 60 * 60;
            Ok(token_for(device, script.plan.unwrap(), script.expires_at))
        }

        fn revoke(&self, _: &str) -> Result<(), LicenseError> {
            self.0.lock().unwrap().revokes += 1;
            Ok(())
        }
    }

    fn client(dir: Option<PathBuf>, script: &Arc<Mutex<Script>>) -> LicenseClient {
        LicenseClient::load(
            &config(),
            Some(Box::new(FakeBroker(Arc::clone(script)))),
            dir,
            0,
        )
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lumepeer-license-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn pro_script(expires_at: u64) -> Arc<Mutex<Script>> {
        Arc::new(Mutex::new(Script {
            plan: Some(Plan::Pro),
            expires_at,
            ..Script::default()
        }))
    }

    #[test]
    fn a_fresh_install_is_free_and_never_calls_the_broker() {
        let script = pro_script(100_000);
        let mut client = client(None, &script);
        let status = client.tick(1_000);
        assert_eq!(script.lock().unwrap().heartbeats, 0);
        assert_eq!(status.plan, Plan::Free);
        assert!(!status.licensed);
        assert!(!status.offline);
        assert!(status.activation_available);
    }

    #[test]
    fn a_build_without_a_broker_cannot_activate() {
        let mut client = LicenseClient::load(&LicenseConfig::default(), None, None, 0);
        assert_eq!(
            client.activate("good-key", 0),
            Err(LicenseError::NotConfigured)
        );
        let status = client.status(0);
        assert_eq!(status.plan, Plan::Free);
        assert!(!status.activation_available);
    }

    #[test]
    fn activation_survives_a_restart_and_deactivation_returns_to_free() {
        let dir = temp_dir("restart");
        let script = pro_script(100_000);
        let mut first = client(Some(dir.clone()), &script);
        let status = first.activate("good-key", 10).unwrap();
        assert_eq!(status.plan, Plan::Pro);
        assert!(status.licensed);
        assert_eq!(status.seconds_left, Some(100_000 - 10));
        drop(first);

        let mut second = client(Some(dir.clone()), &script);
        assert_eq!(second.status(20).plan, Plan::Pro);
        let status = second.deactivate(30).unwrap();
        assert_eq!(status.plan, Plan::Free);
        assert_eq!(script.lock().unwrap().revokes, 1);
        assert_eq!(
            client(Some(dir.clone()), &script).status(40).plan,
            Plan::Free
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_refused_key_changes_nothing() {
        let script = pro_script(100_000);
        let mut client = client(None, &script);
        let error = client.activate("wrong-key", 0).unwrap_err();
        assert!(
            matches!(error, LicenseError::Rejected { ref code, .. } if code == "NO_ACTIVE_LICENSE")
        );
        assert_eq!(client.status(0).plan, Plan::Free);
    }

    #[test]
    fn a_token_bound_to_another_device_is_refused() {
        let script = pro_script(100_000);
        script.lock().unwrap().foreign_device = true;
        let mut client = client(None, &script);
        assert_eq!(
            client.activate("good-key", 0),
            Err(LicenseError::InvalidToken)
        );
        assert_eq!(client.status(0).plan, Plan::Free);
    }

    #[test]
    fn an_old_token_is_refreshed_on_the_heartbeat() {
        let script = pro_script(REFRESH_BEFORE_SECS + 1_000);
        let mut client = client(None, &script);
        client.activate("good-key", 0).unwrap();
        client.tick(500);
        assert_eq!(script.lock().unwrap().refreshes, 0);
        client.tick(2_000);
        assert_eq!(script.lock().unwrap().refreshes, 1);
        let status = client.status(2_000);
        assert!(status.licensed);
        assert!(status.seconds_left.unwrap() > REFRESH_BEFORE_SECS);
    }

    #[test]
    fn a_lost_seat_drops_to_free_and_says_why() {
        let dir = temp_dir("seat");
        let script = pro_script(100_000);
        let mut client = client(Some(dir.clone()), &script);
        client.activate("good-key", 0).unwrap();
        script.lock().unwrap().heartbeat = Some(Ok(HeartbeatAnswer {
            ok: false,
            reason: Some("another device took this seat".to_owned()),
        }));
        let status = client.tick(100);
        assert_eq!(status.plan, Plan::Free);
        assert_eq!(
            status.reason.as_deref(),
            Some("another device took this seat")
        );
        assert!(!dir.join(TOKEN_FILE).exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_unreachable_broker_keeps_the_plan_inside_the_grace() {
        let script = pro_script(u64::MAX);
        let mut client = client(None, &script);
        client.activate("good-key", 0).unwrap();
        script.lock().unwrap().heartbeat = Some(Err(LicenseError::Unreachable));
        let status = client.tick(100);
        assert_eq!(status.plan, Plan::Pro);
        assert!(status.offline);
        // Past the Pro grace of 7 days the plan lapses to Free, the token stays.
        let status = client.tick(8 * 24 * 60 * 60);
        assert_eq!(status.plan, Plan::Free);
        assert!(status.offline);
        assert!(status.reason.is_some());
    }

    #[test]
    fn the_public_key_parses_from_hex() {
        let hex = data_encoding::HEXLOWER.encode(signing_key().verifying_key().as_bytes());
        assert_eq!(parse_public_key(&hex), Some(signing_key().verifying_key()));
        assert_eq!(parse_public_key("zz"), None);
    }
}
