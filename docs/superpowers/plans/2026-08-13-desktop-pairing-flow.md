# Desktop Pairing Flow Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire QR-based host/guest pairing (invite, connect, consent) into the
desktop Tauri app, which today only manipulates in-memory session state with
no network behind it.

**Architecture:** A single long-lived `NetworkActor` tokio task owns
`PeerEndpoint`, `TicketRegistry` and `SessionManager`. Tauri IPC commands
become thin async request/response calls over an `mpsc` channel into that
actor; the actor's accept loop runs `host_handshake` + ticket claim on every
incoming connection and pushes results into its own `SessionManager`.

**Tech Stack:** Rust (tokio, iroh via `lumepeer-net`, `lumepeer-core`), Tauri
2 IPC, TypeScript + lit-html frontend, vitest.

**Spec:** `docs/superpowers/specs/2026-08-13-desktop-pairing-flow-design.md`

## Global Constraints

- No `unsafe_code` (workspace lint: deny).
- No `.unwrap()`/`.expect()` on network, parse, keystore or permission paths
  (workspace lint: warn, design doc §21).
- Raw `NodeId` must never reach the webview; the only peer-identifying
  string IPC exchanges with the frontend is the pseudonymized label.
- Every connection — first time or reconnect — gets a fresh host consent
  decision; nothing auto-grants (design doc §2.3).
- Frontend has no new test framework beyond the existing vitest + axe-core
  + testing-library setup.
- Tauri commands only ever act from the `main` window (`check_window`).

---

## File Structure

New:
- `apps/desktop/src-tauri/src/network.rs` — `NetworkActor`, `ActorCommand`,
  `ActorHandle`, DTOs shared between the actor and the IPC layer.
- `apps/desktop/src/invite-view.ts` — invite creation (QR render) + connect
  form UI.
- `tests/integration/tests/pairing.rs` — end-to-end host/guest round trip
  over two `PeerEndpoint::bind_local` endpoints.

Modified:
- `apps/desktop/src-tauri/Cargo.toml` — enable `lumepeer-net`'s
  `secret-service` feature so the Linux keystore backend actually builds
  into this binary.
- `apps/desktop/src-tauri/src/main.rs` — spawn the actor, `AppState` shrinks
  to an `ActorHandle`.
- `apps/desktop/src-tauri/src/commands.rs` — commands become `async fn`
  bodies that round-trip through the actor; adds `invite_create`/
  `invite_connect`; drops the hex-`NodeId` `parse_peer`/`bad_peer` path.
- `apps/desktop/package.json` — adds `qrcode` (+ `@types/qrcode` dev dep).
- `apps/desktop/src/session-status.ts` — `SessionStatus` gains `state`.
- `apps/desktop/src/main.ts` — splits poll results into pending vs. active,
  mounts the new invite view.
- `apps/desktop/src/keyboard-nav.test.ts`,
  `apps/desktop/src/accessibility.test.ts` — fixtures gain `state`.

---

### Task 1: Enable the Linux keystore backend for the desktop binary

**Files:**
- Modify: `apps/desktop/src-tauri/Cargo.toml:22`

**Interfaces:**
- Produces: `lumepeer_net::keystore::open()` returns
  `Ok(Box<dyn Keystore>)` on Linux instead of erroring "backend is not
  built into this binary" — required by Task 3.

- [ ] **Step 1: Add the feature**

Change line 22 of `apps/desktop/src-tauri/Cargo.toml` from:
```toml
lumepeer-net.workspace = true
```
to:
```toml
lumepeer-net = { workspace = true, features = ["secret-service"] }
```

- [ ] **Step 2: Verify it builds**

Run: `cargo build -p lumepeer-desktop`
Expected: builds clean (this only changes which optional dep is pulled in
for `lumepeer-net` on Linux; no code uses it yet).

- [ ] **Step 3: Commit**

```bash
git add apps/desktop/src-tauri/Cargo.toml
git commit -m "desktop: build the Linux Secret Service keystore backend into the app"
```

---

### Task 2: NetworkActor skeleton — status/grant/revoke over a channel, label bug fixed

Replaces the direct `Mutex<SessionManager>` locking in `commands.rs` with an
actor + channel, without touching the network yet. This is the smallest
slice that both fixes the `peer_label`-as-`NodeId` bug and proves the actor
plumbing works end to end.

**Files:**
- Create: `apps/desktop/src-tauri/src/network.rs`
- Modify: `apps/desktop/src-tauri/src/main.rs` (whole file)
- Modify: `apps/desktop/src-tauri/src/commands.rs` (whole file)
- Test: inline `#[cfg(test)]` module in `network.rs`

**Interfaces:**
- Produces (used by Task 3, 4, and `commands.rs`):
  - `pub struct ActorHandle(mpsc::Sender<ActorCommand>)` with
    `pub fn clone(&self) -> Self` (derive `Clone`) and
    `pub async fn call<T>(&self, build: impl FnOnce(oneshot::Sender<T>) -> ActorCommand) -> Result<T, ActorError>`
  - `pub enum ActorError { UnknownPeer, Core(lumepeer_core::CoreError), Net(lumepeer_net::NetError), ChannelClosed }`
  - `pub enum SessionStateDto { Pending, Active }`
  - `pub struct SessionSnapshot { pub label: String, pub state: SessionStateDto, pub role: lumepeer_core::consent::Role, pub input: bool }`
  - `pub fn spawn_actor() -> ActorHandle` (Task 2 version: no
    `PeerEndpoint`, just `SessionManager` + `install_salt`; Task 3 replaces
    the body, keeping the same signature and `ActorHandle` type)

- [ ] **Step 1: Write `network.rs` (actor skeleton)**

