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

use ed25519_dalek::SigningKey;
use lumepeer_core::consent::Role;
use lumepeer_core::constants::{CONTROL_HANDSHAKE_TIMEOUT_SECS, MAX_INFLIGHT_HANDSHAKES};
use lumepeer_core::protocol::MessageKind;
use lumepeer_core::session::{SessionManager, SessionState};
use lumepeer_core::{CoreError, NodeId};
use lumepeer_net::keystore::load_or_create;
use lumepeer_net::ticket::TicketRegistry;
use lumepeer_net::{Channel, ControlConnection, InviteTicket, NetError, PeerEndpoint};
use rand::Rng as _;
use tokio::sync::{Semaphore, broadcast, mpsc, oneshot};

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
    /// String to render as a QR code and/or show as text (§7).
    pub qr_string: String,
    /// Unix seconds after which the invite is dead.
    pub expires_at: u64,
}

/// Something the actor observed that a listener may want to react to.
///
/// Deliberately carries no peer identity: it crosses no trust boundary today
/// and must not become a channel for one (§15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

/// Thin handle IPC commands hold. Cloneable: every command gets its own
/// clone of the sender.
#[derive(Debug, Clone)]
pub struct ActorHandle {
    tx: mpsc::Sender<ActorCommand>,
    notify: broadcast::Sender<ActorNotification>,
}

