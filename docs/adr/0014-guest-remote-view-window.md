# ADR 0014 — Guest remote-view window: media channel, IPC ACL, failure policy

Status: accepted
Date: 2026-08-18

## Context

Everything the remote screen needs existed and was unit-tested in isolation
— `CaptureController` (§8.1/§11), `select_encoder` (§11), `DecoderHandle`
(§11.3), `platform_injector()` (§11), `ALPN_MEDIA` (§4.1),
`MessageKind::InputEvent` (§9.1) — and nothing wired them together.
`apps/desktop` had never opened a media connection, driven capture or
rendered a frame; a successful pairing showed one text row in the main
window and nothing else. Design work is in
`docs/superpowers/specs/2026-08-18-guest-remote-view-window-design.md`.

This ADR records the four decisions in that wiring that the design doc does
not already settle.

## Decision

### 1. A separate media framing layer, not `MessageEnvelope`

`crates/net/src/media.rs` adds `MediaFrameWriter`/`MediaFrameReader`:
`u32_be length || bytes`, bounded by a new `MAX_MEDIA_FRAME_BYTES` (§14).
`framing.rs` is hard-typed to `MessageEnvelope` — it decodes postcard,
checks `Direction` and enforces the per-direction `seq` of §9.1 — none of
which applies to a video bitstream, and its `MAX_CONTROL_FRAME_BYTES` bound
of 64 KiB is two orders of magnitude below one keyframe.

What is copied is the part that matters: the announced length is validated
before any buffer is allocated (§3.2). There is deliberately no anti-replay
tuple — a media stream carries no authorization, so replaying a picture
cannot widen anything, QUIC already gives per-stream ordering, and
reordering across frames is `jitter`'s job upstream of decode.

The media stream is opened **by the host** (`open_uni`) after the host has
decided the peer may have it. A guest that was never granted `view` simply
never sees a stream, so there is no guest-side proof to verify on this
channel and no second handshake to keep in step with consent.

### 2. `ALPN_MEDIA` is accepted, but only behind a live granted session

`spawn_handshake` used to close every non-control connection. It now
routes `Channel::Media` to the actor as `ActorEvent::MediaAccepted` — the
spawned task authenticates, it does not authorize, because it cannot see
`SessionManager` and guessing would be a way to widen a grant outside
`lumepeer-core` (§2.3). The actor accepts it only when the same `NodeId`
has a live control connection, a session in `Active`, and a `view` grant;
everything else is closed. `network.rs`'s
`a_media_alpn_connection_is_refused_without_a_handshake` was extended
rather than replaced to pin that, including that no capture starts.

### 3. Tauri's ACL now covers this application's own commands

`build.rs` declares `AppManifest::commands(...)`. In Tauri 2.11 an app that
declares an ACL manifest gets its *own* commands ACL-checked, not just
plugin commands (`tauri::webview`'s `has_app_acl_manifest` branch), so each
window's capability must name every command it may call. `main.json` lists
the six existing commands; the new `view.json` is scoped to `windows:
["view-*"]` (glob support verified against the installed schema, the one
open question the design doc left) and grants exactly `view_next_frame`,
the three `input_*` commands, `session_revoke` and a scoped close —
explicitly **not** `core:default`.

This makes the "no wildcard entries" rule `main.json` documents for itself
enforced by Tauri rather than by review. `check_window`/`check_view_window`
in `commands.rs` stay as the Rust-side check, narrowing further to *that
peer's own* window so one open view cannot poll another session's frames.

### 4. The terminal media failure is modal in the view window

§ "Error handling" of the design doc asks, on an unrecoverable media
failure, for the window to close *and* a modal error to appear. A modal in
a window that is being closed shows nothing, and the alternative — routing
the error to the main window — needs an event channel and listener that
exist for nothing else.

Implemented instead: the pipeline is torn down immediately (no decoder, no
capture, nothing running), the view window renders the modal in place, and
acknowledging it closes the window, which is already the guest's revoke.
The window-equals-active-session invariant holds; the only difference from
the letter of the spec is that the close happens after the user has read
why.

The rest of the connection-health policy is as specified: one recovery
pass bounded by `RECONNECT_WINDOW_SECS` (reused, not duplicated), inline
non-blocking "reconnecting" over the last frame, and a rolling one-shot
budget that any delivered frame resets. `MEDIA_REDIAL_BACKOFF_MS` (§14) is
new and is not a second window — it only keeps a host that refuses instantly
from turning that one pass into a busy loop.

## Consequences

- Adding an IPC command now takes three edits, not one: the handler, the
  `build.rs` command list and the capability of every window allowed to
  call it. That is the point — a command reachable from a window nobody
  reviewed is exactly what the ACL is for.
- The guest reacts to `ConsentGrant` inside `on_inbound`, where the peer is
  known, rather than by subscribing to `ActorNotification`. The
  notification enum deliberately carries no peer identity (§15) and adding
  one to open a media connection would have made it a channel for it.
- Media backends stay behind features. `apps/desktop/src-tauri` gained
  pass-throughs (`capture-x11`, `encode-openh264`, …), default empty, so
  `cargo build --workspace` still needs no platform SDK. A client built
  without them pairs and consents normally and reports "no capture
  backend"; `Taskfile.yml`'s Linux client targets turn on
  `capture-x11,encode-openh264`.
- Windows and macOS show `CaptureUnavailable` until their capture backends
  land; the pipeline above is what they plug into.
