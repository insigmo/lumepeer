//! This application, hosting the machine's logon screen (ADR 0126).
//!
//! Launched by the helper service with one argument
//! ([`lumepeer_service::LOGON_HOST_ARG`]) into the console session's
//! `WinSta0\Winlogon` desktop, as `LocalSystem`, whenever nobody is signed in
//! there — after a reboot, after a sign-out. It is the whole host, not a part
//! of one: the same runtime the client runs, with the identity, the device
//! password, the address book and the audit log of the account that turned
//! the feature on. So a guest dials the same saved host it always dials, gets
//! in with the device password it remembers, sees the logon screen and types
//! into it.
//!
//! Three things differ from the client, and each is the logon screen's rather
//! than a choice:
//!
//! - **Nobody is there to ask.** [`LogonScreen::attendance`] is always
//!   unattended, so the device password is the only way in (ADR 0085 §2), and
//!   every admission is audited as a login to an empty machine (ADR 0088 §3).
//! - **The screen is the secure desktop.** A keystroke therefore takes
//!   `secure_desktop_input` on top of the ordinary input grant, exactly as on a
//!   UAC prompt (ADR 0061); a full-control role carries it.
//! - **There is no banner.** The only desktop to draw one on is `Winlogon`
//!   (ADR 0088 §3, the known gap). The person who signs in gets the client's
//!   own, because the moment they do the service asks this process to give the
//!   host role up, it exits, and the client takes over.
//!
//! It refuses — with [`LOGON_HOST_EXIT_NOT_ENABLED`] — unless the feature is
//! on, the owner's store already holds an identity (a new one would be a
//! different host from the one every guest saved) and a device password is
//! set: with nobody to answer a dialog, a host without one would admit nobody
//! and only listen.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use lumepeer_core::consent::HostAttendance;
use lumepeer_net::PeerEndpoint;
use lumepeer_net::keystore::{IDENTITY_ENTRY, UNATTENDED_PASSWORD_ENTRY, load_or_create};
use lumepeer_runtime::network::{ActorPolicy, ActorStores, default_capture, spawn_actor_with};
use lumepeer_runtime::view::{ViewSurface, ViewWindows};
use lumepeer_service::host_role::{HostRoleClaim, ReleaseRequests};
use lumepeer_service::{LOGON_HOST_EXIT_NOT_ENABLED, LOGON_HOST_EXIT_ROLE_TAKEN};

use crate::placement::Placement;

/// `LocalSystem`'s SID: the only account this mode runs as.
const LOCAL_SYSTEM_SID: &str = "S-1-5-18";

/// Where the relay measurement of ADR 0098 is kept, in the owner's directory
/// beside the stores it hosts with. Its own file: the client keeps its copy in
/// the profile, which this process has no way to reach.
const RELAY_CACHE_FILE: &str = "relays-logon-host.json";

/// How often the service's request to give the role up is looked for.
const RELEASE_POLL: Duration = Duration::from_millis(250);

/// Exit code of every other refusal: something that should have worked did
/// not, and the service backs off and tries again.
const EXIT_FAILED: u32 = 1;

/// Runs this process as the logon host and never returns.
pub fn run() -> ! {
    // `%ProgramData%\Lumepeer\logs`, beside the services' own files:
    // `LocalSystem` has no profile anybody reads.
    let _ = lumepeer_service::log::init(lumepeer_service::log::LOGON_HOST_LOG_FILE);
    tracing::info!("starting as this machine's logon-screen host (ADR 0126)");
    let code = host();
    tracing::info!(code, "the logon-screen host is leaving");
    std::process::exit(i32::try_from(code).unwrap_or(1));
}

