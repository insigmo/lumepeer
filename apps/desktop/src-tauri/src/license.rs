//! The license the actor admits sessions under (§12; ADR 0163).
//!
//! Owns a [`LicenseClient`], keeps a snapshot of its status for the UI to
//! read without waiting on the broker, and tells the actor which plan is in
//! force every time that may have changed.

use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use lumepeer_core::constants::HEARTBEAT_INTERVAL_SECS;
use lumepeer_runtime::license_client::{
    HttpBroker, LicenseClient, LicenseConfig, LicenseError, LicenseStatus,
};
use lumepeer_runtime::network::ActorHandle;

/// The client behind a lock, and what it last reported.
#[derive(Debug)]
pub struct License {
    client: Mutex<LicenseClient>,
    snapshot: RwLock<LicenseStatus>,
}

impl License {
    /// Loads the cached token from the app's data directory, if it can be
    /// resolved; otherwise the install runs on Free for this run.
    pub fn load(app: &tauri::AppHandle) -> Arc<Self> {
        use tauri::Manager as _;

        let dir = match app.path().app_local_data_dir() {
            Ok(dir) => Some(dir.join("license")),
            Err(error) => {
                tracing::warn!(%error, "cannot resolve the app data directory; a license will not persist");
                None
            }
        };
        let config = LicenseConfig::from_env();
        if !config.is_configured() {
            tracing::info!("licensing is not configured in this build; running on the free plan");
        }
        let broker = config
            .broker_url
            .clone()
            .map(|url| Box::new(HttpBroker::new(url)) as Box<_>);
        let mut client = LicenseClient::load(&config, broker, dir, now_secs());
        let snapshot = client.status(now_secs());
        Arc::new(Self {
            client: Mutex::new(client),
            snapshot: RwLock::new(snapshot),
        })
    }

    /// What the license last reported. Never waits on the broker.
    pub fn snapshot(&self) -> LicenseStatus {
        self.snapshot
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Activates `account_key`. Blocks on the broker.
    ///
    /// # Errors
    /// Whatever [`LicenseClient::activate`] says.
    pub fn activate(&self, account_key: &str) -> Result<LicenseStatus, LicenseError> {
        self.with_client(|client, now| client.activate(account_key, now))
    }

    /// Returns this device to Free. Blocks on the broker.
    ///
    /// # Errors
    /// Whatever [`LicenseClient::deactivate`] says.
    pub fn deactivate(&self) -> Result<LicenseStatus, LicenseError> {
        self.with_client(LicenseClient::deactivate)
    }

    /// One heartbeat round. Blocks on the broker.
    fn tick(&self) -> LicenseStatus {
        self.with_client(|client, now| Ok::<_, LicenseError>(client.tick(now)))
            .unwrap_or_else(|_| self.snapshot())
    }

    fn with_client<T>(
        &self,
        f: impl FnOnce(&mut LicenseClient, u64) -> Result<T, LicenseError>,
    ) -> Result<T, LicenseError> {
        let mut client = self.client.lock().unwrap_or_else(PoisonError::into_inner);
        let now = now_secs();
        let result = f(&mut client, now);
        *self
            .snapshot
            .write()
            .unwrap_or_else(PoisonError::into_inner) = client.status(now);
        result
    }
}

/// Tells the actor the plan now, then keeps the license alive on the
/// heartbeat interval of §12.2 for as long as the runtime runs.
pub fn spawn_heartbeat(
    runtime: &tokio::runtime::Runtime,
    license: Arc<License>,
    network: ActorHandle,
) {
    runtime.spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(HEARTBEAT_INTERVAL_SECS));
        loop {
            interval.tick().await;
            let round = Arc::clone(&license);
            let status = match tokio::task::spawn_blocking(move || round.tick()).await {
                Ok(status) => status,
                Err(error) => {
                    tracing::warn!(%error, "license heartbeat task failed");
                    license.snapshot()
                }
            };
            if network.set_plan(status.plan).await.is_err() {
                return;
            }
        }
    });
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}