```rust
// apps/desktop/src-tauri/src/network.rs
//! Owner of the network/session runtime (design doc §2.3, §4, §13).
//!
//! `NetworkActor` is the single owner of `SessionManager` (and, from Task 3
//! on, of `PeerEndpoint`/`TicketRegistry` too). Tauri commands never lock
//! anything directly: they send an `ActorCommand` and await the reply, so
//! there is exactly one place that decides authorization (§2.3).

use lumepeer_core::consent::Role;
use lumepeer_core::session::SessionManager;
use lumepeer_core::{CoreError, NodeId};
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

/// Failure returned by an actor call.
#[derive(Debug)]
pub enum ActorError {
    /// The label the caller sent does not resolve to a known peer. Not a
    /// leak: an unknown label is exactly as safe to report as a known one,
    /// since the label never carried identity to begin with.
    UnknownPeer,
    /// A `SessionManager` decision was refused.
    Core(CoreError),
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
            }
        }
    }
}

/// Spawns the actor and returns a handle to it.
///
/// This is the Task 2 shape: `SessionManager` only, nothing on the network
/// yet. Task 3 replaces the body to also bind a `PeerEndpoint`, keeping this
/// signature.
#[must_use]
pub fn spawn_actor() -> ActorHandle {
    let (tx, rx) = mpsc::channel(32);
    let mut install_salt = [0u8; 32];
    rand::rng().fill_bytes(&mut install_salt);
    let actor = Actor {
        rx,
        sessions: SessionManager::new(),
        install_salt,
        labels: std::collections::HashMap::new(),
    };
    tokio::spawn(actor.run());
    ActorHandle { tx }
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
```

- [ ] **Step 2: Run the actor test**

Run: `cargo test -p lumepeer-desktop network::tests`
Expected: PASS (both assertions in
`status_starts_empty_and_unknown_label_is_refused`).

- [ ] **Step 3: Rewrite `main.rs`**

```rust
// apps/desktop/src-tauri/src/main.rs
//! Lumepeer desktop application (design doc §4, §13).
//!
//! The Tauri layer owns the window and forwards typed IPC calls into the
//! network actor. It holds no authority of its own: the webview is an
//! untrusted presentation layer (§2.3).

#![forbid(unsafe_code)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![allow(
    unreachable_pub,
    reason = "binary crate: `pub` marks the IPC surface of §13, not a library API"
)]

mod commands;
mod network;

/// State shared by every IPC command: a handle into the network actor.
#[derive(Debug)]
pub struct AppState {
    /// Channel handle into the `NetworkActor` (§2.3): the only way commands
    /// reach `SessionManager` or the transport.
    pub network: network::ActorHandle,
}

fn main() {
    init_tracing();

    tauri::Builder::default()
        .manage(AppState {
            network: network::spawn_actor(),
        })
        .invoke_handler(tauri::generate_handler![
            commands::session_grant,
            commands::session_revoke,
            commands::session_status,
            commands::license_status,
        ])
        .run(tauri::generate_context!())
        .unwrap_or_else(|error| {
            // Nothing sensitive here: this is a startup failure of the window
            // layer, before any peer or license data exists (§15).
            eprintln!("fatal: failed to start the application: {error}");
            std::process::exit(1);
        });
}

/// Human-readable logs in development, structured JSON in release (§16.1).
fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    if cfg!(debug_assertions) {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    } else {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .init();
    }
}
```

