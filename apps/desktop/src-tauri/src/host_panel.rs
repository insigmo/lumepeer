//! Both ends of the channel between the session host and the client
//! (ADR 0169 §4, §7), as this binary — which is both — speaks it.
//!
//! The wire, its bounds and its closed list of requests are
//! `lumepeer_service::host_channel_protocol`; the pipe and who may open it are
//! `lumepeer_service::host_channel`. What is here is what the bytes mean:
//!
//! - **The session host answers** ([`Served::answer`]) from its actor: the
//!   sessions as the same rows `session_status` gives the webview, a chat
//!   transcript, and the narrowing requests carried out through the actor's
//!   own calls — `revoke`, `set_grant(.., false)`. A grant name is parsed into
//!   `IndependentGrant`, a closed enum, and refused if it is not one.
//! - **The client asks** ([`HostPanel`]) for as long as it runs, which is what
//!   "a client is attached" means to the host: one connection held open, with
//!   the window's own commands riding it. While the host has a session, the
//!   client keeps its host bar up — the bar stays in the client (ADR 0169 §7).

use std::sync::Mutex;

use lumepeer_service::host_channel_protocol::{Reply, Request};
use serde::{Deserialize, Serialize};

use crate::commands::{
    ChatEntryDto, ExportDto, RecordingDto, SessionStatusDto, UnattendedStatusDto,
};

/// What the session host says about itself in one snapshot.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostSnapshot {
    /// Whether its endpoint is bound: this machine can be dialled.
    pub open: bool,
    /// Whether the endpoint has reached a relay.
    pub ready: bool,
    /// Its sessions, as `session_status` rows.
    pub sessions: Vec<SessionStatusDto>,
    /// Its unattended access, to be read here and changed only in the
    /// elevated access-settings window (ADR 0169 §5); `None` while closed.
    #[serde(default)]
    pub unattended: Option<UnattendedStatusDto>,
    /// Where its updates stand (ADR 0169 §8).
    #[serde(default)]
    pub update: HostUpdate,
    /// Whether it accepts connections from sign-in on, with no client attached
    /// (ADR 0169 §9); `None` when it cannot say. Open or closed alike.
    #[serde(default)]
    pub accept_after_sign_in: Option<bool>,
}

/// Where the session host's updates stand, as the client shows them
/// (ADR 0169 §8).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum HostUpdate {
    /// Not asked yet.
    #[default]
    Unknown,
    /// This host installs no updates: a development build, an `.msi`
    /// install, no channel. The client updates the way it always has.
    Off,
    /// The channel offers nothing newer.
    Current,
    /// A newer release is there.
    Available {
        /// Its version.
        version: String,
        /// Its release notes, or empty.
        notes: String,
    },
    /// It is being downloaded and verified.
    Downloading {
        /// Its version.
        version: String,
    },
    /// Its installer is starting: the host is about to restart into it.
    Installing {
        /// Its version.
        version: String,
    },
}

/// Why a request through the panel did not happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelError {
    /// No session host answers on this machine.
    Unreachable,
    /// It answered and did not do it.
    Refused,
}

/// The client's end: one held connection to the session host serving this
/// user, reopened when it drops.
#[derive(Debug)]
pub struct HostPanel {
    sid: String,
    link: Mutex<Option<Link>>,
    /// When a session host last answered, or when this panel was made if
    /// none has yet: what [`Self::unreachable`] measures from.
    answered: Mutex<std::time::Instant>,
}

#[cfg(target_os = "windows")]
type Link = lumepeer_service::host_channel::PanelLink;

/// No session host anywhere else, so nothing to link to.
#[cfg(not(target_os = "windows"))]
#[derive(Debug)]
enum Link {}

impl HostPanel {
    /// The panel for the user this process runs as; `None` where there is no
    /// session host to have one for.
    #[must_use]
    pub fn for_this_user() -> Option<Self> {
        #[cfg(target_os = "windows")]
        {
            let (sid, _) = lumepeer_service::program_data::this_process()?;
            Some(Self {
                sid,
                link: Mutex::new(None),
                answered: Mutex::new(std::time::Instant::now()),
            })
        }
        #[cfg(not(target_os = "windows"))]
        {
            None
        }
    }

