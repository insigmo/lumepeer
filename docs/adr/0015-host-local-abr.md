# ADR 0015 — Host-local adaptive bitrate, not guest-fed `QualityAdjust`

Status: accepted
Date: 2026-08-18

## Context

`crates/media/src/abr.rs::AbrController` and `VideoEncoder::set_bitrate`
were both implemented and unit-tested, but nothing in
`apps/desktop/src-tauri/src/view.rs::spawn_encode_loop` ever constructed an
`AbrController` or called `set_bitrate`: a session ran at the static
`ENCODE_DEFAULT_BITRATE_KBPS` (4000 kbps) for its whole lifetime regardless
of what the P2P link could actually carry. `crates/core/src/protocol.rs`
also already defines `MessageKind::QualityAdjust { target_fps,
target_bitrate_kbps }`, presumably the wire message §11 specifies for
carrying receiver feedback back to the host — but this checkout does not
contain the design document text (`p2p-iroh-tauri-design-v12.md` is not in
the repo per `CLAUDE.md`), so the exact guest-side computation §11 intends
for `ReceiverFeedback` could not be confirmed here.

Two things made wiring `QualityAdjust` as specified look wrong rather than
merely unconfirmed:

- `rd/media/1` is a QUIC unidirectional stream, reliable and ordered
  (`crates/net/src/media.rs`'s own doc comment: "QUIC already guarantees the
  bytes of one stream arrive in order and exactly once"). Nothing on it is
  ever silently lost the way `ReceiverFeedback.loss` (0.0..=1.0, "fraction
  of packets lost") is named for — a guest computing that field would be
  measuring something that structurally cannot happen on this transport,
  not real congestion.
- The actual congestion symptom on a reliable stream under a
  bandwidth-constrained link is queuing delay: writes on the host side
  start taking longer than the frame budget as the QUIC send window fills.
  That signal is available directly and locally to the host, with no wire
  round trip needed at all.

## Decision

`spawn_encode_loop` constructs one `AbrController` and, after each
`write_frame`, derives a `ReceiverFeedback` from how long that write itself
took relative to the loop's own target frame interval
(`write_congestion_feedback` in `view.rs`): a write finishing inside budget
reports `loss: 0.0`; one taking twice the budget or more reports `loss:
1.0`, saturating `AbrController`'s multiplicative-decrease branch; between
those it scales linearly. `rtt_ms` is always `0` (no local equivalent
exists) and `goodput_kbps` is computed honestly from the frame size and
write duration even though `AbrController::on_feedback`'s current decision
logic does not read it — both are documented as such rather than left as
unexplained placeholders. When `on_feedback` returns a new target,
`encoder.set_bitrate(...)` is called immediately. `AbrController` already
self-rate-limits to `ABR_ADJUST_MAX_RATE_PER_SEC`, so this runs every loop
iteration with no separate timer and no new constant.

`MessageKind::QualityAdjust` and a guest-computed `ReceiverFeedback` are
deliberately **not** wired up by this change. If a future reading of the
actual design document text shows §11 mandates the guest-fed path for a
reason not visible from this checkout (e.g. accounting for the guest's own
render/decode backlog, which a host-local write-timing signal cannot see),
wiring `QualityAdjust` as a second, additive input to the same
`AbrController` is the natural extension — nothing here forecloses that.

The pacing bug this sits next to was fixed in the same change: the loop
previously slept a fixed `interval` *after* capture+encode+write completed,
so real throughput fell under `ENCODE_DEFAULT_FPS` whenever that work took
non-negligible time, compounding every tick. It now sleeps only
`interval.checked_sub(elapsed_since_tick_start)`.

A write-timeout-and-drop-the-frame policy was considered and rejected for
the same congestion case. H.264 P-frames reference the *encoder's own*
previous output, not what was actually transmitted; dropping an
already-encoded frame after a stalled write would desync the decoder's
reference from the encoder's and corrupt the picture until the next
keyframe, and today a keyframe only happens at session start (a fresh
`spawn_encode_loop` and a fresh encoder run per accepted media connection,
so a reconnect is unaffected) or on a bitrate rebuild. Backing off the
bitrate before writes are that badly behind is the safe lever; a real
frame-drop policy would need either forced periodic keyframes or a switch
to an unreliable transport for video, neither attempted here.

## Consequences

A session now adapts its bitrate to the real P2P link instead of running a
fixed 4000 kbps that may exceed what a constrained link can carry — the
previously-dead `AbrController`/`set_bitrate` path is live. The signal is a
local proxy, not literal receiver-observed loss, so it cannot see
congestion that shows up only downstream of the write (e.g. the guest's own
decode falling behind); that gap is what a future `QualityAdjust` wire-up
would close. `MessageKind::QualityAdjust` stays defined but unconstructed
outside its own module, same as before.

## Alternatives considered

- **Wire `QualityAdjust` as specified**, guest-computed. Rejected for this
  pass: without the design document text in this checkout, the exact
  intended computation of `ReceiverFeedback.loss` on a stream that cannot
  lose bytes could not be confirmed, and inventing one on the guest side
  carries the same risk as inventing one on the host side while adding a
  wire round trip and control-channel load for no confirmed benefit.
- **A write-timeout with frame drop.** Rejected: corrupts the picture until
  the next keyframe, per the P-frame reference-desync reasoning above.
- **Do nothing** (leave `AbrController` unwired). Rejected: it is fully
  implemented and tested, and a static bitrate on a constrained P2P link is
  a direct, plausible cause of exactly the lag this change addresses.