Note `session_request` and `license_status`'s `SessionManager::plan()` use
are gone from the handler list / need adjusting — `license_status` is
rewritten in Step 4 below to ask the actor. `session_request` is dropped
here: nothing in the frontend calls it (it existed to be "raised by the
transport layer" per its own doc comment, never by the webview); Task 4's
accept loop is that transport layer and calls `SessionManager::
request_consent_as` directly, inside the actor.

- [ ] **Step 4: Rewrite `commands.rs`**

```rust
// apps/desktop/src-tauri/src/commands.rs
//! Tauri IPC surface (design doc §13).
//!
//! Every command takes a typed DTO, never `serde_json::Value`, and every
//! decision is taken by the network actor: the webview is an untrusted
//! presentation layer (§2.3, §4). The only peer-identifying string that
//! ever crosses this boundary is the pseudonymized label the actor handed
//! out on a previous `session_status` poll — never a raw `NodeId`.

#![allow(
    clippy::needless_pass_by_value,
    reason = "tauri command handlers take Window and State by value"
)]

use serde::{Deserialize, Serialize};
use tauri::Window;

use crate::AppState;
use crate::network::{ActorError, SessionStateDto};

/// Label of the only window allowed to call these commands.
const MAIN_WINDOW_LABEL: &str = "main";

/// Error returned to the webview. Carries a code and a short message, never
/// secrets, tickets, tokens or raw peer identities (§15).
#[derive(Debug, Clone, Serialize)]
pub struct IpcError {
    /// Stable machine-readable code.
    pub code: &'static str,
    /// Short human-readable message, safe to display.
    pub message: String,
}

impl IpcError {
    fn denied() -> Self {
        Self {
            code: "WINDOW_NOT_ALLOWED",
            message: "command is not available from this window".to_owned(),
        }
    }

    fn poisoned() -> Self {
        Self {
            code: "STATE_POISONED",
            message: "session state is unavailable".to_owned(),
        }
    }

    fn unknown_peer() -> Self {
        Self {
            code: "UNKNOWN_PEER",
            message: "no session matches that peer".to_owned(),
        }
    }

    fn core(error: &lumepeer_core::CoreError) -> Self {
        Self {
            code: "CORE",
            message: error.to_string(),
        }
    }
}

impl From<ActorError> for IpcError {
    fn from(error: ActorError) -> Self {
        match error {
            ActorError::UnknownPeer => Self::unknown_peer(),
            ActorError::Core(e) => Self::core(&e),
            ActorError::ChannelClosed => Self::poisoned(),
        }
    }
}

/// Rejects calls coming from any window other than the main one (§13).
fn check_window(window: &Window) -> Result<(), IpcError> {
    if window.label() == MAIN_WINDOW_LABEL {
        Ok(())
    } else {
        Err(IpcError::denied())
    }
}

/// Role as seen by the webview.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleDto {
    /// Only `view`.
    ViewOnly,
    /// `view` plus allowlisted actions.
    ControlLimited,
    /// `view` plus keyboard and mouse.
    FullControl,
}

impl From<RoleDto> for lumepeer_core::consent::Role {
    fn from(value: RoleDto) -> Self {
        match value {
            RoleDto::ViewOnly => Self::ViewOnly,
            RoleDto::ControlLimited => Self::ControlLimited,
            RoleDto::FullControl => Self::FullControl,
        }
    }
}

impl From<lumepeer_core::consent::Role> for RoleDto {
    fn from(value: lumepeer_core::consent::Role) -> Self {
        match value {
            lumepeer_core::consent::Role::ViewOnly => Self::ViewOnly,
            lumepeer_core::consent::Role::ControlLimited => Self::ControlLimited,
            lumepeer_core::consent::Role::FullControl => Self::FullControl,
        }
    }
}

/// Session state as seen by the webview.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStateDtoWire {
    /// Queued, waiting for the host's decision.
    Pending,
    /// Consent granted, grants are live.
    Active,
}

impl From<SessionStateDto> for SessionStateDtoWire {
    fn from(value: SessionStateDto) -> Self {
        match value {
            SessionStateDto::Pending => Self::Pending,
            SessionStateDto::Active => Self::Active,
        }
    }
}

/// Argument of [`session_grant`].
#[derive(Debug, Clone, Deserialize)]
pub struct SessionGrantArgs {
    /// Pseudonymized label of the peer, as handed out by `session_status`.
    pub peer: String,
    /// Role the host chose, which may be lower than the requested one.
    pub role: RoleDto,
}

/// Argument of [`session_revoke`].
#[derive(Debug, Clone, Deserialize)]
pub struct SessionRevokeArgs {
    /// Pseudonymized label of the peer being revoked.
    pub peer: String,
}

/// Snapshot of one session for the status UI.
#[derive(Debug, Clone, Serialize)]
pub struct SessionStatusDto {
    /// Pseudonymized peer label; never a raw `NodeId` (§15).
    pub peer_label: String,
    /// Pending or active.
    pub state: SessionStateDtoWire,
    /// Role requested (pending) or granted (active).
    pub role: RoleDto,
    /// Whether input injection is currently permitted.
    pub input: bool,
}

/// License state for the UI.
#[derive(Debug, Clone, Serialize)]
pub struct LicenseStatusDto {
    /// Plan name.
    pub plan: String,
    /// Seconds left in the current license window, if bounded.
    pub seconds_left: Option<u64>,
    /// Whether the app is currently running on cached license data (§12.4).
    pub offline: bool,
}

/// Grants a role. The decision is taken by the actor, never in the webview
/// (§2.3).
///
/// # Errors
/// Rejects calls from other windows; propagates [`ActorError`] as an
/// [`IpcError`] (plan ceiling of §8.2, single-controller rule, unknown
/// label).
#[tauri::command]
pub async fn session_grant(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: SessionGrantArgs,
) -> Result<(), IpcError> {
    check_window(&window)?;
    state.network.grant(args.peer, args.role.into()).await?;
    Ok(())
}

/// Revokes every grant of a peer immediately (§8.1).
///
/// # Errors
/// Rejects calls from other windows; propagates [`ActorError`] as an
/// [`IpcError`].
#[tauri::command]
pub async fn session_revoke(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: SessionRevokeArgs,
) -> Result<(), IpcError> {
    check_window(&window)?;
    state.network.revoke(args.peer).await?;
    Ok(())
}

/// Lists pending and active sessions for the status/consent UI.
///
/// # Errors
/// Rejects calls from other windows; [`IpcError`] if the actor is gone.
#[tauri::command]
pub async fn session_status(
    window: Window,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<SessionStatusDto>, IpcError> {
    check_window(&window)?;
    let snapshot = state.network.status().await?;
    Ok(snapshot
        .into_iter()
        .map(|s| SessionStatusDto {
            peer_label: s.label,
            state: s.state.into(),
            role: s.role.into(),
            input: s.input,
        })
        .collect())
}

/// Reports the license state.
///
/// # Errors
/// Rejects calls from other windows.
#[tauri::command]
pub fn license_status(window: Window) -> Result<LicenseStatusDto, IpcError> {
    check_window(&window)?;
    // Phase 3 fills these from a verified license token (§12.1, §12.4); the
    // plan is Trial until then, same default `SessionManager::new()` used.
    Ok(LicenseStatusDto {
        plan: "trial".to_owned(),
        seconds_left: None,
        offline: true,
    })
}
```

Note: `license_status` no longer reads `SessionManager::plan()` through the
actor (that would need a new `ActorCommand::Plan` for no behavioral gain —
the value was already hardcoded to `Trial`/`None`/`true` before this
change, since the license layer isn't wired yet per the existing comment).
This keeps the same observable output as before.

- [ ] **Step 5: Build and run existing Rust tests**

Run: `cargo build --workspace && cargo test -p lumepeer-desktop -p lumepeer-core -p lumepeer-net`
Expected: builds clean, all tests pass, including the new actor test from
Step 2.

- [ ] **Step 6: Commit**

```bash
git add apps/desktop/src-tauri/src/network.rs apps/desktop/src-tauri/src/main.rs apps/desktop/src-tauri/src/commands.rs
git commit -m "desktop: route session grant/revoke/status through a NetworkActor, fix label/NodeId mismatch"
```

---

### Task 3: Bind a real `PeerEndpoint`, add `invite_create`/`invite_connect`

**Files:**
- Modify: `apps/desktop/src-tauri/src/network.rs`
- Modify: `apps/desktop/src-tauri/src/commands.rs`

**Interfaces:**
- Consumes: `ActorHandle`, `ActorCommand`, `ActorError`, `Actor` from Task 2.
- Produces: `ActorHandle::invite_create(role) -> Result<InviteDto, ActorError>`,
  `ActorHandle::invite_connect(ticket: String) -> Result<(), ActorError>`,
  `pub struct InviteDto { pub qr_string: String, pub expires_at: u64 }`.
  `spawn_actor` becomes `async fn spawn_actor() -> Result<ActorHandle, lumepeer_net::NetError>`
  (binding now needs the keystore + endpoint, both fallible) — `main.rs`
  updated accordingly.

- [ ] **Step 1: Add endpoint/keystore/ticket state and the two new commands to `Actor`**

Add to the top of `network.rs`:
```rust
use ed25519_dalek::SigningKey;
use lumepeer_net::keystore::load_or_create;
use lumepeer_net::ticket::TicketRegistry;
use lumepeer_net::{InviteTicket, PeerEndpoint};
```

Add a new DTO next to `SessionSnapshot`:
```rust
/// What `invite_create` hands back to the UI.
#[derive(Debug, Clone)]
pub struct InviteDto {
    /// String to render as a QR code and/or show as text (§7).
    pub qr_string: String,
    /// Unix seconds after which the invite is dead.
    pub expires_at: u64,
}
```

Add a `unix_now` helper (module level, above `Actor`):
```rust
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
```

Extend `ActorCommand`:
```rust
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
```

Add `ActorError::Net`:
```rust
pub enum ActorError {
    UnknownPeer,
    Core(CoreError),
    Net(lumepeer_net::NetError),
    ChannelClosed,
}
```

Add the two handle methods:
```rust
impl ActorHandle {
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
```

Add fields to `Actor` and update its constructor:
```rust
struct Actor {
    rx: mpsc::Receiver<ActorCommand>,
    sessions: SessionManager,
    install_salt: [u8; 32],
    labels: std::collections::HashMap<String, NodeId>,
    endpoint: PeerEndpoint,
    identity: SigningKey,
    tickets: TicketRegistry,
}
```

Add the two match arms in `Actor::run`:
```rust
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
```

Add the connect helper as an `impl Actor` method:
```rust
impl Actor {
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
```

This needs `postcard` as a direct dependency of `lumepeer-desktop` (it's
already a transitive dep via `lumepeer-net`/`lumepeer-core`, but Rust
requires it listed directly to `use` it here). Add to
`apps/desktop/src-tauri/Cargo.toml`:
```toml
postcard.workspace = true
```

- [ ] **Step 2: Rewrite `spawn_actor` to bind identity + endpoint**

```rust
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
```

- [ ] **Step 3: Update `main.rs`**

Tauri's `Builder::run` is synchronous, so binding the endpoint (async) has
to happen before it. Change `main()`:

```rust
fn main() {
    init_tracing();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|error| {
            eprintln!("fatal: failed to start the async runtime: {error}");
            std::process::exit(1);
        });
    let network = runtime
        .block_on(network::spawn_actor())
        .unwrap_or_else(|error| {
            eprintln!("fatal: failed to bind the network endpoint: {error}");
            std::process::exit(1);
        });

    tauri::Builder::default()
        .manage(AppState { network })
        .invoke_handler(tauri::generate_handler![
            commands::session_grant,
            commands::session_revoke,
            commands::session_status,
            commands::license_status,
            commands::invite_create,
            commands::invite_connect,
        ])
        .run(tauri::generate_context!())
        .unwrap_or_else(|error| {
            eprintln!("fatal: failed to start the application: {error}");
            std::process::exit(1);
        });
}
```

Tauri itself needs a tokio runtime for its own async commands (`#[tauri::
command] async fn`); its `tauri` feature set already includes one via the
`rt-multi-thread` workspace feature on `tokio`, so commands running inside
`tauri::Builder::default().run(...)` execute on that runtime, not the
`runtime` variable above — the `runtime` here is only used once, to drive
`spawn_actor()` to completion before Tauri starts. This matches how the
rest of the file already treats `tokio` as ambient (no runtime is
constructed elsewhere in this file today).

- [ ] **Step 4: Add the two commands to `commands.rs`**

```rust
/// Argument of [`invite_create`].
#[derive(Debug, Clone, Deserialize)]
pub struct InviteCreateArgs {
    /// Role the invite allows the guest to request.
    pub role: RoleDto,
}

/// What [`invite_create`] hands back to the UI to render as a QR code.
#[derive(Debug, Clone, Serialize)]
pub struct InviteDto {
    /// String to encode as a QR code (also usable as plain text).
    pub qr_string: String,
    /// Unix seconds after which the invite is dead.
    pub expires_at: u64,
}

/// Argument of [`invite_connect`].
#[derive(Debug, Clone, Deserialize)]
pub struct InviteConnectArgs {
    /// The scanned/pasted QR string.
    pub ticket: String,
}

/// Issues an invite for `args.role` and returns the QR payload.
///
/// # Errors
/// Rejects calls from other windows; propagates [`ActorError`].
#[tauri::command]
pub async fn invite_create(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: InviteCreateArgs,
) -> Result<InviteDto, IpcError> {
    check_window(&window)?;
    let dto = state.network.invite_create(args.role.into()).await?;
    Ok(InviteDto {
        qr_string: dto.qr_string,
        expires_at: dto.expires_at,
    })
}

/// Connects to the host named by `args.ticket`.
///
/// # Errors
/// Rejects calls from other windows; propagates [`ActorError`].
#[tauri::command]
pub async fn invite_connect(
    window: Window,
    state: tauri::State<'_, AppState>,
    args: InviteConnectArgs,
) -> Result<(), IpcError> {
    check_window(&window)?;
    state.network.invite_connect(args.ticket).await?;
    Ok(())
}
```

Add the `Net` arm to the `From<ActorError> for IpcError` impl:
```rust
ActorError::Net(e) => Self {
    code: "NET",
    message: e.to_string(),
},
```
(`NetError` already derives `thiserror::Error`/`Display` per
`crates/net/src/error.rs`, so `.to_string()` works.)

- [ ] **Step 5: Build**

Run: `cargo build --workspace`
Expected: builds clean. This step has no new automated test beyond the
build — `spawn_actor` now binds a real network endpoint and can't run
under `cargo test` without a reachable relay, which is exactly why Task 5
tests the connect/handshake logic against `bind_local` in the integration
crate instead.

- [ ] **Step 6: Commit**

```bash
git add apps/desktop/src-tauri/src/network.rs apps/desktop/src-tauri/src/commands.rs apps/desktop/src-tauri/src/main.rs apps/desktop/src-tauri/Cargo.toml
git commit -m "desktop: bind a real PeerEndpoint, add invite_create/invite_connect"
```

---

### Task 4: Accept loop — host-side handshake, ticket claim, auto-queue, ConsentGrant/Revoke over the wire

**Files:**
- Modify: `apps/desktop/src-tauri/src/network.rs`
- Modify: `apps/desktop/src-tauri/Cargo.toml`

**Interfaces:**
- Consumes: `Actor`, `ActorCommand`, `ActorHandle`, `TicketRegistry`,
  `PeerEndpoint::accept()`, `lumepeer_net::host_handshake`,
  `ControlConnection::send`/`recv` from earlier tasks and `lumepeer-net`.
- Produces: no new public API — `Actor::run` now also drives the accept
  loop and keeps live `ControlConnection`s so `Grant`/`Revoke` can write to
  the wire.

- [ ] **Step 1: Add `iroh` as a direct dependency**

`spawn_handshake` below names `iroh::endpoint::Connection` explicitly in its
signature. Rust's extern prelude only resolves crate paths for direct
dependencies, so `iroh` (currently only pulled in transitively through
`lumepeer-net`) needs to be listed directly. Add to
`apps/desktop/src-tauri/Cargo.toml`, next to the `postcard` line added in
Task 3:
```toml
iroh.workspace = true
```

Run: `cargo build -p lumepeer-desktop`
Expected: builds clean (no new code uses it yet).

- [ ] **Step 2: Add a connection table and an internal event channel**

Add to `Actor` (`ticket` replaces the earlier `role_request` field on the
handshake event — the claim needs the whole parsed ticket, not just the
role, and it has to run on the actor's own thread where `&mut
TicketRegistry` is available, not inside the spawned handshake task):
```rust
struct Actor {
    rx: mpsc::Receiver<ActorCommand>,
    sessions: SessionManager,
    install_salt: [u8; 32],
    labels: std::collections::HashMap<String, NodeId>,
    endpoint: PeerEndpoint,
    identity: SigningKey,
    tickets: TicketRegistry,
    /// Live control connections, keyed by peer, so `Grant`/`Revoke` can
    /// write `ConsentGrant`/`ConsentRevoke` back to the right stream.
    connections: std::collections::HashMap<NodeId, lumepeer_net::ControlConnection>,
    /// Handshake results and stream-closed notifications from the
    /// per-connection tasks spawned by the accept loop.
    events_tx: mpsc::Sender<ActorEvent>,
    events_rx: mpsc::Receiver<ActorEvent>,
}

/// What a spawned per-connection task reports back to the main loop.
enum ActorEvent {
    /// A guest's `Hello` passed the handshake and the ed25519 signature
    /// check; the ticket still needs `TicketRegistry::claim` before any
    /// consent is queued, which only the actor's own thread can do.
    Handshaked {
        connection: lumepeer_net::ControlConnection,
        peer: NodeId,
        ticket: InviteTicket,
    },
    /// A live connection's stream closed or errored.
    Closed { peer: NodeId },
}
```

- [ ] **Step 3: Update `spawn_actor` to build the event channel and connection table**

In `spawn_actor` (from Task 3; there is only one constructor at this point)
add, before the `Actor { ... }` literal:
```rust
let (events_tx, events_rx) = mpsc::channel(32);
```
and add these three fields to the `Actor { ... }` literal:
```rust
connections: std::collections::HashMap::new(),
events_tx,
events_rx,
```

- [ ] **Step 4: Drive the accept loop and the event channel in `Actor::run`**

Replace `Actor::run` with:
```rust
async fn run(mut self) {
    loop {
        tokio::select! {
            command = self.rx.recv() => {
                let Some(command) = command else { break };
                self.handle_command(command).await;
            }
            incoming = self.endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                if let Ok(connection) = incoming {
                    self.spawn_handshake(connection);
                }
            }
            event = self.events_rx.recv() => {
                if let Some(event) = event {
                    self.handle_event(event);
                }
            }
        }
    }
}

fn spawn_handshake(&self, connection: iroh::endpoint::Connection) {
    let tx = self.events_tx.clone();
    let now = unix_now();
    let verifying_key = self.identity.verifying_key();
    let peer = connection.remote_id();
    tokio::spawn(async move {
        let Ok((control, hello)) = lumepeer_net::host_handshake(connection).await else {
            let _ = tx.send(ActorEvent::Closed { peer }).await;
            return;
        };
        let Ok(ticket) = postcard::from_bytes::<InviteTicket>(&hello.invite_proof) else {
            control.close_with(&lumepeer_net::NetError::InvalidTicket);
            let _ = tx.send(ActorEvent::Closed { peer }).await;
            return;
        };
        if ticket.verify(&verifying_key, now).is_err() {
            control.close_with(&lumepeer_net::NetError::InvalidTicket);
            let _ = tx.send(ActorEvent::Closed { peer }).await;
            return;
        }
        let _ = tx
            .send(ActorEvent::Handshaked {
                connection: control,
                peer,
                ticket,
            })
            .await;
    });
}

fn handle_event(&mut self, event: ActorEvent) {
    match event {
        ActorEvent::Handshaked { connection, peer, ticket } => {
            // Single-use enforcement runs here, on the actor's own thread,
            // so two connections racing the same ticket cannot both win it.
            let now = unix_now();
            if self.tickets.claim(&ticket, now).is_err() {
                connection.close_with(&lumepeer_net::NetError::InvalidTicket);
                return;
            }
            let _ = self.sessions.request_consent_as(peer, ticket.allowed_request);
            self.connections.insert(peer, connection);
            self.rebuild_labels_and_snapshot();
        }
        ActorEvent::Closed { peer } => {
            self.connections.remove(&peer);
            let _ = self.sessions.on_disconnect(peer);
            self.rebuild_labels_and_snapshot();
        }
    }
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
                        if let Some(connection) = self.connections.get_mut(&peer) {
                            let _ = connection.send(lumepeer_core::protocol::MessageKind::ConsentGrant(role)).await;
                        }
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
                        if let Some(connection) = self.connections.get_mut(&peer) {
                            let _ = connection.send(lumepeer_core::protocol::MessageKind::ConsentRevoke).await;
                        }
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
            let _ = reply.send(result);
        }
        ActorCommand::InviteConnect { ticket, reply } => {
            let result = self.connect_with_ticket(&ticket).await;
            let _ = reply.send(result);
        }
    }
}
```

- [ ] **Step 5: Build**

Run: `cargo build --workspace`
Expected: builds clean.

- [ ] **Step 6: Commit**

```bash
git add apps/desktop/src-tauri/src/network.rs apps/desktop/src-tauri/Cargo.toml
git commit -m "desktop: accept-loop handshake, ticket claim, and ConsentGrant/Revoke over the wire"
```

---

### Task 5: End-to-end integration test

**Files:**
- Create: `tests/integration/tests/pairing.rs`

**Interfaces:**
- Consumes: `lumepeer_net::{PeerEndpoint, InviteTicket, host_handshake, guest_handshake}`,
  `lumepeer_net::ticket::TicketRegistry`, `lumepeer_core::session::SessionManager`,
  `ed25519_dalek::SigningKey`. All already public per `crates/net/src/lib.rs`
  and `crates/core/src/lib.rs` — no new exports needed.

- [ ] **Step 1: Write the round-trip test**

```rust
// tests/integration/tests/pairing.rs
//! End-to-end host/guest pairing over two local endpoints (design doc §7,
//! §9.1). First test that exercises `host_handshake` + `guest_handshake`
//! together against a live `TicketRegistry`, rather than each in isolation.

