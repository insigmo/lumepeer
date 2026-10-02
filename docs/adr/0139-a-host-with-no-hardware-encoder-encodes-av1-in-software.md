# ADR 0139 — A host with no hardware encoder encodes AV1 in software, at 30 fps and up to 1080p, once it has measured itself

Status: accepted
Date: 2026-10-02

Amends §11's mutual-hardware-support rule for optional codecs,
[ADR 0069](0069-av1-asks-each-backend-and-va-api-cannot-answer.md) ("No
software AV1, anywhere") and
[ADR 0072](0072-h265-is-removed-and-av1-is-the-only-codec-above-the-baseline.md)
("there is no software AV1 encoder in this workspace"). Builds on
[ADR 0067](0067-codec-negotiation-guest-advertises-host-intersects.md), which
put the codec on the wire, and on
[ADR 0136](0136-the-frame-rate-follows-the-host-display-up-to-144.md), which
gave each preset its frame rate. Stage 1 of the research is
`docs/research/software-av1.md` (branch `research/software-av1`); this is its
stage 2.

## Context

A host without a hardware encoder encodes with the `openh264` fallback. Stage 1
measured that fallback against software AV1 on the reference host without a
hardware encoder (beta: Ryzen 7 5700U, 15 W, 8 cores/16 threads) and on the
development machine (i7-11800H), on a recorded 1080p desktop and a 1080p game,
in real time at 30 fps:

- `openh264` as lumepeer configures it takes 25–27 ms a frame at p95 on beta's
  desktop and drops 40% of a game's frames to stay inside its bitrate.
- libaom's realtime mode at speed 10 takes 15.8 / 17.4 / 20.0 ms at p95 at
  2 / 4 / 8 Mbit/s on the same desktop — 12.9 / 14.6 / 16.8 without the
  BGRA-to-I420 conversion — on 1.1–1.2 cores, and encodes every frame of the
  game in 22–27 ms on 2.3–3.2 cores.
- For the same picture libaom needs 46% (VMAF) to 61% (PSNR-Y) fewer bits than
  `openh264` as shipped on the desktop, and 55% fewer on the game.

The owner decided:

- **Threshold.** The p95 frame time — conversion plus encode — is no worse than
  `openh264`'s on the same machine *and* no more than 22 ms at 30 fps.
- **Scope.** libaom realtime at speed 10; at most 1080p; 30 fps, the `quality`
  preset; x86-64 Windows and Linux; hosts without a hardware encoder only. A
  guest already says whether it decodes AV1 (ADR 0067, ADR 0070).

Nothing ARM and no weak x86 was measured. Neither was 60 or 144 fps: at 1080p
on beta no software encoder keeps up with either.

## Decision

### 1. The exception to §11

§11 allows a codec above the H.264 baseline only with mutual hardware support.
This ADR makes one exception: **software AV1, on a host with no hardware
encoder at all, under every condition in section 2.** Everywhere else the rule
stands — VP9 is still never chosen, hardware AV1 is still the rule of ADR 0069,
and a host with a hardware H.264 encoder never encodes AV1 in software.

`select_encoder` builds libaom for an AV1 request with no hardware AV1 behind
it, but checks only what is fixed for the life of the process: the build has
`encode-aom` and the processor has AVX2. Every condition that can change
between two sessions is `choose_media_codec`'s, the only caller that ever asks
for AV1, so the answer cannot move between the codec being announced on the
wire and the encoder being built for it.

### 2. The conditions, in `choose_media_codec`

A guest gets AV1 when it decoded AV1 in its own `WebView` (ADR 0070) and
either a hardware AV1 encoder answers its probe (ADR 0069, unchanged) or **all**
of the following hold (`software_av1_refusal`, in this order; the first that
fails is logged as the reason):

1. **The host's own answer** (`lumepeer_media::encode::software_av1::readiness`)
   is `Ready`. That covers, in order:
   - the build has `encode-aom` and the target is x86-64 Windows or Linux
     (`NotBuilt` otherwise);
   - the processor has AVX2 and SSE4.1, which every measured run relied on
     (`UnsupportedCpu`);
   - no hardware H.264 encoder answers its probe (`HardwareEncoder`);
   - this host has measured itself and passed (section 5; `Unmeasured`,
     `TooSlow`);
   - no live session has given up on it since (section 5; `Demoted`).
2. **The frame rate** the guest's preset names is at most 30
   (`SOFTWARE_AV1_MAX_FPS`). A session accepted before its guest named a preset
   counts as 30: the guest names it as soon as its window opens, and every
   window opens on `quality`. A guest older than ADR 0136 never names one, and
   its session runs at 30 (section 3).
3. **The captured picture** is at most 1920×1080 pixels
   (`SOFTWARE_AV1_MAX_PIXELS`) — by count, so a portrait 1080×1920 qualifies
   and a 16:10 1920×1200 does not. The size is what the peer's previous encode
   loop last captured, before any reduction for the guest's window. A first
   session has no previous loop and counts as fitting; if the screen turns out
   larger, the encode loop ends the stream at its first frame and the guest's
   redial, half a second later, is chosen for with the size known — H.264.

Anything else is H.264.

### 3. What a preset change to 60 or 144 does

Software AV1 is a 30 fps encoder. `AomEncoder` holds its declared rate at 30,
and the encode loop holds a software AV1 session's ceiling there too, whatever
the display could do — so the ladder and a preset's rate inherit it.

A guest that switches to `balance` (60) or `performance` (144) changes
condition 2, and **its session moves to H.264**. A stream cannot change codec
in place — the guest's decoder is configured for one (ADR 0067) — so the host
stops the media stream (`recheck_media_codec`), the guest redials media as it
does after any lost stream, and the redial is chosen for afresh. Switching back
to `quality` moves it back to AV1 the same way. The guest keeps its last
picture under "reconnecting" for the half second in between. The same path
moves a session that started on H.264 because its host had not finished
measuring yet, when its preset arrives.

### 4. The bitrate target

libaom is asked for **half** the H.264 target the session runs at
(`SOFTWARE_AV1_BITRATE_PERCENT`): the `quality` preset's 8 Mbit/s is 4 Mbit/s
of AV1. Half sits inside every BD-rate stage 1 measured (−46% VMAF and −61%
PSNR-Y on the desktop against `openh264` as shipped, −55% on the game), so the
picture is at least as good as the H.264 one it replaces. It is also faster:
on beta's desktop 17.4 ms at p95 instead of 20.0 at the full 8 Mbit/s, and on
the game 23.2 instead of 25.2.

The ratio lives in the encoder, not in the ladder. The adaptive controller
compares what the guest received with what the host sent, never with the
target, so an AV1 stream sending half of it reads exactly like a quiet screen.
A bitrate change goes to libaom through `aom_codec_enc_config_set`: the stream
carries on, with no keyframe.

### 5. Not choosing it on a machine that cannot keep up

ARM and weak x86 were not measured. Instead of a list of processors, each host
answers for itself, three times over:

1. **Before the first session**, once per process: the product's own path —
   BGRA to I420, then libaom at the session's settings — against the product's
   own `openh264` fallback, both on the same synthetic 1080p desktop in the
   three acts of the stage-1 recording (a line typed, a page scrolled, a window
   of text dragged; 30 frames each), paced at 30 frames a second. Software AV1 is `Ready` only when its p95
   is no worse than `openh264`'s **and** within 22 ms
   (`SOFTWARE_AV1_FRAME_BUDGET_MS`) — the owner's threshold, applied to the
   machine in front of it. The measurement starts on its own thread when the
   first guest that decodes AV1 says `Hello`, which is before consent, so it is
   usually done by the time the media connection is. It asks for a hardware
   H.264 encoder first and measures nothing on a host that has one. About
   six seconds of wall time, mostly idle between paced frames; a frame over
   250 ms ends it at once. Scrolling alone was tried first and measured
   8–10 ms kinder than a rendered-text desktop on the same machine — the
   window drag is what a desktop's p95 is made of.
2. **During every session**, the encode loop checks the p95 of its own
   scale-and-encode time over windows of 90 frames. Two windows in a row over
   the 30 fps frame interval (33 ms) and software AV1 is off for the rest of
   the process; the loop ends and the guest's redial gets H.264. The interval,
   not the 22 ms: whether to offer it was decided by the measurement, and in a
   session the only question left is whether it keeps the frame rate. On beta
   a game took 22–27 ms in AV1 against 31–67 ms and 40% dropped frames in
   `openh264`; anything stricter would hand that session to the encoder that
   does worse.
3. **On failure**: an encoder that refuses frames up to the bound of ADR 0135
   turns software AV1 off for the process and ends the loop the same way,
   rather than ending the session — H.264 is right behind it on the same host.

ARM never gets this far: the feature is built for x86-64 Windows and Linux
only, and the vendored libaom's build refuses any other target.

### 6. The build

- libaom **3.15.1**, the release stage 1 measured, is vendored in
  `crates/aom-sys/libaom` from the official release tarball, minus its tests,
  documentation, examples, applications and the third-party code only those
  use (`crates/aom-sys/VENDORED.md` says exactly what and how). It is built
  from source with `cmake`, statically, realtime-only (as WebRTC builds it),
  encoder only, with runtime CPU detection — and the result encodes byte for
  byte what the untrimmed release does.
- **The build fails without nasm.** `openh264-sys2` quietly drops its assembly
  when it cannot find one, which stage 1 timed at four to eight times slower.
  `lumepeer-aom-sys` looks for nasm itself, stops with the reason when there is
  none, and after libaom's configure step checks that it came out with x86-64,
  SSE2 through AVX2 and runtime CPU detection.
- **The binary is checked too.** `ci/find-libaom-asm.py` looks for the machine
  code nasm assembled in the executable a release row ships, and the
  `windows-amd64` and `linux-amd64` rows fail without it. Only those two rows
  carry `encode-aom`; both install nasm. A side effect worth knowing: the
  Linux row never had nasm, so its `openh264` has been built without assembly
  until now, and is built with it from here on.
- An installed lumepeer needs nothing on the machine: libaom is inside the
  executable.
- The FFI goes through the stage-1 C shim (`crates/aom-sys/shim/aom_shim.c`),
  extended only by a bitrate setter and by keeping errors for the caller
  instead of printing them; `encode::aom` is the one module that calls it, every
  `unsafe` block with its `SAFETY:` note. Pixels are read through
  `Frame::as_cpu`, never `Frame::data`, so a zero-copy frame (ADR 0073) encodes
  like any other.
- Licence: libaom is BSD 2-Clause with the AOMedia Patent License 1.0,
  recorded in `deny.toml` next to `openh264`'s BSD 2-Clause and in the crate's
  own `license` field for the SBOM.

### 7. The guest's codec string

The guest configured AV1 as `av01.0.05M.08` — level 3.1, which covers 720p —
under a comment that said level 4.1. It is now `av01.0.09M.08`, level 4.1, as
the comment always said.

## Consequences

- A host with no hardware encoder that passes its measurement shows a guest that
  decodes AV1 an AV1 picture on `quality`: every frame encoded, at half the bits
  and better text than the `openh264` fallback, at 1.1–1.2 cores on a desktop
  and 2.3–3.2 on a game where `openh264` used 0.6–0.7.
- `balance` and `performance` on such a host are H.264, as before; switching
  between them and `quality` costs the guest half a second of "reconnecting".
- The first session after a start can run on H.264 if consent and the media
  connection beat the six-second measurement, and the guest named its preset
  before the measurement finished too. The next session gets AV1.
- A host with a screen above 1080p and an AV1 guest loses about half a second
  at the start of each session while the first stream finds out.
- The measurement costs every host that builds `encode-aom`, has AVX2 and no
  hardware encoder about five core-seconds of CPU over six of wall time, once per
  process, the first time a guest that decodes AV1 connects.
- Binary size grows by libaom's encoder (the static library is 7 MB; what the
  linker keeps of it is less).