    /// Asks one thing, opening the channel first if it is not open, and once
    /// more on a fresh connection if the held one has dropped — the session
    /// host restarts each time it closes its endpoint.
    fn ask(&self, request: &Request) -> Option<Reply> {
        let Ok(mut link) = self.link.lock() else {
            return None;
        };
        for _ in 0..2 {
            if link.is_none() {
                *link = connect(&self.sid);
            }
            let held = link.as_mut()?;
            match ask_on(held, request) {
                Some(reply) => {
                    if let Ok(mut answered) = self.answered.lock() {
                        *answered = std::time::Instant::now();
                    }
                    return Some(reply);
                }
                None => *link = None,
            }
        }
        None
    }

    /// Whether no session host has answered for
    /// [`SESSION_HOST_UNREACHABLE_SECS`]: nobody can reach this machine until
    /// the service runs again (ADR 0169 §2). Kept fresh by [`stay_attached`]'s
    /// poll; the restart a session host makes each time it closes its
    /// endpoint is shorter than that.
    ///
    /// [`SESSION_HOST_UNREACHABLE_SECS`]: lumepeer_core::constants::SESSION_HOST_UNREACHABLE_SECS
    #[must_use]
    pub fn unreachable(&self) -> bool {
        self.answered.lock().is_ok_and(|answered| {
            answered.elapsed()
                >= std::time::Duration::from_secs(
                    lumepeer_core::constants::SESSION_HOST_UNREACHABLE_SECS,
                )
        })
    }

    /// The host's state; `None` when no session host answers.
    #[must_use]
    pub fn snapshot(&self) -> Option<HostSnapshot> {
        match self.ask(&Request::Snapshot)? {
            Reply::Snapshot(json) => serde_json::from_slice(&json).ok(),
            Reply::Transcript(_)
            | Reply::Done
            | Reply::Refused
            | Reply::Recordings(_)
            | Reply::Exported(_)
            | Reply::GuestStores(_) => None,
        }
    }

    /// The host's unattended access as it stands; `None` when no session host
    /// answers or it is closed (ADR 0169 §5).
    #[must_use]
    pub fn unattended(&self) -> Option<UnattendedStatusDto> {
        self.snapshot()?.unattended
    }

    /// Asks one thing and hands back whatever came back; `None` when no
    /// session host answers. For the one-time move of the guest stores
    /// (`placement::guest_stores`), which reads its own replies.
    #[must_use]
    pub fn request(&self, request: &Request) -> Option<Reply> {
        self.ask(request)
    }

    /// The chat with `peer`, one of the host's sessions.
    ///
    /// # Errors
    /// [`PanelError`] when there is no host or no such session.
    pub fn transcript(&self, peer: String) -> Result<Vec<ChatEntryDto>, PanelError> {
        match self.ask(&Request::Transcript { peer }) {
            Some(Reply::Transcript(json)) => {
                serde_json::from_slice(&json).map_err(|_| PanelError::Refused)
            }
            Some(_) => Err(PanelError::Refused),
            None => Err(PanelError::Unreachable),
        }
    }

    /// The recordings the session host has written (ADR 0169 §6).
    ///
    /// # Errors
    /// [`PanelError`] when there is no host, or it could not list them.
    pub fn recordings(&self) -> Result<Vec<RecordingDto>, PanelError> {
        match self.ask(&Request::Recordings) {
            Some(Reply::Recordings(json)) => {
                serde_json::from_slice(&json).map_err(|_| PanelError::Refused)
            }
            Some(_) => Err(PanelError::Refused),
            None => Err(PanelError::Unreachable),
        }
    }