use ed25519_dalek::SigningKey;
use lumepeer_core::consent::Role;
use lumepeer_core::session::SessionManager;
use lumepeer_net::ticket::{InviteTicket, TicketRegistry};
use lumepeer_net::{PeerEndpoint, host_handshake, guest_handshake};

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before epoch")
        .as_secs()
}

#[tokio::test]
async fn full_pairing_round_trip_grants_and_the_guest_sees_it() {
    let host_secret = iroh::SecretKey::from_bytes(&[1u8; 32]);
    let host_identity = SigningKey::from_bytes(&host_secret.to_bytes());
    let host = PeerEndpoint::bind_local(host_secret)
        .await
        .expect("host bind");

    let guest_secret = iroh::SecretKey::from_bytes(&[2u8; 32]);
    let guest = PeerEndpoint::bind_local(guest_secret)
        .await
        .expect("guest bind");

    let now = unix_now();
    let ticket = InviteTicket::issue(&host_identity, &host.addr(), Role::ViewOnly, now)
        .expect("issue ticket");
    let mut registry = TicketRegistry::new();
    registry.register(&ticket);

    let host_addr = host.addr();
    let accept = tokio::spawn(async move {
        let incoming = host.accept().await.expect("accepted")?;
        let (control, hello) = host_handshake(incoming).await?;
        Ok::<_, lumepeer_net::NetError>((control, hello))
    });

    let proof = postcard::to_allocvec(&ticket).expect("encode proof");
    let connection = guest
        .connect_control(host_addr)
        .await
        .expect("guest dials host");
    let guest_control = guest_handshake(connection, Role::ViewOnly, proof, Vec::new())
        .await
        .expect("guest handshake");

    let (host_control, hello) = accept.await.expect("join").expect("host handshake");
    let claimed_ticket: InviteTicket =
        postcard::from_bytes(&hello.invite_proof).expect("decode proof");
    registry
        .claim(&claimed_ticket, now)
        .expect("first claim succeeds");
    assert!(
        registry.claim(&claimed_ticket, now).is_err(),
        "a second claim of the same ticket must be refused"
    );

    let mut sessions = SessionManager::new();
    sessions
        .request_consent_as(host_control.peer(), hello.role_request)
        .expect("queued");
    sessions
        .grant(host_control.peer(), Role::ViewOnly)
        .expect("granted");

    let mut host_control = host_control;
    host_control
        .send(lumepeer_core::protocol::MessageKind::ConsentGrant(
            Role::ViewOnly,
        ))
        .await
        .expect("send grant");

    let mut guest_control = guest_control;
    let envelope = guest_control.recv().await.expect("guest receives grant");
    assert!(matches!(
        envelope.kind,
        lumepeer_core::protocol::MessageKind::ConsentGrant(Role::ViewOnly)
    ));
}

