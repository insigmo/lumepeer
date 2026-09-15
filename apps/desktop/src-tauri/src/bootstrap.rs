//! Building the session runtime out of a Tauri application (ADR 0085 §1).
//!
//! `lumepeer-runtime` binds the endpoint, owns the session and serves every
//! peer without knowing what a window is. What it cannot do on its own is
//! answer three questions that only a Tauri application can: where this
//! installation's data directory is, which windows exist, and which
//! `AppHandle` a view window would be opened from. Those three answers are
//! this file, and they are the whole of the Tauri surface the runtime needs.
//!
//! A session-0 host (ADR 0085) supplies the same three answers from
//! `lumepeer_service::machine_store` instead, which is the point of them being
//! here rather than inside the actor.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use ed25519_dalek::SigningKey;
use lumepeer_net::NetError;
use lumepeer_net::PeerEndpoint;
use lumepeer_net::keystore::{Keystore, load_or_create};
use lumepeer_runtime::network::{
    ActorHandle, ActorPolicy, ActorStores, address_book_path, default_capture, invite_path,
    spawn_actor_with,
};

/// Binds the endpoint from the OS keystore identity and spawns the actor.
///
/// Reaching a relay is **not** awaited here: on a LAN-only machine that wait
/// never finishes, and `main` blocks on this call before Tauri creates a
/// window, so blocking it would leave the app with no window and no error at
/// all. Ticket pairing does not need a relay (§7), so the wait runs in the
/// background and only logs.
///
/// # Errors
/// [`NetError`] if the keystore or the endpoint bind fails — surfaced as a
/// startup failure rather than silently degrading (§11.2, §24.5).
/// Opens the keystore, honouring the `LUMEPEER_KEYSTORE=file` override.
///
/// Default is the OS-native backend (`crates/net::keystore::open`). The
/// override selects the encrypted-file store — the documented fallback for
/// headless environments (CI, SSH-run E2E) where no secret-service prompter
/// exists to unlock the keyring. `LUMEPEER_KEYSTORE_PATH` chooses the file
/// location; it defaults to the app data directory.
///
/// # Errors
/// [`NetError`] as [`keystore::open`], or when `LUMEPEER_KEYSTORE=file` is
/// set but no usable path can be derived.
fn open_keystore() -> Result<Box<dyn lumepeer_net::keystore::Keystore>, NetError> {
    const KEYSTORE_ENV: &str = "LUMEPEER_KEYSTORE";
    if std::env::var(KEYSTORE_ENV).as_deref() != Ok("file") {
        return lumepeer_net::keystore::open();
    }
    let path = std::env::var("LUMEPEER_KEYSTORE_PATH").map_err(|_| {
        NetError::Keystore("LUMEPEER_KEYSTORE=file also needs LUMEPEER_KEYSTORE_PATH".to_owned())
    })?;
    let path = std::path::PathBuf::from(path);
    tracing::info!(path = %path.display(), "using the encrypted-file keystore (LUMEPEER_KEYSTORE=file)");
    // The user secret mixes the machine id with the user name: stable for
    // this user on this machine, never written anywhere (§11.2).
    let machine = machine_id();
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default();
    Ok(Box::new(lumepeer_net::keystore::FileKeystore::new(
        path,
        format!("{machine}:{user}").as_bytes(),
    )))
}

/// Reads `/etc/machine-id` (or the fallback `DBUS` path) for the file-keystore
/// user secret. A missing file is not fatal: an empty id only weakens the
/// secret to the user name, matching the fallback's documented threat model.
fn machine_id() -> String {
    for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(id) = std::fs::read_to_string(path) {
            return id.trim().to_owned();
        }
    }
    String::new()
}

