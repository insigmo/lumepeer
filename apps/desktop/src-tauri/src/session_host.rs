//! This application, hosting the signed-in console session (ADR 0169 §1).
//!
//! Launched by the helper service with one argument
//! ([`lumepeer_service::SESSION_HOST_ARG`]) into the console session's
//! `WinSta0\Default` desktop, as `LocalSystem`, while somebody is signed in
//! there. It is the logon host's other half: the same headless runtime
//! ([`crate::system_host`]), with the stores of the person signed in — the
//! directory under `%ProgramData%\Lumepeer\users\<SID>` their elevated client
//! fills (ADR 0123) — so the identity every guest saved is the one it serves,
//! and nothing moves.
//!
//! What its position buys, measured before any of this was written
//! (`docs/research/session-host-probe.md`): DXGI capture of the desktop and
//! WASAPI loopback work from here, and its own `SendInput` reaches elevated
//! windows and a `VMware` guest the way the ADR 0114 injector's does — so it
//! injects in-process ([`SignedInDesktop::injects_as_system`]). The secure
//! desktop is still the helper's: its pipe admits `SY`, and the actor already
//! asks it when its capture finds a UAC prompt.
//!
//! Its life has two halves (ADR 0169 §1):
//!
//! - **Closed.** It holds the host role and the person's channel
//!   ([`crate::host_panel`]) and binds no endpoint: nothing can dial it.
//! - **Open**, once a client of the person's is attached to the channel or
//!   "accept connections after sign-in" is on in their store. It stays open
//!   while either holds or a session runs; when none does it leaves with
//!   [`SESSION_HOST_EXIT_CLOSED`], and the service starts the next one closed.
//!
//! While open, a session nobody here can see does not go on: with no client
//! attached, it starts the person's client, and if none attaches within
//! [`SESSION_INDICATOR_DEADLINE_SECS`] it ends every session (§7).
//!
//! The person at the machine is asked in this process's own dialog
//! ([`crate::session_host_consent`], ADR 0169 §5): a guest without the device
//! password, and every widening of a running session, wait for their answer
//! in a window no program of theirs can press.
//!
//! That password, the rest of the access policy and the trusted devices are
//! written only by the elevated `--access-settings` window
//! ([`crate::access_settings`]); when it raises the person's policy-changed
//! event, this host reads them again without restarting, and the next
//! admission is decided by what was written (ADR 0169 §5).
//!
//! It refuses — [`SESSION_HOST_EXIT_NOT_ENABLED`] — until an administrator has
//! turned ADR 0169 on for this machine, and — [`SESSION_HOST_EXIT_NO_STORE`]
//! — for a person whose store holds no identity: they never opened Lumepeer,
//! and a new identity would be a host no guest has saved.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lumepeer_core::consent::HostAttendance;
use lumepeer_core::constants::SESSION_INDICATOR_DEADLINE_SECS;
use lumepeer_net::keystore::IDENTITY_ENTRY;
use lumepeer_runtime::network::{ActorHandle, SessionStateDto};
use lumepeer_runtime::view::{ViewSurface, ViewWindows};
use lumepeer_service::access_settings::PolicyChanges;
use lumepeer_service::host_role::ReleaseRequests;
use lumepeer_service::{
    SESSION_HOST_EXIT_CLOSED, SESSION_HOST_EXIT_NO_STORE, SESSION_HOST_EXIT_NOT_ENABLED,
    SESSION_HOST_EXIT_ROLE_TAKEN,
};

use crate::host_panel::Served;
use crate::placement::Placement;
use crate::session_host_consent::Consents;
use crate::system_host::{self, EXIT_FAILED, Hosting};

/// Where the relay measurement of ADR 0098 is kept, in the person's directory
/// beside the stores it hosts with — a file of its own, so the logon host's
/// and this one's do not overwrite each other at every sign-in.
const RELAY_CACHE_FILE: &str = "relays-session-host.json";

/// How often the host looks at its sessions, its channel and the service's
/// request to leave.
const TICK: Duration = Duration::from_millis(250);

/// Runs this process as the session host and never returns.
pub fn run() -> ! {
    system_host::run(
        lumepeer_service::log::SESSION_HOST_LOG_FILE,
        "starting as this machine's host on the signed-in desktop (ADR 0169)",
        host,
    )
}