## Still open

- **Decoding on beta.** Stage 1 could not measure AV1 decode on beta (Vega 8,
  no hardware AV1 decode); this cloud session could not reach it either. It is
  the condition for merging this into master: p95 of software decode of a
  1080p libaom stream at most ~10 ms.
- **Why beta finds no hardware H.264** although the 5700U has VCN: not
  investigated (no access).
- **The recorded clips.** `codec-bench`'s `lumepeer-aom` variant runs the
  product encoder; on a synthetic 1080p desktop it produced byte for byte the
  stream the stage-1 shim did on the same libaom build (the product at a
  session target of 2T, the shim at T), within run-to-run noise of its speed.
  Its numbers against `aom-s10` on the recorded 1080p desktop are still to be
  taken on a machine that has the clips (`matrix.py --only
  lumepeer-aom,aom-s10`).
- **The measurement is a little kind.** On the 4-core cloud VM stage 2 was
  built on, the product path took 24–28 ms at p95 on a 1080p desktop clip with
  rendered text — over the 22 ms, though 1.4–1.6 times faster than `openh264`
  there — while the measurement took 21.7–22.4 ms and passed in one run of
  three. Its synthetic glyphs are simpler than a font. A host within about
  10% of the threshold can therefore pass it; the live check of section 5
  only catches one that cannot hold 30 frames a second.
- **A live pair on Windows**, and on beta: the end-to-end test runs a host and
  a guest in one process with the real libaom (`a_host_without_hardware_streams_software_av1_and_follows_the_preset`),
  but no two real windows have yet shown each other an AV1 picture from it.
- **Games against the 22 ms.** On beta at the `quality` preset's 4 Mbit/s of
  AV1 a game measured 23.2 ms at p95: inside "no worse than `openh264`" by a
  wide margin, 1.2 ms outside the absolute half of the threshold. The
  measurement that gates the choice uses screen content, as stage 1's
  threshold did.