pub async fn spawn_actor(
    app: tauri::AppHandle,
    settings: &lumepeer_runtime::config::Settings,
    policy: ActorPolicy,
) -> Result<ActorHandle, NetError> {
    let store = open_keystore()?;
    let secret_key = load_or_create(store.as_ref())?;
    let identity = SigningKey::from_bytes(&secret_key.to_bytes());
    let relay = settings.relay_url();
    // Relay-only is a WAN test, never the default: with the IP transports
    // cleared every session lives or dies with one relay link, and a client
    // whose relay flaps cannot connect at all (ADR 0026).
    let endpoint = if settings.relay_only() {
        tracing::info!(
            "transport: relay only — direct IP paths are off, so every session goes over the internet"
        );
        PeerEndpoint::bind_relay_only(secret_key, relay).await?
    } else {
        tracing::info!("transport: direct IP paths preferred, relay as the fallback");
        PeerEndpoint::bind_with_lan(secret_key, relay).await?
    };
    let audit = open_audit_log(&app, store.as_ref()).await;
    // A second, independent handle on the same keystore: every native backend
    // opens its own connection per operation rather than holding one open
    // (see e.g. `SecretServiceKeystore`'s own doc comment), so this costs
    // nothing beyond what `UnattendedStore` below already pays, and it is
    // what lets the two stores own their `Box<dyn Keystore>` outright instead
    // of sharing one behind an `Arc` (docs/bugs/02-connect-form.md, task 6).
    let remembered_password_keystore = open_keystore()?;
    let stores = ActorStores {
        history_path: connection_history_path(&app),
        address_book_path: address_book_path(),
        invite_path: invite_path(),
        // The same keystore the identity came from: the unattended password
        // hash and TOTP secret are secret material and `CLAUDE.md` keeps
        // secrets out of `config/*.toml` (§11.2; ADR 0033).
        keystore: store,
        remembered_password_keystore,
        audit,
    };

    let handle = spawn_actor_with(
        endpoint.clone(),
        identity,
        Arc::new(crate::view_windows::TauriViewWindows::new(app)),
        default_capture(),
        lumepeer_runtime::clipboard_os::platform_clipboard(),
        stores,
        policy,
    );

    tokio::spawn({
        let online = handle.online_flag();
        async move {
            endpoint.online().await;
            online.store(true, Ordering::Relaxed);
            tracing::info!("endpoint reached a relay; invites are dialable from outside the LAN");
        }
    });

    Ok(handle)
}

/// Opens the audit log and starts its daily retention sweep (§15; ADR 0041).
///
/// Every failure here is a warning and a `None`, never a refusal to start: §18
/// says a storage fault degrades the feature that needs the storage. A host
/// that cannot write an audit trail is still a host, and refusing to run would
/// hand anyone who can break the database a way to take the machine offline.
///
/// The one failure worth its own message is a lost install salt over a
/// non-empty log: minting a new one would silently split every peer's history
/// in two, so the log is left untouched and unwritten instead.
async fn open_audit_log(
    app: &tauri::AppHandle,
    keystore: &dyn Keystore,
) -> Option<lumepeer_runtime::audit_store::AuditStore> {
    use tauri::Manager as _;

    let path = match app.path().app_local_data_dir() {
        Ok(dir) => dir.join("audit.db"),
        Err(error) => {
            tracing::warn!(%error, "cannot resolve the app data directory; no audit log this run");
            return None;
        }
    };
    let store = match lumepeer_runtime::audit_store::AuditStore::open(path, keystore).await {
        Ok(store) => store,
        Err(error) => {
            tracing::warn!(%error, "audit log unavailable; the host runs without an audit trail");
            return None;
        }
    };

    // Once a day, not once per record: the sweep is a table scan and the
    // cutoff moves by seconds. `AuditStore::open` already swept once.
    let daily = store.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(
            lumepeer_core::constants::AUDIT_RETENTION_SWEEP_SECS,
        ));
        ticker.tick().await; // fires immediately; the open already pruned
        loop {
            ticker.tick().await;
            if let Err(error) = daily.prune().await {
                tracing::warn!(%error, "audit log: retention sweep failed");
            }
        }
    });
    Some(store)
}

/// Where the connection history file lives, if the app data directory can be
/// resolved at all. `None` degrades the feature to in-memory-only for this
/// run rather than failing startup over a convenience list (§18).
fn connection_history_path(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    use tauri::Manager as _;
    match app.path().app_local_data_dir() {
        Ok(dir) => Some(dir.join("connection_history.json")),
        Err(error) => {
            tracing::warn!(%error, "cannot resolve the app data directory; connection history will not persist");
            None
        }
    }
}
