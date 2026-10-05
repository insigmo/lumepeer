# ADR 0143 — The host measures itself at start, and the guest can pick its encoder

Status: accepted
Date: 2026-10-05

Amends §5.1 of [ADR 0141](0141-a-host-with-no-hardware-encoder-encodes-av1-in-software.md)
(when the measurement runs) and adds a guest-side override to its section 2.
Builds on [ADR 0142](0142-the-software-av1-budget-is-the-frame-interval.md).
Protocol minor 23.

## Context

v0.0.132 still showed H.264 on beta, the reference host without a hardware
encoder. Its log:

```text
the guest decodes AV1 but this session gets H.264 (ADR 0141)  refusal=Host(Unmeasured)
software AV1 measured on this host (ADR 0141)  found="software AV1 p95 28.5 ms, openh264 p95 26.3 ms" budget_ms=33 ready=false
```

The 33 ms of ADR 0142 passed. The comparison with `openh264` did not: on beta
the two take about the same time (26.3 against 27.8 ms, 27.2 against 27.7,
28.5 against 26.3 in three runs), so noise decides it. The way the
measurement was started made the noise worse. It started at the first AV1
guest's `Hello`, and on beta that `Hello` is a few milliseconds ahead of the
media connection. So the first session always got H.264, and the measurement
then ran beside that session's own `openh264` encode loop.

The owner decided:

- **Measure at the start.** The host checks which encoder is faster as soon
  as it starts, picks the suitable one, and does not wait for a guest.
- **A session that started before the answer moves.** When the measurement
  finishes, every live session's codec is chosen again.
- **A switch in the toolbar, for now.** The guest's session toolbar gets an
  encoder list in its settings, so the encoders can be compared by eye on a
  real pair. The hardware encoders the host does not have are greyed out.

## Decision

### 1. The measurement runs at start

The actor starts `software_av1::start_measurement` from its own `run`,
`SOFTWARE_AV1_MEASURE_DELAY_SECS` (10 s) after it starts. The delay keeps the
measurement out of the process's own start (building windows, finding the
network) without leaving it for a guest to race. The `Hello` of an AV1 guest
no longer starts it, and neither does choosing a codec.

The start is a `HostMedia` seam (`measure_software_av1`), as readiness already
was: a real host passes `software_av1::start_measurement`, and a test passes a
function that does nothing.

When the answer is in (anything but `Unmeasured`), the actor gets
`ActorEvent::SoftwareAv1Settled` and runs `recheck_media_codec` for every live
media session. A session that started on H.264 because the host had not
measured yet is restarted in AV1 when the host is `Ready`, with the half
second of "reconnecting" a preset change already costs (ADR 0141 §3).

The rule itself is unchanged: software AV1 is `Ready` when its p95 is no
worse than `openh264`'s and within 33 ms.

### 2. The guest can pick the encoder

Two messages, appended after `DeviceInfo` in minor 23:

- **`EncoderOptions { available, chosen }`**, host to guest. Sent with every
  media stream the host starts, after `MediaCodec`, to a guest whose `Hello`
  minor is at least 23. `available` always holds `Auto`. It holds a hardware
  encoder only when its probe answered (the H.264 probe is asked once per
  process, the AV1 one per stream as before), `H264Software` when `openh264`
  is built in, and an AV1 encoder only for a guest that decodes AV1.
  `Av1Software` needs `encode-aom` and AVX2 too. `chosen` is what this stream
  is actually using: the guest's pick, or `Auto`.
- **`EncoderSelect { choice }`**, guest to host, sent only to a host whose
  `HelloAck` minor is at least 23 and only for a choice it offered. The host
  acts on it for a guest holding the `view` grant, re-checks that it could
  make that choice, keeps it with the guest's preset in `StreamCaps`, and runs
  `recheck_media_codec`. The guest's redial gets the new encoder.

`EncoderChoice` is a closed set: `Auto`, `H264Hardware`, `H264Software`,
`Av1Hardware`, `Av1Software`. A sixth value, or a list longer than five, is a
malformed frame.

A pick other than `Auto` overrides ADR 0141's rule entirely. A picked
software AV1 encoder runs at the 60 and 144 fps presets (still at 30 frames a
second, which is all the encoder runs at), on a host whose measurement failed,
and on a screen above 1080p. The encode loop's own ways out of software AV1
are for the host's own choice only. Those are ending the stream for a screen
above 1080p, the live 33 ms check, and the refusal bound of ADR 0135. Taken
with a pick in place, the redial would come straight back to the same pick.
A picked encoder that refuses frames ends the session's picture the way any
other encoder does (`EncoderFailed`). A software pick is built by the new
`lumepeer_media::encode::select_software_encoder`, which builds `openh264` or
libaom even on a host with a hardware encoder. A hardware pick goes through
`select_encoder` as before.

### 3. The toolbar

The settings popover of the view window lists all five, in a fixed order,
under "Encoder". The ones the host did not offer are disabled rather than
hidden, so the list shows what the host lacks. The list is fetched each time
settings opens, and again 1.5 s after a pick, once the redial has brought the
new stream's announcement. A host older than minor 23 (or one with no picture
yet) shows "The host offers no choice of encoder." The strings are in all 13
locales.

## Consequences

- Every host process that builds `encode-aom`, has AVX2 and no hardware H.264
  encoder spends about five core-seconds measuring 10 s after it starts,
  whether or not an AV1 guest ever comes. ADR 0141 spent them only for an
  AV1 guest.
- A guest let in during the first ~16 s gets H.264 and is moved to AV1 when
  the measurement passes, at the cost of half a second of "reconnecting".
- On beta, whether the host's own choice is AV1 still depends on a near tie
  with `openh264`, now measured on an idle machine. The toolbar is how to get
  AV1 there regardless.
- The guest can make the host do something slower than the host would choose
  for itself. That costs nothing but the guest's own picture, and it lasts
  until the guest picks `Auto` or the session ends.
- Old peers are unaffected. A host below minor 23 never hears
  `EncoderSelect`, and a guest below it is never sent `EncoderOptions`.

## Still open

- The switch is temporary. Once the encoders have been compared on real
  pairs, either it goes, or it becomes a decision of its own about who
  chooses the encoder.
- No test covers the start-time measurement and the recheck that follows it:
  the 10 s delay is real time in an actor whose network needs real time too.
  The recheck itself is the same `recheck_media_codec` the preset and the
  encoder pick drive, and those are covered.
- Not yet seen on a real pair (beta) with this build.
