# ADR 0135 — A host whose encoder refuses every frame says so

Status: accepted
Date: 2026-09-30

Extends [ADR 0024](0024-host-media-unavailable-wire-message.md) and
[ADR 0110](0110-a-host-the-os-refused-capture-says-so.md), and narrows the
fallback of [ADR 0074](0074-the-picture-ceiling-is-5k-and-the-frame-bound-did-not-move.md).

## Context

On 2026-09-30 a host without a hardware encoder (beta, `openh264` fallback)
received zero-copy GPU frames, which `openh264` read through `Frame::data`
instead of `Frame::as_cpu`. Every frame failed with `encode failed: frame
buffer is short`. That root cause is fixed separately in
`crates/media/src/encode/mod.rs`. This ADR is about what the encode loop did
with the failure:

- The loop logged the refusal, slept out the tick and tried the next frame,
  with no bound. The host's log took about thirty `WARN` lines a second for
  minutes.
- The guest had its one picture and then nothing. It showed no error, and its
  log had none: a screen that does not change also sends nothing (§11.1), so
  from the guest's side the silence looked like a still desktop.
- The first refusal was logged as "the encoder refused the size this guest
  asked for". ADR 0074 spends its one-time fallback on the first refusal
  whenever the guest has named a size, and the log line assumed the size was
  the cause. On beta it was not.

ADR 0024's closed set has no fitting reason. `NoEncoder` is recorded in
`MediaHealth`, so every later session on that host is refused at grant time
and the operator's own window says the device has no video encoder. That is
right for a build with no encoder at all. For an encoder that exists and has
failed, it is wrong: it would lock every guest out of a host nobody may be
sitting at until the app restarts, and the next session builds a fresh encoder
that may well work.

## Decision

- After `ENCODE_REFUSALS_BEFORE_FAULT` (60) refused frames in a row, the
  encode loop stops and reports `MediaUnavailableReason::EncoderFailed` through
  its `faults` channel, the same way the capture-refused path of ADR 0110 does.
  Sixty is two seconds at the default 30 fps and six at the ABR floor of
  10 fps. Frames are counted, not time, because a still screen produces none. A
  frame the encoder takes resets the count.
- `EncoderFailed` is appended after `CaptureDenied`. `PROTOCOL_MINOR` goes to 22.
  It is sent only to a guest that advertised `FEATURE_MEDIA_UNAVAILABLE` and is
  at minor 22 or later. An older guest would decode the unknown variant as
  malformed and close the connection (§9.1).
- It is **not** recorded in `MediaHealth`. The host has an encoder, and the
  next session tries a fresh one.
- On the guest it is terminal (`ViewStatus::EncoderFailed`, code 8). The
  window says the other device's video encoder stopped working and that
  connecting again may help.
- Refused frames are logged at most once per 10 s, and each line carries the
  number of refusals since the previous one. The first refusal is logged at
  once, and so is the one that ends the loop. An encoder that refuses every
  other frame never reaches the bound, but it cannot flood the log either.
- ADR 0074's fallback becomes a trial. The first refusal at the guest's size
  withdraws the size, and the next frame is encoded at the picture budget. If
  that frame goes through, the size was the cause: the existing warning is
  logged and the size stays withdrawn for the session. If the budget is
  refused too, the size was not the cause: it is honoured again and is not put
  on trial again until the encoder takes a frame.

## Consequences

- The minor 19 golden vector `minor19_media_unavailable_unknown_reason`
  (reason byte 4) decodes at minor 22. Its bytes are unchanged; it is renamed
  `minor19_media_unavailable_unknown_through_minor21` and marked valid, with a
  comment, the same way minor 14 handled the file-offer ceiling (ADR 0077). A
  new invalid vector freezes reason byte 5 as unknown.
- A guest below minor 22 is told nothing, as before. But its media stream now
  ends when the loop gives up, where it used to stay open and silent. That
  guest's receiver redials, and each redial runs a fresh encode loop with a
  fresh encoder. A transient encoder fault can recover that way, and a
  persistent one ends in the guest's ordinary `Failed` state instead of a
  frozen picture.
- An encoder that stalls rather than refuses takes `ENCODE_HW_EVENT_TIMEOUT_MS`
  (2 s) per frame to fail, so it reaches the bound in about two minutes.
- `spawn_encode_loop` now delegates to `spawn_encode_loop_with`, which takes
  the encoder's builder instead of the codec. This lets a unit test put an
  encoder that refuses every frame behind the real loop.
