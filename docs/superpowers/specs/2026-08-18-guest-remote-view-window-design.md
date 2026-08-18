# Guest remote-view window: capture, encode, decode, render, input forwarding

Status: proposed (design discussion only, not yet implemented).

## Problem

A user reported that connecting to a host produces no visible remote-screen
window on the guest side. Investigation found this is not a regression: the
commit that claims to add it, `7dc88a5` ("add video window"), touches all 153
files in the repository with **zero content changes** (`git show --stat`
shows every file at `0 insertions, 0 deletions`) — a no-op, most likely an
accidental empty commit. Nothing in `apps/desktop` has ever opened
`ALPN_MEDIA`, driven capture, or rendered a frame. `session-status.ts`
renders an active session as a text row (peer label, role, input toggle,
revoke button) in the existing main window — that is the entire visible
result of a successful pairing today.

The pairing flow itself (`docs/superpowers/specs/2026-08-13-desktop-pairing-
flow-design.md`) is implemented and working; this spec picks up exactly
where that one explicitly stopped (it lists media as out of scope).

This is an integration gap, not a from-scratch build — most of the machinery
already exists and is tested in isolation:

- `CaptureController` (`crates/media/src/capture/mod.rs:271`) — its own doc
  comment already states the intended lifecycle: *"Capture starts only once
  a viewer holds a `view` grant and stops with the last viewer (§8.1, §11)"*.
  `add_viewer`/`remove_viewer`/`next_frame`/`viewer_count` are implemented
  and unit-tested.
- `select_encoder`/`probe_hardware` (`crates/media/src/encode/mod.rs`) —
  hardware-first encoder selection (ADR 0011).
- `DecoderHandle::spawn`/`decode` (`crates/media/src/decode/mod.rs`) —
  sandboxed decode over a shared-memory ring, covered by
  `tests/integration/tests/media_pipeline.rs`.
- `platform_injector()`/`InputInjector` (`crates/media/src/capture/mod.rs`)
  — host-side input injection, already implemented.
- `MessageKind` already has `input_pointer_move`/`input_press`/`input_wheel`
  variants (proven by the fuzz corpus at
  `tests/fuzz/corpus/control_envelope/`) — the wire protocol for input
  exists.
- `ALPN_MEDIA` is a defined channel (`crates/net/src/endpoint.rs:15`,
  dispatched in `crates/net/src/connection.rs:37`).
- `NetworkActor` already broadcasts `ActorNotification::ConsentGranted` /
  `ConsentRevoked` / `Disconnected` (`apps/desktop/src-tauri/src/
  network.rs:72-84`) — currently nothing outside `status()` polling
  listens to this stream.

What's missing is entirely in `apps/desktop`: nothing wires these pieces
together, there is no view window, no canvas, no frame- or input-IPC.

Also found while reading `crates/media`: despite `Grants::view`'s doc
comment ("Receive video and audio"), there is no audio capture, encode, or
mixing anywhere in the crate. Treated here as a pre-existing gap, not
something this spec fixes.

## Decisions confirmed with the user (2026-08-18)

- The guest's view window opens **automatically** the moment its session
  reaches `active` — no manual "View" button.
- **Closing** the view window **ends the session** (equivalent to today's
  revoke button) — one on/off switch, not two independent states to keep in
  sync.

## Non-goals

- Audio — not implemented anywhere in the codebase yet; a separate effort.
- `ControlLimited`'s "allowlisted actions" — driven by
  `config/control_policy.toml`, a mechanism distinct from raw input
  forwarding. This spec wires input only for `FullControl`
  (`Grants::from_role`, `crates/core/src/consent.rs:66`, is the only role
  with `input: true`).
- Multi-monitor picker UI. `CaptureTarget::Display(u32)` already exists at
  the data level; v1 always requests `CaptureTarget::PrimaryDisplay`.
- Host-side reverse view (host watching/controlling the guest) — this spec
  is guest-views-host only, matching the existing pairing model.
- Clipboard, file transfer, recording windows/UI — independent grants,
  independent specs.
- Guest-side saved connections (reconnect without re-scanning a QR) —
  already called out as a follow-up in the pairing-flow spec; unaffected by
  this one.

## Architecture

### Host-side `NetworkActor` additions

