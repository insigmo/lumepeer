//! Owner of the network/session runtime (design doc §2.3, §4, §13).
//!
//! `NetworkActor` is the single owner of `SessionManager`, `PeerEndpoint` and
//! `TicketRegistry`. Tauri commands never lock anything directly: they send an
//! `ActorCommand` and await the reply, so there is exactly one place that
//! decides authorization (§2.3).
//!
//! Every live control connection is split in two: a reader task owns the
//! inbound half and reports what it sees back to the actor, while the actor
//! keeps only an outbound channel. That is what makes `ConsentGrant` delivery
//! and disconnect detection work symmetrically on the host and the guest side,
//! and it keeps a partially read frame from ever being cancelled by an
//! outbound write.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ed25519_dalek::SigningKey;
use lumepeer_core::chat::{ChatEntry, ChatLog};
use lumepeer_core::clipboard::ClipboardSync;
use lumepeer_core::consent::{Grants, Role};
use lumepeer_core::constants::{CONTROL_HANDSHAKE_TIMEOUT_SECS, MAX_INFLIGHT_HANDSHAKES};
use lumepeer_core::protocol::{InputEventPayload, MessageKind};
use lumepeer_core::session::{SessionManager, SessionState};
use lumepeer_core::{CoreError, NodeId};
use lumepeer_media::capture::{
    CaptureController, CaptureTarget, InputInjector, StubCapturer, platform_capturer,
    platform_injector,
};
use lumepeer_net::keystore::load_or_create;
use lumepeer_net::ticket::TicketRegistry;
use lumepeer_net::{Channel, ControlConnection, InviteTicket, NetError, PeerEndpoint};
use rand::Rng as _;
use tokio::sync::{Semaphore, broadcast, mpsc, oneshot, watch};

use crate::connection_history::{ConnectionHistory, HistoryEntry};
use crate::view::{
    MediaTarget, SharedCapture, ViewSlot, ViewWindows, encode_view_response, lock_capture,
    slot_for_poll, spawn_encode_loop, spawn_media_receiver, window_label,
};

/// Capacity of the notification broadcast. Listeners that fall behind lag;
/// nothing in the actor's own progress depends on them.
const NOTIFY_CAPACITY: usize = 32;

/// State of one session as the webview needs to know it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStateDto {
    /// Queued, waiting for the host's decision.
    Pending,
    /// Consent granted, grants are live.
    Active,
}

/// One row of the status list the webview polls.
#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    /// Pseudonymized label; the only peer-identifying string that ever
    /// crosses the IPC boundary in either direction.
    pub label: String,
    /// Pending or active.
    pub state: SessionStateDto,
    /// Requested role if pending, granted role if active.
    pub role: Role,
    /// Whether input injection is currently permitted (always `false` for
    /// a pending entry).
    pub input: bool,
}

/// What `invite_create` hands back to the UI.
#[derive(Debug, Clone)]
pub struct InviteDto {
    /// The invite code to show in the sidebar (§7).
    pub code: String,
    /// Unix seconds after which the invite is dead.
    pub expires_at: u64,
}

/// Guest side: how far this node's own outgoing connect attempt has got (§21
/// punch-list item 6).
///
/// A dial is not the end of the story — the host user still has to decide, and
/// that decision can take as long as it takes. Without this the connect form
/// has nothing to wait on: it would go back to idle the moment the handshake
/// returned, invite a second attempt against a host that is already deciding
/// on the first, and show no sign of the wait at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectPhase {
    /// Nothing outgoing in flight.
    Idle,
    /// Connected to the host; its user has not decided yet.
    AwaitingConsent,
    /// The host granted and the view window is open.
    Connected,
    /// The host refused, or ended the request without granting.
    Denied,
    /// The dial or the handshake failed, or the host dropped mid-request.
    Failed,
}

impl ConnectPhase {
    /// Whether the connect form should stay disabled: something is in flight
    /// and a second attempt would only race it.
    #[must_use]
    pub const fn is_pending(self) -> bool {
        matches!(self, Self::AwaitingConsent)
    }

    /// Stable wire string for the webview.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::AwaitingConsent => "awaiting_consent",
            Self::Connected => "connected",
            Self::Denied => "denied",
            Self::Failed => "failed",
        }
    }
}

/// Something the actor observed that a listener may want to react to.
///
/// Deliberately carries no peer identity: it crosses no trust boundary today
/// and must not become a channel for one (§15).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActorNotification {
    /// A remote host granted `role` to this node (guest side).
    ConsentGranted {
        /// Role the host decided on, which may be lower than the one asked for.
        role: Role,
    },
    /// A remote host withdrew its grant (guest side).
    ConsentRevoked,
    /// A consent request was queued for the host user to decide (host side).
    ConsentRequested,
    /// A control connection closed, in either direction.
    Disconnected,
    /// A chat message arrived from a peer. The label is pseudonymized (§15);
    /// the transcript itself stays inside the actor and is polled by the UI.
    ChatFromPeer {
        /// Pseudonymized peer label the message came from.
        label: String,
    },
    /// The peer's clipboard changed and passed §9.2 validation; the payload
    /// is delivered through `clipboard_inbound`, never on the notification
    /// bus (§15: notifications are broadcast to every listener).
    ClipboardFromPeer,
}

/// Failure returned by an actor call.
#[derive(Debug)]
pub enum ActorError {
    /// The label the caller sent does not resolve to a known peer. Not a
    /// leak: an unknown label is exactly as safe to report as a known one,
    /// since the label never carried identity to begin with.
    UnknownPeer,
    /// A `SessionManager` decision was refused.
    Core(CoreError),
    /// A network operation failed.
    Net(NetError),
    /// The actor task is gone; the caller's channel op failed.
    ChannelClosed,
}

