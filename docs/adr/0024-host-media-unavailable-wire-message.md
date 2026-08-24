# ADR 0024 — Telling the guest a host cannot produce a picture

Status: accepted
Date: 2026-08-24

Written against an earlier tree as ADR 0017 and renumbered on the way in:
0017 through 0023 were taken in the meantime. The decision below is unchanged
apart from the protocol numbers, which moved with it — see "Decision".

## Context

§18 forbids silent degradation. Two host-side paths degraded silently anyway,
and both did it with nothing but a `tracing::warn!`:

- `network.rs::default_capture()` falls back to `StubCapturer` when
  `platform_capturer()` fails. The stub starts, stops and returns `Ok(None)`
  forever, so the capture side of the pipeline looks healthy and produces
  nothing.
- `view.rs::spawn_encode_loop` returns as soon as `select_encoder()` fails —
  *after* `open_media_stream` has already accepted the connection, so the
  guest sees a media stream that exists and never carries a frame.

What the guest actually saw in both cases: `ViewStatus::Waiting`, then the
media receiver's single recovery pass expiring after `RECONNECT_WINDOW_SECS`
(60), then the `view.failed.*` modal — "Connection lost / The remote screen
could not be reconnected". Every word of that is wrong when the host simply
has no capture backend: nothing was lost, nothing will be gained by
reconnecting, and the network is fine. The host's own UI said nothing at all,
so the operator sharing their screen had no way to know either.

`crates/core/src/protocol.rs` had no message for this. `MessageKind` covers
consent, licensing, input, clipboard, files, quality and keepalive; nothing
says "this session will carry no picture". The nearest existing messages are
`ConsentRevoke` and `LicenseDeny`, and both *end the session* — which is a
different, and wrong, thing to do to a session whose control channel, consent
and input all work.

The constraint on adding one: `tests/interop/golden_vectors.txt` is frozen per
`PROTOCOL_MINOR` (§17.2), and `unknown_message_kind` is one of its *invalid*
vectors — a peer that receives a discriminant it does not know treats the
frame as malformed and closes the connection (§9.1). So a new message kind
cannot simply be sent at anyone.

## Decision

**One new host-to-guest message**, appended as the last variant of
`MessageKind` so it takes a fresh discriminant (30) and no existing one moves:

```rust
MediaUnavailable(MediaUnavailableReason)   // NoCaptureBackend | NoEncoder
```

The reason is a closed enum, not a string. This crosses onto a screen the
host's operator does not control, and §15 keeps host-identifying detail
(device names, driver versions, paths) off the wire; the guest turns the
variant into its own localized text.

**`PROTOCOL_MINOR` goes to 2.** Every vector frozen for minors 0 and 1 is
still in `golden_vectors.txt` byte for byte and still re-encodes unchanged —
that is the compatibility claim, and `protocol_golden.rs` is what checks it.
Two vectors were added for the new kind. `PROTOCOL_MAJOR` does not move:
`check_version` only gates on major, so an older peer still connects.

**Sending it is gated on feature negotiation, not on the minor version.**
`Hello.features` is the extension point §9.1 already defines ("unknown ones
are ignored"), so:

- the guest advertises `FEATURE_MEDIA_UNAVAILABLE` (`"media-unavailable"`) in
  its `Hello`; an older host ignores the string and never sends the message;
- the host remembers, per connection, whether that string was present
  (`ConnectionHandle::announces_media_faults`) and sends `MediaUnavailable`
  only to a guest that advertised it. A guest that did not is left exactly
  where it was before this change — a window that waits, and the reason in
  the host's log. That is worse than the new behaviour and much better than
  closing a working control connection over an undecodable frame.

**It is not a revoke.** The control session and every grant on it stay exactly
as they were; the guest keeps its window, and the host keeps the viewer it
registered. What ends is the *waiting*: the guest aborts its media receiver
instead of letting the recovery pass time out, and paints one of two new
terminal `ViewStatus` values (`NoCapture` / `NoEncoder`, IPC codes 4 and 5)
with text that says the connection is fine and the other device cannot send a
picture. `set_status` refuses to overwrite a terminal status, so the aborted
receiver cannot repaint "reconnecting" over the reason on its way out.

**The host says it on its own screen too**, which needs no protocol at all:
`MediaHealth` (`view.rs`) carries the two facts, `default_capture()` sets
`capture_missing` at startup, an encode loop reports `NoEncoder` back to the
actor over a `MediaFault` channel, and `network_status` returns `can_capture`
/ `can_encode` for the warning banner in `main.ts`.

The host announces at two moments: when it grants a role carrying `view` (the
guest's window opens on that same `ConsentGrant`, and the control stream is
ordered, so the reason is the window's first news), and when a media
connection arrives that it knows it cannot feed — where it now declines to
start a doomed encode loop at all.

Deliberately **not** done: probing `select_encoder` at startup to fill
`can_encode` before the first session. Probing builds a real encoder (COM
enumeration on Windows), it would run on the app's startup path for a machine
that may never share a screen, and its answer can go stale anyway.
`can_encode` stays "nothing has said otherwise yet" until a session asks,
which is what it honestly is.

No grant check was touched. `on_media_accepted` still refuses any media
connection without a live, granted `view` session, and it does so *before*
this new branch runs.

## Consequences

The failure §18 names is now visible on both screens and in the right words:
the operator sees it before anyone connects, and the guest sees it in seconds
instead of a minute of waiting followed by a wrong explanation. The reconnect
window is left for what it is for — connections that really were lost.

Two peers of different minors interoperate in both directions: a minor-2 host
with an older guest behaves exactly as before, and a minor-2 guest talking to
an older host advertises a feature string that host ignores. That property
rests on the host honouring the feature gate — a future message added to
`MessageKind` without one would break an older peer's connection outright.

`MediaHealth` is per-process and only ever gains faults; nothing clears
`encoder_missing` short of a restart. That is deliberate for now (an encoder
that failed once on this machine is not expected to appear mid-run), but it
does mean a transient encoder failure is remembered for the life of the
process.

## Alternatives considered

- **Reuse `ConsentRevoke` or `LicenseDeny`.** Rejected: both end the session.
  A host with no capture backend still has a working control channel, consent
  and input, and §18 asks for the cause to be surfaced, not for a working
  session to be torn down.
- **A free-text reason string.** Rejected under §15: the useful strings are
  exactly the host-identifying ones (driver, adapter, path), and the guest
  cannot localize an opaque string anyway.
- **Bump `PROTOCOL_MAJOR`.** Rejected: a major bump refuses the connection
  outright (`check_version`), which would turn "no picture" into "no session"
  for every peer on the older build.
- **Gate on the peer's `minor` instead of a feature string.** Would work, and
  the plumbing is the same, but §9.1 already designates `features` as the
  ignore-if-unknown extension point; a minor number says which build a peer
  is, a feature string says what it understands.
- **Signal it on `rd/media/1` instead of the control channel** (a sentinel
  frame, or just closing the stream). Rejected: the case where there is no
  capture backend is exactly the case where the host should not accept a media
  connection at all, so there is no stream to say it on. The control channel
  is the one that is guaranteed to exist.
- **Host-side UI only, no wire message.** Rejected: the symptom appears on the
  guest's screen, and the guest is where the wrong explanation was being
  shown. It is also the side that would otherwise keep retrying.
