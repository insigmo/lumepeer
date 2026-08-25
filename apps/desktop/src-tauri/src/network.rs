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

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ed25519_dalek::SigningKey;
use lumepeer_core::chat::{ChatEntry, ChatLog};
use lumepeer_core::clipboard::ClipboardSync;
use lumepeer_core::consent::{Grants, Role};
use lumepeer_core::constants::{
    CONNECT_ATTEMPT_TIMEOUT_SECS, CONTROL_HANDSHAKE_TIMEOUT_SECS, DIAL_ATTEMPTS,
    DIAL_RETRY_BACKOFF_MS, INCOMING_ACCEPT_TIMEOUT_SECS, MAX_INFLIGHT_HANDSHAKES,
};
use lumepeer_core::protocol::{
    FEATURE_MEDIA_UNAVAILABLE, InputEventPayload, MediaUnavailableReason, MessageKind, MonitorInfo,
};
use lumepeer_core::session::{SessionManager, SessionState};
use lumepeer_core::{CoreError, NodeId};
use lumepeer_media::capture::{
    CaptureController, CaptureTarget, InputInjector, StubCapturer, platform_backend,
    platform_injector,
};
use lumepeer_net::keystore::load_or_create;
use lumepeer_net::ticket::TicketRegistry;
use lumepeer_net::{Channel, ControlConnection, InviteTicket, NetError, PeerEndpoint};
use rand::Rng as _;
use tokio::sync::{Semaphore, broadcast, mpsc, oneshot, watch};

use crate::connection_history::{ConnectionHistory, HistoryEntry};
use crate::view::{
    HostMedia, MediaFault, MediaHealth, MediaTarget, SharedCapture, ViewSlot, ViewStatus,
    ViewWindows, encode_view_response, lock_capture, slot_for_poll, spawn_encode_loop,
    spawn_media_receiver, window_label,
};

/// Capacity of the notification broadcast. Listeners that fall behind lag;
/// nothing in the actor's own progress depends on them.
const NOTIFY_CAPACITY: usize = 32;

/// What a view window needs to paint one frame, readable without the actor.
///
/// The picture already lives in a `watch` channel the media task writes and
/// nothing else mutates, so serving a frame poll never needed the actor's own
/// thread — it only ever went through the mailbox because that was where the
/// grant lived. Routing it there put every frame of every view window behind
/// whatever else the actor was doing, which at 30 fps is the difference
/// between a remote desktop and a slideshow (ADR 0027).
///
/// This widens nothing (§2.3). `input` is a copy the *actor* writes, on grant
/// and on revoke, of a decision `lumepeer-core` already made; a reader here
/// can only observe it. The entry is removed before the window is told to
/// close, so a poll racing a revoke reads either the live grant or nothing.
#[derive(Debug, Clone)]
struct ViewFeed {
    slot: watch::Receiver<ViewSlot>,
    input: Arc<AtomicBool>,
}

