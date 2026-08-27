# 0037 — Receiver reports, and the order quality is given up in

Status: accepted
Date: 2026-08-27

## Context

§11 specifies adaptive bitrate driven by receiver feedback every
`ABR_FEEDBACK_INTERVAL_MS`. Two things were missing from the implementation,
and they are related.

The first is the feedback itself. No message carried it: `QualityAdjust` says
what an encoder should *do*, and nothing said what a receiver had *seen*. ADR
0015 recorded the consequence — the host derived congestion from how long its
own `write_frame` took relative to the frame budget, and called that "loss".
That is a genuine signal about the local send path and it is the only one a
host has on its own, but it says nothing about the far end: a guest whose
decoder is failing, or whose link delivers a third of what is being sent,
looks identical to a healthy one.

The second is what adaptation may move. §11 names the bitrate. Below
`ABR_MIN_BITRATE_KBPS` — 300 kbit/s — a 1080p desktop at 30 fps has nothing
left to give, and the controller simply stopped there while the link kept
getting worse. Frame rate and resolution were never touched, although
`crates/media/src/scale.rs` had been able to reduce a picture since ADR 0018.

Both gaps land on the same complaint ADR 0026 was written about: the product
could not say what was happening, and could not do much about it either.

## Decision

**A new control message, `ReceiverReport`, at `PROTOCOL_MINOR` 6.** Guest to
host, every `ABR_FEEDBACK_INTERVAL_MS` while a media stream is live, carrying
three numbers the guest actually measured: frames its decoder could not turn
into a picture (permille), the round trip it measured on its own control
channel, and media bytes that arrived (kbit/s). It is appended last in
`MessageKind`, rides behind `FEATURE_RECEIVER_REPORT` on the host side and
behind the `HelloAck` minor on the guest side, and gets golden vectors of its
own — the same shape every added message since minor 2 has taken.

Nothing in the report is range-checked while decoding. It comes from a peer
that has proven nothing about it, so an absurd claim is a *frame of feedback to
drop* at the point of use, not a malformed frame that tears the session down;
`AbrController::on_feedback` drops a loss outside `0.0..=1.0` without even
spending its rate-limit budget, so a peer cannot mute adaptation by sending
garbage.

**Loss is frames, not packets.** `rd/media/1` is a reliable ordered QUIC
stream: bytes are never silently lost on it, which is exactly why ADR 0015 had
to invent a stand-in. What a receiver on such a stream can honestly lose is a
frame it could not decode, and what it can honestly measure is how much arrived
per second. Those are the two numbers, and there is no third.

**Goodput is a congestion signal in its own right.** A link that delivers less
than `ABR_GOODPUT_SHORTFALL_PERCENT` of the current bitrate target cannot carry
what is being sent, whether or not anything failed to decode. It counts only
when the guest actually measured one: a still desktop legitimately produces
almost no bytes, and reading that as congestion would degrade a session that is
working perfectly.

**The host-local estimate stays, as the fallback it always was.** A peer older
than minor 6, or one that has gone quiet for `ABR_FEEDBACK_STALE_AFTER_MS`,
puts the host back on ADR 0015's write-latency signal. Removing it would leave
a host with no congestion signal at all against an old peer, and would let a
guest freeze quality by advertising the feature and then saying nothing.

**The degradation ladder is bitrate, then frame rate, then resolution.**
Recovery walks the same rungs back up in reverse. Each has its own floor in
§14: `ABR_MIN_BITRATE_KBPS`, `ABR_MIN_FPS`, `ABR_MIN_SCALE_PERCENT`.

The order is the decision this ADR exists to record:

- **Bits first.** The picture stays whole, current and correctly sized; it
  gets softer. Nothing about the session's shape changes.
- **Frames second.** The picture stays whole and sharp; it updates less often.
  A slower remote desktop is still a usable one, and a user notices this before
  they notice it, which is why it comes after bits.
- **Pixels last.** A downscaled desktop is the one degradation that can make
  text unreadable, and unreadable text is not a degraded session — it is a
  broken one. Recovery therefore gives resolution back *first*.

`ABR_ADJUST_MAX_RATE_PER_SEC` covers the whole target, not one knob each: three
knobs moving independently at one change per second is ripple, and ripple reads
worse than a steadily lower picture.

**`KeyframeRequest` is rate-limited on the host.** Its doc comment said
"rate-limited by the caller", which describes politeness, not protection: a
keyframe is the most expensive frame in the stream, so a guest that asked on
every frame would be deciding what the host's uplink is spent on. Both sides
now hold a `KEYFRAME_MIN_INTERVAL_MS` budget, and the host's is the one that
matters. The encoder side is a real `VideoEncoder::request_keyframe`,
implemented by openh264 (`force_intra_frame`) and by Media Foundation
(`ICodecAPI` / `CODECAPI_AVEncVideoForceKeyFrame`) — not a bitrate nudge that
happens to produce an IDR as a side effect.

**`Ping`/`Pong` finally run, and only as a measurement.** Each side sends a
random nonce every `PING_INTERVAL_SECS` and answers an incoming ping
immediately with the same value. A returned nonce this side is not waiting on,
and a sample beyond `RTT_MAX_PLAUSIBLE_MS`, are both ignored in silence — they
are not errors, they are simply not measurements. A missing `Pong` does not end
anything: QUIC and `RECONNECT_WINDOW_SECS` decide when a session is gone, and a
second liveness watchdog with its own opinion would only be able to disagree
with them.

**What the user is shown is measured, and stops at the region.** The pill
carries path type, round trip, loss, goodput and the target being sent, all
observed on this machine — no setting is reported as if it were an outcome,
which is the failure mode ADR 0026 names. The path type comes from iroh's own
open paths rather than from configuration. A relay is named by the leading
label of its hostname and nothing more: a relay address is a fact about the
*host's* network, and §15 keeps that class of detail off a screen the host does
not control.

## Consequences

- `PROTOCOL_MINOR` is 6. A minor-5 peer is unaffected: it never advertises
  `FEATURE_RECEIVER_REPORT`, is never sent the message, and keeps the ADR 0015
  behaviour exactly.
- `VideoEncoder` gains a required method. Both backends implement it; a third
  cannot be added without answering the question.
- `AbrController::on_feedback` returns a `QualityTarget` rather than a bitrate.
  The encode loop applies all three parts: bitrate to the encoder, frame rate
  to its own pacing, scale through `scale::scale_to_percent` before
  `fit_within_budget` — the adaptive reduction is a choice, the budget one is
  the ceiling of §15, and the ceiling goes last.
- Frame rate is applied by pacing the loop, not by reconfiguring the encoder.
  Bitrate is bits per second regardless of how many frames carry them, so a
  slower loop spends the same budget on fewer, better frames — and neither
  encoder has to be rebuilt to change cadence.
- The quality panel says "not measured yet" where nothing has been measured.
  A zero standing in for an unknown is the one thing a diagnostics view must
  not do.
