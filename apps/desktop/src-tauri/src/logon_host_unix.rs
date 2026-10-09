//! This application hosting the machine's sign-in screen on Linux and macOS
//! (ADR 0151), the counterpart of [`crate::logon_host`] on Windows.
//!
//! Launched with [`lumepeer_service::LOGON_HOST_ARG`] by whatever keeps the
//! sign-in screen hosted on this platform — on Linux the supervisor
//! (`lumepeer-service --logon-supervisor`), on macOS a `LoginWindow` launchd
//! agent — while nobody is signed in there. It runs as `root`, in the
//! environment of the sign-in screen's own display, and it is the whole host:
//! the same runtime the client runs, with the identity, the device password,
//! the role and the audit salt of the account that turned the feature on. So
//! a guest dials the same saved host it always dials and gets in with the
//! device password it remembers.
//!
//! Where those credentials come from is the one platform difference. On
//! Windows they are in a directory only administrators can open (ADR 0126);
//! here the keyring that holds them is locked until the owner signs in — which
//! is exactly when the sign-in screen is gone. So the owner's client hands a
//! copy to the keeper while its keyring is open, and this process is given
//! that copy, never the live keyring:
//!
//! - **Linux:** the supervisor's `root`-only state directory
//!   ([`lumepeer_service::logon_enroll::STATE_DIR`]), which it wrote from the
//!   client's enrolment.
//! - **macOS:** the owner's file keystore under their data directory
//!   (ADR 0128), handed over through the launchd agent's configuration.
//!
//! It refuses to run — exit [`lumepeer_service::LOGON_HOST_EXIT_NOT_ENABLED`]
//! — unless it was given an identity and a device password, since with nobody
//! to answer a dialog a host without one would admit nobody.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use lumepeer_core::consent::HostAttendance;
use lumepeer_net::PeerEndpoint;
use lumepeer_net::keystore::{
    AUDIT_SALT_ENTRY, IDENTITY_ENTRY, Keystore, MemoryKeystore, UNATTENDED_PASSWORD_ENTRY,
    UNATTENDED_ROLE_ENTRY, UNATTENDED_TOTP_ENTRY, load_or_create,
};
use lumepeer_runtime::network::{ActorPolicy, ActorStores, default_capture, spawn_actor_with};
use lumepeer_runtime::view::{ViewSurface, ViewWindows};

/// Exit code of every refusal that should simply be tried again later.
const EXIT_FAILED: u32 = 1;

/// How often the "please give the role up" flag is looked at. The keeper sets
/// it, or stops the process outright, the moment somebody signs in.
const STOP_POLL: Duration = Duration::from_millis(250);

/// Runs this process as the sign-in screen's host and never returns.
pub fn run() -> ! {
    let _ = lumepeer_service::log::init(lumepeer_service::log::LOGON_HOST_LOG_FILE);
    tracing::info!("starting as this machine's sign-in screen host (ADR 0151)");
    let code = host();
    tracing::info!(code, "the sign-in screen host is leaving");
    std::process::exit(i32::try_from(code).unwrap_or(1));
}