#[tokio::test]
async fn an_expired_ticket_is_refused_by_the_registry_after_a_real_handshake() {
    let host_secret = iroh::SecretKey::from_bytes(&[3u8; 32]);
    let host_identity = SigningKey::from_bytes(&host_secret.to_bytes());
    let host = PeerEndpoint::bind_local(host_secret)
        .await
        .expect("host bind");

    let past = 1_000u64;
    let ticket = InviteTicket::issue(&host_identity, &host.addr(), Role::ViewOnly, past)
        .expect("issue ticket");
    let mut registry = TicketRegistry::new();
    registry.register(&ticket);

    let far_future = past + lumepeer_core::constants::INVITE_TICKET_TTL_SECS + 1;
    assert!(registry.claim(&ticket, far_future).is_err());
}
```

- [ ] **Step 2: Run it**

Run: `cargo test -p lumepeer-integration-tests --test pairing`
Expected: both tests PASS.

- [ ] **Step 3: Commit**

```bash
git add tests/integration/tests/pairing.rs
git commit -m "test: end-to-end host/guest pairing round trip over local endpoints"
```

---

### Task 6: Frontend — pending vs. active split, invite view, QR rendering

**Files:**
- Modify: `apps/desktop/package.json`
- Modify: `apps/desktop/src/session-status.ts` (whole file)
- Modify: `apps/desktop/src/main.ts` (whole file)
- Create: `apps/desktop/src/invite-view.ts`

**Interfaces:**
- Consumes: `invite_create`/`invite_connect`/`session_grant`/
  `session_revoke`/`session_status` Tauri commands from Task 3/4; `SessionStatus`
  type extended in this task.
- Produces: `export interface SessionStatus { peer_label: string; role: Role;
  input: boolean; state: 'pending' | 'active' }`, `export function
  inviteView(): TemplateResult` (mounted by `main.ts`).

- [ ] **Step 1: Add the `qrcode` dependency**

Edit `apps/desktop/package.json`:
```json
  "dependencies": {
    "@tauri-apps/api": "^2",
    "lit-html": "^3",
    "qrcode": "^1.5.4"
  },
  "devDependencies": {
    "@testing-library/dom": "^10.4.1",
    "@types/qrcode": "^1.5.5",
    "axe-core": "^4.13.0",
    "jsdom": "^30.0.1",
    "typescript": "^5",
    "vite": "^6",
    "vitest": "^4.1.10"
  }