/// Live view feeds by window label, shared between the actor and the IPC layer.
type ViewFeeds = Arc<std::sync::RwLock<HashMap<String, ViewFeed>>>;

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
    /// The dial is in flight: the ticket parsed, and this node is trying to
    /// reach the host it names (ADR 0027).
    ///
    /// A phase of its own because the dial no longer happens inside the IPC
    /// call. `invite_connect` returns as soon as the attempt has *started*, so
    /// without this the form would have nothing to stay disabled on between
    /// the click and the host's `Hello` landing.
    Dialing,
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
        matches!(self, Self::Dialing | Self::AwaitingConsent)
    }

    /// Stable wire string for the webview.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Dialing => "dialing",
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
    /// Guest side: how this node's own outgoing connect attempt is going, and
    /// the §18 code of the last failure if it ended in one.
    ConnectState {
        reply: oneshot::Sender<(ConnectPhase, Option<&'static str>)>,
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
    /// Host side: start streaming desktop audio to `label` (§11 `AudioStart`).
    /// Requires a live `view` grant and an accepted media connection.
    AudioOn {
        label: String,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    /// Host side: stop the audio stream started by [`ActorCommand::AudioOn`].
    AudioOff {
        label: String,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    /// Host side: start recording the session with `label` into `path`
    /// (§9.2, §17). Requires the independent `recording` grant (§8.2).
    RecordOn {
        label: String,
        path: String,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    /// Host side: stop the recording and flush it to disk.
    RecordOff {
        label: String,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    /// Guest side: ask the host to deliver the Secure Attention Sequence
    /// (Ctrl+Alt+Del) to its user (§11; ADR 0028). The host answers on the
    /// wire with `SasAck`, and the reply here only says the request went out
    /// from a session shape that could carry it.
    SasRequest {
        label: String,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    /// Guest side: turn the view window's own microphone towards `label`'s
    /// host on or off (§11; ADR 0028). Requires a live session with a live
    /// `input` grant.
    MicToggle {
        label: String,
        on: bool,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    /// Host side: retarget capture at the monitor `label`'s guest picked
    /// (§11 `MonitorSelect`; ADR 0028). Requires a live `view` grant.
    MonitorSelect {
        label: String,
        monitor_id: u32,
        reply: oneshot::Sender<Result<(), ActorError>>,
    },
    /// Host side: announce this host's monitors to `label`'s guest
    /// (§11 `MonitorsList`; ADR 0028). The reply carries what was sent.
    MonitorsList {
        label: String,
        reply: oneshot::Sender<Result<Vec<MonitorInfo>, ActorError>>,
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
    /// What this host knows about its own ability to produce a picture.
    /// Shared with the actor, which is what learns of an encoder fault.
    health: Arc<MediaHealth>,
    /// Guest side: one entry per open view window, so `view_frame` can serve a
    /// picture without queueing behind the actor's mailbox (ADR 0027).
    /// Written only by the actor.
    views: ViewFeeds,
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

    /// What this host can do about producing a picture, for the status the UI
    /// shows its own operator (§18). Carries no authorization of its own.
    #[must_use]
    pub fn media_health(&self) -> &MediaHealth {
        &self.health
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
    pub async fn connect_state(&self) -> Result<(ConnectPhase, Option<&'static str>), ActorError> {
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
    /// Read straight from the shared feed rather than through the actor's
    /// mailbox: a frame poll is a read of state the actor does not have to be
    /// consulted about, and making it wait for the actor is what made a busy
    /// app drop the picture (ADR 0027).
    ///
    /// # Errors
    /// [`ActorError::UnknownPeer`] if no view window belongs to `label`.
    pub fn view_frame(&self, label: &str, since_us: u64) -> Result<Vec<u8>, ActorError> {
        let feed = self
            .views
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(label)
            .cloned()
            .ok_or(ActorError::UnknownPeer)?;
        let response = slot_for_poll(&feed.slot.borrow(), since_us);
        Ok(encode_view_response(
            &response,
            feed.input.load(Ordering::Relaxed),
        ))
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

    /// Host side: turns the desktop-audio stream to `label` on or off (§11).
    ///
    /// Refused without a live granted session; audio rides the same media
    /// connection the picture uses, so it also needs an accepted media dial.
    ///
    /// # Errors
    /// [`ActorError::Core::NotPermitted`] without a granted view session;
    /// [`ActorError::UnknownPeer`] when no media connection exists yet;
    /// [`ActorError::ChannelClosed`] if the actor task is gone.
    pub async fn audio_toggle(&self, label: String, on: bool) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(if on {
                ActorCommand::AudioOn { label, reply }
            } else {
                ActorCommand::AudioOff { label, reply }
            })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Host side: starts or stops the session recording of `label` (§17).
    /// Gated on the independent `recording` grant inside the actor (§8.2).
    ///
    /// # Errors
    /// [`ActorError::Core::NotPermitted`] without the recording grant;
    /// [`ActorError::Net`] when the file cannot be created;
    /// [`ActorError::ChannelClosed`] if the actor task is gone.
    pub async fn record_toggle(
        &self,
        label: String,
        path: Option<String>,
    ) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(match path {
                Some(path) => ActorCommand::RecordOn { label, path, reply },
                None => ActorCommand::RecordOff { label, reply },
            })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
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

    /// Guest side: asks the host to deliver the Secure Attention Sequence
    /// (§11; ADR 0028). The `Ok(())` here only means the request was sent
    /// from a session that could carry it; whether the host actually
    /// synthesized the sequence arrives on the wire as `SasAck` and is
    /// surfaced in the view window's own UI.
    ///
    /// # Errors
    /// [`ActorError::Core::NotPermitted`] without a granted session with a
    /// live `input` grant; [`ActorError::ChannelClosed`] if the actor is gone.
    pub async fn sas_request(&self, label: String) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::SasRequest { label, reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Host side: retargets capture at `monitor_id` for `label`'s session
    /// (§11 `MonitorSelect`; ADR 0028). The guest learns nothing back on this
    /// call — the next picture it receives simply shows the new monitor.
    ///
    /// # Errors
    /// [`ActorError::Core::NotPermitted`] without a granted view session;
    /// [`ActorError::Core::Malformed`] when the id names no announced monitor;
    /// [`ActorError::ChannelClosed`] if the actor is gone.
    pub async fn monitor_select(&self, label: String, monitor_id: u32) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::MonitorSelect {
                label,
                monitor_id,
                reply,
            })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Host side: announces this host's monitors to `label`'s guest
    /// (§11 `MonitorsList`; ADR 0028). Returns the list that was sent, so the
    /// caller can show the same numbers the guest will see.
    ///
    /// # Errors
    /// [`ActorError::UnknownPeer`] without a live session;
    /// [`ActorError::ChannelClosed`] if the actor is gone.
    pub async fn monitors_list(&self, label: String) -> Result<Vec<MonitorInfo>, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::MonitorsList { label, reply })
            .await
            .map_err(|_| ActorError::ChannelClosed)?;
        rx.await.map_err(|_| ActorError::ChannelClosed)?
    }

    /// Guest side: turns the view window's own microphone towards `label`'s
    /// host on or off (§11; ADR 0028). Gated inside the actor on a live
    /// session with a live `input` grant.
    ///
    /// # Errors
    /// [`ActorError::Core::NotPermitted`] without a granted session with a
    /// live `input` grant; [`ActorError::ChannelClosed`] if the actor is gone.
    pub async fn mic_toggle(&self, label: String, on: bool) -> Result<(), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ActorCommand::MicToggle { label, on, reply })
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
    /// Whether this peer's `Hello` advertised `FEATURE_MEDIA_UNAVAILABLE`.
    ///
    /// A peer that did not must never be sent `MessageKind::MediaUnavailable`:
    /// it speaks an older minor, where that discriminant does not exist, and
    /// an undecodable frame closes the connection (§9.1). Always false on the
    /// guest side, which never sends the message at all.
    announces_media_faults: bool,
    /// Whether this peer's `Hello` advertised `FEATURE_REMOTE_SAS`, the same
    /// gate for `MessageKind::SasAck` (§9.1; ADR 0028). Always false on the
    /// host side, which never sends the request.
    speaks_remote_sas: bool,
}

/// Host side: one running capture/encode loop, plus the connection it writes
/// on, so a revoke can stop both without waiting for the loop to notice.
/// `recorder` is the §17 slot the actor swaps a session recorder into; it
/// lives as long as the media session itself.
struct MediaSession {
    task: tokio::task::JoinHandle<()>,
    connection: iroh::endpoint::Connection,
    #[allow(
        dead_code,
        reason = "kept for the actor to swap recorders into mid-session"
    )]
    recorder: crate::view::SharedRecorder,
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

/// Host side: one running audio loop for a peer, with its own stop flag.
///
/// Audio is opt-in per session: the host user turns it on (`AudioStart` of
/// §11) and off again, and every teardown path — revoke, disconnect, media
/// session replacement — flips the flag and aborts the task.
struct AudioSession {
    stop: Arc<AtomicBool>,
    /// The §17 recorder slot, swapped by the actor mid-session; the loop picks
    /// up whatever is here when it encodes its next packet.
    recorder: crate::view::SharedRecorder,
    task: tokio::task::JoinHandle<()>,
}

impl AudioSession {
    /// Stops capture and ends the loop.
    fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        self.task.abort();
    }

    /// Swaps the session recorder in or out (§17).
    fn set_recorder(&mut self, recorder: Option<Arc<crate::recorder::SessionRecorder>>) {
        *self
            .recorder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = recorder;
    }
}

/// Host side: one guest-microphone playout loop, with its own stop flag
/// (§11; ADR 0028).
///
/// The guest opens the tagged `M` media stream when its user turns the mic
/// on; this side accepts it, decodes Opus and pushes PCM into the speakers.
/// Opt-out is the stream ending (the guest turned the mic off), which the
/// accept loop notices by itself; the entry here only bounds the task.
struct MicSession {
    task: tokio::task::JoinHandle<()>,
}

impl MicSession {
    /// Ends the playout loop.
    fn stop(self) {
        self.task.abort();
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
    /// `grants.input` as the frame poll reads it, without the actor. Written
    /// here, on grant and on role change; the entry is removed on revoke.
    input: Arc<AtomicBool>,
    /// Single-slot newest picture plus pipeline health. Dropping this receiver
    /// is also how the media task learns the view is gone.
    slot: watch::Receiver<ViewSlot>,
    /// The other end of `slot`, shared with the media task.
    ///
    /// Held here too because one thing the window has to show does not come
    /// from the media pipeline at all: a host announcing it cannot produce a
    /// picture says so on the *control* stream, which only the actor reads
    /// (docs/adr/0024).
    slot_tx: Arc<watch::Sender<ViewSlot>>,
    task: tokio::task::JoinHandle<()>,
    /// The media connection the picture rides. Written by the media task
    /// once dialed, read by the mic toggle; `None` until the first dial
    /// lands and after the media task ends.
    media_connection: Arc<std::sync::Mutex<Option<iroh::endpoint::Connection>>>,
}

impl ViewState {
    /// The media connection the picture rides, for a second tagged stream
    /// (the guest's own microphone; §11; ADR 0028).
    ///
    /// The mic stream must ride the *same* `rd/media/1` connection as the
    /// picture: a second connection is indistinguishable on the host from a
    /// redial, and the host's accept path would replace the encode loop
    /// (§4.1). `None` means the media task has not landed a dial yet — the
    /// toolbar's mic press is refused and can be pressed again once a
    /// picture is showing.
    fn media_connection(&self) -> Option<iroh::endpoint::Connection> {
        self.media_connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
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
        /// Whether the guest's `Hello` advertised `FEATURE_MEDIA_UNAVAILABLE`.
        announces_media_faults: bool,
        /// Whether the guest's `Hello` advertised `FEATURE_REMOTE_SAS`
        /// (§9.1; ADR 0028).
        speaks_remote_sas: bool,
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
    /// Guest side: an outgoing dial started by [`Actor::spawn_dial`] finished,
    /// one way or the other (ADR 0027).
    ///
    /// The dial runs off the actor loop, so this is how its result gets back
    /// to the one thread allowed to store a connection or move the connect
    /// phase. `peer` is what the ticket claimed, which is also what the
    /// handshake proved when `result` is `Ok`.
    Dialed {
        peer: NodeId,
        /// The invite code the dial used, kept so a successful connection can
        /// still be recorded in the remembered-hosts list (ADR 0016).
        code: String,
        addr: Box<iroh::EndpointAddr>,
        result: Result<Box<ControlConnection>, NetError>,
    },
}

/// Outcome of one accepted incoming connection, before the actor sees it.
enum Accepted {
    /// Control ALPN: handshake ran and the invite ticket verified.
    Control {
        connection: Box<ControlConnection>,
        peer: NodeId,
        ticket: Box<InviteTicket>,
        /// Whether the guest's `Hello` advertised `FEATURE_MEDIA_UNAVAILABLE`.
        announces_media_faults: bool,
        /// Whether the guest's `Hello` advertised `FEATURE_REMOTE_SAS`
        /// (§9.1; ADR 0028).
        speaks_remote_sas: bool,
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
    /// Encode loops report here when they cannot produce a picture at all.
    /// Separate from `events_tx` so `crate::view` stays free of the actor's
    /// own event type (§18, docs/adr/0024).
    faults_tx: mpsc::Sender<MediaFault>,
    faults_rx: mpsc::Receiver<MediaFault>,
    /// What this host knows about its own ability to produce a picture.
    health: Arc<MediaHealth>,
    notify: broadcast::Sender<ActorNotification>,
    /// Host side: the single "capture only with a viewer" gate of §8.1/§11,
    /// shared with every encode loop.
    capture: SharedCapture,
    /// Host side: one encode loop per peer currently receiving video.
    media: std::collections::HashMap<NodeId, MediaSession>,
    /// Host side: one audio loop per peer the host user enabled sound for.
    /// Opt-in per §11's `AudioStart`; dies with the session like everything
    /// else per-peer.
    audio: std::collections::HashMap<NodeId, AudioSession>,
    /// Host side: one guest-mic playout loop per peer the guest enabled their
    /// microphone for (§11; ADR 0028). The guest opens the tagged `M` media
    /// stream; this side accepts it and plays it on the speakers.
    guest_mic: std::collections::HashMap<NodeId, MicSession>,
    /// Whether this peer's `Hello` advertised `FEATURE_REMOTE_SAS`, so a
    /// `SasAck` may be sent back to it (§9.1: never send what an older minor
    /// would decode as malformed).
    speaks_remote_sas: std::collections::HashSet<NodeId>,
    /// Host side: live session recordings keyed by peer (§17). Gated on the
    /// independent `recording` grant; flushed and dropped when the session
    /// ends so no recorder can outlive what it was allowed to record.
    recorders: HashMap<NodeId, Arc<crate::recorder::SessionRecorder>>,
    /// Host side: platform input adapter, opened on the first authorized event
    /// so a host that never grants `input` never touches it.
    injector: Option<Box<dyn InputInjector>>,
    /// Guest side: one open view window per host being watched.
    views: std::collections::HashMap<NodeId, ViewState>,
    /// The same windows as `views`, in the form the IPC layer reads directly.
    /// Kept in step by `start_view` and `stop_view`, which are the only two
    /// places a view begins or ends.
    view_feeds: ViewFeeds,
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
    /// Why the last attempt failed, as the §18 code the UI shows. Set only
    /// alongside `ConnectPhase::Failed`; cleared when a new attempt starts.
    ///
    /// The dial no longer runs inside the IPC call, so its error cannot come
    /// back as the call's own `Err` any more — without this the user would be
    /// told "could not connect" and nothing else, which is the report ADR 0026
    /// was written about (ADR 0027).
    connect_failure: Option<&'static str>,
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
                    self.handle_command(command);
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
                fault = self.faults_rx.recv() => {
                    if let Some((peer, reason)) = fault {
                        self.on_media_fault(peer, reason);
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
            // Two deadlines, not one. Finishing the QUIC handshake is a
            // network wait whose length is the far side's hole punching, and
            // it used to share the ten seconds meant for a single control
            // round trip — so a guest that was still working got dropped by
            // the host just before it would have arrived, and both sides then
            // reported a failure neither had caused (ADR 0027).
            let accept_deadline = std::time::Duration::from_secs(INCOMING_ACCEPT_TIMEOUT_SECS);
            let handshake_deadline = std::time::Duration::from_secs(CONTROL_HANDSHAKE_TIMEOUT_SECS);
            let Ok(connection) = tokio::time::timeout(accept_deadline, async move {
                PeerEndpoint::finish_accept(incoming).await.ok()
            })
            .await
            else {
                tracing::warn!(
                    timeout_secs = INCOMING_ACCEPT_TIMEOUT_SECS,
                    "dropping an incoming connection that did not finish its QUIC handshake in time"
                );
                return;
            };
            let Ok(outcome) = tokio::time::timeout(
                handshake_deadline,
                classify_incoming(connection, &verifying_key, &salt),
            )
            .await
            else {
                tracing::warn!(
                    timeout_secs = CONTROL_HANDSHAKE_TIMEOUT_SECS,
                    "dropping an incoming connection that did not finish its control handshake in time"
                );
                return;
            };
            let event = match outcome {
                Some(Accepted::Control {
                    connection,
                    peer,
                    ticket,
                    announces_media_faults,
                    speaks_remote_sas,
                }) => ActorEvent::Handshaked {
                    connection,
                    peer,
                    announces_media_faults,
                    speaks_remote_sas,
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
    fn adopt(
        &mut self,
        connection: ControlConnection,
        peer: NodeId,
        announces_media_faults: bool,
        speaks_remote_sas: bool,
    ) {
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
                announces_media_faults,
                speaks_remote_sas,
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
                announces_media_faults,
                speaks_remote_sas,
            } => {
                if speaks_remote_sas {
                    self.speaks_remote_sas.insert(peer);
                } else {
                    self.speaks_remote_sas.remove(&peer);
                }
                self.on_handshaked(*connection, peer, &ticket, announces_media_faults);
            }
            ActorEvent::Inbound { peer, id, kind } => self.on_inbound(peer, id, &kind),
            ActorEvent::Closed { peer, id } => self.on_closed(peer, id),
            ActorEvent::MediaAccepted { connection, peer } => {
                self.on_media_accepted(*connection, peer);
            }
            ActorEvent::Dialed {
                peer,
                code,
                addr,
                result,
            } => self.on_dialed(peer, code, *addr, result),
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
        // §18: a host that cannot produce a picture says so, instead of
        // accepting a media connection it will never write a frame on and
        // leaving the guest to time the session out and blame the network
        // (docs/adr/0024). Dropping `connection` here closes it.
        if let Some(reason) = self.health.fault() {
            tracing::warn!(
                peer = %tag,
                ?reason,
                "refusing a media connection this host cannot feed"
            );
            self.announce_media_fault(peer, reason);
            return;
        }
        tracing::info!(peer = %tag, "media connection accepted; starting the encode loop");
        let recorder: crate::view::SharedRecorder = Arc::new(std::sync::Mutex::new(None));
        let task = spawn_encode_loop(
            connection.clone(),
            Arc::clone(&self.capture),
            Arc::clone(&recorder),
            tag.clone(),
            peer,
            self.faults_tx.clone(),
        );
        self.media.insert(
            peer,
            MediaSession {
                task,
                connection: connection.clone(),
                recorder,
            },
        );
        // The guest-mic pass rides the same connection and parks until the
        // guest actually opens its tagged `M` stream (§11; ADR 0028); it is
        // bounded by the media session's own lifetime.
        crate::view::spawn_guest_mic_pass(connection, tag);
    }

    /// Host side: an encode loop found it cannot produce a picture at all.
    ///
    /// Recorded on this host first. A missing encoder is a property of the
    /// machine, not of whichever peer happened to ask for it first, so the
    /// operator's own status must keep saying so after this session ends.
    fn on_media_fault(&mut self, peer: NodeId, reason: MediaUnavailableReason) {
        self.health.record(reason);
        // The loop has already returned; its media connection has nothing
        // left to carry, and dropping the entry closes it.
        self.media.remove(&peer);
        self.announce_media_fault(peer, reason);
    }

    /// Host side: tells `peer` that this session will carry no picture, and
    /// why — but only if its `Hello` said it understands the message (§9.1).
    ///
    /// A guest that did not is left exactly where it was before this existed:
    /// a window that waits, and the reason in this host's log. Sending it
    /// anyway would put a discriminant its minor does not know on the wire,
    /// and an undecodable control frame closes the connection (§9.1) — a
    /// strictly worse outcome than the missing picture.
    fn announce_media_fault(&mut self, peer: NodeId, reason: MediaUnavailableReason) {
        let announces = self
            .connections
            .get(&peer)
            .is_some_and(|c| c.announces_media_faults);
        if !announces {
            tracing::debug!(
                peer = %self.label_of(&peer),
                ?reason,
                "not announcing: this guest does not speak MediaUnavailable"
            );
            return;
        }
        tracing::info!(
            peer = %self.label_of(&peer),
            ?reason,
            "telling the guest this host cannot produce a picture"
        );
        self.send_to(&peer, MessageKind::MediaUnavailable(reason));
    }

    /// Guest side: the host says this session will never carry a picture.
    ///
    /// Not a revoke and not a failure of this connection: the control session
    /// and every grant on it stay as they are, and the window stays open with
    /// the real reason on it. What ends is the waiting — the receiver's
    /// recovery pass has nothing to reconnect to, so it is stopped rather
    /// than left to time out and report a connection that was never lost.
    fn on_media_unavailable(&mut self, peer: NodeId, reason: MediaUnavailableReason) {
        let tag = self.label_of(&peer);
        let Some(state) = self.views.get(&peer) else {
            return;
        };
        tracing::warn!(peer = %tag, ?reason, "the host cannot produce a picture for this session");
        state.task.abort();
        let status = ViewStatus::from(reason);
        state.slot_tx.send_modify(|slot| slot.status = status);
    }

    /// Host side: stops sending video to `peer` and drops it as a viewer, which
    /// stops capture altogether if it was the last one (§8.1, §11).
    /// The audio stream, if the host user had enabled it, dies with the media
    /// session — there is no audio without a picture's session. A live
    /// recording is flushed and closed too: a recording may only cover the
    /// session it was granted for (§8.2, §17).
    fn stop_media(&mut self, peer: NodeId) {
        if let Some(session) = self.media.remove(&peer) {
            session.stop();
        }
        if let Some(session) = self.audio.remove(&peer) {
            session.stop();
        }
        if let Some(session) = self.guest_mic.remove(&peer) {
            session.stop();
        }
        if let Some(recorder) = self.recorders.remove(&peer) {
            recorder.write_event(0, r#"{"event":"record-stop","reason":"session-end"}"#);
            let clean = recorder.stop();
            tracing::info!(peer = %self.label_of(&peer), clean, "recording flushed at session end");
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
            // The feed carries the live grant, so a role change has to reach it
            // before the next frame is served — the window stops accepting
            // input on the very next poll (§8.1).
            state.input.store(grants.input, Ordering::Relaxed);
            tracing::info!(peer = %tag, input = grants.input, "view grants updated");
            return;
        }
        let Some(addr) = self.host_addrs.get(&peer).cloned() else {
            tracing::warn!(peer = %tag, "no remembered address for this host: cannot open media");
            return;
        };

        let label = window_label(&tag);
        let (slot_tx, slot_rx) = watch::channel(ViewSlot::waiting());
        // Shared rather than handed over: the media task writes pictures into
        // it, and the actor writes the one status that does not come from the
        // media pipeline at all (`MediaUnavailable`, docs/adr/0024).
        let slot_tx = Arc::new(slot_tx);
        // The media connection lands here once the media task dials, so the
        // mic toggle can open its tagged stream on the *same* `rd/media/1`
        // the picture uses (§4.1; ADR 0028).
        let media_connection: Arc<std::sync::Mutex<Option<iroh::endpoint::Connection>>> =
            Arc::new(std::sync::Mutex::new(None));
        let task = spawn_media_receiver(
            MediaTarget {
                endpoint: self.endpoint.clone(),
                addr,
                tag: tag.clone(),
                worker: None,
                connection_cell: Arc::clone(&media_connection),
            },
            Arc::clone(&slot_tx),
        );
        let input = Arc::new(AtomicBool::new(grants.input));
        // Keyed by the peer tag: that is the only name the view window knows
        // itself by across IPC — `view_next_frame` takes `peer`, and the
        // window label is derived from it, not the other way round.
        self.view_feeds
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                tag.clone(),
                ViewFeed {
                    slot: slot_rx.clone(),
                    input: Arc::clone(&input),
                },
            );
        self.views.insert(
            peer,
            ViewState {
                label: label.clone(),
                role,
                grants,
                input,
                slot: slot_rx,
                slot_tx,
                task,
                media_connection,
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
        // Taken out before the window is told to close, so a frame poll racing
        // a revoke can only read the live grant or nothing at all — never a
        // grant that has just been withdrawn (§8.1).
        self.view_feeds
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.label_of(&peer));
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
        // A dial is still in flight towards this very peer: whatever just
        // ended belongs to an older connection, and settling now would report
        // an outcome for an attempt that has not produced one yet.
        if self.connect_phase == ConnectPhase::Dialing && outcome != ConnectPhase::Connected {
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
        announces_media_faults: bool,
    ) {
        // `speaks_remote_sas` is already recorded by `handle_event` before
        // this runs; the parameter list stays untouched here.
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
        self.adopt(
            connection,
            peer,
            announces_media_faults,
            self.speaks_remote_sas.contains(&peer),
        );
        let _ = self.notify.send(ActorNotification::ConsentRequested);
        self.rebuild_labels_and_snapshot();
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one arm per message kind reads best; splitting arms into \
                  helper fns would scatter the protocol across the file"
    )]
    fn on_inbound(&mut self, peer: NodeId, id: u64, kind: &MessageKind) {
        if self.connections.get(&peer).is_none_or(|c| c.id != id) {
            return;
        }
        let tag = self.label_of(&peer);
        #[allow(
            clippy::too_many_lines,
            reason = "one arm per message kind reads best; splitting arms into \
                      helper fns would scatter the protocol across the file"
        )]
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
            // Guest side: the host announced it has no picture to send.
            MessageKind::MediaUnavailable(reason) => self.on_media_unavailable(peer, reason),
            // Host side: the guest asks for the Secure Attention Sequence
            // (§11; ADR 0028). Same per-event re-check as every injected key:
            // only a live `input` grant acts, everything else is refused.
            MessageKind::SasRequest => {
                let permitted = self.sessions.authorize_input(
                    &peer,
                    &InputEventPayload {
                        logical: 0,
                        scancode: 0,
                        modifiers: 0,
                        detail: lumepeer_core::protocol::InputDetail::Press,
                    },
                );
                if permitted.is_err() {
                    tracing::warn!(peer = %tag, "SAS request without a live input grant; refused");
                    self.send_sas_ack(peer, false);
                    return;
                }
                match lumepeer_media::sas::send_sas() {
                    Ok(()) => {
                        tracing::info!(peer = %tag, "SAS delivered on the guest's request");
                        self.send_sas_ack(peer, true);
                    }
                    Err(reason) => {
                        tracing::warn!(peer = %tag, %reason, "SAS refused by the host OS");
                        self.send_sas_ack(peer, false);
                    }
                }
            }
            // Host side: the guest picked a monitor to watch (§11; ADR 0028).
            // The id must name a monitor this host announced; anything else is
            // a malformed request and is dropped with a log line.
            MessageKind::MonitorSelect { monitor_id } => {
                if self.media.contains_key(&peer) {
                    match self.on_monitor_select(&self.label_of(&peer), monitor_id) {
                        Ok(()) => tracing::info!(peer = %tag, monitor_id, "capture retargeted"),
                        Err(error) => tracing::warn!(
                            peer = %tag,
                            monitor_id,
                            ?error,
                            "monitor select refused"
                        ),
                    }
                } else {
                    tracing::debug!(peer = %tag, monitor_id, "monitor select without a media session");
                }
            }
            // Everything else belongs to a phase this build does not run yet.
            // Nothing a peer sends may ever grant itself consent (§2.3).
            ref other => tracing::debug!(peer = %tag, ?other, "ignoring a control message"),
        }
    }

    /// Sends `SasAck` to `peer`, but only if its `Hello` advertised
    /// [`lumepeer_core::protocol::FEATURE_REMOTE_SAS`] — an older guest would
    /// decode the unknown discriminant as malformed and close the connection
    /// (§9.1).
    fn send_sas_ack(&mut self, peer: NodeId, delivered: bool) {
        if self
            .connections
            .get(&peer)
            .is_some_and(|c| c.speaks_remote_sas)
        {
            self.send_to(&peer, MessageKind::SasAck { delivered });
        } else {
            tracing::debug!(
                peer = %self.label_of(&peer),
                delivered,
                "not acking: this guest does not speak remote-sas"
            );
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

    /// Not `async` any more, and that is the property worth keeping: the loop
    /// awaits this, so anything that blocks here blocks the whole actor. The
    /// dial was the last thing in it that talked to the network (ADR 0027).
    fn handle_command(&mut self, command: ActorCommand) {
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
                    Some(code) => self.spawn_dial(&code),
                    None => Err(ActorError::UnknownPeer),
                };
                if let Err(ActorError::Net(ref error)) = result {
                    tracing::warn!(%error, "reconnecting to a remembered host failed");
                }
                let _ = reply.send(result);
            }
            ActorCommand::ConnectState { reply } => {
                let _ = reply.send((self.connect_phase, self.connect_failure));
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
                let result = self.spawn_dial(&ticket);
                if let Err(ActorError::Net(ref error)) = result {
                    tracing::warn!(%error, "invite connect refused before dialing");
                }
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
            ActorCommand::AudioOn { label, reply } => {
                let _ = reply.send(self.on_audio_toggle(&label, true));
            }
            ActorCommand::AudioOff { label, reply } => {
                let _ = reply.send(self.on_audio_toggle(&label, false));
            }
            ActorCommand::RecordOn { label, path, reply } => {
                let _ = reply.send(self.on_record_toggle(&label, Some(path)));
            }
            ActorCommand::RecordOff { label, reply } => {
                let _ = reply.send(self.on_record_toggle(&label, None));
            }
            ActorCommand::SasRequest { label, reply } => {
                let _ = reply.send(self.on_sas_request(&label));
            }
            ActorCommand::MicToggle { label, on, reply } => {
                let _ = reply.send(self.on_mic_toggle(&label, on));
            }
            ActorCommand::MonitorSelect {
                label,
                monitor_id,
                reply,
            } => {
                let _ = reply.send(self.on_monitor_select(&label, monitor_id));
            }
            ActorCommand::MonitorsList { label, reply } => {
                let _ = reply.send(self.on_monitors_list(&label));
            }
        }
    }

    /// Host side: starts or stops the session recording of `label` (§17).
    ///
    /// Gated on the independent `recording` grant (§8.2): a session that was
    /// granted view/input/clipboard but not `recording` cannot be recorded,
    /// no matter what the UI asks. The file lands only where the host user
    /// chose; the guest is never told recording state changed beyond what the
    /// §15 log policy already allows.
    fn on_record_toggle(&mut self, label: &str, path: Option<String>) -> Result<(), ActorError> {
        let peer = self.resolve(label)?;
        if let Some(path) = path {
            let permitted = self.connections.contains_key(&peer)
                && self.sessions.state(&peer) == SessionState::Active
                && self.sessions.grants(&peer).is_some_and(|g| g.recording);
            if !permitted {
                return Err(ActorError::Core(CoreError::NotPermitted));
            }
            if self.recorders.contains_key(&peer) {
                return Ok(()); // already recording this session
            }
            let recorder = Arc::new(
                crate::recorder::SessionRecorder::start(std::path::PathBuf::from(path)).map_err(
                    |error| {
                        tracing::warn!(peer = %label, %error, "cannot open the recording file");
                        ActorError::Net(NetError::Io(error.to_string()))
                    },
                )?,
            );
            recorder.write_event(
                0,
                &format!(r#"{{"event":"record-start","session":"{label}"}}"#),
            );
            // Hand the recorder to whichever loops are running for this
            // session: the video slot is picked up on the next frame; the
            // audio loop reads its own copy the same way.
            if let Some(session) = self.media.get_mut(&peer) {
                *session
                    .recorder
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some(Arc::clone(&recorder));
            }
            if let Some(audio) = self.audio.get_mut(&peer) {
                audio.set_recorder(Some(Arc::clone(&recorder)));
            }
            self.recorders.insert(peer, recorder);
            tracing::info!(peer = %label, "recording started");
        } else if let Some(recorder) = self.recorders.remove(&peer) {
            // Take it out of the live loops first so no new record lands after
            // the stop event.
            if let Some(session) = self.media.get_mut(&peer) {
                *session
                    .recorder
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            }
            if let Some(audio) = self.audio.get_mut(&peer) {
                audio.set_recorder(None);
            }
            recorder.write_event(0, r#"{"event":"record-stop"}"#);
            let clean = recorder.stop();
            tracing::info!(peer = %label, clean, "recording stopped");
        }
        Ok(())
    }

    /// Host side: turns the desktop-audio stream to `label` on or off (§11).
    ///
    /// Authorization mirrors `on_media_accepted`: a live control connection,
    /// an Active session and a `view` grant — audio is part of what `view`
    /// may carry (§8.1 "receive video **and audio**"), but it is opt-in per
    /// session because it is the host user's microphone-adjacent surface.
    fn on_audio_toggle(&mut self, label: &str, on: bool) -> Result<(), ActorError> {
        let peer = self.resolve(label)?;
        let granted = self.connections.contains_key(&peer)
            && self.sessions.state(&peer) == SessionState::Active
            && self.sessions.grants(&peer).is_some_and(|g| g.view);
        if !granted {
            return Err(ActorError::Core(CoreError::NotPermitted));
        }
        if on {
            if self.audio.contains_key(&peer) {
                return Ok(()); // already streaming; idempotent
            }
            let Some(session) = self.media.get(&peer) else {
                // No media connection to ride on yet.
                return Err(ActorError::UnknownPeer);
            };
            let stop = Arc::new(AtomicBool::new(false));
            // The §17 slot starts empty; the actor fills it when a recording
            // is turned on, and the loop picks it up on its next packet.
            let recorder: crate::view::SharedRecorder = Arc::new(std::sync::Mutex::new(
                self.recorders.get(&peer).map(Arc::clone),
            ));
            let task = crate::view::spawn_audio_loop(
                session.connection.clone(),
                Arc::clone(&stop),
                Arc::clone(&recorder),
                self.label_of(&peer),
            );
            self.audio.insert(
                peer,
                AudioSession {
                    stop,
                    recorder,
                    task,
                },
            );
            tracing::info!(peer = %label, "audio streaming enabled");
        } else if let Some(session) = self.audio.remove(&peer) {
            session.stop();
            tracing::info!(peer = %label, "audio streaming disabled");
        }
        Ok(())
    }

    /// Guest side: turns the view window's own microphone towards `label`'s
    /// host on or off (§11; ADR 0028).
    ///
    /// The mic is an *input* surface — it carries the guest user's voice, not
    /// the host's screen — so it is gated on the same live `input` grant the
    /// keyboard and pointer use, re-checked here at toggle time and by the
    /// host per request. The stream rides the media connection the picture
    /// already dialed; without one there is nothing to ride, which is the
    /// same refusal `audio_toggle` gives.
    fn on_mic_toggle(&mut self, label: &str, on: bool) -> Result<(), ActorError> {
        let peer = self.resolve(label)?;
        // The guest's own microphone is a surface that feeds the host, so it
        // is gated exactly like [`Self::on_input`] (§8.1): a view-only grant
        // may watch but not speak.
        let permitted = self
            .views
            .get(&peer)
            .ok_or(ActorError::UnknownPeer)?
            .grants
            .input;
        if !permitted {
            return Err(ActorError::Core(CoreError::NotPermitted));
        }
        if on {
            if self.guest_mic.contains_key(&peer) {
                return Ok(()); // already streaming; idempotent
            }
            let view = self.views.get(&peer).ok_or(ActorError::UnknownPeer)?;
            // The mic stream rides the media connection the picture already
            // dialed (§4.1); without one there is nothing to ride yet, and
            // the toolbar's press is refused — a picture showing means the
            // cell is populated.
            let Some(connection) = view.media_connection() else {
                tracing::debug!(peer = %label, "no media connection yet: mic press refused");
                return Err(ActorError::UnknownPeer);
            };
            let task = crate::view::spawn_mic_loop(connection, self.label_of(&peer));
            self.guest_mic.insert(peer, MicSession { task });
            tracing::info!(peer = %label, "guest microphone streaming enabled");
        } else if let Some(session) = self.guest_mic.remove(&peer) {
            session.stop();
            tracing::info!(peer = %label, "guest microphone streaming disabled");
        }
        Ok(())
    }

    /// Guest side: forwards the toolbar's Ctrl+Alt+Del request to the host
    /// being watched (§11; ADR 0028).
    ///
    /// Gated exactly like [`Self::on_input`]: the host re-checks the `input`
    /// grant per event, but this side refuses to even send without a live
    /// session that believes it holds the grant. Whether the host actually
    /// synthesized the sequence arrives as `SasAck` on the wire; this reply
    /// only covers the send itself.
    fn on_sas_request(&mut self, label: &str) -> Result<(), ActorError> {
        let peer = self.resolve(label)?;
        let permitted = self.sessions.authorize_input(
            &peer,
            &InputEventPayload {
                logical: 0,
                scancode: 0,
                modifiers: 0,
                detail: lumepeer_core::protocol::InputDetail::Press,
            },
        );
        if permitted.is_err() {
            return Err(ActorError::Core(CoreError::NotPermitted));
        }
        self.send_to(&peer, MessageKind::SasRequest);
        Ok(())
    }

    /// Host side: retargets capture at `monitor_id` for `label`'s guest
    /// (§11 `MonitorSelect`; ADR 0028).
    ///
    /// The id must be one of this host's own monitors, counted in the same
    /// DXGI order the announcement uses. Retargeting restarts the capturer on
    /// the new display; the running encode loop simply picks the new geometry
    /// up on its next frame, exactly as it would if the user had changed the
    /// display resolution.
    fn on_monitor_select(&mut self, label: &str, monitor_id: u32) -> Result<(), ActorError> {
        let peer = self.resolve(label)?;
        let granted = self.connections.contains_key(&peer)
            && self.sessions.state(&peer) == SessionState::Active
            && self.sessions.grants(&peer).is_some_and(|g| g.view);
        if !granted {
            return Err(ActorError::Core(CoreError::NotPermitted));
        }
        let count = crate::view::host_display_count().map_err(|error| {
            tracing::warn!(peer = %label, %error, "cannot enumerate this host's monitors");
            ActorError::Core(CoreError::Malformed)
        })?;
        if usize::try_from(monitor_id).is_ok_and(|index| index >= count) {
            tracing::warn!(peer = %label, monitor_id, count, "monitor id out of range");
            return Err(ActorError::Core(CoreError::Malformed));
        }
        {
            let shared = Arc::clone(&self.capture);
            crate::view::lock_capture(&shared)
                .set_target(CaptureTarget::Display(monitor_id))
                .map_err(|error| {
                    tracing::warn!(
                        peer = %label,
                        monitor_id,
                        %error,
                        "the capturer refused the new monitor"
                    );
                    ActorError::Core(CoreError::NotPermitted)
                })?;
        }
        tracing::info!(peer = %label, monitor_id, "capture retargeted by the guest");
        Ok(())
    }

    /// Host side: announces this host's monitors to `label`'s guest
    /// (§11 `MonitorsList`; ADR 0028). Returns what was sent.
    fn on_monitors_list(&mut self, label: &str) -> Result<Vec<MonitorInfo>, ActorError> {
        let peer = self.resolve(label)?;
        if self.sessions.state(&peer) != SessionState::Active {
            return Err(ActorError::UnknownPeer);
        }
        let monitors = crate::view::host_monitors()
            .map_err(|error| {
                tracing::warn!(peer = %label, %error, "cannot enumerate this host's monitors");
                ActorError::Core(CoreError::Malformed)
            })?
            .into_iter()
            .map(|monitor| MonitorInfo {
                id: monitor.id,
                width: monitor.width,
                height: monitor.height,
                primary: monitor.primary,
            })
            .collect::<Vec<_>>();
        if let Some(reason) = self.health.fault() {
            // A host that cannot produce a picture at all has no meaningful
            // list to give; say so rather than offering monitors that will
            // never show anything.
            let _ = reason;
            return Err(ActorError::Core(CoreError::NotPermitted));
        }
        self.send_to(
            &peer,
            MessageKind::MonitorsList {
                monitors: monitors.clone(),
            },
        );
        Ok(monitors)
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
        // With direct paths on (ADR 0026) the address set fills up from the
        // local interfaces long before a relay is reached, so an invite issued
        // in that window is dialable across the room and nowhere else. It is
        // still worth issuing — a LAN-only deployment is legitimate — but it
        // is exactly the "works locally, not over the internet" report of ADR
        // 0020, so it is said out loud here rather than discovered on the
        // guest's machine minutes later.
        if addr.relay_urls().next().is_none() {
            tracing::warn!(
                "issuing an invite with no relay address: this code is dialable from the local network only until the endpoint reaches a relay"
            );
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
        if self.sessions.grants(&peer).is_some_and(|g| g.view) {
            if let Err(error) = lock_capture(&self.capture).add_viewer(peer) {
                tracing::warn!(
                    peer = %label,
                    %error,
                    "consent granted but this platform cannot capture"
                );
            }
            // Announced here rather than waiting for the guest's media dial:
            // the guest opens its window on the `ConsentGrant` this function
            // just queued, and the control stream is reliable and ordered, so
            // the reason arrives as that window's first news instead of a
            // reconnect window later (§18).
            if let Some(reason) = self.health.fault() {
                self.announce_media_fault(peer, reason);
            }
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

    /// Guest side: validate the invite here, then run the dial and the
    /// handshake **off** the actor loop (ADR 0027).
    ///
    /// Everything this can decide without touching the network is decided
    /// synchronously, so a bad code or a duplicate still fails the IPC call
    /// outright and the user is told which it was. The part that talks to the
    /// network is the part that can take fifteen seconds, and it must not run
    /// here: `run` awaits `handle_command`, so a dial on this thread stops the
    /// actor accepting incoming connections, delivering `ConsentGrant`s,
    /// serving the status the UI polls once a second and answering the view
    /// window's frame poll. That is the "the app freezes and then says it
    /// could not connect" report this fixes.
    fn spawn_dial(&mut self, raw: &str) -> Result<(), ActorError> {
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
        // A dial already in flight owns `connect_phase`. Letting a second one
        // start would leave two tasks racing to report an outcome into one
        // slot, and the loser would overwrite the winner.
        if self.connect_phase == ConnectPhase::Dialing {
            tracing::info!("a connect attempt is already in flight");
            return Err(ActorError::Net(NetError::AlreadyConnected));
        }
        let proof = postcard::to_allocvec(&ticket)
            .map_err(|_| ActorError::Net(NetError::MalformedTicket))?;

        self.connect_phase = ConnectPhase::Dialing;
        self.connect_peer = Some(addr.id);
        self.connect_failure = None;

        let endpoint = self.endpoint.clone();
        let tx = self.events_tx.clone();
        let tag = self.label_of(&addr.id);
        let code = raw.to_owned();
        let role = ticket.allowed_request;
        let target = addr.clone();
        tokio::spawn(async move {
            let result = dial_with_retries(&endpoint, &target, role, proof, &tag).await;
            let _ = tx
                .send(ActorEvent::Dialed {
                    peer: target.id,
                    code,
                    addr: Box::new(target),
                    result: result.map(Box::new),
                })
                .await;
        });
        Ok(())
    }

    /// Guest side: takes the outcome of [`Self::spawn_dial`] on the actor's own
    /// thread, which is the only one allowed to store a connection.
    fn on_dialed(
        &mut self,
        peer: NodeId,
        code: String,
        addr: iroh::EndpointAddr,
        result: Result<Box<ControlConnection>, NetError>,
    ) {
        let tag = self.label_of(&peer);
        // The attempt this reports on has been superseded — the user started
        // another one, or the session it belonged to is already gone. Dropping
        // the connection here closes it, which is what we want: nothing is
        // waiting for it.
        if self.connect_peer != Some(peer) {
            tracing::info!(peer = %tag, "discarding the result of a superseded dial");
            return;
        }
        let control = match result {
            Ok(control) => *control,
            Err(error) => {
                tracing::warn!(peer = %tag, %error, "invite connect failed");
                self.connect_failure = Some(crate::commands::net_error_code(&error));
                self.connect_phase = ConnectPhase::Failed;
                self.connect_peer = None;
                return;
            }
        };
        // The handshake proves who answered; the ticket only claimed it.
        let peer = control.peer();
        tracing::info!(peer = %self.label_of(&peer), "connected to a host, awaiting consent");
        // Remembered for the media dial that follows a `ConsentGrant`: the
        // ticket is the only place this address is known without discovery.
        self.host_addrs.insert(peer, addr);
        // Remembered so the history row written when this session ends can dial
        // the same host again (ADR 0016).
        self.host_invites.insert(peer, code);
        self.connect_phase = ConnectPhase::AwaitingConsent;
        self.connect_peer = Some(peer);
        // Guest side: this node is the one that *receives* `MediaUnavailable`,
        // so there is no peer capability to remember here.
        self.adopt(control, peer, false, false);
    }
}

/// Sorts one accepted connection by ALPN and, if it is control, runs the host
/// handshake and verifies the invite it carries (§4.1, §9.1, §18).
///
/// Authenticates; authorizes nothing. Whether a media connection may exist,
/// and whether a verified ticket may become a session, are questions only the
/// actor can answer, because only the actor can read `SessionManager` (§2.3).
async fn classify_incoming(
    connection: Option<iroh::endpoint::Connection>,
    verifying_key: &ed25519_dalek::VerifyingKey,
    salt: &[u8; 32],
) -> Option<Accepted> {
    let connection = connection?;
    let peer = connection.remote_id();
    let tag = peer_tag(salt, &peer);
    match Channel::from_alpn(connection.alpn()) {
        Some(Channel::Control) => {}
        // Media is authenticated here and authorized by the actor: this task
        // cannot see whether the peer holds a live, granted control session,
        // and guessing would be a way to widen a grant outside
        // `lumepeer-core` (§2.3).
        Some(Channel::Media) => {
            return Some(Accepted::Media {
                connection: Box::new(connection),
                peer,
            });
        }
        // An unauthenticated peer must not be able to park a file connection
        // in the control handshake's read (§4.1).
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
    if let Err(error) = ticket.verify(verifying_key, unix_now()) {
        tracing::warn!(peer = %tag, %error, "invite ticket did not verify");
        control.close_with(&NetError::InvalidTicket);
        return None;
    }
    Some(Accepted::Control {
        announces_media_faults: hello
            .features
            .iter()
            .any(|feature| feature == FEATURE_MEDIA_UNAVAILABLE),
        speaks_remote_sas: hello
            .features
            .iter()
            .any(|feature| feature == lumepeer_core::protocol::FEATURE_REMOTE_SAS),
        connection: Box::new(control),
        peer,
        ticket: Box::new(ticket),
    })
}

/// One outgoing control connection — dial and handshake — retried up to
/// [`DIAL_ATTEMPTS`] times.
///
/// The retry is not decoration. A ticket carries the address set the host had
/// when it read the code out, and by the time a human has pasted it the host
/// may have moved to another relay, its NAT binding may have changed, or its
/// discovery record may not have propagated yet. iroh repairs all of that by
/// itself — but only if something connects again, and until now the only thing
/// that did was the user, who cannot tell a stale address from a dead host
/// (ADR 0027).
///
/// The handshake is retried too, and that is the case this was actually
/// written for. Between these two machines the failure is not a dial that
/// never lands: it is a connection that comes up over the host's relay link
/// and then loses it mid-`Hello` —
///
/// ```text
/// host   21:36:41  issuing an invite  addrs={Relay(euc1-1), …}
/// host   21:36:45  home is now relay use1-1, was Some(euc1-1)
/// host   21:36:52  dropping an incoming connection that did not finish its control handshake in time
/// host   21:36:57  Lost connection to relay server: Ping timeout
/// guest  21:36:59  invite connect failed: stream i/o failed: connection lost
/// ```
///
/// — which is [`NetError::Io`]: this side's own observation that a stream
/// stopped, not anybody's decision, and the very thing a second attempt fixes
/// because by then the host is on a relay it can hold, or a direct path has
/// been punched. What is *not* retried is an answer: a bad ticket, a version
/// mismatch or a refusal is a verdict, and asking again only collects it twice.
async fn dial_with_retries(
    endpoint: &PeerEndpoint,
    addr: &iroh::EndpointAddr,
    role: Role,
    proof: Vec<u8>,
    tag: &str,
) -> Result<ControlConnection, NetError> {
    let attempt_budget = std::time::Duration::from_secs(CONNECT_ATTEMPT_TIMEOUT_SECS);
    let mut last = NetError::Dial("no attempt was made".to_owned());
    for attempt in 1..=DIAL_ATTEMPTS {
        let outcome = tokio::time::timeout(
            attempt_budget,
            connect_once(endpoint, addr, role, proof.clone()),
        )
        .await
        .unwrap_or_else(|_| {
            Err(NetError::Dial(format!(
                "no answer within {CONNECT_ATTEMPT_TIMEOUT_SECS}s"
            )))
        });
        let error = match outcome {
            Ok(control) => return Ok(control),
            Err(error) => error,
        };
        let retryable = matches!(error, NetError::Dial(_) | NetError::Io(_));
        tracing::warn!(
            peer = %tag,
            %error,
            attempt,
            of = DIAL_ATTEMPTS,
            retryable,
            "connect attempt failed"
        );
        if !retryable {
            return Err(error);
        }
        last = error;
        if attempt < DIAL_ATTEMPTS {
            tokio::time::sleep(std::time::Duration::from_millis(DIAL_RETRY_BACKOFF_MS)).await;
        }
    }
    Err(last)
}

/// One attempt: dial the control ALPN and run the guest half of the handshake.
async fn connect_once(
    endpoint: &PeerEndpoint,
    addr: &iroh::EndpointAddr,
    role: Role,
    proof: Vec<u8>,
) -> Result<ControlConnection, NetError> {
    let connection = endpoint.connect_control(addr.clone()).await?;
    // The one feature this build advertises: it understands a host saying it
    // cannot produce a picture. An older host ignores the string (§9.1) and
    // simply never sends the message.
    let features = vec![FEATURE_MEDIA_UNAVAILABLE.to_owned()];
    lumepeer_net::guest_handshake(connection, role, proof, features).await
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
    settings: &crate::config::Settings,
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

/// Builds the host's media side: the capture controller, whether this platform
/// gave a capture backend at all, and the injector when it can only come from
/// the same session as the pixels (ADR 0010).
///
/// A missing backend must not stop the app from starting or from accepting a
/// session: consent, input and the rest of the control channel work regardless
/// (§18). What it must not do either is stay a log line — the fallback
/// capturer produces nothing forever, which is exactly the silent degradation
/// §18 forbids. So the absence is recorded in [`MediaHealth`], where this
/// host's own status screen and the guest's `MediaUnavailable` both read it
/// (docs/adr/0024).
#[must_use]
pub fn default_capture() -> HostMedia {
    fn controller(capturer: Box<dyn lumepeer_media::capture::ScreenCapturer>) -> SharedCapture {
        Arc::new(std::sync::Mutex::new(CaptureController::new(
            capturer,
            CaptureTarget::PrimaryDisplay,
        )))
    }

    match platform_backend() {
        Ok((capturer, injector)) => HostMedia {
            capture: controller(capturer),
            health: Arc::new(MediaHealth::healthy()),
            injector,
        },
        Err(error) => {
            tracing::warn!(%error, "no capture backend on this platform: sessions stay blank");
            HostMedia {
                capture: controller(Box::new(StubCapturer::default())),
                health: Arc::new(MediaHealth::without_capture()),
                injector: None,
            }
        }
    }
}

/// Spawns the actor over an already bound endpoint. Split out of
/// [`spawn_actor`] so the loop can be driven in tests without a keystore, a
/// relay or a Tauri window: `windows` and `media` are the two seams that
/// would otherwise need one.
#[must_use]
pub fn spawn_actor_with(
    endpoint: PeerEndpoint,
    identity: SigningKey,
    windows: Arc<dyn ViewWindows>,
    media: HostMedia,
    history_path: Option<std::path::PathBuf>,
) -> ActorHandle {
    let HostMedia {
        capture,
        health,
        injector,
    } = media;
    let (tx, rx) = mpsc::channel(32);
    let (events_tx, events_rx) = mpsc::channel(32);
    let view_feeds: ViewFeeds = Arc::new(std::sync::RwLock::new(HashMap::new()));
    let (faults_tx, faults_rx) = mpsc::channel(8);
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
        faults_tx,
        faults_rx,
        health: Arc::clone(&health),
        notify: notify.clone(),
        capture,
        media: std::collections::HashMap::new(),
        audio: std::collections::HashMap::new(),
        guest_mic: std::collections::HashMap::new(),
        speaks_remote_sas: std::collections::HashSet::new(),
        recorders: HashMap::new(),
        injector,
        views: std::collections::HashMap::new(),
        view_feeds: Arc::clone(&view_feeds),
        host_addrs: std::collections::HashMap::new(),
        host_invites: std::collections::HashMap::new(),
        chat: ChatLog::new(),
        clipboard: std::collections::HashMap::new(),
        clipboard_inbound: std::collections::HashMap::new(),
        windows,
        history: ConnectionHistory::open(history_path),
        connect_phase: ConnectPhase::Idle,
        connect_peer: None,
        connect_failure: None,
    };
    tokio::spawn(actor.run());
    ActorHandle {
        tx,
        notify,
        online: Arc::new(AtomicBool::new(false)),
        health,
        views: view_feeds,
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

    /// A host that has a capture backend, like every machine the other tests
    /// pretend to run on.
    fn test_media(capture: &SharedCapture) -> HostMedia {
        HostMedia {
            capture: Arc::clone(capture),
            health: Arc::new(MediaHealth::healthy()),
            injector: None,
        }
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
        let capture = test_capture();
        let media = test_media(&capture);
        let (handle, endpoint) = actor_with_media(Arc::clone(&windows), media).await;
        (handle, endpoint, capture, windows)
    }

    /// The seam every actor in these tests is built through: `media` is what
    /// decides whether this host believes it can produce a picture at all.
    async fn actor_with_media(
        windows: Arc<dyn ViewWindows>,
        media: HostMedia,
    ) -> (ActorHandle, PeerEndpoint) {
        let secret = iroh::SecretKey::generate();
        let identity = SigningKey::from_bytes(&secret.to_bytes());
        let endpoint = PeerEndpoint::bind_local(secret).await.unwrap();
        let handle = spawn_actor_with(endpoint.clone(), identity, windows, media, None);
        (handle, endpoint)
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
            let (phase, _) = handle.connect_state().await.unwrap();
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

        assert_eq!(guest.connect_state().await.unwrap().0, ConnectPhase::Idle);

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.code).await.unwrap();
        // `invite_connect` returns when the attempt starts, not when it lands
        // (ADR 0027), so the form has to stay disabled through both waits —
        // the dial and then the host's decision.
        assert!(
            guest.connect_state().await.unwrap().0.is_pending(),
            "the attempt is in flight from the moment it is started"
        );
        wait_for_phase(&guest, ConnectPhase::AwaitingConsent).await;
        assert!(
            guest.connect_state().await.unwrap().0.is_pending(),
            "the handshake is done but nobody has decided yet"
        );

        let label = tokio::time::timeout(TIMEOUT, wait_for_pending(&host))
            .await
            .unwrap();
        host.grant(label.clone(), Role::ViewOnly).await.unwrap();
        wait_for_phase(&guest, ConnectPhase::Connected).await;
        assert!(!guest.connect_state().await.unwrap().0.is_pending());

        host.revoke(label).await.unwrap();
        wait_for_phase(&guest, ConnectPhase::Idle).await;
    }

    /// ADR 0027: a dial that is going nowhere must not take the app with it.
    ///
    /// Before the dial moved off the actor loop, `handle_command` awaited it,
    /// so for as long as iroh kept trying — up to fifteen seconds per attempt
    /// — nothing else the actor owns could make progress: no incoming
    /// connection was accepted, no `ConsentGrant` was delivered, and the four
    /// commands the UI polls once a second all queued behind it. The app
    /// looked frozen and then said it could not connect.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_dial_in_flight_does_not_stall_the_actor() {
        let (guest, _guest_endpoint, _guest_capture) = actor().await;
        // A host that exists on paper and answers nothing: the ticket verifies,
        // the address is in a range that goes nowhere, so the dial runs its
        // full budget.
        let (unreachable, _endpoint, _capture) = actor().await;
        let invite = unreachable.invite_create(Role::ViewOnly).await.unwrap();
        drop(unreachable);

        guest.invite_connect(invite.code).await.unwrap();
        assert_eq!(
            guest.connect_state().await.unwrap().0,
            ConnectPhase::Dialing,
            "the attempt is in flight, and saying so is what keeps the form disabled"
        );

        // The assertion that matters: every other command still answers while
        // that dial is outstanding.
        for _ in 0..5 {
            tokio::time::timeout(Duration::from_secs(2), guest.status())
                .await
                .expect("the actor must still answer while a dial is in flight")
                .unwrap();
            tokio::time::timeout(Duration::from_secs(2), guest.history())
                .await
                .expect("the actor must still answer while a dial is in flight")
                .unwrap();
        }
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
            guest.connect_state().await.unwrap().0,
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

        let frame = guest.view_frame(&peer_label, 0).unwrap();
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
            guest.view_frame(&peer_label, 0),
            Err(ActorError::UnknownPeer)
        ));
    }

    /// §18, docs/adr/0024: a host with no capture backend must say so, on the
    /// wire and on its own status, instead of leaving the guest to sit out the
    /// reconnect window and then be told the connection failed.
    ///
    /// The session itself is untouched: this is not a revoke.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_host_with_no_capture_backend_tells_the_guest_why() {
        let capture = test_capture();
        let (host, _host_endpoint) = actor_with_media(
            Arc::new(DetachedViewWindows),
            HostMedia {
                capture: Arc::clone(&capture),
                health: Arc::new(MediaHealth::without_capture()),
                injector: None,
            },
        )
        .await;
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
        let (_window, peer_label, _input) = recorder.opened().remove(0);

        // Well inside `RECONNECT_WINDOW_SECS`, which is what the guest used to
        // spend waiting before being told the wrong thing.
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            let frame = guest.view_frame(&peer_label, 0).unwrap();
            if frame[0] == ViewStatus::NoCapture.code() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the guest was never told why there is no picture (status {})",
                frame[0]
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        // Not a revoke: the grant, the session and the viewer registration all
        // stand, and the guest can still drive the session it was given.
        let rows = host.status().await.unwrap();
        assert!(
            rows.iter().any(|r| r.state == SessionStateDto::Active),
            "announcing a missing backend must not end the session"
        );
        assert_eq!(viewers(&capture), 1);

        // The operator's own screen says it too, before anyone else reports it.
        assert!(!host.media_health().can_capture());
        assert!(host.media_health().can_encode());
        assert!(
            guest.media_health().can_capture(),
            "the fault belongs to the host that has it, not to whoever heard about it"
        );
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
        let frame = guest.view_frame(&peer_label, 0).unwrap();
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
            host.view_frame("deadbeefdeadbeef", 0),
            Err(ActorError::UnknownPeer)
        ));
        assert!(matches!(
            host.input("deadbeefdeadbeef".to_owned(), pointer_event(1, 1))
                .await,
            Err(ActorError::UnknownPeer)
        ));
    }
}