/// Hosts until the service asks for the role back or the endpoint closes;
/// returns the exit code.
fn host() -> u32 {
    if !system_host::is_local_system() {
        return EXIT_FAILED;
    }
    if !lumepeer_service::program_data::session_host_enabled() {
        tracing::info!(
            "the session host is not turned on for this machine; the client hosts as before"
        );
        return SESSION_HOST_EXIT_NOT_ENABLED;
    }
    let Some(session) = lumepeer_service::agent_launch::current_session() else {
        tracing::warn!("cannot tell which session this is; not hosting");
        return EXIT_FAILED;
    };
    let Some(sid) = lumepeer_service::agent_launch::user_sid(session) else {
        tracing::warn!("cannot tell who is signed in to this session; not hosting");
        return EXIT_FAILED;
    };
    let (Some(directory), Some(placement)) = (
        lumepeer_service::program_data::user_directory(&sid),
        crate::placement::for_system_host(&sid),
    ) else {
        tracing::warn!("the signed-in person has no store this host can open");
        return SESSION_HOST_EXIT_NO_STORE;
    };
    if !matches!(placement.keystore.load_secret(IDENTITY_ENTRY), Ok(Some(_))) {
        tracing::info!(
            "the signed-in person has never opened Lumepeer: there is no identity any guest saved"
        );
        return SESSION_HOST_EXIT_NO_STORE;
    }

    // The request event first, so a sign-out that lands while this is still
    // starting is not missed; then the role (ADR 0085 §4).
    let releases = ReleaseRequests::create();
    let role = match system_host::claim_role(SESSION_HOST_EXIT_ROLE_TAKEN) {
        Ok(role) => role,
        Err(code) => return code,
    };
    let Some(runtime) = system_host::runtime() else {
        return EXIT_FAILED;
    };
    let served = Served::new(runtime.handle().clone());
    if let Some(channel) = lumepeer_service::host_channel::HostChannel::new(&sid) {
        crate::host_panel::serve(channel, &served);
    } else {
        tracing::error!("no channel for the client: it cannot attach");
    }
    // The access-settings window raises it after writing the policy
    // (ADR 0169 §5); without it the policy is read only at start.
    let policy_changes = lumepeer_service::access_settings::PolicyChanges::create(&sid);
    if policy_changes.is_none() {
        tracing::warn!(
            "no policy-changed event: a changed device password takes effect at the next start"
        );
    }
    let code = runtime.block_on(serve(
        placement,
        &directory,
        &served,
        releases.as_ref(),
        session,
        policy_changes.as_ref(),
    ));
    system_host::give_back(role, releases.as_ref());
    code
}

/// Waits closed until somebody wants this host listening, then hosts until
/// asked to leave or nobody does any more.
async fn serve(
    placement: Placement,
    directory: &Path,
    served: &Arc<Served>,
    releases: Option<&ReleaseRequests>,
    session: u32,
    policy_changes: Option<&PolicyChanges>,
) -> u32 {
    let wanted = || {
        lumepeer_service::program_data::accept_after_sign_in(directory)
            || served.somebody_wants_hosting()
    };
    tracing::info!("closed: waiting for a client to attach, or for connections after sign-in");
    loop {
        if system_host::released(releases) {
            return 0;
        }
        // Closed, nothing holds the policy: the endpoint reads it when it
        // opens. Taken down here so it is not read twice then.
        let _ = policy_changes.is_some_and(PolicyChanges::changed);
        if wanted() {
            break;
        }
        tokio::time::sleep(TICK).await;
    }
    tracing::info!(
        client = served.somebody_wants_hosting(),
        "opening the endpoint"
    );
    let Some(handle) = system_host::open(Hosting {
        placement,
        screen: Arc::new(SignedInDesktop),
        relay_cache_file: RELAY_CACHE_FILE,
        serving: "the signed-in desktop",
        role_taken: SESSION_HOST_EXIT_ROLE_TAKEN,
        // The logon host comes next, and the guests' redial reaches it.
        leaving: |_| 0,
    })
    .await
    else {
        return EXIT_FAILED;
    };
    served.opened(handle.clone());
    let consents = Arc::new(Consents::new(
        handle.clone(),
        tokio::runtime::Handle::current(),
    ));
    served.asks_with(Arc::clone(&consents));

    let mut unseen = Unseen::default();
    loop {
        tokio::time::sleep(TICK).await;
        if system_host::released(releases) {
            return system_host::hand_over(&handle, |_| 0).await;
        }
        if policy_changes.is_some_and(PolicyChanges::changed) {
            tracing::info!("the access settings were changed; reading the policy again");
            if let Err(error) = handle.reload_policy().await {
                tracing::warn!(?error, "cannot read the access policy again");
            }
        }
        let rows = handle.status().await.unwrap_or_default();
        consents.follow(&rows);
        let sessions = rows.len();
        // A guest still waiting on the dialog is not a session anybody needs
        // to see yet (§7); it is the dialog's to answer.
        let running = rows.iter().any(|row| row.state == SessionStateDto::Active);
        match unseen.next(running, served.anybody_attached(), Instant::now()) {
            Step::Nothing => {}
            Step::StartClient => {
                tracing::info!("a session started with no client to show it; starting the client");
                let started = tokio::task::spawn_blocking(move || {
                    lumepeer_service::client_launch::start_client(session)
                })
                .await
                .unwrap_or(false);
                if !started {
                    tracing::warn!(
                        "the client did not start; the session ends unless one attaches"
                    );
                }
            }
            Step::EndSessions => end_unseen_sessions(&handle).await,
        }
        if sessions == 0 && !wanted() {
            tracing::info!(
                "closing the endpoint: no client attached, no connections after sign-in, nobody on"
            );
            return SESSION_HOST_EXIT_CLOSED;
        }
    }
}