```

Run: `cd apps/desktop && npm install`
Expected: lockfile updates, install succeeds.

- [ ] **Step 2: Extend `SessionStatus` with `state`**

```typescript
// apps/desktop/src/session-status.ts
// Active session indicator (design doc §15, §21).
//
// While anyone is connected this must stay visible, and revoke must be one
// click away. Peers are shown by pseudonymized label: raw identities never
// reach the UI.

import { html, type TemplateResult } from 'lit-html';

import type { Locale } from './i18n';
import { t } from './i18n';
import type { Role } from './consent-dialog';

export type SessionState = 'pending' | 'active';

export interface SessionStatus {
  peer_label: string;
  role: Role;
  input: boolean;
  state: SessionState;
}

async function revoke(peer: string): Promise<void> {
  const { invoke } = await import('@tauri-apps/api/core');
  await invoke('session_revoke', { args: { peer } });
}

const roleKey: Record<Role, 'status.role.viewOnly' | 'status.role.controlLimited' | 'status.role.fullControl'> = {
  view_only: 'status.role.viewOnly',
  control_limited: 'status.role.controlLimited',
  full_control: 'status.role.fullControl',
};

export function sessionStatus(sessions: SessionStatus[], locale: Locale): TemplateResult {
  if (sessions.length === 0) {
    return html`<section class="status" aria-live="polite"><p>${t(locale, 'status.notSharing')}</p></section>`;
  }

  return html`
    <section class="status" aria-live="polite">
      <h2>${t(locale, 'status.heading')}</h2>
      <ul>
        ${sessions.map(
          (session) => html`
            <li>
              <span>${session.peer_label}</span>
              <span>${t(locale, roleKey[session.role])}</span>
              <span>${session.input ? t(locale, 'status.inputOn') : t(locale, 'status.inputOff')}</span>
              <button type="button" @click=${() => void revoke(session.peer_label)}>
                ${t(locale, 'status.revoke')}
              </button>
            </li>
          `,
        )}
      </ul>
    </section>
  `;
}
```

(Only the added `SessionState` type and `state` field changed; rendering
logic is unchanged — `sessionStatus` is called with the *active*-only
subset from `main.ts` now, same as it implicitly was before this task,
since `session_status` used to return only active sessions.)

- [ ] **Step 3: Create `invite-view.ts`**

```typescript
// apps/desktop/src/invite-view.ts
// Invite creation and connect form (design doc §7).
//
// The host asks for a QR; the guest pastes/scans the resulting string back.
// Neither side's identity is typed in by a human — the ticket carries it.