impl ActorHandle {
    /// Stream of [`ActorNotification`]s from this point on.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<ActorNotification> {
        self.notify.subscribe()
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
                // An unauthenticated peer must not be able to park a media or
                // file connection in the control handshake's read (§4.1).
                if Channel::from_alpn(connection.alpn()) != Some(Channel::Control) {
                    tracing::warn!(peer = %tag, "closing a non-control connection");
                    connection.close(
                        lumepeer_net::connection::CLOSE_MALFORMED.into(),
                        lumepeer_net::error::close_code::MALFORMED.as_bytes(),
                    );
                    return None;
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
                Some((control, peer, ticket))
            })
            .await
            else {
                tracing::warn!(
                    timeout_secs = CONTROL_HANDSHAKE_TIMEOUT_SECS,
                    "dropping an incoming connection that did not finish its handshake in time"
                );
                return;
            };
            if let Some((connection, peer, ticket)) = outcome {
                let _ = tx
                    .send(ActorEvent::Handshaked {
                        connection: Box::new(connection),
                        peer,
                        ticket,
                    })
                    .await;
            }
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
            }
            MessageKind::ConsentRevoke => {
                tracing::info!(peer = %tag, "remote host revoked consent");
                let _ = self.notify.send(ActorNotification::ConsentRevoked);
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
        if self.sessions.on_disconnect(peer).is_err() {
            // No active session to move into the reconnect window, so this was
            // a guest that dropped before the host decided: drop its queued
            // request instead of leaving it pending forever.
            let _ = self.sessions.revoke(peer);
        }
        tracing::info!(peer = %peer_tag(&self.install_salt, &peer), "peer disconnected");
        let _ = self.notify.send(ActorNotification::Disconnected);
        self.rebuild_labels_and_snapshot();
    }

    async fn handle_command(&mut self, command: ActorCommand) {
        match command {
            ActorCommand::Status { reply } => {
                let snapshot = self.rebuild_labels_and_snapshot();
                let _ = reply.send(snapshot);
            }
            ActorCommand::Grant { label, role, reply } => {
                let result = match self.resolve(&label) {
                    Ok(peer) => match self.sessions.grant(peer, role) {
                        Ok(()) => {
                            self.send_to(&peer, MessageKind::ConsentGrant(role));
                            tracing::info!(peer = %label, ?role, "consent granted");
                            Ok(())
                        }
                        Err(e) => Err(ActorError::Core(e)),
                    },
                    Err(e) => Err(e),
                };
                self.rebuild_labels_and_snapshot();
                let _ = reply.send(result);
            }
            ActorCommand::Revoke { label, reply } => {
                let result = match self.resolve(&label) {
                    Ok(peer) => match self.sessions.revoke(peer) {
                        Ok(()) => {
                            self.send_to(&peer, MessageKind::ConsentRevoke);
                            tracing::info!(peer = %label, "consent revoked");
                            Ok(())
                        }
                        Err(e) => Err(ActorError::Core(e)),
                    },
                    Err(e) => Err(e),
                };
                self.rebuild_labels_and_snapshot();
                let _ = reply.send(result);
            }
            ActorCommand::InviteCreate { role, reply } => {
                let now = unix_now();
                let issued = InviteTicket::issue(&self.identity, &self.endpoint.addr(), role, now);
                let result = match issued {
                    Ok(ticket) => match ticket.to_qr_string() {
                        Ok(qr_string) => {
                            self.tickets.register(&ticket);
                            Ok(InviteDto {
                                qr_string,
                                expires_at: ticket.expires_at,
                            })
                        }
                        Err(e) => Err(ActorError::Net(e)),
                    },
                    Err(e) => Err(ActorError::Net(e)),
                };
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
        }
    }

    /// Guest side: dial the host named by the ticket, run the handshake and
    /// **keep** the connection. Dropping it here would close the QUIC
    /// connection under it and the host's `ConsentGrant` would never arrive.
    async fn connect_with_ticket(&mut self, raw: &str) -> Result<(), ActorError> {
        let ticket = InviteTicket::from_qr_string(raw).map_err(ActorError::Net)?;
        let addr = ticket.endpoint_addr().map_err(ActorError::Net)?;
        let connection = self
            .endpoint
            .connect_control(addr)
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
pub async fn spawn_actor() -> Result<ActorHandle, NetError> {
    let store = lumepeer_net::keystore::open()?;
    let secret_key = load_or_create(store.as_ref())?;
    let identity = SigningKey::from_bytes(&secret_key.to_bytes());
    let endpoint = PeerEndpoint::bind(secret_key).await?;

    tokio::spawn({
        let endpoint = endpoint.clone();
        async move {
            endpoint.online().await;
            tracing::info!("endpoint reached a relay; invites are dialable from outside the LAN");
        }
    });

    Ok(spawn_actor_with(endpoint, identity))
}

/// Spawns the actor over an already bound endpoint. Split out of
/// [`spawn_actor`] so the loop can be driven in tests without a keystore, a
/// relay or a Tauri window.
#[must_use]
pub fn spawn_actor_with(endpoint: PeerEndpoint, identity: SigningKey) -> ActorHandle {
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
    };
    tokio::spawn(actor.run());
    ActorHandle { tx, notify }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "a failed assumption must fail the test"
    )]

    use std::time::Duration;

    use super::*;

    /// Anything slower than this on loopback means the test is stuck.
    const TIMEOUT: Duration = Duration::from_secs(20);

    async fn actor() -> (ActorHandle, PeerEndpoint) {
        let secret = iroh::SecretKey::generate();
        let identity = SigningKey::from_bytes(&secret.to_bytes());
        let endpoint = PeerEndpoint::bind_local(secret).await.unwrap();
        (spawn_actor_with(endpoint.clone(), identity), endpoint)
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
        let (host, _host_endpoint) = actor().await;
        let (guest, _guest_endpoint) = actor().await;
        let mut events = guest.subscribe();

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.qr_string).await.unwrap();

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
        let (host, _host_endpoint) = actor().await;
        let (guest, guest_endpoint) = actor().await;

        let invite = host.invite_create(Role::ViewOnly).await.unwrap();
        guest.invite_connect(invite.qr_string).await.unwrap();
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
    #[tokio::test(flavor = "multi_thread")]
    async fn a_media_alpn_connection_is_refused_without_a_handshake() {
        let (host, host_endpoint) = actor().await;
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
        // The host closes it; the guest never gets a control stream.
        assert!(
            tokio::time::timeout(TIMEOUT, connection.closed())
                .await
                .is_ok(),
            "the host left a media connection open"
        );
        assert!(host.status().await.unwrap().is_empty());
    }

    /// Unknown labels are refused cleanly, with no panic and no peer parsing.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unknown_label_is_refused() {
        let (host, _endpoint) = actor().await;
        assert!(matches!(
            host.grant("deadbeefdeadbeef".to_owned(), Role::ViewOnly)
                .await,
            Err(ActorError::UnknownPeer)
        ));
        assert!(matches!(
            host.revoke("deadbeefdeadbeef".to_owned()).await,
            Err(ActorError::UnknownPeer)
        ));
    }
}