/// One request the actor understands.
enum ActorCommand {
    Status {
        reply: oneshot::Sender<Vec<SessionSnapshot>>,
    },
    /// Hosts this node has connected to before (§21 punch-list item 5).
    History {
        reply: oneshot::Sender<Vec<HistoryEntry>>,
    },
    /// Guest side: dial a remembered host again, using the invite code the
    /// history row kept. The code never leaves the Rust side (§13).
    HistoryConnect {
        label: String,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    /// Guest side: how this node's own outgoing connect attempt is going.
    ConnectState {
        reply: oneshot::Sender<ConnectPhase>,
    },
    Grant {
        label: String,
        role: Role,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    Revoke {
        label: String,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    InviteCreate {
        role: Role,
        reply: oneshot::Sender<Result<InviteDto, ActorError>>,
    },
    InviteConnect {
        ticket: String,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    /// Guest side: newest decoded picture for one view window, already encoded
    /// as the binary IPC response of `view_next_frame`.
    ViewFrame {
        label: String,
        /// Timestamp of the picture the caller already has, or 0 for none.
        /// Matching the current frame skips re-serializing its pixels.
        since_us: u64,
        reply: oneshot::Sender<Result<Vec<u8>, ActorError>>,
    },
    /// Guest side: one input event to forward to the host being viewed.
    Input {
        label: String,
        event: Box<InputEventPayload>,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    /// Either side: send one chat message to `label`.
    ChatSend {
        label: String,
        text: String,
        reply: oneshot::Sender<Result<ChatEntry, ActorError>>,
    },
    /// Either side: fetch the chat transcript of `label`.
    ChatTranscript {
        label: String,
        reply: oneshot::Sender<Vec<ChatEntry>>,
    },
    /// Either side: push the local clipboard text to the peer, gated on the
    /// session's clipboard grants (§8.2).
    ClipboardPush {
        label: String,
        text: String,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    /// Either side: fetch the newest inbound clipboard payload, if any.
    ClipboardPull {
        label: String,
        reply: oneshot::Sender<Option<String>>,
    },
}

/// Thin handle IPC commands hold. Cloneable: every command gets its own
/// clone of the sender.
#[derive(Debug, Clone)]
pub struct ActorHandle {
    tx: mpsc::Sender<ActorCommand>,
    notify: broadcast::Sender<ActorNotification>,
    /// Whether the local endpoint has reached a relay and is dialable from
    /// outside the LAN. False from process start; flips true at most once,
    /// when `PeerEndpoint::online()` first resolves (see `spawn_actor`) —
    /// this crate never observes a relay going back offline.
    online: Arc<AtomicBool>,
}

impl ActorHandle {
    /// Stream of [`ActorNotification`]s from this point on.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<ActorNotification> {
        self.notify.subscribe()
    }

    /// Whether this host is currently ready to accept incoming connections
    /// from outside the LAN. Purely a status report for the UI — carries no
    /// authorization of its own (§2.3).
    #[must_use]
    pub fn online(&self) -> bool {
        self.online.load(Ordering::Relaxed)
    }

    /// Snapshot of every pending and active session.
    ///
    /// # Errors
    /// [`ActorError::ChannelClosed`] if the actor task is gone.
    pub async fn status(&self) -> Result<Vec<SessionSnapshot>, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::Status { reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)
    }

    /// Hosts this node has connected to before, most recent first (§21
    /// punch-list item 5).
    ///
    /// # Errors
    /// [`ActorError::ChannelClosed`] if the actor task is gone.
    pub async fn history(&self) -> Result<Vec<HistoryEntry>, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::History { reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)
    }

    /// Dials a remembered host again by its history label.
    ///
    /// # Errors
    /// [`ActorError::UnknownPeer`] if no history row carries that label;
    /// [`ActorError::Net`] if the remembered invite no longer works or the
    /// dial fails; [`ActorError::ChannelClosed`] if the actor is gone.
    pub async fn history_connect(&self, label: String) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::HistoryConnect { label, reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// How this node's own outgoing connect attempt is going.
    ///
    /// # Errors
    /// [`ActorError::ChannelClosed`] if the actor task is gone.
    pub async fn connect_state(&self) -> Result<ConnectPhase, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::ConnectState { reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)
    }

    /// Grants `role` to the peer behind `label`.
    ///
    /// # Errors
    /// [`ActorError::UnknownPeer`] if `label` resolves to nothing;
    /// [`ActorError::Core`] if [`SessionManager::grant`] refuses;
    /// [`ActorError::ChannelClosed`] if the actor task is gone.
    pub async fn grant(&self, label: String, role: Role) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::Grant { label, role, reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Revokes every grant of the peer behind `label`.
    ///
    /// # Errors
    /// Same as [`Self::grant`].
    pub async fn revoke(&self, label: String) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::Revoke { label, reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Issues and registers an invite for `role`.
    ///
    /// # Errors
    /// [`ActorError::Net`] if the ticket cannot be signed/encoded;
    /// [`ActorError::ChannelClosed`] if the actor is gone.
    pub async fn invite_create(&self, role: Role) -> Result<InviteDto, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::InviteCreate { role, reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Parses `ticket` and dials the host it names.
    ///
    /// # Errors
    /// [`ActorError::Net`] if the ticket is malformed or the dial/handshake
    /// fails; [`ActorError::ChannelClosed`] if the actor is gone.
    pub async fn invite_connect(&self, ticket: String) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::InviteConnect { ticket, reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Newest picture of the view onto `label`, as the raw bytes
    /// `view_next_frame` hands to the webview.
    ///
    /// `since_us` is the timestamp of the picture the caller already has, or
    /// 0 for none; the pixel payload is omitted when it already matches the
    /// current frame (§15: a caller polling faster than the video updates
    /// should not pay for re-serializing an unchanged picture).
    ///
    /// # Errors
    /// [`ActorError::UnknownPeer`] if no view window belongs to `label`;
    /// [`ActorError::ChannelClosed`] if the actor is gone.
    pub async fn view_frame(&self, label: String, since_us: u64) -> Result<Vec<u8>, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::ViewFrame {
                label,
                since_us,
                reply,
            })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Forwards one input event to the host behind `label`.
    ///
    /// The guest drops it if its own copy of the grant no longer carries
    /// `input`; the host checks again, authoritatively, per event (§2.3, §8.1).
    ///
    /// # Errors
    /// [`ActorError::UnknownPeer`] if no view window belongs to `label`;
    /// [`ActorError::Core`] with [`CoreError::NotPermitted`] if the session
    /// holds no `input` grant; [`ActorError::ChannelClosed`] if the actor is
    /// gone.
    /// Sends one chat message to the session with `label` and returns the
    /// stored transcript entry.
    ///
    /// # Errors
    /// [`ActorError::UnknownPeer`] / [`ActorError::Core`] as refused;
    /// [`ActorError::ChannelClosed`] if the actor task is gone.
    pub async fn chat_send(&self, label: String, text: String) -> Result<ChatEntry, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::ChatSend { label, text, reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// The chat transcript of `label`, oldest first.
    ///
    /// # Errors
    /// [`ActorError::ChannelClosed`] if the actor task is gone.
    pub async fn chat_transcript(&self, label: String) -> Result<Vec<ChatEntry>, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::ChatTranscript { label, reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)
    }

    /// Pushes the local clipboard to `label` (grant-gated, §8.2).
    ///
    /// # Errors
    /// [`ActorError::Core::NotPermitted`] without the clipboard grant;
    /// [`ActorError::ChannelClosed`] if the actor task is gone.
    pub async fn clipboard_push(&self, label: String, text: String) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::ClipboardPush { label, text, reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Takes the newest inbound clipboard payload from `label`, if any.
    /// Pull semantics keep payloads off the broadcast bus (§15).
    ///
    /// # Errors
    /// [`ActorError::ChannelClosed`] if the actor task is gone.
    pub async fn clipboard_pull(&self, label: String) -> Result<Option<String>, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::ClipboardPull { label, reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)
    }

    pub async fn input(&self, label: String, event: InputEventPayload) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::Input {
                label,
                event: Box::new(event),
                reply,
            })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }
}

/// Current Unix time in whole seconds; 0 if the clock is before the epoch.
fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Pseudonymized peer identifier, safe for logs and for the webview (§15).
///
/// The same value is used as the IPC label and as the `tracing` field, so an
/// operator reading the log can line a warning up with a row in the UI without
/// either side ever seeing a raw `NodeId`.
fn peer_tag(install_salt: &[u8; 32], peer: &NodeId) -> String {
    let hash = lumepeer_core::audit::peer_hash(install_salt, peer);
    hex_prefix(&hash)
}

/// Fixed domain-separation salt for [`host_tag`]. Not a secret and not an
/// install salt: its only job is to keep this hash from colliding with the
/// per-run `peer_tag` namespace.
const HOST_LABEL_SALT: [u8; 32] = *b"lumepeer/connection-history/v1\0\0";

/// Pseudonymized label of a *host this node connected to*, stable across
/// restarts (§15, ADR 0016).
///
/// [`peer_tag`]'s install salt is deliberately not used here. That salt is
/// regenerated on every start precisely so a guest's label cannot be
/// correlated across runs — right for someone else appearing in this host's
/// UI, and exactly wrong for the list of hosts this user chose to connect to
/// and expects to recognize tomorrow. It is still a one-way hash: no raw
/// `NodeId` reaches the webview or the history file.
fn host_tag(peer: &NodeId) -> String {
    let hash = lumepeer_core::audit::peer_hash(&HOST_LABEL_SALT, peer);
    hex_prefix(&hash)
}

fn hex_prefix(hash: &[u8; 32]) -> String {
    hash[..8].iter().fold(String::new(), |mut out, byte| {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// The actor's end of one live control connection.
struct ConnectionHandle {
    /// Distinguishes generations of connection to the same peer, so a stale
    /// `Closed` event cannot tear down a freshly established replacement.
    id: u64,
    outbound: mpsc::Sender<MessageKind>,
    /// Kept so this side can close the QUIC connection outright. Dropping the
    /// outbound sender alone only ends the writer task; the reader would sit
    /// in `recv` until the far end noticed.
    connection: iroh::endpoint::Connection,
}

/// Host side: one running capture/encode loop, plus the connection it writes
/// on, so a revoke can stop both without waiting for the loop to notice.
struct MediaSession {
    task: tokio::task::JoinHandle<()>,
    connection: iroh::endpoint::Connection,
}

impl MediaSession {
    /// Stops the loop and closes the media connection.
    fn stop(self) {
        self.task.abort();
        self.connection.close(
            lumepeer_net::connection::CLOSE_MALFORMED.into(),
            lumepeer_net::error::close_code::MALFORMED.as_bytes(),
        );
    }
}

/// Guest side: one open remote-view window and the pipeline feeding it.
struct ViewState {
    /// Tauri window label, `view-{peer label}`.
    label: String,
    /// Role the host announced, kept so the history row written when this view
    /// closes says what was actually granted.
    role: Role,
    /// Grants as the host last announced them. Advisory on this side: the host
    /// re-checks every event (§2.3).
    grants: Grants,
    /// Single-slot newest picture plus pipeline health. Dropping this receiver
    /// is also how the media task learns the view is gone.
    slot: watch::Receiver<ViewSlot>,
    task: tokio::task::JoinHandle<()>,
}

/// What a spawned per-connection task reports back to the main loop.
enum ActorEvent {
    /// A guest's `Hello` passed the handshake and the ed25519 signature
    /// check; the ticket still needs `TicketRegistry::claim` before any
    /// consent is queued, which only the actor's own thread can do.
    Handshaked {
        connection: Box<ControlConnection>,
        peer: NodeId,
        ticket: InviteTicket,
    },
    /// A live connection delivered a control message.
    Inbound {
        peer: NodeId,
        id: u64,
        kind: MessageKind,
    },
    /// A live connection's stream closed or errored.
    Closed { peer: NodeId, id: u64 },
    /// An incoming `rd/media/1` connection finished its QUIC handshake. It has
    /// proven nothing beyond its `NodeId`; whether it may exist at all is a
    /// question only the actor can answer, since only the actor knows which
    /// peers hold a live, granted control session (§4.1).
    MediaAccepted {
        connection: Box<iroh::endpoint::Connection>,
        peer: NodeId,
    },
}

/// Outcome of one accepted incoming connection, before the actor sees it.
enum Accepted {
    /// Control ALPN: handshake ran and the invite ticket verified.
    Control {
        connection: Box<ControlConnection>,
        peer: NodeId,
        ticket: Box<InviteTicket>,
    },
    /// Media ALPN: authenticated only, nothing decided.
    Media {
        connection: Box<iroh::endpoint::Connection>,
        peer: NodeId,
    },
}

/// Runtime state the actor owns and loops over.
struct Actor {
    rx: mpsc::Receiver<ActorCommand>,
    sessions: SessionManager,
    /// Per-process salt for pseudonymized labels (§15): regenerated on every
    /// start, so a label is stable within a run and meaningless across runs.
    install_salt: [u8; 32],
    /// label -> `NodeId`, rebuilt on every command that changes session state.
    labels: std::collections::HashMap<String, NodeId>,
    endpoint: PeerEndpoint,
    identity: SigningKey,
    tickets: TicketRegistry,
    /// Live control connections, keyed by peer, so `Grant`/`Revoke` can
    /// write `ConsentGrant`/`ConsentRevoke` back to the right stream.
    connections: std::collections::HashMap<NodeId, ConnectionHandle>,
    next_connection_id: u64,
    /// Caps concurrent handshakes so a flood of half-open connections cannot
    /// spawn unbounded tasks (§3.2).
    handshake_slots: Arc<Semaphore>,
    /// Handshake results, inbound messages and stream-closed notifications
    /// from the per-connection tasks.
    events_tx: mpsc::Sender<ActorEvent>,
    events_rx: mpsc::Receiver<ActorEvent>,
    notify: broadcast::Sender<ActorNotification>,
    /// Host side: the single "capture only with a viewer" gate of §8.1/§11,
    /// shared with every encode loop.
    capture: SharedCapture,
    /// Host side: one encode loop per peer currently receiving video.
    media: std::collections::HashMap<NodeId, MediaSession>,
    /// Host side: platform input adapter, opened on the first authorized event
    /// so a host that never grants `input` never touches it.
    injector: Option<Box<dyn InputInjector>>,
    /// Guest side: one open view window per host being watched.
    views: std::collections::HashMap<NodeId, ViewState>,
    /// Guest side: dialable address per host, remembered from its invite so the
    /// media dial does not have to wait for discovery.
    host_addrs: std::collections::HashMap<NodeId, iroh::EndpointAddr>,
    /// Guest side: the invite code used to reach each host, kept so the history
    /// row written when the session ends can dial it again (ADR 0016).
    host_invites: std::collections::HashMap<NodeId, String>,
    /// Per-peer chat transcripts (§9.2), dropped when the session ends.
    chat: ChatLog,
    /// Per-peer clipboard sync state (§9.2): echo suppression both ways.
    clipboard: std::collections::HashMap<NodeId, ClipboardSync>,
    /// Newest inbound clipboard payload per peer, for the UI to pull.
    clipboard_inbound: std::collections::HashMap<NodeId, String>,
    /// How view windows are created and closed.
    windows: Arc<dyn ViewWindows>,
    /// Guest side: hosts this node has connected to before (§21 punch-list
    /// item 5). Nothing is recorded on the host side — see
    /// `connection_history`'s module docs.
    history: ConnectionHistory,
    /// Guest side: phase of this node's own outgoing connect attempt.
    connect_phase: ConnectPhase,
    /// Host the phase above is about, once the dial has resolved one.
    connect_peer: Option<NodeId>,
}

impl Actor {
    fn label_of(&self, peer: &NodeId) -> String {
        peer_tag(&self.install_salt, peer)
    }

    fn resolve(&self, label: &str) -> Result<NodeId, ActorError> {
        self.labels
            .get(label)
            .copied()
            .ok_or(ActorError::UnknownPeer)
    }

    /// Rebuilds the label table from current pending + active peers, and
    /// returns the snapshot list in the same pass.
    ///
    /// A session in `Reconnecting` is deliberately omitted: `SessionManager::
    /// active()` still returns it (it holds its slot against the plan ceiling
    /// for the reconnect window of §10), but its transport is gone, so showing
    /// it as `Active` would tell the host user that a guest is watching when
    /// nobody is.
    fn rebuild_labels_and_snapshot(&mut self) -> Vec<SessionSnapshot> {
        self.labels.clear();
        let mut out = Vec::new();
        for ticket in self.sessions.pending() {
            let label = peer_tag(&self.install_salt, &ticket.peer);
            self.labels.insert(label.clone(), ticket.peer);
            out.push(SessionSnapshot {
                label,
                state: SessionStateDto::Pending,
                role: ticket.requested_role,
                input: false,
            });
        }
        for (peer, role, grants) in self.sessions.active() {
            if self.sessions.state(&peer) != SessionState::Active {
                continue;
            }
            let label = peer_tag(&self.install_salt, &peer);
            self.labels.insert(label.clone(), peer);
            out.push(SessionSnapshot {
                label,
                state: SessionStateDto::Active,
                role,
                input: grants.input,
            });
        }
        // Guest side: a host being watched has no entry in this node's own
        // `SessionManager` (it is not *our* guest), but its view window still
        // has to be able to name it over IPC — `view_next_frame`, the input
        // commands and the window's own close/revoke all go through a label.
        for peer in self.views.keys().copied().collect::<Vec<_>>() {
            let label = peer_tag(&self.install_salt, &peer);
            self.labels.insert(label, peer);
        }
        out
    }

    async fn run(mut self) {
        loop {
            tokio::select! {
                command = self.rx.recv() => {
                    let Some(command) = command else { break };
                    self.handle_command(command).await;
                }
                // Single await, so losing this race cannot drop a connection
                // that is already half accepted.
                incoming = self.endpoint.accept_incoming() => {
                    let Some(incoming) = incoming else { break };
                    self.spawn_handshake(incoming);
                }
                event = self.events_rx.recv() => {
                    if let Some(event) = event {
                        self.handle_event(event);
                    }
                }
            }
        }
    }

    /// Finishes the QUIC handshake, checks the ALPN and runs the control
    /// handshake, all on its own task and under one deadline (§9.1, §18).
    fn spawn_handshake(&self, incoming: iroh::endpoint::Incoming) {
        let Ok(permit) = Arc::clone(&self.handshake_slots).try_acquire_owned() else {
            tracing::warn!(
                limit = MAX_INFLIGHT_HANDSHAKES,
                "refusing an incoming connection: handshake slots exhausted"
            );
            drop(incoming);
            return;
        };
        let tx = self.events_tx.clone();
        let verifying_key = self.identity.verifying_key();
        let salt = self.install_salt;
        tokio::spawn(async move {
            let _permit = permit;
            let deadline = std::time::Duration::from_secs(CONTROL_HANDSHAKE_TIMEOUT_SECS);
            let Ok(outcome) = tokio::time::timeout(deadline, async move {
                let connection = PeerEndpoint::finish_accept(incoming).await.ok()?;
                let peer = connection.remote_id();
                let tag = peer_tag(&salt, &peer);
                match Channel::from_alpn(connection.alpn()) {
                    Some(Channel::Control) => {}
                    // Media is authenticated here and authorized by the actor:
                    // this task cannot see whether the peer holds a live,
                    // granted control session, and guessing would be a way to
                    // widen a grant outside `lumepeer-core` (§2.3).
                    Some(Channel::Media) => {
                        return Some(Accepted::Media {
                            connection: Box::new(connection),
                            peer,
                        });
                    }
                    // An unauthenticated peer must not be able to park a file
                    // connection in the control handshake's read (§4.1).
                    Some(Channel::File) | None => {
                        tracing::warn!(peer = %tag, "closing a non-control connection");
                        connection.close(
                            lumepeer_net::connection::CLOSE_MALFORMED.into(),
                            lumepeer_net::error::close_code::MALFORMED.as_bytes(),
                        );
                        return None;
                    }
                }
                let (control, hello) = match lumepeer_net::host_handshake(connection).await {
                    Ok(pair) => pair,
                    Err(error) => {
                        tracing::warn!(peer = %tag, %error, "control handshake failed");
                        return None;
                    }
                };
                let Ok(ticket) = postcard::from_bytes::<InviteTicket>(&hello.invite_proof) else {
                    tracing::warn!(peer = %tag, "invite proof is not a ticket");
                    control.close_with(&NetError::InvalidTicket);
                    return None;
                };
                if let Err(error) = ticket.verify(&verifying_key, unix_now()) {
                    tracing::warn!(peer = %tag, %error, "invite ticket did not verify");
                    control.close_with(&NetError::InvalidTicket);
                    return None;
                }
                Some(Accepted::Control {
                    connection: Box::new(control),
                    peer,
                    ticket: Box::new(ticket),
                })
            })
            .await
            else {
                tracing::warn!(
                    timeout_secs = CONTROL_HANDSHAKE_TIMEOUT_SECS,
                    "dropping an incoming connection that did not finish its handshake in time"
                );
                return;
            };
            let event = match outcome {
                Some(Accepted::Control {
                    connection,
                    peer,
                    ticket,
                }) => ActorEvent::Handshaked {
                    connection,
                    peer,
                    ticket: *ticket,
                },
                Some(Accepted::Media { connection, peer }) => {
                    ActorEvent::MediaAccepted { connection, peer }
                }
                None => return,
            };
            let _ = tx.send(event).await;
        });
    }

    /// Takes ownership of an authenticated connection: the reader half runs as
    /// its own task, the writer half is driven from the actor's outbound
    /// channel. Neither can cancel the other mid-frame.
    ///
    /// This is what makes a stored connection *live* rather than merely
    /// retained: without the reader, nothing would ever observe a
    /// `ConsentGrant` on the guest side or a closed stream on either side.
    fn adopt(&mut self, connection: ControlConnection, peer: NodeId) {
        self.next_connection_id = self.next_connection_id.wrapping_add(1);
        let id = self.next_connection_id;
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<MessageKind>(8);
        let quic = connection.connection().clone();
        let (mut reader, mut writer) = connection.split();
        let tag = self.label_of(&peer);

        let events = self.events_tx.clone();
        let read_tag = tag.clone();
        tokio::spawn(async move {
            loop {
                match reader.recv().await {
                    Ok(envelope) => {
                        if events
                            .send(ActorEvent::Inbound {
                                peer,
                                id,
                                kind: envelope.kind,
                            })
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(error) => {
                        tracing::debug!(peer = %read_tag, %error, "control stream ended");
                        let _ = events.send(ActorEvent::Closed { peer, id }).await;
                        return;
                    }
                }
            }
        });

        tokio::spawn(async move {
            while let Some(kind) = outbound_rx.recv().await {
                if let Err(error) = writer.send(kind).await {
                    tracing::warn!(peer = %tag, %error, "control send failed");
                    return;
                }
            }
        });

        // Replacing an entry drops the old outbound sender, which ends that
        // generation's writer task; its reader reports `Closed` with the older
        // id, which `handle_event` ignores.
        self.connections.insert(
            peer,
            ConnectionHandle {
                id,
                outbound: outbound_tx,
                connection: quic,
            },
        );
    }

    /// Queues one message on a peer's connection without ever blocking the
    /// actor loop on a slow or stalled writer.
    fn send_to(&mut self, peer: &NodeId, kind: MessageKind) {
        let Some(handle) = self.connections.get(peer) else {
            return;
        };
        if handle.outbound.try_send(kind).is_err() {
            tracing::warn!(
                peer = %peer_tag(&self.install_salt, peer),
                "dropping a control message: the connection is gone or backed up"
            );
        }
    }

    fn handle_event(&mut self, event: ActorEvent) {
        match event {
            ActorEvent::Handshaked {
                connection,
                peer,
                ticket,
            } => self.on_handshaked(*connection, peer, &ticket),
            ActorEvent::Inbound { peer, id, kind } => self.on_inbound(peer, id, &kind),
            ActorEvent::Closed { peer, id } => self.on_closed(peer, id),
            ActorEvent::MediaAccepted { connection, peer } => {
                self.on_media_accepted(*connection, peer);
            }
        }
    }

    /// Host side: a media connection may exist only for a peer that already
    /// holds a live, granted control session with a `view` grant (§4.1, §8.1).
    ///
    /// Deny-by-default: everything else is closed, including a peer whose
    /// control session is merely pending or already revoked. The check is made
    /// here, on the actor's own thread, because this is the only place that can
    /// read `SessionManager` — a media connection must never be able to
    /// authorize itself.
    fn on_media_accepted(&mut self, connection: iroh::endpoint::Connection, peer: NodeId) {
        let tag = self.label_of(&peer);
        let granted = self.connections.contains_key(&peer)
            && self.sessions.state(&peer) == SessionState::Active
            && self.sessions.grants(&peer).is_some_and(|g| g.view);
        if !granted {
            tracing::warn!(peer = %tag, "refusing a media connection without a granted view session");
            connection.close(
                lumepeer_net::connection::CLOSE_MALFORMED.into(),
                lumepeer_net::error::close_code::MALFORMED.as_bytes(),
            );
            return;
        }
        // A redial replaces the previous stream rather than adding a second
        // encode loop against the same capture.
        if let Some(previous) = self.media.remove(&peer) {
            previous.stop();
        }
        tracing::info!(peer = %tag, "media connection accepted; starting the encode loop");
        let task = spawn_encode_loop(connection.clone(), Arc::clone(&self.capture), tag);
        self.media.insert(peer, MediaSession { task, connection });
    }

    /// Host side: stops sending video to `peer` and drops it as a viewer, which
    /// stops capture altogether if it was the last one (§8.1, §11).
    fn stop_media(&mut self, peer: NodeId) {
        if let Some(session) = self.media.remove(&peer) {
            session.stop();
        }
        lock_capture(&self.capture).remove_viewer(&peer);
    }

    /// Guest side: opens the view window for a host that just granted, or
    /// refreshes the grants of one already open.
    fn start_view(&mut self, peer: NodeId, role: Role) {
        let grants = Grants::from_role(role);
        let tag = self.label_of(&peer);
        if !grants.view {
            self.stop_view(peer);
            return;
        }
        // A second `ConsentGrant` on a live view is a role change, not a new
        // window: keep the pipeline, update what the window may do.
        if let Some(state) = self.views.get_mut(&peer) {
            state.grants = grants;
            state.role = role;
            tracing::info!(peer = %tag, input = grants.input, "view grants updated");
            return;
        }
        let Some(addr) = self.host_addrs.get(&peer).cloned() else {
            tracing::warn!(peer = %tag, "no remembered address for this host: cannot open media");
            return;
        };

        let label = window_label(&tag);
        let (slot_tx, slot_rx) = watch::channel(ViewSlot::waiting());
        let task = spawn_media_receiver(
            MediaTarget {
                endpoint: self.endpoint.clone(),
                addr,
                tag: tag.clone(),
                worker: None,
            },
            slot_tx,
        );
        self.views.insert(
            peer,
            ViewState {
                label: label.clone(),
                role,
                grants,
                slot: slot_rx,
                task,
            },
        );
        self.windows.open(&label, &tag, grants.input);
        self.rebuild_labels_and_snapshot();
    }

    /// Guest side: closes the view window, tears the pipeline down, and
    /// remembers the host (§21 punch-list item 5, ADR 0016).
    ///
    /// Every way a session this node started can end runs through here — the
    /// host revoking, the transport dropping, the operator closing the window
    /// — so this is the one place the remembered-hosts list has to be written,
    /// and it is written on the side that dialed rather than the side that was
    /// dialed.
    fn stop_view(&mut self, peer: NodeId) {
        let Some(state) = self.views.remove(&peer) else {
            return;
        };
        // Dropping the receiver already tells the media task to stop; aborting
        // makes sure a decoder does not outlive the session it belonged to
        // (§8.1).
        state.task.abort();
        drop(state.slot);
        self.windows.close(&state.label);
        if let Some(code) = self.host_invites.get(&peer).cloned() {
            self.history.record(host_tag(&peer), state.role, code);
        }
        tracing::info!(peer = %self.label_of(&peer), "view window closed");
    }

    /// Drops this node's control connection to `peer` outright.
    ///
    /// Dropping the outbound sender alone only ends the writer task; the far
    /// end would sit in `recv` until it noticed by itself.
    fn close_connection(&mut self, peer: NodeId) {
        if let Some(handle) = self.connections.remove(&peer) {
            handle.connection.close(
                lumepeer_net::connection::CLOSE_MALFORMED.into(),
                lumepeer_net::error::close_code::MALFORMED.as_bytes(),
            );
        }
    }

    /// Guest side: moves the connect form out of its pending state when the
    /// host it was waiting on resolves, one way or the other.
    ///
    /// `outcome` is what to report if the host never granted. Once it has,
    /// the form has nothing left to wait for, so a session ending after that
    /// is simply idle rather than a failure worth putting on screen.
    fn settle_connect(&mut self, peer: NodeId, outcome: ConnectPhase) {
        if self.connect_peer != Some(peer) {
            return;
        }
        if outcome == ConnectPhase::Connected {
            self.connect_phase = ConnectPhase::Connected;
            return;
        }
        self.connect_phase = if self.connect_phase.is_pending() {
            outcome
        } else {
            ConnectPhase::Idle
        };
        self.connect_peer = None;
    }

    /// Host side: injects one authorized input event (§11).
    ///
    /// The authorization is re-taken per event rather than once at grant time,
    /// so a `session_grant` that lowered the role in between takes effect on
    /// the very next event.
    fn inject(&mut self, peer: NodeId, event: &InputEventPayload) {
        let tag = self.label_of(&peer);
        if let Err(error) = self.sessions.authorize_input(&peer, event) {
            tracing::warn!(peer = %tag, %error, "dropping an unauthorized input event");
            return;
        }
        if self.injector.is_none() {
            match platform_injector() {
                Ok(injector) => self.injector = Some(injector),
                Err(error) => {
                    // §18: the session degrades to view-only and says so in the
                    // log rather than failing the session.
                    tracing::warn!(peer = %tag, %error, "no input adapter: staying view-only");
                    return;
                }
            }
        }
        if let Some(injector) = self.injector.as_mut()
            && let Err(error) = injector.inject(event)
        {
            tracing::warn!(peer = %tag, %error, "input injection failed");
        }
    }

    fn on_handshaked(
        &mut self,
        connection: ControlConnection,
        peer: NodeId,
        ticket: &InviteTicket,
    ) {
        let tag = self.label_of(&peer);
        // Single-use enforcement runs here, on the actor's own thread, so two
        // connections racing the same ticket cannot both win it.
        if let Err(error) = self.tickets.claim(ticket, unix_now()) {
            tracing::warn!(peer = %tag, %error, "invite claim refused");
            connection.close_with(&NetError::InvalidTicket);
            return;
        }
        // Every connection, first time or reconnect, gets a fresh decision.
        if let Err(error) = self
            .sessions
            .request_consent_as(peer, ticket.allowed_request)
        {
            tracing::warn!(peer = %tag, %error, "cannot queue a consent request");
            // The ticket is already burned and nobody will ever decide on this
            // peer, so the connection must not linger: close it here, before
            // it is ever stored.
            connection.close_with(&NetError::ConsentUnavailable);
            return;
        }
        tracing::info!(peer = %tag, "consent request queued");
        self.adopt(connection, peer);
        let _ = self.notify.send(ActorNotification::ConsentRequested);
        self.rebuild_labels_and_snapshot();
    }

    fn on_inbound(&mut self, peer: NodeId, id: u64, kind: &MessageKind) {
        if self.connections.get(&peer).is_none_or(|c| c.id != id) {
            return;
        }
        let tag = self.label_of(&peer);
        match *kind {
            // Guest side: the host decided.
            MessageKind::ConsentGrant(role) => {
                tracing::info!(peer = %tag, ?role, "remote host granted consent");
                let _ = self.notify.send(ActorNotification::ConsentGranted { role });
                // Reacted to here rather than through the notification stream:
                // `ActorNotification` deliberately carries no peer identity
                // (§15), and opening a media connection needs to know exactly
                // which host granted.
                self.settle_connect(peer, ConnectPhase::Connected);
                self.start_view(peer, role);
            }
            MessageKind::ConsentRevoke => {
                tracing::info!(peer = %tag, "remote host revoked consent");
                let _ = self.notify.send(ActorNotification::ConsentRevoked);
                // A revoke while the request is still pending is the host
                // pressing Deny; after a grant it is an ordinary end of
                // session, and the connect form should just go quiet.
                let dialed = self.host_addrs.contains_key(&peer);
                self.settle_connect(peer, ConnectPhase::Denied);
                self.stop_view(peer);
                if dialed {
                    // Guest side: with the grant withdrawn there is nothing
                    // left to say on this connection. Leaving it open would
                    // hold a stream on the host for a session that no longer
                    // exists, and would make the next Connect to the same host
                    // look like the duplicate `connect_with_ticket` refuses.
                    self.close_connection(peer);
                }
            }
            // Host side: a guest asks for input. Authorization is `lumepeer-
            // core`'s, per event.
            MessageKind::InputEvent(ref event) => self.inject(peer, event),
            // Chat: validate, store, tell the UI there is something to pull.
            // A refused message is dropped with a log line — it never closes
            // the session (chat is content, not control).
            MessageKind::Chat { ref text } => {
                let at = unix_now_secs();
                match self.chat.record(peer, false, text, at) {
                    Ok(_) => {
                        let _ = self
                            .notify
                            .send(ActorNotification::ChatFromPeer { label: tag.clone() });
                    }
                    Err(error) => {
                        tracing::warn!(peer = %tag, %error, "dropping an invalid chat message");
                    }
                }
            }
            // Clipboard from the peer: §9.2 validation plus grant check on
            // the receiving side's own copy of the grants; then stage the
            // payload for the UI to apply and pull.
            MessageKind::ClipboardSync { ref data } => {
                let granted_to_guest = self
                    .sessions
                    .grants(&peer)
                    .is_some_and(|g| g.clipboard_read);
                let permitted = self
                    .views
                    .get(&peer)
                    .is_some_and(|v| v.grants.clipboard_write)
                    || (granted_to_guest && !self.views.contains_key(&peer));
                if !permitted {
                    tracing::warn!(peer = %tag, "clipboard update without a grant; ignored");
                    return;
                }
                let sync = self.clipboard.entry(peer).or_default();
                match sync.remote_received(data) {
                    Ok(text) => {
                        self.clipboard_inbound
                            .insert(peer, String::from_utf8_lossy(text).into_owned());
                        let _ = self.notify.send(ActorNotification::ClipboardFromPeer);
                    }
                    Err(error) => {
                        tracing::warn!(peer = %tag, %error, "dropping an invalid clipboard payload");
                    }
                }
            }
            // Everything else belongs to a phase this build does not run yet.
            // Nothing a peer sends may ever grant itself consent (§2.3).
            ref other => tracing::debug!(peer = %tag, ?other, "ignoring a control message"),
        }
    }

    fn on_closed(&mut self, peer: NodeId, id: u64) {
        // Only the current generation may tear the peer's state down.
        if self.connections.get(&peer).is_some_and(|c| c.id != id) {
            return;
        }
        self.connections.remove(&peer);
        // Chat and clipboard state are per-session by design (§15): nothing
        // about a past peer survives its connection here.
        self.chat.drop_transcript(&peer);
        self.clipboard.remove(&peer);
        self.clipboard_inbound.remove(&peer);
        // Both sides of the media pipeline end with the control connection: the
        // host stops capturing for this viewer, the guest closes its window
        // (and, on the guest, records the host it was watching).
        self.stop_media(peer);
        self.settle_connect(peer, ConnectPhase::Failed);
        self.stop_view(peer);
        let label = peer_tag(&self.install_salt, &peer);
        if self.sessions.on_disconnect(peer).is_err() {
            // No active session to move into the reconnect window, so this was
            // a guest that dropped before the host decided: drop its queued
            // request instead of leaving it pending forever.
            let _ = self.sessions.revoke(peer);
        }
        tracing::info!(peer = %label, "peer disconnected");
        let _ = self.notify.send(ActorNotification::Disconnected);
        self.rebuild_labels_and_snapshot();
    }

    async fn handle_command(&mut self, command: ActorCommand) {
        match command {
            ActorCommand::Status { reply } => {
                let snapshot = self.rebuild_labels_and_snapshot();
                let _ = reply.send(snapshot);
            }
            ActorCommand::History { reply } => {
                let _ = reply.send(self.history.entries().to_vec());
            }
            ActorCommand::HistoryConnect { label, reply } => {
                let result = match self.history.code_of(&label).map(ToOwned::to_owned) {
                    Some(code) => self.connect_with_ticket(&code).await,
                    None => Err(ActorError::UnknownPeer),
                };
                if let Err(ActorError::Net(ref error)) = result {
                    tracing::warn!(%error, "reconnecting to a remembered host failed");
                }
                let _ = reply.send(result);
            }
            ActorCommand::ConnectState { reply } => {
                let _ = reply.send(self.connect_phase);
            }
            ActorCommand::Grant { label, role, reply } => {
                let result = self.on_grant(&label, role);
                self.rebuild_labels_and_snapshot();
                let _ = reply.send(result);
            }
            ActorCommand::Revoke { label, reply } => {
                let result = self.on_revoke(&label);
                self.rebuild_labels_and_snapshot();
                let _ = reply.send(result);
            }
            ActorCommand::InviteCreate { role, reply } => {
                let result = self.on_invite_create(role);
                if let Err(ActorError::Net(ref error)) = result {
                    tracing::warn!(%error, "could not issue an invite");
                }
                let _ = reply.send(result);
            }
            ActorCommand::InviteConnect { ticket, reply } => {
                let result = self.connect_with_ticket(&ticket).await;
                if let Err(ActorError::Net(ref error)) = result {
                    tracing::warn!(%error, "invite connect failed");
                }
                let _ = reply.send(result);
            }
            ActorCommand::ViewFrame {
                label,
                since_us,
                reply,
            } => {
                let result = self.resolve(&label).and_then(|peer| {
                    let state = self.views.get(&peer).ok_or(ActorError::UnknownPeer)?;
                    let response = slot_for_poll(&state.slot.borrow(), since_us);
                    Ok(encode_view_response(&response, state.grants.input))
                });
                let _ = reply.send(result);
            }
            ActorCommand::Input {
                label,
                event,
                reply,
            } => {
                let result = self.on_input(&label, *event);
                let _ = reply.send(result);
            }
            ActorCommand::ChatSend { label, text, reply } => {
                let result = self.on_chat_send(&label, &text);
                let _ = reply.send(result);
            }
            ActorCommand::ChatTranscript { label, reply } => {
                let _ = reply.send(self.on_chat_transcript(&label));
            }
            ActorCommand::ClipboardPush { label, text, reply } => {
                let result = self.on_clipboard_push(&label, &text);
                let _ = reply.send(result);
            }
            ActorCommand::ClipboardPull { label, reply } => {
                let _ = reply.send(self.on_clipboard_pull(&label));
            }
        }
    }

    /// Issues an invite for `role`, refusing while the endpoint has no
    /// dialable address (§7).
    fn on_invite_create(&mut self, role: Role) -> Result<InviteDto, ActorError> {
        let now = unix_now();
        let addr = self.endpoint.addr();
        // An invite is only worth anything if it carries somewhere to
        // dial. Until the endpoint has reached a relay (and, with the
        // direct transports cleared, it has nothing else to offer) the
        // address set is empty and the code would be dead on arrival —
        // which is far worse than saying "not yet": the host reads it
        // out, the guest pastes it, and the failure surfaces on the
        // wrong machine.
        if addr.addrs.is_empty() {
            tracing::warn!("refusing to issue an invite: the endpoint has no dialable address yet");
            return Err(ActorError::Net(NetError::Offline));
        }
        tracing::info!(addrs = ?addr.addrs, "issuing an invite");
        let issued = InviteTicket::issue(&self.identity, &addr, role, now);
        match issued {
            Ok(ticket) => match ticket.to_code() {
                Ok(code) => {
                    // Exactly one invite is live at a time: issuing a
                    // replacement is also how the host withdraws the
                    // code it read out earlier (ADR 0016).
                    self.tickets.retire_all();
                    self.tickets.register(&ticket);
                    Ok(InviteDto {
                        code,
                        expires_at: ticket.expires_at,
                    })
                }
                Err(e) => Err(ActorError::Net(e)),
            },
            Err(e) => Err(ActorError::Net(e)),
        }
    }

    /// Transcript of `label`; empty for an unknown label rather than an
    /// error, because a transcript poll races session teardown routinely.
    fn on_chat_transcript(&self, label: &str) -> Vec<ChatEntry> {
        match self.resolve(label) {
            Ok(peer) => self.chat.transcript(&peer).to_vec(),
            Err(_) => Vec::new(),
        }
    }

    /// Takes and clears `label`'s staged inbound clipboard payload.
    fn on_clipboard_pull(&mut self, label: &str) -> Option<String> {
        match self.resolve(label) {
            Ok(peer) => self.clipboard_inbound.remove(&peer),
            Err(_) => None,
        }
    }

    /// Both directions: record and forward one chat message (§9.2).
    ///
    /// The sender's own copy of the grants is advisory here; the receiving
    /// host re-checks what its session state allows before showing anything.
    /// A chat needs no separate grant: it is part of every granted session.
    fn on_chat_send(&mut self, label: &str, text: &str) -> Result<ChatEntry, ActorError> {
        let peer = self.resolve(label)?;
        if !self.connections.contains_key(&peer) {
            return Err(ActorError::UnknownPeer);
        }
        let at = unix_now_secs();
        let stored = self
            .chat
            .record(peer, true, text, at)
            .map_err(ActorError::Core)?
            .clone();
        self.send_to(
            &peer,
            MessageKind::Chat {
                text: text.to_owned(),
            },
        );
        Ok(stored)
    }

    /// Push the local clipboard to `label`, gated on this side's own copy of
    /// the clipboard grants (§8.2): a host pushes only with
    /// `clipboard_read` of that guest session, a guest only with
    /// `clipboard_write` announced by the host. The host re-checks.
    fn on_clipboard_push(&mut self, label: &str, text: &str) -> Result<(), ActorError> {
        use lumepeer_core::clipboard as clip;
        let peer = self.resolve(label)?;
        let permitted = if self.host_addrs.contains_key(&peer) {
            // This node dialed the peer: it is the guest. Sending our
            // clipboard *to* the host writes there, so the grant needed is
            // the one the host gave us for writing its clipboard... which is
            // exactly `clipboard_write`.
            self.views
                .get(&peer)
                .is_some_and(|v| v.grants.clipboard_write)
        } else {
            // Host side: handing our clipboard out reads it, so the session
            // must carry `clipboard_read`.
            self.sessions
                .grants(&peer)
                .is_some_and(|g| g.clipboard_read)
        };
        if !permitted {
            return Err(ActorError::Core(CoreError::NotPermitted));
        }
        clip::validate_payload(text.as_bytes()).map_err(ActorError::Core)?;
        let sync = self.clipboard.entry(peer).or_default();
        if sync
            .local_changed(text.as_bytes())
            .map_err(ActorError::Core)?
            .is_none()
        {
            // Echo of a remote update we applied ourselves: do not send.
            return Ok(());
        }
        self.send_to(
            &peer,
            MessageKind::ClipboardSync {
                data: text.as_bytes().to_vec(),
            },
        );
        Ok(())
    }

    /// Host side: grants `role` and, if it carries `view`, registers the peer
    /// as a viewer — which is what starts capture (§8.1, §11).
    ///
    /// A platform with no capture backend still grants: consent, input and the
    /// control channel work regardless, and the guest is told there is no
    /// picture rather than being refused a session it did ask for (§18).
    fn on_grant(&mut self, label: &str, role: Role) -> Result<(), ActorError> {
        let peer = self.resolve(label)?;
        self.sessions.grant(peer, role).map_err(ActorError::Core)?;
        self.send_to(&peer, MessageKind::ConsentGrant(role));
        if self.sessions.grants(&peer).is_some_and(|g| g.view)
            && let Err(error) = lock_capture(&self.capture).add_viewer(peer)
        {
            tracing::warn!(
                peer = %label,
                %error,
                "consent granted but this platform cannot capture"
            );
        }
        tracing::info!(peer = %label, ?role, "consent granted");
        Ok(())
    }

    /// Both directions of "end this session now".
    ///
    /// On the host it is the revoke of §8.1. On the guest, where the watched
    /// host has no entry in this node's own `SessionManager`, it is the view
    /// window closing: the control connection is dropped, which the host sees
    /// as a disconnect and answers with its own `remove_viewer`.
    fn on_revoke(&mut self, label: &str) -> Result<(), ActorError> {
        let peer = self.resolve(label)?;
        if self.views.contains_key(&peer) {
            tracing::info!(peer = %label, "leaving a session from the view window");
            self.settle_connect(peer, ConnectPhase::Idle);
            // Records the host into the remembered list on its way out.
            self.stop_view(peer);
            self.host_addrs.remove(&peer);
            self.close_connection(peer);
            return Ok(());
        }
        // Nothing is written to the remembered-hosts list here. This branch is
        // the *host* ending a session it granted, and a host keeps no record of
        // who visited it: it decided once, the decision ended with the session,
        // and a list it never asked for is not the app's to build (ADR 0016).
        self.sessions.revoke(peer).map_err(ActorError::Core)?;
        self.send_to(&peer, MessageKind::ConsentRevoke);
        self.stop_media(peer);
        tracing::info!(peer = %label, "consent revoked");
        Ok(())
    }

    /// Guest side: forwards one input event, gated on this node's own copy of
    /// the grant. The host re-checks authoritatively (§2.3).
    fn on_input(&mut self, label: &str, event: InputEventPayload) -> Result<(), ActorError> {
        let peer = self.resolve(label)?;
        let permitted = self
            .views
            .get(&peer)
            .ok_or(ActorError::UnknownPeer)?
            .grants
            .input;
        if !permitted {
            return Err(ActorError::Core(CoreError::NotPermitted));
        }
        self.send_to(&peer, MessageKind::InputEvent(event));
        Ok(())
    }

    /// Guest side: dial the host named by the ticket, run the handshake and
    /// **keep** the connection. Dropping it here would close the QUIC
    /// connection under it and the host's `ConsentGrant` would never arrive.
    async fn connect_with_ticket(&mut self, raw: &str) -> Result<(), ActorError> {
        let ticket = InviteTicket::from_code(raw).map_err(ActorError::Net)?;
        let addr = ticket.endpoint_addr().map_err(ActorError::Net)?;
        // Dialing a host this node is already talking to would replace the live
        // connection in `connections`, and the replacement's own teardown would
        // then close the session that was working. Refusing here is what makes
        // a second Connect harmless rather than destructive (§21 punch-list
        // item 6).
        if self.connections.contains_key(&addr.id) {
            tracing::info!(peer = %self.label_of(&addr.id), "already connected to this host");
            return Err(ActorError::Net(NetError::AlreadyConnected));
        }
        let connection = self
            .endpoint
            .connect_control(addr.clone())
            .await
            .map_err(ActorError::Net)?;
        let proof = postcard::to_allocvec(&ticket)
            .map_err(|_| ActorError::Net(NetError::MalformedTicket))?;
        let control =
            lumepeer_net::guest_handshake(connection, ticket.allowed_request, proof, Vec::new())
                .await
                .map_err(ActorError::Net)?;
        let peer = control.peer();
        tracing::info!(peer = %self.label_of(&peer), "connected to a host, awaiting consent");
        // Remembered for the media dial that follows a `ConsentGrant`: the
        // ticket is the only place this address is known without discovery.
        self.host_addrs.insert(peer, addr);
        // Remembered so the history row written when this session ends can dial
        // the same host again (ADR 0016).
        self.host_invites.insert(peer, raw.to_owned());
        self.connect_phase = ConnectPhase::AwaitingConsent;
        self.connect_peer = Some(peer);
        self.adopt(control, peer);
        Ok(())
    }
}

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
pub async fn spawn_actor(app: tauri::AppHandle) -> Result<ActorHandle, NetError> {
    let store = lumepeer_net::keystore::open()?;
    let secret_key = load_or_create(store.as_ref())?;
    let identity = SigningKey::from_bytes(&secret_key.to_bytes());
    let endpoint = PeerEndpoint::bind(secret_key).await?;
    if lumepeer_net::endpoint::lan_direct_enabled() {
        tracing::info!("transport: relay + direct IP paths (LUMEPEER_LAN_DIRECT is set)");
    } else {
        tracing::info!(
            "transport: relay only — direct IP paths are off, so every session goes over the internet. Set LUMEPEER_LAN_DIRECT=1 to put the LAN path back."
        );
    }
    let history_path = connection_history_path(&app);

    let handle = spawn_actor_with(
        endpoint.clone(),
        identity,
        Arc::new(crate::view::TauriViewWindows::new(app)),
        default_capture(),
        history_path,
    );

    tokio::spawn({
        let online = Arc::clone(&handle.online);
        async move {
            endpoint.online().await;
            online.store(true, Ordering::Relaxed);
            tracing::info!("endpoint reached a relay; invites are dialable from outside the LAN");
        }
    });

    Ok(handle)
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

/// Builds the host's capture controller, falling back to a capturer that
/// produces nothing when the platform has no backend compiled in.
///
/// A missing backend must not stop the app from starting or from accepting a
/// session: consent, input and the rest of the control channel work regardless,
/// and the guest is told there is no picture rather than being disconnected
/// (§18).
#[must_use]
pub fn default_capture() -> SharedCapture {
    let capturer = platform_capturer().unwrap_or_else(|error| {
        tracing::warn!(%error, "no capture backend on this platform: sessions stay blank");
        Box::new(StubCapturer::default())
    });
    Arc::new(std::sync::Mutex::new(CaptureController::new(
        capturer,
        CaptureTarget::PrimaryDisplay,
    )))
}

/// Spawns the actor over an already bound endpoint. Split out of
/// [`spawn_actor`] so the loop can be driven in tests without a keystore, a
/// relay or a Tauri window: `windows` and `capture` are the two seams that
/// would otherwise need one.
#[must_use]
pub fn spawn_actor_with(
    endpoint: PeerEndpoint,
    identity: SigningKey,
    windows: Arc<dyn ViewWindows>,
    capture: SharedCapture,
    history_path: Option<std::path::PathBuf>,
) -> ActorHandle {
    let (tx, rx) = mpsc::channel(32);
    let (events_tx, events_rx) = mpsc::channel(32);
    let (notify, _) = broadcast::channel(NOTIFY_CAPACITY);
    let mut install_salt = [0u8; 32];
    rand::rng().fill_bytes(&mut install_salt);
    let actor = Actor {
        rx,
        sessions: SessionManager::new(),
        install_salt,
        labels: std::collections::HashMap::new(),
        endpoint,
        identity,
        tickets: TicketRegistry::new(),
        connections: std::collections::HashMap::new(),
        next_connection_id: 0,
        handshake_slots: Arc::new(Semaphore::new(MAX_INFLIGHT_HANDSHAKES)),
        events_tx,
        events_rx,
        notify: notify.clone(),
        capture,
        media: std::collections::HashMap::new(),
        injector: None,
        views: std::collections::HashMap::new(),
        host_addrs: std::collections::HashMap::new(),
        host_invites: std::collections::HashMap::new(),
        chat: ChatLog::new(),
        clipboard: std::collections::HashMap::new(),
        clipboard_inbound: std::collections::HashMap::new(),
        windows,
        history: ConnectionHistory::open(history_path),
        connect_phase: ConnectPhase::Idle,
        connect_peer: None,
    };
    tokio::spawn(actor.run());
    ActorHandle {
        tx,
        notify,
        online: Arc::new(AtomicBool::new(false)),
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "a failed assumption must fail the test"
    )]

    use std::time::Duration;

    use lumepeer_media::capture::{Frame, InputCapability, ScreenCapturer};
    use lumepeer_media::error::{MediaError, Result as MediaResult};

    use super::*;
    use crate::view::DetachedViewWindows;

    /// Anything slower than this on loopback means the test is stuck.
    const TIMEOUT: Duration = Duration::from_secs(20);

    /// Capturer that never produces a picture but does start and stop, so the
    /// viewer bookkeeping of `CaptureController` is exercised on a machine with
    /// no capture backend compiled in.
    #[derive(Debug, Default)]
    struct SilentCapturer {
        running: bool,
    }

    impl ScreenCapturer for SilentCapturer {
        fn start(&mut self, _target: CaptureTarget) -> MediaResult<()> {
            self.running = true;
            Ok(())
        }

        fn next_frame(&mut self) -> MediaResult<Option<Frame>> {
            if self.running {
                Ok(None)
            } else {
                Err(MediaError::CaptureUnavailable("stopped".to_owned()))
            }
        }

        fn stop(&mut self) {
            self.running = false;
        }

        fn input_capability(&self) -> InputCapability {
            InputCapability::None
        }
    }

    fn test_capture() -> SharedCapture {
        Arc::new(std::sync::Mutex::new(CaptureController::new(
            Box::new(SilentCapturer::default()),
            CaptureTarget::PrimaryDisplay,
        )))
    }

    /// [`ViewWindows`] that records what the actor asked for, so the guest side
    /// of a grant can be asserted without a Tauri runtime.
    #[derive(Debug, Default)]
    struct RecordingWindows {
        opened: std::sync::Mutex<Vec<(String, String, bool)>>,
        closed: std::sync::Mutex<Vec<String>>,
    }

    impl RecordingWindows {
        fn opened(&self) -> Vec<(String, String, bool)> {
            self.opened.lock().unwrap().clone()
        }

        fn closed(&self) -> Vec<String> {
            self.closed.lock().unwrap().clone()
        }
    }

    impl ViewWindows for RecordingWindows {
        fn open(&self, label: &str, peer_label: &str, input: bool) {
            self.opened
                .lock()
                .unwrap()
                .push((label.to_owned(), peer_label.to_owned(), input));
        }

        fn close(&self, label: &str) {
            self.closed.lock().unwrap().push(label.to_owned());
        }
    }

    async fn actor() -> (ActorHandle, PeerEndpoint, SharedCapture) {
        let (handle, endpoint, capture, _windows) =
            actor_with_windows(Arc::new(DetachedViewWindows)).await;
        (handle, endpoint, capture)
    }

    async fn actor_with_windows(
        windows: Arc<dyn ViewWindows>,
    ) -> (
        ActorHandle,
        PeerEndpoint,
        SharedCapture,
        Arc<dyn ViewWindows>,
    ) {
        let secret = iroh::SecretKey::generate();
        let identity = SigningKey::from_bytes(&secret.to_bytes());
        let endpoint = PeerEndpoint::bind_local(secret).await.unwrap();
        let capture = test_capture();
        let handle = spawn_actor_with(
            endpoint.clone(),
            identity,
            Arc::clone(&windows),
            Arc::clone(&capture),
            None,
        );
        (handle, endpoint, capture, windows)
    }

    /// Polls until `predicate` holds, or fails the test.
    async fn wait_until(what: &str, mut predicate: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            if predicate() {
                return;
            }
            assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    fn viewers(capture: &SharedCapture) -> usize {
        lock_capture(capture).viewer_count()
    }

    fn pointer_event(x: u16, y: u16) -> InputEventPayload {
        use lumepeer_core::protocol::InputDetail;
        InputEventPayload {
            logical: 0,
            scancode: 0,
            modifiers: 0,
            detail: InputDetail::PointerMove { x, y },
        }
    }

    /// Polls the remembered-hosts list until it has an entry, or fails.
    async fn wait_for_history(handle: &ActorHandle, what: &str) -> Vec<HistoryEntry> {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            let entries = handle.history().await.unwrap();
            if !entries.is_empty() {
                return entries;
            }
            assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Polls the connect phase the way the connect form does, until it reaches
    /// `want`.
    async fn wait_for_phase(handle: &ActorHandle, want: ConnectPhase) {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            let phase = handle.connect_state().await.unwrap();
            if phase == want {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "connect phase stuck at {phase:?}, wanted {want:?}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Polls `status` the way the frontend does until a pending row shows up.
    async fn wait_for_pending(handle: &ActorHandle) -> String {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            let rows = handle.status().await.unwrap();
            if let Some(row) = rows.iter().find(|r| r.state == SessionStateDto::Pending) {
                return row.label.clone();
            }
            assert!(tokio::time::Instant::now() < deadline, "no pending session");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// C1: the guest actor must keep its `ControlConnection` after the
    /// handshake and keep reading it, or the host's `ConsentGrant` is never
    /// observed. Both sides run as real actors, driven only through the same
    /// handle the Tauri commands use.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_guest_actor_observes_the_hosts_grant_and_revoke() {
        let (host, _host_endpoint, _host_capture) = actor().await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;
        let mut events = guest.subscribe();

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();

        let label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();
        host.grant(label.clone(), Role::ViewOnly).await.unwrap();

        let granted = tokio::time::timeout(TIMEOUT, events.recv()).await.unwrap();
        assert_eq!(
            granted.unwrap(),
            ActorNotification::ConsentGranted {
                role: Role::ViewOnly
            }
        );

        let rows = host.status().await.unwrap();
        assert!(rows.iter().any(|r| r.state == SessionStateDto::Active));

        host.revoke(label).await.unwrap();
        let revoked = tokio::time::timeout(TIMEOUT, events.recv()).await.unwrap();
        assert_eq!(revoked.unwrap(), ActorNotification::ConsentRevoked);
    }

    /// I3: when the guest goes away before the host decides, the host's reader
    /// task must notice and the pending row must disappear.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_guest_that_leaves_before_the_grant_stops_being_pending() {
        let (host, _host_endpoint, _host_capture) = actor().await;
        let (guest, guest_endpoint, _guest_capture) = actor().await;

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        let label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();

        guest_endpoint.close().await;

        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            if host.status().await.unwrap().is_empty() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the pending entry for {label} survived the disconnect"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// I7: a connection on a non-control ALPN must never reach the control
    /// handshake, and it must not become a pending consent request.
    ///
    /// Media is now an accepted ALPN, so this also pins the rule that makes
    /// accepting it safe: without a granted control session behind the same
    /// `NodeId` the connection is still closed, and no capture starts.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_media_alpn_connection_is_refused_without_a_handshake() {
        let (host, host_endpoint, capture) = actor().await;
        let stranger = PeerEndpoint::bind_local(iroh::SecretKey::generate())
            .await
            .unwrap();

        // Issued so the host has a live ticket: the ALPN must be what stops
        // this, not the absence of an invite.
        let _invite = host.invite_create(Role::ViewOnly).await.unwrap();
        let connection = stranger
            .connect(host_endpoint.addr(), lumepeer_net::ALPN_MEDIA)
            .await
            .unwrap();
        // The host closes it; the stranger never gets a media stream.
        assert!(
            tokio::time::timeout(TIMEOUT, connection.closed())
                .await
                .is_ok(),
            "the host left a media connection open"
        );
        assert!(host.status().await.unwrap().is_empty());
        assert!(
            !lock_capture(&capture).is_capturing(),
            "a refused media dial must never start capture"
        );
    }

    /// The whole point of this phase: granting `view` registers the guest as a
    /// viewer, which is what starts capture, and revoking takes it away again
    /// (§8.1, §11).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_grant_adds_a_viewer_and_a_revoke_removes_it() {
        let (host, _host_endpoint, capture) = actor().await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;

        assert_eq!(viewers(&capture), 0);
        assert!(!lock_capture(&capture).is_capturing());

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        let label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();

        host.grant(label.clone(), Role::ViewOnly).await.unwrap();
        assert_eq!(viewers(&capture), 1, "a view grant must start capture");
        assert!(lock_capture(&capture).is_capturing());

        host.revoke(label).await.unwrap();
        assert_eq!(viewers(&capture), 0, "a revoke must stop capture");
        assert!(!lock_capture(&capture).is_capturing());
    }

    /// §21 punch-list item 5 / ADR 0016: an ended session is remembered by the
    /// side that dialed, so it can go back, and by nobody on the side that was
    /// dialed. The host decided once; it keeps no roster of who visited.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_guest_remembers_the_host_and_the_host_remembers_nobody() {
        let (host, host_endpoint, _capture) = actor().await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;

        assert!(host.history().await.unwrap().is_empty());
        assert!(guest.history().await.unwrap().is_empty());

        let invite = host.invite_create(Role::FullControl).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        let label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();
        host.grant(label.clone(), Role::FullControl).await.unwrap();
        host.revoke(label.clone()).await.unwrap();

        let remembered = wait_for_history(&guest, "the guest never remembered the host").await;
        assert_eq!(remembered.len(), 1);
        assert_eq!(remembered[0].peer_label, host_tag(&host_endpoint.addr().id));
        assert_eq!(remembered[0].role, Role::FullControl);
        assert!(
            host.history().await.unwrap().is_empty(),
            "a host must not build a record of the guests it let in"
        );
    }

    /// The same has to hold when nobody revokes anything: a host that simply
    /// disappears is still a host worth remembering on the guest side, since
    /// its view window closing is the only signal either side gets.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_disconnect_records_the_host_without_an_explicit_revoke() {
        let (host, host_endpoint, _capture) = actor().await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        let label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();
        host.grant(label, Role::ViewOnly).await.unwrap();
        // `grant` returns once the message is queued, not once it lands. Tear
        // the host down before the guest has the grant and there is no session
        // to lose, which would make this test pass or fail on timing rather
        // than on the behaviour it is about.
        wait_for_phase(&guest, ConnectPhase::Connected).await;

        let host_id = host_endpoint.addr().id;
        host_endpoint.close().await;

        let remembered =
            wait_for_history(&guest, "a vanished host was never recorded on the guest").await;
        assert_eq!(remembered[0].peer_label, host_tag(&host_id));
        assert_eq!(remembered[0].role, Role::ViewOnly);
    }

    /// The reason the guest keeps the list at all (ADR 0016): a remembered row
    /// dials the host again on its own, without the operator hunting down a
    /// code, and the host is asked for consent again exactly as the first time.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_remembered_host_can_be_dialed_again_and_still_needs_consent() {
        let (host, _host_endpoint, _capture) = actor().await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        let label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();
        host.grant(label.clone(), Role::ViewOnly).await.unwrap();
        host.revoke(label).await.unwrap();
        let remembered = wait_for_history(&guest, "nothing to reconnect to").await;

        // Nothing is retyped: the row carries the code, and the code stays in
        // Rust — the caller only names the host.
        guest
            .history_connect(remembered[0].peer_label.clone())
            .await
            .unwrap();

        let again = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .expect("a reconnect must queue a fresh consent request, not walk straight in");
        assert!(
            host.status()
                .await
                .unwrap()
                .iter()
                .any(|row| row.label == again && row.state == SessionStateDto::Pending),
            "the host decides again on every reconnect (§2.3)"
        );

        assert!(
            matches!(
                guest.history_connect("no-such-host".to_owned()).await,
                Err(ActorError::UnknownPeer)
            ),
            "a label that names no remembered host must dial nothing"
        );
    }

    /// §21 punch-list item 6: the connect form has to know that a dial which
    /// returned is not a session yet, or it re-enables itself while the host
    /// is still looking at the consent dialog.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_connect_phase_waits_for_the_host_to_decide() {
        let (host, _host_endpoint, _capture) = actor().await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;

        assert_eq!(guest.connect_state().await.unwrap(), ConnectPhase::Idle);

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        assert_eq!(
            guest.connect_state().await.unwrap(),
            ConnectPhase::AwaitingConsent,
            "the handshake is done but nobody has decided yet"
        );
        assert!(guest.connect_state().await.unwrap().is_pending());

        let label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();
        host.grant(label.clone(), Role::ViewOnly).await.unwrap();
        wait_for_phase(&guest, ConnectPhase::Connected).await;
        assert!(!guest.connect_state().await.unwrap().is_pending());

        host.revoke(label).await.unwrap();
        wait_for_phase(&guest, ConnectPhase::Idle).await;
    }

    /// A host that says no has to say it in a way the guest's form can show:
    /// a denial is not the same as a dial that never landed.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_denied_request_leaves_the_connect_phase_denied() {
        let (host, _host_endpoint, _capture) = actor().await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        let label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();

        // Deny is a revoke of a session that was never granted (§8.1).
        host.revoke(label).await.unwrap();
        wait_for_phase(&guest, ConnectPhase::Denied).await;
    }

    /// The bug behind §21 punch-list item 6: a second Connect against a host
    /// this node is already talking to used to replace the live connection,
    /// and the replacement's own teardown then killed the working session.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_connect_to_the_same_host_is_refused_and_leaves_the_session_alone() {
        let (host, _host_endpoint, _capture) = actor().await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code.clone()).await.unwrap();
        let label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();
        host.grant(label.clone(), Role::ViewOnly).await.unwrap();
        wait_for_phase(&guest, ConnectPhase::Connected).await;

        assert!(
            matches!(
                guest.invite_connect(invite.code).await,
                Err(ActorError::Net(NetError::AlreadyConnected))
            ),
            "a duplicate dial must be refused, not raced"
        );

        assert_eq!(
            guest.connect_state().await.unwrap(),
            ConnectPhase::Connected,
            "the refused second dial must not disturb the live session"
        );
        assert!(
            host.status()
                .await
                .unwrap()
                .iter()
                .any(|row| row.label == label && row.state == SessionStateDto::Active)
        );
    }

    /// ADR 0016 end to end: the same code the host read out once has to work
    /// again after the session ends, or "remembered host" is a list of dead
    /// links and the operator is back to asking for a new code every time.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_same_invite_code_still_works_after_a_session_has_ended() {
        let (host, _host_endpoint, _capture) = actor().await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code.clone()).await.unwrap();
        let label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();
        host.grant(label.clone(), Role::ViewOnly).await.unwrap();
        host.revoke(label).await.unwrap();
        // Granted and then ended is not a denial: the form goes quiet rather
        // than reporting a refusal that never happened.
        wait_for_phase(&guest, ConnectPhase::Idle).await;

        guest
            .invite_connect(invite.code)
            .await
            .expect("a live invite is a way back in, not a one-shot");
        tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .expect("the second connection must reach the host's consent queue");
    }

    /// The other half of ADR 0016: "refresh invite" is the host's withdrawal
    /// switch, so the code it replaced has to stop working immediately.
    #[tokio::test(flavor = "multi_thread")]
    async fn refreshing_the_invite_retires_the_code_it_replaced() {
        let (host, _host_endpoint, _capture) = actor().await;
        let (guest, _guest_endpoint, _guest_capture) = actor().await;

        let first = host.invite_create(Role::ViewOnly).await.unwrap();
        let second = host.invite_create(Role::ViewOnly).await.unwrap();
        assert_ne!(first.code, second.code);

        // Whether the dial itself reports success is a race and not the point:
        // the host refuses the retired ticket *after* the handshake, by
        // closing, and that close can land on either side of the guest's own
        // handshake completing. What must never happen either way is a consent
        // request reaching the host.
        let _ = guest.invite_connect(first.code).await;
        assert!(
            tokio::time::timeout(Duration::from_secs(3), wait_for_pending(&host))
                .await
                .is_err(),
            "a replaced invite must not reach the host's consent queue"
        );
    }

    /// A guest that disappears without a revoke must not leave the host
    /// capturing its own screen for nobody.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_disconnect_removes_the_viewer_too() {
        let (host, _host_endpoint, capture) = actor().await;
        let (guest, guest_endpoint, _guest_capture) = actor().await;

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        let label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();
        host.grant(label, Role::ViewOnly).await.unwrap();
        assert_eq!(viewers(&capture), 1);

        guest_endpoint.close().await;
        wait_until("the viewer survived the disconnect", || {
            viewers(&capture) == 0
        })
        .await;
        assert!(!lock_capture(&capture).is_capturing());
    }