    /// Has the session host export its recording `name` into this person's
    /// profile, as them (ADR 0169 §6).
    ///
    /// # Errors
    /// [`PanelError`] when there is no host, or the export did not happen.
    pub fn export_recording(&self, name: String) -> Result<ExportDto, PanelError> {
        match self.ask(&Request::ExportRecording { name }) {
            Some(Reply::Exported(json)) => {
                serde_json::from_slice(&json).map_err(|_| PanelError::Refused)
            }
            Some(_) => Err(PanelError::Refused),
            None => Err(PanelError::Unreachable),
        }
    }
    /// Asks the host to install the update it found (ADR 0169 §8). The
    /// request names nothing: what is installed is the host's to choose.
    ///
    /// # Errors
    /// [`PanelError`] when there is no host, or it has nothing to install.
    pub fn install_update(&self) -> Result<(), PanelError> {
        self.act(&Request::InstallUpdate)
    }

    /// Asks the host to do one narrowing thing, or to take a chat line.
    ///
    /// # Errors
    /// [`PanelError`] when there is no host, or it did not do it.
    pub fn act(&self, request: &Request) -> Result<(), PanelError> {
        match self.ask(request) {
            Some(Reply::Done) => Ok(()),
            Some(_) => Err(PanelError::Refused),
            None => Err(PanelError::Unreachable),
        }
    }
}

#[cfg(target_os = "windows")]
fn connect(sid: &str) -> Option<Link> {
    Link::connect(sid)
}

#[cfg(not(target_os = "windows"))]
fn connect(_sid: &str) -> Option<Link> {
    None
}

#[cfg(target_os = "windows")]
fn ask_on(link: &mut Link, request: &Request) -> Option<Reply> {
    link.ask(request)
}

#[cfg(not(target_os = "windows"))]
fn ask_on(link: &mut Link, _request: &Request) -> Option<Reply> {
    match *link {}
}

/// How often the client looks at the host's sessions to keep its host bar
/// right, and its connection held.
const ATTACHED_POLL: std::time::Duration = std::time::Duration::from_secs(1);

/// Keeps the client attached to the session host for as long as it runs, and
/// its host bar up while the host has a session (ADR 0169 §7). Never returns;
/// run it on a thread of its own.
pub fn stay_attached(panel: &HostPanel, app: &tauri::AppHandle) {
    use lumepeer_runtime::view::ViewWindows as _;

    let windows = crate::view_windows::TauriViewWindows::new(app.clone());
    let mut bar_up = false;
    loop {
        let somebody_on = panel
            .snapshot()
            .is_some_and(|snapshot| !snapshot.sessions.is_empty());
        if somebody_on != bar_up {
            windows.set_host_bar(somebody_on);
            bar_up = somebody_on;
        }
        std::thread::sleep(ATTACHED_POLL);
    }
}

#[cfg(target_os = "windows")]
pub use served::{Served, serve};