/// Ends every session, because nobody at this machine could see them
/// (ADR 0169 §7, §2.2).
async fn end_unseen_sessions(handle: &ActorHandle) {
    let sessions = handle.status().await.unwrap_or_default();
    // Running sessions only: a guest still asking is the dialog's to answer.
    for session in sessions
        .into_iter()
        .filter(|session| session.state == SessionStateDto::Active)
    {
        tracing::warn!(
            peer = %session.label,
            deadline_secs = SESSION_INDICATOR_DEADLINE_SECS,
            "ending a session nobody at this machine can see: no client attached in time (ADR 0169 §7)"
        );
        let _ = handle.revoke(session.label).await;
    }
}

/// What to do next about sessions nobody here can see (ADR 0169 §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Nothing,
    /// A session runs and no client is attached: start one.
    StartClient,
    /// The client started for it did not attach in time.
    EndSessions,
}

/// Since when sessions have run with no client attached.
#[derive(Debug, Default)]
struct Unseen {
    since: Option<Instant>,
}

impl Unseen {
    /// The step for this tick: whether anybody is on, whether a client is
    /// attached, and the time.
    fn next(&mut self, somebody_on: bool, attached: bool, now: Instant) -> Step {
        if !somebody_on || attached {
            self.since = None;
            return Step::Nothing;
        }
        match self.since {
            None => {
                self.since = Some(now);
                Step::StartClient
            }
            Some(since)
                if now.duration_since(since)
                    >= Duration::from_secs(SESSION_INDICATOR_DEADLINE_SECS) =>
            {
                self.since = None;
                Step::EndSessions
            }
            Some(_) => Step::Nothing,
        }
    }
}

/// The session host's screen seam: no windows of its own, and input from
/// where the ADR 0114 injector stands.
#[derive(Debug)]
struct SignedInDesktop;

impl ViewWindows for SignedInDesktop {
    /// The session host never views another machine: that is the client's,
    /// at the person's own level (ADR 0169 §3).
    fn open(
        &self,
        label: &str,
        _peer_label: &str,
        _host_label: &str,
        _input: bool,
        _surface: ViewSurface,
    ) {
        tracing::warn!(window = %label, "the session host has no view windows");
    }

    fn close(&self, _label: &str) {}

    /// The bar is the client's, which raises it from this host's channel
    /// (ADR 0169 §7).
    fn set_host_bar(&self, _visible: bool) {}

    /// Somebody is signed in, and this process asks them in its own dialog
    /// (ADR 0169 §5): a guest without the device password waits for their
    /// answer, as it did for the elevated client's window.
    fn attendance(&self) -> HostAttendance {
        HostAttendance::Attended
    }

    fn on_secure_desktop(&self) -> bool {
        false
    }

    fn nobody_signed_in(&self) -> bool {
        false
    }

    fn injects_as_system(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The signed-in desktop is somebody's, is not the secure desktop, takes
    /// input from this process itself, and asks the person there (pack 04).
    #[test]
    fn the_signed_in_desktop_injects_itself_and_asks_the_person() {
        let screen = SignedInDesktop;
        assert_eq!(screen.attendance(), HostAttendance::Attended);
        assert!(!screen.on_secure_desktop());
        assert!(!screen.nobody_signed_in());
        assert!(screen.injects_as_system());
    }

    /// ADR 0169 §7: a session with no client starts one at once, and is ended
    /// once the deadline passes with still none attached.
    #[test]
    fn an_unseen_session_starts_the_client_then_ends() {
        let start = Instant::now();
        let deadline = Duration::from_secs(SESSION_INDICATOR_DEADLINE_SECS);
        let mut unseen = Unseen::default();
        assert_eq!(unseen.next(true, false, start), Step::StartClient);
        assert_eq!(
            unseen.next(true, false, start + deadline / 2),
            Step::Nothing
        );
        assert_eq!(
            unseen.next(true, false, start + deadline),
            Step::EndSessions
        );
        assert_eq!(
            unseen.next(true, false, start + deadline * 2),
            Step::StartClient,
            "a session still on after that is a new episode"
        );
    }

    /// A client that attaches in time ends the episode; nobody on starts
    /// nothing.
    #[test]
    fn an_attached_client_or_nobody_on_is_nothing_to_do() {
        let start = Instant::now();
        let deadline = Duration::from_secs(SESSION_INDICATOR_DEADLINE_SECS);
        let mut unseen = Unseen::default();
        assert_eq!(unseen.next(false, false, start), Step::Nothing);
        assert_eq!(unseen.next(true, true, start), Step::Nothing);
        assert_eq!(unseen.next(true, false, start), Step::StartClient);
        assert_eq!(unseen.next(true, true, start + deadline / 2), Step::Nothing);
        assert_eq!(
            unseen.next(true, false, start + deadline),
            Step::StartClient,
            "the client that left later is a new episode, not a late one"
        );
    }
}