    /// The guest side of a grant: a view window opens by itself, is pollable
    /// through the same pseudonymized-label IPC as everything else, and closing
    /// it ends the session.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_granted_guest_gets_a_view_window_and_closing_it_ends_the_session() {
        let (host, _host_endpoint, host_capture) = actor().await;
        let recorder = Arc::new(RecordingWindows::default());
        let (guest, _guest_endpoint, _guest_capture, _windows) =
            actor_with_windows(Arc::clone(&recorder) as Arc<dyn ViewWindows>).await;

        let invite = host.invite_create(Role::FullControl).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        let label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();
        host.grant(label, Role::FullControl).await.unwrap();

        wait_until("no view window was opened", || {
            !recorder.opened().is_empty()
        })
        .await;
        let (window_label, peer_label, input) = recorder.opened().remove(0);
        assert_eq!(window_label, crate::view::window_label(&peer_label));
        assert!(input, "FullControl carries a live input grant");

        let frame = guest.view_frame(peer_label.clone(), 0).await.unwrap();
        assert_eq!(
            frame.len(),
            crate::view::VIEW_RESPONSE_HEADER_BYTES,
            "no picture has been decoded yet, so only the header comes back"
        );
        assert_eq!(frame[1], 1, "the live input grant rides on every frame");

        // Input is forwarded while the grant carries it.
        guest
            .input(peer_label.clone(), pointer_event(1, 2))
            .await
            .unwrap();