/// The session host's end.
#[cfg(target_os = "windows")]
mod served {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};

    use lumepeer_service::person_token::SignedInPerson;

    use lumepeer_core::consent::IndependentGrant;
    use lumepeer_runtime::network::ActorHandle;
    use lumepeer_service::host_channel::{HostChannel, PanelConnection};
    use lumepeer_service::host_channel_protocol::{Reply, Request};

    use super::HostSnapshot;
    use crate::commands::{ChatEntryDto, UnattendedStatusDto, session_status_dto};
    use crate::placement::guest_stores::{GuestStores, InAdminTree};
    use crate::session_host_consent::Consents;

    /// What the session host's channel threads and its own loop share.
    #[derive(Debug)]
    pub struct Served {
        runtime: tokio::runtime::Handle,
        actor: Mutex<Option<ActorHandle>>,
        /// Connections open right now: a client the person can see sessions
        /// in (ADR 0169 §7).
        connected: AtomicUsize,
        /// Connections open right now that have not turned hosting off: the
        /// clients that keep the endpoint open (ADR 0169 §1).
        wanting: AtomicUsize,
        /// Who puts a question the client asked for in front of the person
        /// at this machine (pack 04); none until the endpoint is open.
        consents: Mutex<Option<Arc<Consents>>>,
        /// Whom a recording export is written as and for (ADR 0169 §6).
        person: OnceLock<SignedInPerson>,
        /// The person's guest stores still in the admin directory, handed to
        /// their client once (ADR 0169 §3). `None` holds nothing to hand.
        guest: Mutex<Option<InAdminTree>>,
        /// Where this host's updates stand, and a client's "install now"
        /// (ADR 0169 §8).
        updates: Arc<crate::host_update::Updates>,
        /// The person's directory, where "accept connections after sign-in"
        /// is kept (ADR 0169 §9).
        directory: OnceLock<PathBuf>,
    }

    impl Served {
        /// Nothing served yet: no actor, nobody connected.
        #[must_use]
        pub fn new(runtime: tokio::runtime::Handle) -> Arc<Self> {
            Arc::new(Self {
                runtime,
                actor: Mutex::new(None),
                connected: AtomicUsize::new(0),
                wanting: AtomicUsize::new(0),
                consents: Mutex::new(None),
                person: OnceLock::new(),
                guest: Mutex::new(None),
                updates: Arc::default(),
                directory: OnceLock::new(),
            })
        }

        /// The session host's own dialog, which a client's
        /// [`Request::AskToWiden`] puts up (pack 04).
        pub fn asks_with(&self, consents: Arc<Consents>) {
            if let Ok(mut slot) = self.consents.lock() {
                *slot = Some(consents);
            }
        }

        fn consents(&self) -> Option<Arc<Consents>> {
            self.consents.lock().ok()?.clone()
        }

        /// The person this host serves, for what it writes in their name.
        pub fn act_for(&self, person: SignedInPerson) {
            let _ = self.person.set(person);
        }

        /// The person's directory, which this host reads and narrows the
        /// "accept connections after sign-in" setting in (ADR 0169 §9).
        pub fn keeps_settings_in(&self, directory: PathBuf) {
            let _ = self.directory.set(directory);
        }

        fn accept_after_sign_in(&self) -> Option<bool> {
            self.directory
                .get()
                .map(|directory| lumepeer_service::program_data::accept_after_sign_in(directory))
        }

        /// The person's guest stores this host may hand to their client
        /// (ADR 0169 §3).
        pub fn keeps_guest_stores(&self, stores: InAdminTree) {
            if let Ok(mut slot) = self.guest.lock() {
                *slot = Some(stores);
            }
        }

        /// The guest stores, as JSON; refused when they cannot all be read.
        fn guest_stores(&self) -> Reply {
            let Ok(slot) = self.guest.lock() else {
                return Reply::Refused;
            };
            slot.as_ref()
                .map_or_else(|| Some(GuestStores::default()), InAdminTree::collect)
                .and_then(|stores| serde_json::to_vec(&stores).ok())
                .map_or(Reply::Refused, Reply::GuestStores)
        }

        /// The client has kept them: out of the admin directory with them.
        fn guest_stores_taken(&self) -> Reply {
            let Ok(slot) = self.guest.lock() else {
                return Reply::Refused;
            };
            match slot.as_ref() {
                None => Reply::Done,
                Some(stores) if stores.remove() => {
                    tracing::info!(
                        "the client keeps the remembered hosts now; they are out of the admin directory (ADR 0169 §3)"
                    );
                    Reply::Done
                }
                Some(_) => Reply::Refused,
            }
        }

        /// This host's updates (ADR 0169 §8).
        #[must_use]
        pub const fn updates(&self) -> &Arc<crate::host_update::Updates> {
            &self.updates
        }

        /// The endpoint is open, and this is its actor.
        pub fn opened(&self, actor: ActorHandle) {
            if let Ok(mut slot) = self.actor.lock() {
                *slot = Some(actor);
            }
        }

        /// Whether any client is connected.
        #[must_use]
        pub fn anybody_attached(&self) -> bool {
            self.connected.load(Ordering::SeqCst) > 0
        }

        /// Whether any connected client still wants this host listening.
        #[must_use]
        pub fn somebody_wants_hosting(&self) -> bool {
            self.wanting.load(Ordering::SeqCst) > 0
        }

        /// The actor, once the endpoint is open.
        pub fn actor(&self) -> Option<ActorHandle> {
            self.actor.lock().ok()?.clone()
        }

        /// Serves one client until it hangs up or says something this host
        /// does not understand.
        fn serve_one(&self, mut panel: PanelConnection) {
            let pid = panel.client_pid();
            tracing::info!(pid, "a client attached to the session host");
            self.connected.fetch_add(1, Ordering::SeqCst);
            self.wanting.fetch_add(1, Ordering::SeqCst);
            let mut wants = true;
            // Whether this connection was handed the guest stores: only then
            // may it say they were kept (ADR 0169 §3).
            let mut handed = false;
            while let Some(request) = panel.recv() {
                if request == Request::HostingOff && wants {
                    wants = false;
                    self.wanting.fetch_sub(1, Ordering::SeqCst);
                }
                let reply = match request {
                    Request::GuestStores => {
                        let reply = self.guest_stores();
                        handed |= matches!(reply, Reply::GuestStores(_));
                        reply
                    }
                    Request::GuestStoresTaken if handed => self.guest_stores_taken(),
                    request => self.answer(request),
                };
                if !panel.send(&reply) {
                    break;
                }
            }
            if wants {
                self.wanting.fetch_sub(1, Ordering::SeqCst);
            }
            self.connected.fetch_sub(1, Ordering::SeqCst);
            tracing::info!(pid, "a client left the session host");
        }

        /// One request's answer. Every arm narrows, reads, chats or asks for
        /// the host's own update; there is no other kind of request to answer
        /// (`Request::effect`).
        fn answer(&self, request: Request) -> Reply {
            // The host's recordings are on its disk whether or not its
            // endpoint is open (ADR 0169 §6).
            match request {
                Request::Recordings => return recordings_reply(),
                Request::ExportRecording { name } => return self.export_reply(&name),
                // A file in the person's directory, open or closed: off is
                // how a closed host stays closed after sign-in.
                Request::AcceptAfterSignInOff => return self.accept_after_sign_in_off(),
                _ => {}
            }
            let Some(actor) = self.actor() else {
                // Closed: nothing to read but that and the updates, nothing to
                // narrow.
                return match request {
                    Request::Snapshot => snapshot_reply(&HostSnapshot {
                        update: self.updates.state(),
                        accept_after_sign_in: self.accept_after_sign_in(),
                        ..HostSnapshot::default()
                    }),
                    Request::InstallUpdate => self.install_update(),
                    _ => Reply::Refused,
                };
            };
            let runtime = &self.runtime;
            let done = |ok: bool| if ok { Reply::Done } else { Reply::Refused };
            match request {
                Request::Snapshot => {
                    let sessions = runtime.block_on(actor.status()).unwrap_or_default();
                    snapshot_reply(&HostSnapshot {
                        open: true,
                        ready: actor.online(),
                        sessions: sessions.into_iter().map(session_status_dto).collect(),
                        unattended: unattended_of(runtime, &actor),
                        update: self.updates.state(),
                        accept_after_sign_in: self.accept_after_sign_in(),
                    })
                }
                Request::Transcript { peer } => runtime
                    .block_on(actor.chat_transcript(peer))
                    .ok()
                    .and_then(|rows| {
                        let rows: Vec<ChatEntryDto> = rows
                            .into_iter()
                            .map(|e| ChatEntryDto {
                                outgoing: e.outgoing,
                                text: e.text,
                                at_unix: e.at_unix,
                            })
                            .collect();
                        serde_json::to_vec(&rows).ok()
                    })
                    .map_or(Reply::Refused, Reply::Transcript),
                Request::MarkChatRead { peer } => {
                    done(runtime.block_on(actor.chat_mark_read(peer)).is_ok())
                }
                Request::EndSession { peer } | Request::RefusePending { peer } => {
                    done(runtime.block_on(actor.revoke(peer)).is_ok())
                }
                Request::LowerGrant { peer, grant } => match grant_named(&grant) {
                    Some(grant) => done(
                        runtime
                            .block_on(actor.set_grant(peer, grant, false))
                            .is_ok(),
                    ),
                    None => Reply::Refused,
                },
                Request::HostingOff => {
                    let sessions = runtime.block_on(actor.status()).unwrap_or_default();
                    for session in sessions {
                        let _ = runtime.block_on(actor.revoke(session.label));
                    }
                    tracing::info!("a client turned hosting off: every session ended");
                    Reply::Done
                }
                Request::SendChat { peer, text } => {
                    done(runtime.block_on(actor.chat_send(peer, text)).is_ok())
                }
                // Puts the question up and gives nothing: the person's yes in
                // the session host's own window is what widens (pack 04).
                Request::AskToWiden { peer, widening } => {
                    let sessions = runtime.block_on(actor.status()).unwrap_or_default();
                    done(
                        self.consents().is_some_and(|consents| {
                            consents.ask_to_widen(&sessions, &peer, widening)
                        }),
                    )
                }
                // Narrowing, so a client may ask it with no prompt (ADR 0169
                // §4); turning it back on is the access-settings window's.
                Request::UnattendedOff => {
                    let off = runtime.block_on(actor.unattended_disable()).is_ok();
                    tracing::info!(off, "a client turned unattended access off");
                    done(off)
                }
                // The first three are answered above, open or closed. The
                // guest stores are answered by `serve_one`, which knows
                // whether this connection was handed them; a letting-go that
                // reaches here was not, and lets nothing go.
                Request::Recordings
                | Request::ExportRecording { .. }
                | Request::AcceptAfterSignInOff
                | Request::GuestStores
                | Request::GuestStoresTaken => Reply::Refused,
                Request::InstallUpdate => self.install_update(),
            }
        }

        /// A client's "accept only while I am open" (ADR 0169 §9): a
        /// narrowing, so no prompt; turning it back on is the access-settings
        /// window's.
        fn accept_after_sign_in_off(&self) -> Reply {
            let off = self.directory.get().is_some_and(|directory| {
                lumepeer_service::program_data::set_accept_after_sign_in(directory, false)
            });
            tracing::info!(
                off,
                "a client turned accepting connections after sign-in off"
            );
            if off { Reply::Done } else { Reply::Refused }
        }

        /// A client's "install now" (ADR 0169 §8): the host checks and
        /// installs what it finds itself; refused when it knows there is
        /// nothing, or installs nothing at all.
        fn install_update(&self) -> Reply {
            if self.updates.ask_to_install() {
                tracing::info!("a client asked for the update to be installed now");
                Reply::Done
            } else {
                Reply::Refused
            }
        }

        /// Exports the recording `name` for the person this host serves.
        fn export_reply(&self, name: &str) -> Reply {
            let Some(person) = self.person.get() else {
                tracing::warn!("no person to export a recording for");
                return Reply::Refused;
            };
            match export_for(person, name) {
                Ok(exported) => {
                    serde_json::to_vec(&exported).map_or(Reply::Refused, Reply::Exported)
                }
                Err(error) => {
                    tracing::warn!(%error, "a recording was not exported");
                    Reply::Refused
                }
            }
        }
    }

    /// This host's recordings, as the client's own list would show them.
    fn recordings_reply() -> Reply {
        let recordings = lumepeer_runtime::config::recordings_dir()
            .map(|dir| crate::commands::recordings_in(&dir))
            .unwrap_or_default();
        serde_json::to_vec(&recordings).map_or(Reply::Refused, Reply::Recordings)
    }

    /// Exports this host's recording `name` into `person`'s own profile, as
    /// them (ADR 0169 §6).
    ///
    /// The recording is this host's own file, in its own tree, and is opened
    /// as this process. The export is written while impersonating the
    /// person's own token — never the elevated half: the caller is any program
    /// of theirs, and what it may cause to be written must be no more than
    /// they could write themselves — into the directory their own client
    /// exports into, so it is theirs and lands where they would look.
    fn export_for(
        person: &SignedInPerson,
        name: &str,
    ) -> Result<crate::commands::ExportDto, String> {
        let name = crate::commands::recording_file_name(name)
            .map_err(|error| error.message)?
            .to_owned();
        let source = lumepeer_runtime::config::recordings_dir()
            .ok_or("this host has no recordings directory")?
            .join(&name);
        let stem = std::path::Path::new(&name)
            .file_stem()
            .and_then(std::ffi::OsStr::to_str)
            .ok_or("a recording name with no stem")?
            .to_owned();
        let file = std::fs::File::open(&source).map_err(|error| error.to_string())?;
        let out_dir = person
            .local_app_data()
            .map_err(|error| error.to_string())?
            .join(lumepeer_runtime::config::APP_IDENTIFIER)
            .join("recordings")
            .join(crate::commands::EXPORTS_SUBDIR);
        let token = person.own_token().map_err(|error| error.to_string())?;
        let mut file = Some(file);
        let mut output = None;
        lumepeer_service::person_token::run_impersonating(
            std::os::windows::io::AsHandle::as_handle(&token),
            &mut || {
                if let Some(file) = file.take() {
                    output = Some(lumepeer_media::export::export_open(file, &stem, &out_dir));
                }
            },
        )
        .map_err(|error| error.to_string())?;
        let output = output
            .ok_or("the export never ran")?
            .map_err(|error| error.to_string())?;
        Ok(crate::commands::export_dto(&out_dir, &output))
    }

    /// The host's unattended access, as the client's settings panel reads it.
    fn unattended_of(
        runtime: &tokio::runtime::Handle,
        actor: &ActorHandle,
    ) -> Option<UnattendedStatusDto> {
        let settings = runtime.block_on(actor.unattended_status()).ok()?;
        Some(UnattendedStatusDto {
            enabled: settings.enabled,
            totp_enabled: settings.totp_enabled,
            role: settings.role.into(),
            // Who hosts the logon screen is read where only administrators
            // can, which this process could, but the client changes it only in
            // the access-settings window, which reads it itself.
            logon_screen: None,
            managed_by_service: true,
            // In the snapshot itself, which a closed host answers too.
            accept_after_sign_in: None,
        })
    }

    /// The grant `name` spells, in `IndependentGrant`'s own spelling; `None`
    /// for anything that is not one.
    fn grant_named(name: &str) -> Option<IndependentGrant> {
        serde_json::from_value(serde_json::Value::String(name.to_owned())).ok()
    }

    fn snapshot_reply(snapshot: &HostSnapshot) -> Reply {
        serde_json::to_vec(snapshot).map_or(Reply::Refused, Reply::Snapshot)
    }

    /// Accepts clients on `channel` for as long as this process runs, one
    /// thread each.
    pub fn serve(mut channel: HostChannel, served: &Arc<Served>) {
        let served = Arc::clone(served);
        std::thread::spawn(move || {
            while let Some(panel) = channel.accept() {
                let served = Arc::clone(&served);
                std::thread::spawn(move || served.serve_one(panel));
            }
            tracing::error!("the session host's channel is gone: no client can attach");
        });
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A grant name is one `IndependentGrant` spells, and nothing else is.
        #[test]
        fn a_grant_name_is_parsed_into_the_closed_enum() {
            assert_eq!(
                grant_named("clipboard_read"),
                Some(IndependentGrant::ClipboardRead)
            );
            assert_eq!(
                grant_named("secure_desktop_input"),
                Some(IndependentGrant::SecureDesktopInput)
            );
            assert_eq!(grant_named("input"), None, "input follows the role");
            assert_eq!(grant_named("view"), None);
            assert_eq!(grant_named("everything"), None);
        }

        /// A closed host answers a snapshot that says so, and narrows nothing.
        #[test]
        fn a_closed_host_says_it_is_closed() {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap_or_else(|error| panic!("{error}"));
            let served = Served::new(runtime.handle().clone());
            let Reply::Snapshot(json) = served.answer(Request::Snapshot) else {
                panic!("a snapshot is always answered");
            };
            let snapshot: HostSnapshot =
                serde_json::from_slice(&json).unwrap_or_else(|error| panic!("{error}"));
            assert!(!snapshot.open && snapshot.sessions.is_empty());
            assert!(
                snapshot.unattended.is_none(),
                "nothing to read while closed"
            );
            assert_eq!(served.answer(Request::UnattendedOff), Reply::Refused);
            assert_eq!(
                served.answer(Request::EndSession {
                    peer: "p".to_owned()
                }),
                Reply::Refused
            );
        }

        /// Pack 04: a closed host has nobody to ask about, so an ask to widen
        /// is refused, and puts nothing up.
        #[test]
        fn a_closed_host_asks_nobody_anything() {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap_or_else(|error| panic!("{error}"));
            let served = Served::new(runtime.handle().clone());
            assert_eq!(
                served.answer(Request::AskToWiden {
                    peer: "p".to_owned(),
                    widening: lumepeer_service::host_channel_protocol::Widening::Control,
                }),
                Reply::Refused
            );
        }

        /// ADR 0169 §8: the snapshot says where the updates stand, open or
        /// closed, and "install now" is taken unless the host installs
        /// nothing.
        #[test]
        fn the_updates_are_read_and_asked_for_over_the_channel() {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap_or_else(|error| panic!("{error}"));
            let served = Served::new(runtime.handle().clone());
            assert_eq!(served.answer(Request::InstallUpdate), Reply::Done);

            let Reply::Snapshot(json) = served.answer(Request::Snapshot) else {
                panic!("a snapshot is always answered");
            };
            let snapshot: HostSnapshot =
                serde_json::from_slice(&json).unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(snapshot.update, super::super::HostUpdate::Unknown);
        }
    }
}