/// Hosts until the service asks for the role back; returns the exit code.
fn host() -> u32 {
    match lumepeer_service::program_data::this_process() {
        Some((sid, _)) if sid == LOCAL_SYSTEM_SID => {}
        _ => {
            tracing::error!(
                "the logon host runs only as LocalSystem, launched by the helper service"
            );
            return EXIT_FAILED;
        }
    }
    let Some(owner) = lumepeer_service::program_data::logon_host_owner() else {
        tracing::info!("hosting the logon screen is turned off");
        return LOGON_HOST_EXIT_NOT_ENABLED;
    };
    let Some(placement) = crate::placement::for_logon_host(&owner) else {
        tracing::warn!("the account that turned this on has no store to host with");
        return LOGON_HOST_EXIT_NOT_ENABLED;
    };
    if !matches!(placement.keystore.load_secret(IDENTITY_ENTRY), Ok(Some(_))) {
        tracing::warn!(
            "the owner's store has no identity yet; a new one would be a host no guest has saved"
        );
        return LOGON_HOST_EXIT_NOT_ENABLED;
    }
    if !matches!(
        placement.keystore.load_secret(UNATTENDED_PASSWORD_ENTRY),
        Ok(Some(_))
    ) {
        tracing::info!(
            "no device password is set, so nobody could be let in at the logon screen; not hosting"
        );
        return LOGON_HOST_EXIT_NOT_ENABLED;
    }

    // The request event first, so a sign-in that lands while this is still
    // starting is not missed; then the role (ADR 0085 §4).
    let releases = ReleaseRequests::create();
    if releases.is_none() {
        tracing::warn!("cannot create the handover event; the service will have to stop this host");
    }
    let role = match lumepeer_service::host_role::claim() {
        HostRoleClaim::Held(role) => role,
        HostRoleClaim::Taken => {
            tracing::info!("something else already hosts this machine; not hosting");
            return LOGON_HOST_EXIT_ROLE_TAKEN;
        }
        HostRoleClaim::Unavailable => {
            tracing::error!("cannot read this machine's host role; not hosting");
            return EXIT_FAILED;
        }
    };

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
    let code = runtime.block_on(serve(placement, releases.as_ref()));
    // Handed back before the process goes, so the log records the moment the
    // client could take it.
    drop(role);
    if let Some(releases) = &releases {
        releases.clear();
    }
    tracing::info!("the host role is free again");
    code
}

/// Binds the owner's identity and hosts until asked to stop.
async fn serve(placement: Placement, releases: Option<&ReleaseRequests>) -> u32 {
    let Placement {
        keystore,
        remembered,
        address_book,
        invite_dir,
        history,
        audit,
    } = placement;
    let secret_key = match load_or_create(keystore.as_ref()) {
        Ok(key) => key,
        Err(error) => {
            tracing::error!(%error, "cannot open the owner's identity; not hosting");
            return EXIT_FAILED;
        }
    };
    let identity = SigningKey::from_bytes(&secret_key.to_bytes());
    // The owner's own settings live in their profile, which a process on the
    // logon screen cannot read; the defaults are the relay and transport
    // preference the client ships with (as ADR 0087 §6 decided for the
    // session-0 host).
    let settings = lumepeer_runtime::config::Settings::default();
    let relay_cache = invite_dir.as_ref().map(|dir| dir.join(RELAY_CACHE_FILE));
    let endpoint =
        match PeerEndpoint::bind_with_lan(secret_key, settings.relay_url(), relay_cache).await {
            Ok(endpoint) => endpoint,
            Err(error) => {
                tracing::error!(%error, "cannot bind the endpoint; not hosting");
                return EXIT_FAILED;
            }
        };
    let audit = crate::bootstrap::open_audit_log(audit, keystore.as_ref()).await;
    let stores = ActorStores {
        history_path: history,
        address_book_path: address_book,
        invite_path: invite_dir.map(|dir| {
            dir.join(lumepeer_runtime::invite_store::file_name(
                &identity.verifying_key(),
            ))
        }),
        keystore,
        remembered_password_keystore: remembered,
        audit,
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
                "endpoint reached a relay; the logon screen is dialable from outside the LAN"
            );
        }
    });

    tracing::info!("hosting the logon screen");
    loop {
        tokio::time::sleep(RELEASE_POLL).await;
        if releases.is_some_and(ReleaseRequests::requested) {
            tracing::info!(
                "asked to give the host role up: somebody signed in, or the client wants it"
            );
            return 0;
        }
    }
}

/// The logon host's screen seam: no windows, nobody present, and a screen
/// that is the secure desktop.
#[derive(Debug)]
struct LogonScreen;

impl ViewWindows for LogonScreen {
    /// The logon host never views another machine, so there is nothing to
    /// open.
    fn open(
        &self,
        label: &str,
        _peer_label: &str,
        _host_label: &str,
        _input: bool,
        _surface: ViewSurface,
    ) {
        tracing::warn!(window = %label, "the logon host has no view windows");
    }

    fn close(&self, _label: &str) {}

    /// No bar: the only desktop to put one on is `Winlogon` (ADR 0088 §3).
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

    /// The logon screen asks nobody, is always the secure desktop, and is an
    /// empty machine — the three answers the actor's admission, input gate and
    /// audit read.
    #[test]
    fn the_logon_screen_is_unattended_secure_and_empty() {
        let screen = LogonScreen;
        assert_eq!(screen.attendance(), HostAttendance::Unattended);
        assert!(screen.on_secure_desktop());
        assert!(screen.nobody_signed_in());
    }
}
