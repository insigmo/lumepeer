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
//! What it does for a guest as a person it does as **the person signed in**,
//! never as itself (ADR 0169 §6, [`ThePerson`]): the shell starts with their
//! own token, a guest's file work runs impersonating the elevated half of it
//! where there is one, and a recording export is written into their profile
//! with their own token. Tunnels stay this process's: a TCP connection
//! carries no Windows identity.
//!
//! Until the consent dialog is its own (pack 04), nobody here can be asked:
//! a guest gets in with the device password, or is refused and told why.
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
use lumepeer_net::run_as::RunAs;
use lumepeer_runtime::network::ActorHandle;
use lumepeer_runtime::person::{Person, SignedIn};
use lumepeer_runtime::view::{ViewSurface, ViewWindows};
use lumepeer_service::host_role::ReleaseRequests;
use lumepeer_service::person_token::SignedInPerson;
use lumepeer_service::{
    SESSION_HOST_EXIT_CLOSED, SESSION_HOST_EXIT_NO_STORE, SESSION_HOST_EXIT_NOT_ENABLED,
    SESSION_HOST_EXIT_ROLE_TAKEN,
};
use lumepeer_terminal::{Shell, ShellError, ShellSize};

use crate::host_panel::Served;
use crate::placement::Placement;
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
    let person = SignedInPerson::new(session, sid.clone());
    served.act_for(person.clone());
    if let Some(channel) = lumepeer_service::host_channel::HostChannel::new(&sid) {
        crate::host_panel::serve(channel, &served);
    } else {
        tracing::error!("no channel for the client: it cannot attach");
    }
    let code = runtime.block_on(serve(
        placement,
        &directory,
        &served,
        releases.as_ref(),
        session,
        person,
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
    person: SignedInPerson,
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
        screen: Arc::new(SignedInDesktop {
            person: Arc::new(ThePerson(person)),
        }),
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

    let mut unseen = Unseen::default();
    loop {
        tokio::time::sleep(TICK).await;
        if system_host::released(releases) {
            return system_host::hand_over(&handle, |_| 0).await;
        }
        let sessions = handle.status().await.map_or(0, |sessions| sessions.len());
        match unseen.next(sessions > 0, served.anybody_attached(), Instant::now()) {
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
    for session in sessions {
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

/// The session host's screen seam: no windows of its own, input from where
/// the ADR 0114 injector stands, and a guest's files and shells as the person
/// signed in.
#[derive(Debug)]
struct SignedInDesktop {
    person: Arc<ThePerson>,
}

/// The person signed in, as the actor acts for them (ADR 0169 §6).
#[derive(Debug)]
struct ThePerson(SignedInPerson);

impl RunAs for ThePerson {
    /// A guest's file work, impersonating the elevated half of the person's
    /// token where there is one — exactly what their elevated client acted
    /// with — and their own token otherwise. Never `LocalSystem`.
    fn run(&self, work: &mut (dyn FnMut() + Send)) -> std::io::Result<()> {
        let token = self.0.files_token()?;
        lumepeer_service::person_token::run_impersonating(
            std::os::windows::io::AsHandle::as_handle(&token),
            work,
        )
    }
}

impl SignedIn for ThePerson {
    /// The shell, with the person's own token: their limited half, medium
    /// integrity (ADR 0079, ADR 0102).
    fn spawn_shell(&self, size: ShellSize) -> Result<Shell, ShellError> {
        let token = self.0.own_token().map_err(|error| {
            tracing::warn!(%error, "refusing a remote shell: no token of the person's to start it with");
            ShellError::CannotDropPrivileges
        })?;
        Shell::spawn_as_user(size, std::os::windows::io::AsHandle::as_handle(&token))
    }
}

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

    /// Somebody is signed in, but nothing this process can show them yet
    /// takes an answer: the consent dialog is pack 04's. Until then the
    /// device password is the only way in, and a guest without one is told
    /// nobody can let it in, rather than left waiting on a dialog nobody sees.
    fn attendance(&self) -> HostAttendance {
        HostAttendance::Unattended
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

    fn person(&self) -> Person {
        Person::SignedIn(Arc::clone(&self.person) as Arc<dyn SignedIn>)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The signed-in desktop is somebody's, is not the secure desktop, takes
    /// input from this process itself — and, until pack 04, asks nobody.
    #[test]
    fn the_signed_in_desktop_injects_itself_and_asks_nobody_yet() {
        let screen = SignedInDesktop {
            person: Arc::new(ThePerson(SignedInPerson::new(1, "S-1-5-21-1".to_owned()))),
        };
        assert_eq!(screen.attendance(), HostAttendance::Unattended);
        assert!(!screen.on_secure_desktop());
        assert!(!screen.nobody_signed_in());
        assert!(screen.injects_as_system());
        assert!(matches!(screen.person(), Person::SignedIn(_)));
    }

    /// ADR 0169 §6: a guest's file work and shell are the person's, and with
    /// no token of theirs to be had — anything but `LocalSystem`, as this
    /// test is — both are refused, never done as this process.
    #[test]
    fn without_the_persons_token_nothing_is_done_as_this_process() {
        let session = lumepeer_service::agent_launch::current_session().unwrap_or(1);
        let person = ThePerson(SignedInPerson::new(session, "S-1-5-21-1".to_owned()));
        let mut ran = false;
        assert!(person.run(&mut || ran = true).is_err());
        assert!(!ran, "file work ran without the person's token");
        assert_eq!(
            person.spawn_shell(ShellSize { cols: 80, rows: 24 }).err(),
            Some(ShellError::CannotDropPrivileges)
        );
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
