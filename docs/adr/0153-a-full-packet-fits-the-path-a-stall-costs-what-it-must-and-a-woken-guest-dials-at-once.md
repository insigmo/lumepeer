# ADR 0153 — A full packet fits the path, a stall costs what it must, and a woken guest dials at once

Status: accepted
Date: 2026-10-12

The owner asked for stability: connect as fast as possible, and stop the
connection dropping or getting several times worse as time goes on, without
giving up quality or speed.

## Context

Read from both machines' logs, 2026-10-09 to 2026-10-12 (v0.0.139–0.0.140,
reference machine → `beta`, nearly every session on the obfuscated transport),
and from this machine's power log.

**Drops.** Every session that ended unasked in those four days but one lines
up with the reference laptop going to sleep (Kernel-Power 42, *Button or Lid*)
and waking up (Kernel-Power 1). The one exception was a blip of the guest's
own network, repaired by the resume in 6 s. What cost time was the way back
after a sleep: 2–19 s from the wake to the picture. The guest always tried to
resume first, and the host always answered `UnknownSession`: it had ended the
session hours before, one idle timeout after the lid closed and the resume
window of `RECONNECT_WINDOW_SECS` (300 s) after that. The resume cost the
first tick (3 s), the refused dial, and ADR 0144's second.

**Worse over time.** The host's media line every 5 s, per session:

| session (UTC) | transport | link-cap drops | time at ≤ 500 kbit/s | packets lost |
|---|---|---|---|---|
| 10-11 11:42, 262 min | obfuscated | 166 (0.63 a minute) | 15.7 % | ≈ 1000 per 15 min |
| 10-11 16:55, 43 min | obfuscated | 73 (1.7 a minute) | 49.4 % | |
| 10-12 12:10, 90 min | obfuscated | 61 (0.68 a minute) | 7.5 % | ≈ 300–850 per 15 min |
| 10-11 21:09, 47 min | iroh (direct) | 1 | 0.2 % | 7 in all |

Same two machines, same week, often the same hour: the obfuscated path lost a
packet every second or so, on a still screen too, and iroh's direct path lost
next to none. Two things turned that loss into a picture several times worse
for seconds at a time.

1. **The envelope pushed full packets past the path.** Every obfuscated
   datagram carries 42 bytes of envelope (nonce, length, tag) and up to
   `OBFUSCATE_PADDING_MAX_BYTES` (128) of random padding. QUIC's MTU discovery
   knew nothing of either. A probe that happened to draw little padding got
   through and set a packet size; the next packets drew more and were larger
   than the 1472-byte UDP payload an ordinary 1500-byte path carries. Windows
   does not even send those — a socket with the don't-fragment bit, as every
   QUIC socket has, is refused with `WSAEMSGSIZE` (checked on this machine:
   1472 sent, 1473 refused), and `noq-udp` logs that at debug level only.
   QUIC's black-hole detection cannot see it either, because packets of the
   same size keep arriving. When it did fire, the MTU fell back to 1200, and
   60 s later (`black_hole_cooldown`) discovery raised it into the losses
   again.
2. **A stall threw the bitrate to its floor.** ADR 0144's `LinkPressure`
   halves the preset's bitrate from what the loop actually sent once two
   seconds in a row were mostly spent waiting on the link. On a still desktop
   "what was sent" is a few kilobits a second, so any two-second stall —
   these losses, or Wi-Fi — took 8 Mbit/s to 300 kbit/s in one step. The logs
   show exactly that, at 6–93 kbit/s sent. The cap then needed ten clear
   seconds to come back, and whatever the person did next was a blur.

## Decision

1. **A full packet fits the path with its envelope.** `Obfuscator::seal`
   pads only payloads up to `PADDED_UP_TO_BYTES` (1200, QUIC's own minimum
   packet, which every handshake proves with the padding on). A larger packet
   exists only because MTU discovery found room for it, and it now goes out at
   its own size plus the envelope, the same size as the probe that licensed
   it. MTU discovery's upper bound on this transport is 1452 less the envelope
   (1410), so a sealed packet is never larger on the wire than a plain QUIC
   packet may be. The handshake looks the same on the wire as before.
