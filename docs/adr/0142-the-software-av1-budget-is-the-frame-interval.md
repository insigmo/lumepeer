# ADR 0142 — The software AV1 budget is the frame interval

Status: accepted
Date: 2026-10-03

Amends §5.1 of [ADR 0141](0141-a-host-with-no-hardware-encoder-encodes-av1-in-software.md):
the absolute half of the owner's threshold.

## Context

Reported 2026-10-03: a guest that decodes AV1 connecting to beta, the
reference host without a hardware encoder that ADR 0141 was written for,
always gets H.264. beta's log on v0.0.130 says why, twice:

```text
the guest decodes AV1 but this session gets H.264 (ADR 0141)  refusal=Host(Unmeasured)
software AV1 measured on this host (ADR 0141)  found="software AV1 p95 26.3 ms, openh264 p95 27.8 ms" budget_ms=22 ready=false
```

The first session after a start runs on H.264 because the measurement has not
finished (ADR 0141's Consequences say so). The measurement then refuses
software AV1 for the rest of the process, although AV1 came in faster than
the `openh264` the host stays on. It only missed the 22 ms.

The same measurement run on beta with nothing else running, the test
`measures_this_host` and a per-act breakdown, gave 23–25 ms at p95. The window
drag (25–27 ms) sets the p95, the scrolling takes about 21 ms, the typing
9–11 ms, and the BGRA-to-I420 conversion inside those figures takes 1.8–2.7 ms.
The development machine (i7-11800H) measures 18–21 ms. The stage-1 figure
of 17.4 ms on beta came from the recorded desktop. On beta the synthetic
screen turned out harder than that recording, not "a little kind" as ADR
0141's Still open supposed from the cloud VM.

## Decision

`SOFTWARE_AV1_FRAME_BUDGET_MS` is **33**, the whole frame interval at
`SOFTWARE_AV1_MAX_FPS`, no longer two thirds of it. The other half of the
threshold stays as it was: the p95 must be no worse than `openh264`'s on the
same machine. The measurement itself and the moment it starts do not change.
The owner chose to leave those alone.

That puts the measurement on the same line as the live check of ADR 0141
§5.2, which already ends a session's software AV1 only when two 90-frame
windows in a row go over the interval.

## Consequences

- beta passes on its own measurement, whether it runs idle or beside a
  session. The first session after a start still gets H.264. The next one gets
  AV1, and so does a session that starts on H.264 and has its preset arrive
  after the measurement is done (ADR 0141 §3).
- When the measurement runs beside a live `openh264` session, as it does on
  beta, its "no worse than `openh264`" half has a thin margin: 26.3 against
  27.8 ms and 27.2 against 27.7 ms in the two runs logged. A run that comes
  in the other way still leaves the host on H.264 until it restarts.
- A host between 22 and 33 ms is now offered software AV1. Capture and sending
  no longer have a reserved third of the interval. If such a host cannot keep
  30 frames a second in a session, the live check catches it and falls back
  to H.264 (ADR 0141 §5.2).
