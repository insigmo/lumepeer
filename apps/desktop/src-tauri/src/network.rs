//! Owner of the network/session runtime (design doc §2.3, §4, §13).
//!
//! `NetworkActor` is the single owner of `SessionManager` (and, from Task 3
//! on, of `PeerEndpoint`/`TicketRegistry` too). Tauri commands never lock
//! anything directly: they send an `ActorCommand` and await the reply, so
//! there is exactly one place that decides authorization (§2.3).

use ed25519_dalek::SigningKey;
use lumepeer_core::consent::Role;
use lumepeer_core::session::SessionManager;
use lumepeer_core::{CoreError, NodeId};
use lumepeer_net::keystore::load_or_create;
use lumepeer_net::ticket::TicketRegistry;
use lumepeer_net::{InviteTicket, PeerEndpoint};
use rand::Rng as _;
use tokio::sync::{mpsc, oneshot};

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
    Net(lumepeer_net::NetError),
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
}

impl ActorHandle {
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

/// Runtime state the actor owns and loops over.
struct Actor {
    rx: mpsc::Receiver<ActorCommand>,
    sessions: SessionManager,
    /// Per-process salt for pseudonymized labels (§15): regenerated on every
    /// start, so a label is stable within a run and meaningless across runs.
    install_salt: [u8; 32],
    /// label -> NodeId, rebuilt on every command that changes session state.
    labels: std::collections::HashMap<String, NodeId>,
    endpoint: PeerEndpoint,
    identity: SigningKey,
    tickets: TicketRegistry,
}

impl Actor {
    fn label_of(&self, peer: &NodeId) -> String {
        let hash = lumepeer_core::audit::peer_hash(&self.install_salt, peer);
        hash[..8].iter().fold(String::new(), |mut out, byte| {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
            out
        })
    }

    fn resolve(&self, label: &str) -> Result<NodeId, ActorError> {
        self.labels.get(label).copied().ok_or(ActorError::UnknownPeer)
    }

    /// Rebuilds the label table from current pending + active peers, and
    /// returns the snapshot list in the same pass.
    fn rebuild_labels_and_snapshot(&mut self) -> Vec<SessionSnapshot> {
        self.labels.clear();
        let mut out = Vec::new();
        for ticket in self.sessions.pending() {
            let label = self.label_of(&ticket.peer);
            self.labels.insert(label.clone(), ticket.peer);
            out.push(SessionSnapshot {
                label,
                state: SessionStateDto::Pending,
                role: ticket.requested_role,
                input: false,
            });
        }
        for (peer, role, grants) in self.sessions.active() {
            let label = self.label_of(&peer);
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
        while let Some(command) = self.rx.recv().await {
            match command {
                ActorCommand::Status { reply } => {
                    let snapshot = self.rebuild_labels_and_snapshot();
                    let _ = reply.send(snapshot);
                }
                ActorCommand::Grant { label, role, reply } => {
                    let result = self
                        .resolve(&label)
                        .and_then(|peer| self.sessions.grant(peer, role).map_err(ActorError::Core));
                    self.rebuild_labels_and_snapshot();
                    let _ = reply.send(result);
                }
                ActorCommand::Revoke { label, reply } => {
                    let result = self
                        .resolve(&label)
                        .and_then(|peer| self.sessions.revoke(peer).map_err(ActorError::Core));
                    self.rebuild_labels_and_snapshot();
                    let _ = reply.send(result);
                }
                ActorCommand::InviteCreate { role, reply } => {
                    let now = unix_now();
                    let result = match InviteTicket::issue(&self.identity, &self.endpoint.addr(), role, now) {
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
                    let _ = reply.send(result);
                }
                ActorCommand::InviteConnect { ticket, reply } => {
                    let result = self.connect_with_ticket(&ticket).await;
                    let _ = reply.send(result);
                }
            }
        }
    }

    async fn connect_with_ticket(&self, raw: &str) -> Result<(), ActorError> {
        let ticket = InviteTicket::from_qr_string(raw).map_err(ActorError::Net)?;
        let addr = ticket.endpoint_addr().map_err(ActorError::Net)?;
        let connection = self
            .endpoint
            .connect_control(addr)
            .await
            .map_err(ActorError::Net)?;
        let proof = postcard::to_allocvec(&ticket).map_err(|_| {
            ActorError::Net(lumepeer_net::NetError::MalformedTicket)
        })?;
        lumepeer_net::guest_handshake(connection, Role::ViewOnly, proof, Vec::new())
            .await
            .map_err(ActorError::Net)?;
        Ok(())
    }
}

/// Binds the endpoint from the OS keystore identity and spawns the actor.
///
/// # Errors
/// [`lumepeer_net::NetError`] if the keystore or the endpoint bind fails —
/// surfaced as a startup failure rather than silently degrading (§11.2,
/// §24.5).
pub async fn spawn_actor() -> Result<ActorHandle, lumepeer_net::NetError> {
    let store = lumepeer_net::keystore::open()?;
    let secret_key = load_or_create(store.as_ref())?;
    let identity = SigningKey::from_bytes(&secret_key.to_bytes());
    let endpoint = PeerEndpoint::bind(secret_key).await?;
    endpoint.online().await;

    let (tx, rx) = mpsc::channel(32);
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
    };
    tokio::spawn(actor.run());
    Ok(ActorHandle { tx })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[tokio::test]
    async fn status_starts_empty_and_unknown_label_is_refused() {
        let handle = spawn_actor();
        assert!(handle.status().await.unwrap().is_empty());
        let err = handle.grant("nonexistent".to_owned(), Role::ViewOnly).await;
        assert!(matches!(err, Err(ActorError::UnknownPeer)));
    }
}
