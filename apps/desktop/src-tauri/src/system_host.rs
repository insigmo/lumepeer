//! What the two `LocalSystem` hosts of this binary share: the logon host on
//! `Winlogon` (ADR 0126) and the session host on the signed-in desktop
//! (ADR 0169).
//!
//! Each is the client's runtime run headless by the helper service, with the
//! stores of one account under `%ProgramData%\Lumepeer\users\<SID>` — the ones
//! that account's elevated client fills — so a guest reaches the identity it
//! saved whichever of them is up. They differ in whose stores, in the screen
//! seam the actor asks its questions of, and in what leaving with a guest
//! means. Everything else — refusing to run as anybody but `LocalSystem`,
//! taking the host role, binding the endpoint, spawning the actor and leaving
//! when the service asks — is here, once (ADR 0085 §1: one implementation of
//! "who may do what").

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use ed25519_dalek::SigningKey;
use lumepeer_net::PeerEndpoint;
use lumepeer_net::keystore::load_or_create;
use lumepeer_runtime::network::{
    ActorHandle, ActorPolicy, ActorStores, default_capture, spawn_actor_with,
};
use lumepeer_runtime::view::ViewWindows;
use lumepeer_service::host_role::{HostRole, HostRoleClaim, ReleaseRequests};

use crate::placement::Placement;

/// `LocalSystem`'s SID: the only account either host runs as.
const LOCAL_SYSTEM_SID: &str = "S-1-5-18";

/// Exit code of every refusal that is not one of a host's own: something that
/// should have worked did not, and the service backs off and tries again.
pub const EXIT_FAILED: u32 = 1;

/// How often the service's request to give the role up is looked for.
const RELEASE_POLL: Duration = Duration::from_millis(250);

/// How long a starting host waits for the host role to come free before it
/// reports it taken.
///
/// The two hosts hand the console to each other at every sign-in and
/// sign-out, and each supervisor starts its own as soon as the console says
/// so — while the other is still closing its sessions. Longer than the
/// service's own wait for a host to leave (`host_process::HOST_STOP_TIMEOUT_MS`),
/// which terminates it at the end of that, so a host that started early still
/// gets the role rather than parking the console for minutes.
const ROLE_WAIT: Duration = Duration::from_secs(8);

/// How often the role is asked for again within [`ROLE_WAIT`].
const ROLE_POLL: Duration = Duration::from_millis(200);

/// Opens `log_file` under `%ProgramData%\Lumepeer\logs`, runs `host` and
/// exits with what it returns; never returns.
pub fn run(log_file: &str, starting: &str, host: impl FnOnce() -> u32) -> ! {
    // Beside the services' own files: `LocalSystem` has no profile anybody
    // reads.
    let _ = lumepeer_service::log::init(log_file);
    tracing::info!("{starting}");
    let code = host();
    tracing::info!(code, "the host is leaving");
    std::process::exit(i32::try_from(code).unwrap_or(1));
}

/// Whether this process runs as `LocalSystem`; says why when it does not.
pub fn is_local_system() -> bool {
    let system = matches!(
        lumepeer_service::program_data::this_process(),
        Some((sid, _)) if sid == LOCAL_SYSTEM_SID
    );
    if !system {
        tracing::error!("this host runs only as LocalSystem, launched by the helper service");
    }
    system
}

/// One host's answers to what [`host`] cannot decide for it.
pub struct Hosting {
    /// The stores of the account it hosts as.
    pub placement: Placement,
    /// The screen seam the actor asks who is present, and where input goes.
    pub screen: Arc<dyn ViewWindows>,
    /// The relay measurement's file name, in the account's directory beside
    /// its stores: the client keeps its own copy in the profile, which this
    /// process does not read.
    pub relay_cache_file: &'static str,
    /// What this host serves, for the log: "the logon screen".
    pub serving: &'static str,
    /// The exit code that says the host role was held by somebody else.
    pub role_taken: u32,
    /// The exit code of a host that leaves with this many guests on it.
    pub leaving: fn(usize) -> u32,
}

/// Takes the host role and hosts until the service asks for it back; returns
/// the exit code.
pub fn host(hosting: Hosting) -> u32 {
    // The request event first, so a sign-in that lands while this is still
    // starting is not missed; then the role (ADR 0085 §4).
    let releases = ReleaseRequests::create();
    if releases.is_none() {
        tracing::warn!("cannot create the handover event; the service will have to stop this host");
    }
    let role = match claim_role(hosting.role_taken) {
        Ok(role) => role,
        Err(code) => return code,
    };
    let Some(runtime) = runtime() else {
        return EXIT_FAILED;
    };
    let code = runtime.block_on(async {
        let leaving = hosting.leaving;
        let Some(handle) = open(hosting).await else {
            return EXIT_FAILED;
        };
        loop {
            tokio::time::sleep(RELEASE_POLL).await;
            if released(releases.as_ref()) {
                return hand_over(&handle, leaving).await;
            }
        }
    });
    give_back(role, releases.as_ref());
    code
}