- One `CaptureController` owned by the actor, constructed once at startup
  wrapping `platform_capturer()`. Shared across every peer session — its
  existing `viewer_count()`-driven start/stop logic already handles the
  first-viewer-starts / last-viewer-stops lifecycle; nothing new needed
  there.
- At the point the host already writes `ConsentGrant` to a peer's control
  stream (`network.rs:593`), also call
  `capture_controller.add_viewer(peer)`.
- A capture→encode loop, spawned on first viewer and stopped at
  `viewer_count() == 0`: `capture.next_frame()` → the encoder
  `select_encoder` picked at startup → write the resulting `EncodedFrame`
  onto that peer's media connection.
- Extend the accept loop to also accept `ALPN_MEDIA` connections, gated on
  a live control-channel session already existing for that peer — mirrors
  the file-transfer channel's existing lazy-open guard (see this repo's
  `crates/net` architecture notes: neither an active media stream nor a
  large transfer may delay a revoke on the control channel).
- On `ConsentRevoked` or peer disconnect: `capture_controller.
  remove_viewer(peer)`, close that peer's media connection and encode loop.

### Guest-side `NetworkActor` additions

- A new internal listener on the actor's own `ActorNotification` stream
  (the broadcast channel already exists; nothing currently subscribes to it
  besides the IPC layer's future use). On `ConsentGranted { role }`: dial
  the host's `ALPN_MEDIA`, spawn a `DecoderHandle`, and start a receive
  loop that decodes incoming frames and keeps only the newest
  `DecodedFrame` in a single-slot `watch::channel` (overwrite-on-push —
  "latest frame wins," consistent with `crates/media::jitter::JitterBuffer`
  already handling reordering upstream of decode, not the display side).
- The actor creates the `view-{label}` window itself via a `tauri::
  AppHandle` (passed into `spawn_actor`/`spawn_actor_with` at startup, a
  new parameter) rather than routing window creation through a webview
  poll — keeps "the Tauri layer owns the window, the actor decides
  everything" consistent with `main.rs`'s and `commands.rs`'s existing doc
  comments, and avoids a poll-interval (currently 1s) delay before the
  window appears.
- New Tauri command `view_next_frame(peer) -> tauri::ipc::Response` —
  returns the latest `DecodedFrame`'s pixel buffer and dimensions as a raw
  binary IPC response (not JSON/base64, to keep per-frame overhead down),
  or an explicit "no frame yet" sentinel before the first frame arrives.
- New commands `input_pointer_move`/`input_press`/`input_wheel`, gated by
  `check_window` the same way every existing command is, sent only from the
  `view-{label}` window and only when that session's grant has `input:
  true`. The actor writes these onto the guest's own control stream; the
  host's existing dispatch (`network.rs:552` already matches on
  `MessageKind`) gains arms for these three kinds and calls into
  `platform_injector()`.
- On `ConsentRevoked`/`Disconnected`: close the `view-{label}` window, tear
  down the `DecoderHandle` and media connection.

### New capability file

`capabilities/view.json` (or an equivalent window-label-glob capability,
pending verification against the installed Tauri version's capability
schema — see Open Questions) scoped only to `view-*` windows, granting
exactly `view_next_frame`, `input_pointer_move`, `input_press`,
`input_wheel`, and a close permission scoped to that window — explicitly
**not** `core:default`, keeping the existing "no wildcard entries" rule
`capabilities/main.json` already documents for itself.

### Frontend

- New Vite entry (separate HTML/TS, since each Tauri window can load a
  distinct page) with a `<canvas>`, a `requestAnimationFrame` loop calling
  `invoke('view_next_frame', { peer })` and painting via
  `ImageData`/`putImageData`.
- Pointer/keyboard listeners attached only when the session's `input` grant
  is true (the window needs this fact at load time — likely a one-shot
  `view_session_info(peer)` call or a query-string parameter set when the
  actor creates the window; exact mechanism is an implementation detail,
  not a design fork).
- `onCloseRequested` handler calls the existing `session_revoke` command
  for that peer, per the confirmed decision that closing the window ends
  the session.

## Data flow (happy path: guest connects, host grants `FullControl`)

1. Guest's actor receives `ConsentGrant(FullControl)` on the control stream
   (existing pairing flow) → broadcasts `ActorNotification::
   ConsentGranted { role: FullControl }`.
2. New: the actor's own listener reacts — opens `ALPN_MEDIA` to the host,
   spawns a `DecoderHandle`, creates the `view-{label}` window.
3. Host's actor, at the same moment it wrote `ConsentGrant`
   (`network.rs:593`), calls `capture_controller.add_viewer(peer)`; the
   capture/encode loop starts (or, if other viewers are already active,
   is already running) and begins writing frames once the guest's media
   connection arrives.
4. The guest's window loads; its render loop starts polling
   `view_next_frame`. Before the first real frame exists it gets the "no
   frame yet" sentinel and shows a loading/placeholder state.
5. Pointer/keyboard events in the window (listened for because
   `input: true`) become `input_pointer_move`/`input_press`/`input_wheel`
   commands → written on the guest's control stream → host's dispatch
   calls `platform_injector()`.
6. Either side revokes or disconnects → guest's actor observes
   `ConsentRevoked`/`Disconnected` → closes the window, tears down the
   decoder and media connection. Host's `remove_viewer` stops capture if
   this was the last viewer.
7. Guest closes the window manually → existing `session_revoke` IPC call →
   existing `SessionManager::revoke` path (same one the status-list revoke
   button already uses) → same teardown as step 6, from the other
   direction.

## Error handling

### Connection-health policy (confirmed with the user, 2026-08-18)

One unified recovery policy covers both ways the media pipeline can fail
after it's up — a dropped/failed media connection and a crashed
`DecoderHandle` — since both look identical to the guest ("video stopped");
neither is a revoke, so neither closes the window on the first failure:

1. On failure detection (media connection error/drop, or the decoder
   process exiting unexpectedly): do **not** close the window immediately.
   Show an inline, non-blocking state in the view window itself (e.g.
   "Connection lost, reconnecting…") and attempt exactly **one** recovery
   pass — redial the media connection and/or respawn the `DecoderHandle` as
   needed — bounded by `RECONNECT_WINDOW_SECS` (reusing the existing
   control-channel constant, `crates/core/src/constants.rs:11`, per the
   user's instruction rather than adding a second one).
2. **Recovery succeeds** within the window: clear the inline state, resume
   normal frame display, and reset the failure counter to zero. This is a
   rolling one-shot budget, not a lifetime total — a later, unrelated
   failure gets its own fresh single recovery attempt.
3. **Recovery fails** (window elapses, or the respawned/redialed connection
   fails again before producing a frame): close the view window — same
   teardown path as a manual close, which per the confirmed decision above
   already ends the session (`session_revoke`) — and separately surface a
   modal error dialog explaining what happened. The window-equals-active-
   session invariant stays intact: there is deliberately no third state
   ("session still active, window gone, waiting for the user to somehow
   reopen it") since there is no manual "reopen view" affordance in this
   design. If this isn't the intended behavior, flag it — it's my inference
   to keep the model consistent with the auto-open/close-ends-session
   decisions already made, not something separately confirmed.

- `input_*` commands for a session whose grant has since dropped `input`
  (a later `session_grant` call lowered the role): the host must check the
  *current* `Grants.input` per incoming event, not only at the moment
  `ConsentGrant` was first sent, and drop the event rather than inject it.
  Grants are live, not a one-time decision (matches the project's existing
  consent framing).

## Open questions for the user

All resolved as of 2026-08-18 except:

- `capabilities/view.json`'s window-label glob (`view-*`) needs verifying
  against the Tauri v2 capability schema actually installed here before any
  code is written — flagging as unverified, not asking the user; to be
  confirmed during implementation.

## Testing

- `crates/media`: already covered by `media_pipeline.rs`; no new coverage
  needed for this spec specifically.
- `apps/desktop/src-tauri`: actor-level tests following the pattern the
  pairing-flow spec established (`bind_local` endpoints, drive through the
  real `mpsc` channel) — verify `add_viewer`/`remove_viewer` fire exactly
  on grant/revoke/disconnect, and that a media dial before a live control
  session is refused (already covered by `network.rs:831`'s
  `a_media_alpn_connection_is_refused_without_a_handshake`; extend rather
  than duplicate it).
- Frontend: a new `view-window.test.ts` alongside the existing
  `accessibility.test.ts`/`keyboard-nav.test.ts` pattern — canvas render
  given a fixture `DecodedFrame`, and pointer/keyboard listeners attached
  only when `input: true`.

## Follow-up (separate spec)

- `ControlLimited`'s allowlisted-actions wiring against
  `config/control_policy.toml`.
- Audio.
- Multi-monitor picker.