import { html, type TemplateResult } from 'lit-html';
import QRCode from 'qrcode';

import type { Locale } from './i18n';
import { t } from './i18n';

let lastQr: { dataUrl: string; text: string } | undefined;
let connectError: string | undefined;

async function createInvite(): Promise<void> {
  const { invoke } = await import('@tauri-apps/api/core');
  const invite = await invoke<{ qr_string: string; expires_at: number }>('invite_create', {
    args: { role: 'view_only' },
  });
  const dataUrl = await QRCode.toDataURL(invite.qr_string);
  lastQr = { dataUrl, text: invite.qr_string };
}

async function connect(ticket: string): Promise<void> {
  connectError = undefined;
  const { invoke } = await import('@tauri-apps/api/core');
  try {
    await invoke('invite_connect', { args: { ticket } });
  } catch (error) {
    connectError = error instanceof Error ? error.message : String(error);
  }
}

export function inviteView(locale: Locale): TemplateResult {
  return html`
    <section class="invite">
      <h2>${t(locale, 'invite.heading')}</h2>
      <button type="button" @click=${() => void createInvite()}>
        ${t(locale, 'invite.create')}
      </button>
      ${lastQr
        ? html`<img alt=${t(locale, 'invite.qrAlt')} src=${lastQr.dataUrl} />
            <p>${lastQr.text}</p>`
        : ''}
      <form
        @submit=${(event: SubmitEvent) => {
          event.preventDefault();
          const input = (event.target as HTMLFormElement).elements.namedItem(
            'ticket',
          ) as HTMLInputElement;
          void connect(input.value);
        }}
      >
        <label for="ticket-input">${t(locale, 'invite.connectLabel')}</label>
        <input id="ticket-input" name="ticket" type="text" />
        <button type="submit">${t(locale, 'invite.connect')}</button>
      </form>
      ${connectError ? html`<p role="alert">${connectError}</p>` : ''}
    </section>
  `;
}
```

`t(locale, 'invite.heading')` etc. need entries in the i18n dictionary.
Check `apps/desktop/src/i18n.ts` for the dictionary shape used by existing
keys like `status.heading`, and add `invite.heading`, `invite.create`,
`invite.qrAlt`, `invite.connectLabel`, `invite.connect` for every locale
`SUPPORTED_LOCALES` lists (English and Arabic per the current dictionary),
following the exact structure already there for `status.*`.

- [ ] **Step 4: Wire it into `main.ts`**

```typescript
// apps/desktop/src/main.ts
// Webview entry point (design doc §5.1, §13).
//
// Vanilla TypeScript plus lit-html: the consent screen must render instantly
// on weak hardware, so no React/Vue/Angular. The UI never decides anything —
// it renders what the Rust core reports and forwards the host's clicks back.