        // Closing the window is the guest's revoke: the session ends from this
        // side and the host stops capturing.
        guest.revoke(peer_label.clone()).await.unwrap();
        assert!(recorder.closed().contains(&window_label));
        wait_until("the host kept capturing after the guest left", || {
            viewers(&host_capture) == 0
        })
        .await;
        assert!(matches!(
            guest.view_frame(peer_label, 0).await,
            Err(ActorError::UnknownPeer)
        ));
    }

    /// A `ViewOnly` session must not be able to forward input, and the guest
    /// drops it before it ever reaches the wire (the host checks again).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_view_only_guest_cannot_forward_input() {
        let (host, _host_endpoint, _host_capture) = actor().await;
        let recorder = Arc::new(RecordingWindows::default());
        let (guest, _guest_endpoint, _guest_capture, _windows) =
            actor_with_windows(Arc::clone(&recorder) as Arc<dyn ViewWindows>).await;

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        let label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();
        host.grant(label, Role::ViewOnly).await.unwrap();

        wait_until("no view window was opened", || {
            !recorder.opened().is_empty()
        })
        .await;
        let (_window, peer_label, input) = recorder.opened().remove(0);
        assert!(!input, "ViewOnly must never imply input (§2.2, §8.2)");
        assert!(matches!(
            guest.input(peer_label.clone(), pointer_event(3, 4)).await,
            Err(ActorError::Core(CoreError::NotPermitted))
        ));
        let frame = guest.view_frame(peer_label, 0).await.unwrap();
        assert_eq!(frame[1], 0);
    }

    /// Unknown labels are refused cleanly, with no panic and no peer parsing.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unknown_label_is_refused() {
        let (host, _endpoint, _capture) = actor().await;
        assert!(matches!(
            host.grant("deadbeefdeadbeef".to_owned(), Role::ViewOnly)
                .await,
            Err(ActorError::UnknownPeer)
        ));
        assert!(matches!(
            host.revoke("deadbeefdeadbeef".to_owned()).await,
            Err(ActorError::UnknownPeer)
        ));
        assert!(matches!(
            host.view_frame("deadbeefdeadbeef".to_owned(), 0).await,
            Err(ActorError::UnknownPeer)
        ));
        assert!(matches!(
            host.input("deadbeefdeadbeef".to_owned(), pointer_event(1, 1))
                .await,
            Err(ActorError::UnknownPeer)
        ));
    }
}