/// The host role, or the exit code to leave with when it cannot be had.
///
/// # Errors
/// `role_taken` when somebody else held it for longer than [`ROLE_WAIT`],
/// [`EXIT_FAILED`] when the question could not be put.
pub fn claim_role(role_taken: u32) -> Result<HostRole, u32> {
    match claim_within(ROLE_WAIT) {
        HostRoleClaim::Held(role) => Ok(role),
        HostRoleClaim::Taken => {
            tracing::info!("something else already hosts this machine; not hosting");
            Err(role_taken)
        }
        HostRoleClaim::Unavailable => {
            tracing::error!("cannot read this machine's host role; not hosting");
            Err(EXIT_FAILED)
        }
    }
}

/// Hands the role back and takes the service's request down, in that order:
/// clearing first would let a requester see no request while the role had not
/// moved.
pub fn give_back(role: HostRole, releases: Option<&ReleaseRequests>) {
    // Handed back before the process goes, so the log records the moment the
    // next host could take it.
    drop(role);
    if let Some(releases) = releases {
        releases.clear();
    }
    tracing::info!("the host role is free again");
}

/// The async runtime a host runs on; `None`, logged, when it cannot start.
pub fn runtime() -> Option<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .inspect_err(|error| tracing::error!(%error, "cannot start the async runtime"))
        .ok()
}

/// Whether the service has asked for the role back.
pub fn released(releases: Option<&ReleaseRequests>) -> bool {
    let asked = releases.is_some_and(ReleaseRequests::requested);
    if asked {
        tracing::info!("asked to give the host role up");
    }
    asked
}

/// The host role, waited for up to `wait` while somebody else holds it.
///
/// Waited for, never asked for: the holder is either the other host, which
/// the service is already stopping, or a client, which the person at the
/// machine started and which this host does not knock off (ADR 0085 §4).
fn claim_within(wait: Duration) -> HostRoleClaim {
    let deadline = Instant::now() + wait;
    loop {
        let claim = lumepeer_service::host_role::claim();
        if !matches!(claim, HostRoleClaim::Taken) || Instant::now() >= deadline {
            return claim;
        }
        std::thread::sleep(ROLE_POLL);
    }
}

/// Binds the account's identity and starts serving it; the actor's handle, or
/// `None`, logged, when the endpoint cannot be had.
pub async fn open(hosting: Hosting) -> Option<ActorHandle> {
    let Hosting {
        placement,
        screen,
        relay_cache_file,
        serving,
        ..
    } = hosting;
    let Placement {
        keystore,
        remembered,
        address_book,
        invite_dir,
        history,
        audit,
    } = placement;
    let secret_key = load_or_create(keystore.as_ref())
        .inspect_err(
            |error| tracing::error!(%error, "cannot open the account's identity; not hosting"),
        )
        .ok()?;
    let identity = SigningKey::from_bytes(&secret_key.to_bytes());
    // The account's own settings live in its profile, which is the account's
    // to write and therefore nothing a `LocalSystem` host decides by; the
    // defaults are the relay and transport preference the client ships with
    // (as ADR 0087 §6 decided for the session-0 host).
    let settings = lumepeer_runtime::config::Settings::default();
    let relay_cache = invite_dir.as_ref().map(|dir| dir.join(relay_cache_file));
    let endpoint = PeerEndpoint::bind_with_lan(secret_key, settings.relay_url(), relay_cache)
        .await
        .inspect_err(|error| tracing::error!(%error, "cannot bind the endpoint; not hosting"))
        .ok()?;
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
        screen,
        default_capture(None),
        lumepeer_runtime::clipboard_os::platform_clipboard(),
        stores,
        ActorPolicy::hosting(settings.obfuscated()),
    );
    tokio::spawn({
        let online = handle.online_flag();
        async move {
            endpoint.online().await;
            online.store(true, Ordering::Relaxed);
            tracing::info!("endpoint reached a relay; {serving} is dialable from outside the LAN");
        }
    });
    tracing::info!("hosting {serving}");
    Some(handle)
}

/// Leaves the guests to whichever host comes next (ADR 0164): each is told,
/// in the close of its connection, to dial a new session at once rather than
/// wait out a dead link and resume one the next host never heard of
/// (ADR 0148).
pub async fn hand_over(handle: &ActorHandle, leaving: fn(usize) -> u32) -> u32 {
    let guests = handle.status().await.map_or(0, |sessions| sessions.len());
    if let Err(error) = handle.announce_restart().await {
        tracing::warn!(?error, "cannot tell the guests to dial again");
    }
    tracing::info!(guests, "handing the machine over");
    leaving(guests)
}