import { render } from 'lit-html';

import { consentDialog } from './consent-dialog';
import { detectLocale, dirOf, type Locale } from './i18n';
import { inviteView } from './invite-view';
import { sessionStatus, type SessionStatus } from './session-status';

const root = document.querySelector('#app');
let locale: Locale = detectLocale(navigator);

function applyDir(): void {
  document.documentElement.lang = locale;
  document.documentElement.dir = dirOf(locale);
}

async function refresh(): Promise<void> {
  if (!root) {
    return;
  }
  applyDir();
  const { invoke } = await import('@tauri-apps/api/core');
  const sessions = await invoke<SessionStatus[]>('session_status');
  const pendingRequest = sessions.find((session) => session.state === 'pending');
  const activeSessions = sessions.filter((session) => session.state === 'active');

  render(
    [inviteView(locale), consentDialog(pendingRequest, locale), sessionStatus(activeSessions, locale)],
    root as HTMLElement,
  );
}

// Exposed for manual/e2e locale switching; the consent screen itself carries
// no locale picker (§19 phase 6 doesn't ask for one, and adding UI chrome to
// a screen that must render instantly is scope creep) — the OS/webview
// locale via `navigator.language` is what `detectLocale` reads.
export function setLocale(next: Locale): void {
  locale = next;
  void refresh();
}

void refresh();
setInterval(() => {
  void refresh();
}, 1000);
```

- [ ] **Step 5: Typecheck**

Run: `cd apps/desktop && npm run typecheck`
Expected: no errors.

- [ ] **Step 6: Commit**

```bash
git add apps/desktop/package.json apps/desktop/package-lock.json apps/desktop/src/session-status.ts apps/desktop/src/main.ts apps/desktop/src/invite-view.ts apps/desktop/src/i18n.ts
git commit -m "desktop: invite/connect UI with client-side QR rendering, split pending vs active status"
```

---

### Task 7: Update existing frontend test fixtures for the `state` field

**Files:**
- Modify: `apps/desktop/src/keyboard-nav.test.ts:29,36`
- Modify: `apps/desktop/src/accessibility.test.ts:37,51`

**Interfaces:**
- Consumes: `SessionStatus` type from Task 6 (now requires `state`).

- [ ] **Step 1: Update `keyboard-nav.test.ts` fixtures**

In both `it(...)` blocks, change:
```typescript
const request: SessionStatus = { peer_label: 'guest-ab12', role: 'view_only', input: false };
```
to:
```typescript
const request: SessionStatus = { peer_label: 'guest-ab12', role: 'view_only', input: false, state: 'pending' };
```

- [ ] **Step 2: Update `accessibility.test.ts` fixtures**

Change:
```typescript
const request: SessionStatus = { peer_label: 'guest-ab12', role: 'view_only', input: false };
```
to:
```typescript
const request: SessionStatus = { peer_label: 'guest-ab12', role: 'view_only', input: false, state: 'pending' };
```
and:
```typescript
const sessions: SessionStatus[] = [
  { peer_label: 'guest-ab12', role: 'full_control', input: true },
];
```
to:
```typescript
const sessions: SessionStatus[] = [
  { peer_label: 'guest-ab12', role: 'full_control', input: true, state: 'active' },
];
```

- [ ] **Step 3: Run the frontend test suite**

Run: `cd apps/desktop && npm test`
Expected: all tests PASS (keyboard-nav, accessibility, i18n).

- [ ] **Step 4: Commit**

```bash
git add apps/desktop/src/keyboard-nav.test.ts apps/desktop/src/accessibility.test.ts
git commit -m "test: add state field to SessionStatus fixtures"
```

---

## Final verification

- [ ] Run: `cargo build --workspace && cargo test --workspace`
  Expected: all green, including the new `network::tests` module and
  `tests/integration/tests/pairing.rs`.
- [ ] Run: `cd apps/desktop && npm run typecheck && npm test`
  Expected: all green.
- [ ] Manually run `cargo run -p lumepeer-desktop` on two machines (or two
  accounts on one LAN), create an invite on one, paste the QR string into
  the other's connect form, confirm a consent prompt appears on the host
  and granting it lets the guest observe `ConsentGrant` — this is the
  scenario from the original request ("запустил и он в сети, показывает qr
  и если с другого устройства запустить бинарь, он смог видеть его").