2. **A stall under light frames halves the cap and no more.** `LinkPressure`
   counts the frames of each window. The cut is still taken from what was sent
   when a window of the pressed run carried heavy frames — at least a quarter
   of what the cap allows a frame at the session's rate, which a software
   encoder undershooting by two thirds still makes. Otherwise the cap itself
   is halved: the frames were not what the link could not carry, and smaller
   ones would relieve nothing. A link frozen right after a picture still goes
   straight to the floor, as before.
3. **A guest that slept past the resume window dials a new session at once.**
   Each control connection remembers, by the wall clock, when anything last
   arrived on it (a monotonic clock does not count a sleep everywhere). When a
   watched session ends after a silence longer than the host keeps a session —
   `RECONNECT_WINDOW_SECS` + `QUIC_MAX_IDLE_TIMEOUT_SECS` +
   `PING_INTERVAL_SECS` — the wait is ADR 0148's for a new session: no resume
   claim, the first dial after `HOST_RESTART_REDIAL_SECS`, the window kept for
   the session to come back into.
4. **The host's media line names the path MTU** (`mtu=`), so the next time a
   loss like this is in the log it is visible without a probe.

## Consequences

- Full packets on the obfuscated transport are all one length per MTU, as
  they are on any other transport: their padding only ever blurred lengths
  that were the path's anyway. Packets up to 1200 bytes — the handshake, acks,
  input, small frames — are padded exactly as before.
- 1410 bytes a packet instead of the ~1430 an ordinary path could take: 1.4 %
  more packets for the same picture, against a loss that cost whole seconds.
- On a light picture, a real collapse of the link now takes a few seconds
  longer to reach the floor: one halving a pressed second instead of one step.
  A heavy picture — what the floor was for — is cut as fast as before.
- A guest that slept for longer than about five and a half minutes asks for a
  new session. A guest the host lets in by an unattended proof (ADR 0123) is
  back without anyone doing anything; any other is asked for consent again,
  as it already was after the refused resume.

## Verification

- `a_full_packet_fits_an_ordinary_path_with_its_envelope`: 4 MiB from host to
  guest through a path that drops datagrams over 1472 bytes. Fails before
  (74 packets lost, MTU knocked back to 1200 by black-hole detection), passes
  after with no loss and an MTU above 1200.
- `a_stall_under_a_still_screen_halves_the_cap_and_no_more` and
  `a_link_frozen_under_a_picture_goes_straight_to_the_floor`, with the rest of
  ADR 0144's `LinkPressure` tests unchanged in what they assert.
- `a_guest_that_slept_past_the_resume_window_dials_a_new_session_at_once`:
  fails without the change (the first dial carries the old session's claim);
  `only_a_silence_past_what_the_host_keeps_skips_the_resume`.
- Live, 2026-10-12, `obfuscated_wan_probe pull` (new mode: a paced
  picture-sized stream for N seconds, then QUIC's own path counters), this
  machine as the guest and `beta` as the host over the internet, two minutes
  per run, the user's own session untouched:

  | build | rate | host lost | congestion events | MTU at the end | stalls > 300 ms |
  |---|---|---|---|---|---|
  | before | 1.5 Mbit/s | 118 | 62 | 1200 | 4 (worst 2.4 s, at +62–66 s) |
  | after | 1.5 Mbit/s | 0 | 0 | 1410 | 0 |
  | after | 3 Mbit/s | 0 | 0 | 1410 | 0 |
  | before | 3 Mbit/s | 68 | 39 | 1200 | 3 (worst 1.5 s) |

  The stalls before came where the black-hole cooldown ended and discovery
  raised the MTU again.