/// The entries a host needs, copied into an in-memory keystore from wherever
/// this platform keeps the owner's copy.
fn owner_keystore() -> Option<MemoryKeystore> {
    #[cfg(target_os = "linux")]
    {
        linux::load()
    }
    #[cfg(target_os = "macos")]
    {
        macos::load()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

fn host() -> u32 {
    if !is_root() {
        tracing::error!("the sign-in screen host runs as root, started by the system keeper");
        return EXIT_FAILED;
    }
    let Some(keystore) = owner_keystore() else {
        tracing::info!("no enrolled identity for the sign-in screen; not hosting");
        return lumepeer_service::LOGON_HOST_EXIT_NOT_ENABLED;
    };
    if !matches!(keystore.load_secret(IDENTITY_ENTRY), Ok(Some(_))) {
        tracing::warn!(
            "the enrolled copy has no identity; a new one would be a host no guest saved"
        );
        return lumepeer_service::LOGON_HOST_EXIT_NOT_ENABLED;
    }
    if !matches!(keystore.load_secret(UNATTENDED_PASSWORD_ENTRY), Ok(Some(_))) {
        tracing::info!(
            "no device password in the enrolled copy; nobody could be let in; not hosting"
        );
        return lumepeer_service::LOGON_HOST_EXIT_NOT_ENABLED;
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(%error, "cannot start the async runtime");
            return EXIT_FAILED;
        }
    };
    runtime.block_on(serve(keystore))
}

async fn serve(keystore: MemoryKeystore) -> u32 {
    let secret_key = match load_or_create(&keystore) {
        Ok(key) => key,
        Err(error) => {
            tracing::error!(%error, "cannot open the enrolled identity; not hosting");
            return EXIT_FAILED;
        }
    };
    let identity = SigningKey::from_bytes(&secret_key.to_bytes());
    // The owner's own relay and transport choices live in their profile, which
    // a process on the sign-in screen cannot read; the defaults are what the
    // client ships with, as ADR 0087 §6 decided for the session-0 host.
    let settings = lumepeer_runtime::config::Settings::default();
    let endpoint = match PeerEndpoint::bind_with_lan(secret_key, settings.relay_url(), None).await {
        Ok(endpoint) => endpoint,
        Err(error) => {
            tracing::error!(%error, "cannot bind the endpoint; not hosting");
            return EXIT_FAILED;
        }
    };

    // Everything is in memory and lives only as long as this screen: no
    // address book file, no invite file, no audit database. The identity and
    // the device password are the enrolled copy; a second keystore handle is
    // the same copy, since nothing here writes to it.
    let stores = ActorStores {
        history_path: None,
        address_book_path: None,
        invite_path: None,
        keystore: Box::new(clone_keystore(&keystore)),
        remembered_password_keystore: Box::new(clone_keystore(&keystore)),
        audit: None,
    };

    let handle = spawn_actor_with(
        endpoint.clone(),
        identity,
        Arc::new(LogonScreen),
        default_capture(),
        lumepeer_runtime::clipboard_os::platform_clipboard(),
        stores,
        ActorPolicy::hosting(settings.obfuscated()),
    );

    tokio::spawn({
        let online = handle.online_flag();
        async move {
            endpoint.online().await;
            online.store(true, Ordering::Relaxed);
            tracing::info!(
                "endpoint reached a relay; the sign-in screen is dialable from outside the LAN"
            );
        }
    });

    tracing::info!("hosting the sign-in screen");
    loop {
        tokio::time::sleep(STOP_POLL).await;
        // The keeper stops this process outright when somebody signs in
        // (Linux: the supervisor's `SIGTERM`; macOS: launchd unloading the
        // `LoginWindow` agent). There is no in-band release flag to watch, so
        // this loop only keeps the process alive and lets the signal end it.
    }
}

/// Copies the live entries of `keystore` into a fresh one, so the two actor
/// store handles each own theirs rather than sharing one behind an `Arc`.
fn clone_keystore(keystore: &MemoryKeystore) -> MemoryKeystore {
    let copy = MemoryKeystore::new();
    for entry in [
        IDENTITY_ENTRY,
        UNATTENDED_PASSWORD_ENTRY,
        UNATTENDED_ROLE_ENTRY,
        UNATTENDED_TOTP_ENTRY,
        AUDIT_SALT_ENTRY,
    ] {
        if let Ok(Some(bytes)) = keystore.load_secret(entry) {
            let _ = copy.store_secret(entry, &bytes);
        }
    }
    copy
}

fn is_root() -> bool {
    #[cfg(unix)]
    {
        rustix::process::geteuid().is_root()
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use lumepeer_net::keystore::{Keystore as _, MemoryKeystore};
    use lumepeer_service::logon_enroll::{SECRETS_FILE, STATE_DIR, decode_entries};

    /// Loads the owner's copy the supervisor wrote, or `None` when none was
    /// enrolled or it cannot be read (this process runs as `root`, and the
    /// file is `root`-only, so a read failure is a real fault, not a
    /// permission one).
    pub(super) fn load() -> Option<MemoryKeystore> {
        let path = std::path::Path::new(STATE_DIR).join(SECRETS_FILE);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(error) => {
                tracing::error!(path = %path.display(), %error, "cannot read the enrolled copy");
                return None;
            }
        };
        let entries = match decode_entries(&bytes) {
            Ok(entries) => entries,
            Err(error) => {
                tracing::error!(%error, "the enrolled copy is malformed");
                return None;
            }
        };
        let keystore = MemoryKeystore::new();
        for (name, value) in entries {
            if keystore.store_secret(&name, &value).is_err() {
                return None;
            }
        }
        Some(keystore)
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use lumepeer_net::keystore::{
        AUDIT_SALT_ENTRY, FileKeystore, IDENTITY_ENTRY, Keystore as _, MemoryKeystore,
        UNATTENDED_PASSWORD_ENTRY, UNATTENDED_ROLE_ENTRY, UNATTENDED_TOTP_ENTRY,
    };

    /// `LUMEPEER_LOGON_KEYSTORE` is the owner's keystore directory (their
    /// data directory's `keystore`, ADR 0128), which the `LoginWindow` agent
    /// passes in. Read through the same [`FileKeystore`] the client opens it
    /// with, keyed by the secret the owner's own run minted beside it.
    pub(super) fn load() -> Option<MemoryKeystore> {
        let directory =
            std::env::var_os("LUMEPEER_LOGON_KEYSTORE").map(std::path::PathBuf::from)?;
        let secret = match std::fs::read(directory.join("keystore.secret")) {
            Ok(secret) => secret,
            Err(error) => {
                tracing::error!(path = %directory.display(), %error, "cannot read the owner's keystore secret");
                return None;
            }
        };
        let file = FileKeystore::new(directory.join("identity.key"), &secret);
        let keystore = MemoryKeystore::new();
        for entry in [
            IDENTITY_ENTRY,
            UNATTENDED_PASSWORD_ENTRY,
            UNATTENDED_ROLE_ENTRY,
            UNATTENDED_TOTP_ENTRY,
            AUDIT_SALT_ENTRY,
        ] {
            match file.load_secret(entry) {
                Ok(Some(bytes)) => {
                    if keystore.store_secret(entry, &bytes).is_err() {
                        return None;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::error!(entry, %error, "cannot read an entry of the owner's keystore");
                    return None;
                }
            }
        }
        Some(keystore)
    }
}

/// The sign-in screen's seam: no view windows, nobody present, and a screen
/// that is the secure desktop — the three answers the actor's admission, input
/// gate and audit read (the same as [`crate::logon_host`]'s on Windows).
#[derive(Debug)]
struct LogonScreen;

impl ViewWindows for LogonScreen {
    fn open(
        &self,
        label: &str,
        _peer_label: &str,
        _host_label: &str,
        _input: bool,
        _surface: ViewSurface,
    ) {
        tracing::warn!(window = %label, "the sign-in screen host has no view windows");
    }

    fn close(&self, _label: &str) {}

    fn set_host_bar(&self, _visible: bool) {}

    fn attendance(&self) -> HostAttendance {
        HostAttendance::Unattended
    }

    fn on_secure_desktop(&self) -> bool {
        true
    }

    fn nobody_signed_in(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sign_in_screen_is_unattended_secure_and_empty() {
        let screen = LogonScreen;
        assert_eq!(screen.attendance(), HostAttendance::Unattended);
        assert!(screen.on_secure_desktop());
        assert!(screen.nobody_signed_in());
    }

    #[test]
    fn a_keystore_is_cloned_entry_for_entry() {
        let original = MemoryKeystore::new();
        original.store_secret(IDENTITY_ENTRY, b"id").unwrap();
        original
            .store_secret(UNATTENDED_PASSWORD_ENTRY, b"pw")
            .unwrap();
        let copy = clone_keystore(&original);
        assert_eq!(
            copy.load_secret(IDENTITY_ENTRY).unwrap().as_deref(),
            Some(&b"id"[..])
        );
        assert_eq!(
            copy.load_secret(UNATTENDED_PASSWORD_ENTRY)
                .unwrap()
                .as_deref(),
            Some(&b"pw"[..])
        );
        assert_eq!(copy.load_secret(UNATTENDED_TOTP_ENTRY).unwrap(), None);
    }
}
