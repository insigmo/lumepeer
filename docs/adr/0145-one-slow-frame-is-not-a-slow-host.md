# ADR 0145 — One slow frame is not a slow host

Status: accepted
Date: 2026-10-08

Amends the measurement of [ADR 0141](0141-a-host-with-no-hardware-encoder-encodes-av1-in-software.md)
§5.1, as [ADR 0143](0143-the-guest-can-pick-the-hosts-encoder.md) runs it at
the host's start.

## Context

The measurement times 5 warm-up frames and then 90 frames of each encoder,
and keeps the p95 of the 90. It also ended the run at once on any frame that
took 250 ms or longer (`HOPELESS`), and reported that one frame's time as the
p95. The comment beside it said "nothing that follows can bring the p95 back
under the budget". That is not true: the p95 of 90 frames, nearest rank, is
the fifth slowest, so it leaves out four frames however slow they are. The
check also ran on the warm-up frames, keyframe included, which are not timed
at all.

beta's client log on v0.0.133 has it twice, from two restarts of the
installed client on 2026-10-07 while a film was playing on that machine:

```text
software AV1 measured on this host (ADR 0141)  found="software AV1 p95 262.9 ms, openh264 p95 67.3 ms" budget_ms=33 ready=false
software AV1 measured on this host (ADR 0141)  found="software AV1 p95 251.8 ms, openh264 p95 284.7 ms" budget_ms=33 ready=false
```

Every figure at or over 250 ms there is one frame, not a p95. The answer is
kept for the rest of the process. Since ADR 0143 the
measurement runs 10 s after the process starts, the moment a machine is most
likely to stall once: the logon, the other autostarted programs, an update.

On those two runs the outcome was probably right, since the machine was busy
with a film. The rule is still wrong. On a host that is quick but stalls once
during its start, one frame turns software AV1 off until lumepeer restarts.
Every Linux host is such a host, since none has a hardware encoder in
lumepeer.

## Decision

The measurement stops early only once the p95 itself is past `HOPELESS`:
when `TAIL` of the timed frames (5 of 90, the frames at or above the p95)
have taken 250 ms or longer. It then reports the fifth slowest of them, the
least the p95 can come to. The warm-up frames are not counted. Otherwise the
whole run is timed and its real p95 is kept.

The rule lives in `timing::Tally`, which `timing::time` feeds one frame at a
time, so it is tested without a clock: a single 1050 ms frame, a slow warm-up
and four slow frames all leave a 20 ms p95, and the fifth slow frame ends the
run with the fifth slowest time.

Nothing else changes: the budget, the comparison with `openh264`, when the
measurement starts, and that its answer holds for the process.

## Consequences

- A host that stalls once during the measurement is judged on its p95, as
  ADR 0141 meant.
- A machine slower than 250 ms a frame now encodes ten frames per encoder
  before it gives up (the five of the warm-up and five timed) instead of
  one: 2.5 s or more per encoder instead of 0.25 s.
- A host measured while it is busy for the whole run still gets the busy
  answer, now as a real p95 rather than one frame. Measuring again later is
  not part of this decision.
