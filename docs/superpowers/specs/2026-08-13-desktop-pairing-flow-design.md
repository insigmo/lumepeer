# Desktop pairing flow: QR invite, connect, consent

Status: approved by user, not yet implemented.

## Problem

The desktop app's Tauri IPC surface (`session_request`/`session_grant`/
`session_revoke`/`session_status`) only manipulates `lumepeer_core::session::
SessionManager` state. Nothing wires it to the network: `AppState` holds no
`PeerEndpoint`, no `TicketRegistry`, and no accept loop. There are no
`invite_create`/`invite_connect` commands and the frontend never renders a
QR code or scans/enters a ticket. Despite the project having reached "Phase
6" commits (release checklist, i18n, accessibility, security review), a user
running the desktop binary cannot actually pair with a second device — the
crypto and framing in `lumepeer-net` (`ticket.rs`, `connection.rs`,
`endpoint.rs`) exist and are unit-tested, but nothing in the desktop binary
calls them.

Two further defects were found while reading the existing surface, both in
scope here because fixing them requires the same wiring:

1. `session_status` returns only `SessionManager::active()` — the
   `AwaitingConsent` queue is never exposed, so `consentDialog(undefined,
   locale)` in `main.ts` can never render a real pending request.
2. `consent-dialog.ts` calls `grant(request.peer_label, ...)` /
   `revoke(request.peer_label, ...)`, passing the pseudonymized label where
   the Rust side (`parse_peer`) expects a hex `NodeId`. This can never
   succeed against a real peer: a truncated BLAKE3 hash does not parse as an
   ed25519 public key.

This spec covers the base pairing flow only: host issues an invite and shows
a QR, a guest enters/scans it and connects, the host is asked for consent and
grants/denies it. A follow-up spec will add guest-side "saved connections"
(reconnect to a previously-paired host without scanning a QR again) on top
of this.

## Non-goals

- LAN/mDNS discovery of unknown peers. Explicitly rejected by the user in
  favor of ticket-based pairing (matches design doc §7: identity is never
  broadcast).
- Auto-granting a role to a returning peer. Every connection — first time or
  reconnect — gets a fresh host decision (design doc §2.3).
- Host-side visibility/management of a guest history list. Saved
  connections are guest-side only (next spec).
- Push events from Rust to the webview. The frontend already polls
  `session_status` every second; this spec extends that payload rather than
  introducing a new transport.

## Architecture

A single long-lived tokio task, `NetworkActor`
(`apps/desktop/src-tauri/src/network.rs`), spawned once at startup and kept
alive for the process lifetime. It is the sole owner of:

- `PeerEndpoint` (bound at startup from the keystore identity)
- `TicketRegistry` (host-side ticket lifecycle)
- `SessionManager` (moves out of `AppState`'s `Mutex` into the actor —
  the actor is now the single authorization decision point's runtime home)
- `HashMap<NodeId, ControlConnection>` — live control streams, needed to
  write `ConsentGrant`/`ConsentDeny` back to the peer that is being decided
  on
- `HashMap<String, NodeId>` — label → NodeId resolution table, rebuilt on
  every state-changing operation, so the IPC surface never has to parse a
  NodeId out of anything the webview sent

`AppState` shrinks to a single `mpsc::Sender<ActorCommand>` clone handle (and
`install_salt`, unchanged). Tauri commands become `async fn`: they build an
`ActorCommand` with a `oneshot::Sender` for the reply, send it, and await the
reply. If the send or the reply await fails (actor task gone), the command
maps that to `IpcError::poisoned()` — same code the current lock-poisoning
path already uses, same meaning to the frontend.

The actor's main loop is a `tokio::select!` between:
- the `mpsc::Receiver<ActorCommand>` (IPC-driven work)
- `PeerEndpoint::accept()` (incoming connections)

Each accepted connection's handshake (`host_handshake` + `TicketRegistry::
claim`) runs in its own `tokio::spawn`, reporting back to the actor over an
internal channel, so one bad/slow handshake never blocks the accept loop or
IPC responsiveness.

## Components / IPC surface

New:
- `invite_create(role: RoleDto) -> Result<InviteDto, IpcError>` — issues and
  registers a ticket, returns `{ qr_string, expires_at }`. QR rendering
  stays client-side (frontend adds the `qrcode` npm package); no new Rust
  dependency.
- `invite_connect(ticket: String) -> Result<(), IpcError>` — guest side:
  parses the ticket, dials, runs `guest_handshake` with the raw ticket bytes
  as `invite_proof`.

Changed:
- `session_status` gains queued (`AwaitingConsent`) entries alongside active
  ones. `SessionStatusDto` gains `state: "pending" | "active"` and its
  `peer_label` becomes the sole peer-identifying string the frontend ever
  sees or echoes back (unchanged in spirit from today's doc comment on
  `parse_peer`, now actually true).
- `session_request`/`session_grant`/`session_revoke` take `peer: String`
  meaning the label, resolved to `NodeId` inside the actor via the
  label→NodeId table. `parse_peer`/`bad_peer()` as hex-NodeId parsing is
  deleted — it never worked and was never meant to (see Problem, defect 2).
- All five existing commands become `async fn` bodies that talk to the
  actor instead of locking a `Mutex<SessionManager>` directly.

## Data flow

**Host issues invite:** UI → `invite_create` → actor: `TicketRegistry::
register` + `InviteTicket::issue` → `qr_string` → frontend renders QR.

**Guest connects:** UI → `invite_connect(ticket_string)` → actor: parse →
`connect_control` → `guest_handshake(invite_proof = raw ticket bytes)` →
on success actor stores the `ControlConnection`, session state
`Authenticating`.

**Host receives Hello:** accept-loop spawn: `host_handshake` → extract
`invite_proof`, parse back into `InviteTicket`, `TicketRegistry::claim
(ticket, now)` (the first real enforcement point — currently unreferenced
anywhere) → on success `SessionManager` gets a new entry, state
`AwaitingConsent`, pushed onto `ConsentQueue`. Any failure (bad major,
expired, reused, malformed) → `close_with`, connection dropped, no consent
ever offered.

**Poll:** `session_status` returns both active and pending entries, labels
resolved from the actor's table.

**Grant:** UI → `session_grant(label, role)` → actor resolves label→NodeId
→ `SessionManager::grant` → actor looks up the session's `ControlConnection`
and writes `ConsentGrant` on its control stream — this send does not exist
today at all.

## Error handling

Guest side (`invite_connect`):
- Malformed/expired ticket string → `NetError::MalformedTicket` →
  `IpcError{code: "BAD_TICKET"}`.
- Dial failure (host offline/unreachable) → `IpcError{code: "DIAL_FAILED"}`.
- Version mismatch → `IpcError{code: "INCOMPATIBLE_VERSION"}`.
- Host closed after Hello (claim rejected server-side for any reason) →
  `IpcError{code: "REJECTED"}` — deliberately undifferentiated so a
  stranger probing a ticket learns nothing about *why* it failed.

Host accept-loop (background, never surfaces as an IPC error):
- Handshake or claim failure → close with the matching §18 close code, log
  via `tracing` with `audit::peer_hash`, no consent ever queued.
- One failed handshake never stops the loop (`tokio::spawn` per connection).

Actor channel:
- Actor task gone (channel closed) → any pending command's send/await fails
  → `IpcError::poisoned()`.
- No `.unwrap()`/`.expect()` on the network/parse path inside the actor,
  consistent with the existing workspace lint (`unwrap_used`/`expect_used`
  = warn) and design doc §21.

Session lifecycle: guest disconnects before grant → actor detects the
closed stream on `recv()`, removes the entry from the queue/session map;
the next poll simply omits it.

## Testing

Unit (`crates/net`):
- Existing `ticket.rs` coverage extended: `TicketRegistry::claim` exercised
  through `host_handshake`, not just directly.
- Label→NodeId resolution: grant/revoke on an unknown label returns a clean
  error, no panic.

Integration (`tests/integration`, already has `iroh`/`tokio` dev-deps):
- Two `PeerEndpoint::bind_local` endpoints in-process, full round trip:
  issue → connect → claim → consent queued → grant → guest observes
  `ConsentGrant`. First test that exercises `host_handshake` +
  `guest_handshake` together against a live `TicketRegistry`.
- Expired ticket rejected; reused ticket rejected over the wire (not just
  at the registry-unit level).
- Version mismatch produces the close code the guest observes.

Actor-level (new, `apps/desktop/src-tauri`):
- Spawn the actor against `bind_local` endpoints in-test, drive it through
  the same `mpsc` channel the real commands use, without a Tauri window.

Frontend (existing `keyboard-nav.test.ts`, `accessibility.test.ts`,
`i18n.test.ts`):
- Update fixtures for `SessionStatusDto`'s new `state` field and for
  `grant`/`revoke` now sending the label they were already shown, not a hex
  NodeId. No new test framework.

## Follow-up (separate spec)

Guest-side saved connections: after a successful pairing, the guest
persists `{ NodeId, display_name }` locally and can reconnect by picking
from that list without re-scanning a QR. The host still runs a full fresh
consent decision every time — no remembered role, no auto-grant. Storage
location/encryption for that list is an open question for that spec, not
this one.