#[cfg(test)]
mod tests {
    /// A snapshot from a session host that says nothing about unattended
    /// access reads as "not known", and one that does reads as kept by the
    /// service (ADR 0169 §5).
    #[test]
    fn the_unattended_state_rides_the_snapshot() {
        let older: super::HostSnapshot =
            serde_json::from_str(r#"{"open":true,"ready":true,"sessions":[]}"#)
                .unwrap_or_else(|error| panic!("{error}"));
        assert!(older.unattended.is_none());
        let newer: super::HostSnapshot = serde_json::from_str(
            r#"{"open":true,"ready":true,"sessions":[],"unattended":{"enabled":true,
                "totp_enabled":false,"role":"full_control","logon_screen":null,
                "managed_by_service":true}}"#,
        )
        .unwrap_or_else(|error| panic!("{error}"));
        let unattended = newer
            .unattended
            .unwrap_or_else(|| panic!("the field was there"));
        assert!(unattended.enabled && unattended.managed_by_service);
    }
    use super::{HostSnapshot, HostUpdate};

    /// The update state is a tagged object on the wire, and a snapshot from a
    /// host that says nothing about updates reads as "not asked yet".
    #[test]
    fn the_update_state_is_tagged_and_optional() {
        let available = HostUpdate::Available {
            version: "0.0.166".to_owned(),
            notes: String::new(),
        };
        assert_eq!(
            serde_json::to_value(&available).unwrap_or_default(),
            serde_json::json!({"stage": "available", "version": "0.0.166", "notes": ""})
        );
        let old: HostSnapshot =
            serde_json::from_str(r#"{"open":false,"ready":false,"sessions":[]}"#)
                .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(old.update, HostUpdate::Unknown);
    }

    /// The protocol's chat bound is the core's, which it cannot import.
    #[test]
    fn the_channel_carries_any_chat_line_the_core_accepts() {
        assert_eq!(
            lumepeer_service::host_channel_protocol::MAX_CHAT_BYTES,
            lumepeer_core::constants::CHAT_MAX_BYTES
        );
    }
}
