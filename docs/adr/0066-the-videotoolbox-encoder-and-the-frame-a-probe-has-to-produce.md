# ADR 0066 — The VideoToolbox encoder, and the frame a probe has to produce

Status: accepted
Date: 2026-09-08

Follows [ADR 0011](0011-windows-hardware-encoder.md) and
[ADR 0040](0040-the-vaapi-encoder-and-what-a-probe-is-allowed-to-claim.md),
which set the rule this one obeys on the third and last desktop platform.
Constrained by
[ADR 0059](0059-the-host-encodes-for-latency-and-measures-congestion-honestly.md).

## Context

ADR 0011 established that `probe_hardware` is a rehearsal rather than a
capability query, and ADR 0040 carried the same rule to Linux. Its own "Still
open" section named what was left:

> `VideoToolbox` on macOS, the third backend the same task asked for, is not
> implemented. It could not be compiled, let alone run: no macOS machine was
> reachable while this work was done.

macOS hosts therefore encoded every session with `openh264`, which is what
`MACOS_MEDIA_FEATURES` shipped and said.

## Decision

**`VideoToolbox` for macOS, H.264 only, behind `encode-videotoolbox`.** Built
on `objc2-video-toolbox` 0.3.2, the same objc2 0.3.2 framework-binding family
and the same pinned versions the `capture-screencapturekit` backend already
resolves to, so the encoder adds one framework crate rather than a second
objc2 generation. The feature is not in `default`, and every crate it needs is
declared optional in the crate's macOS target table, so `cargo build
--workspace` still needs no platform SDK.

The feature ships **alongside** `encode-openh264` on macOS, never instead of
it. A Mac whose probe declines has to keep running on the software fallback
(§18); the release matrix carries both for the same reason the Windows rows
carry both.

**The probe opens a session and encodes a frame.** One step further than
either sibling backend goes, and deliberately so. On Windows and Linux the
interesting question is whether a hardware encoder exists at all — plenty of
machines have none. On macOS every machine of the last decade has one, so
"does `VTCompressionSessionCreate` succeed" is a question whose answer is
almost always yes and almost never the reason a session shows nothing. The
failure that matters there is the session that opens and then produces no
picture, and a probe that stops at `create` cannot tell the two apart. So
`hardware_h264_available` builds the encoder, pushes one 64x64 frame through
it, and reports `Hardware` only when a non-empty bitstream comes back.

**The backend owns the AVCC-to-Annex-B rewrite.** `VideoToolbox` emits each
NAL unit behind a big-endian length prefix; the guest's decoder
(`apps/desktop/src/view-decoder.ts`) walks Annex-B start codes and derives its
own `avc1.PPCCLL` string from the SPS it finds there. The difference is this
backend's output format, not the protocol's, so it is rewritten here rather
than anywhere downstream. The parameter sets get the same treatment for the
same reason: `VideoToolbox` keeps the SPS and PPS in the sample's
`CMFormatDescription` instead of in the bitstream, so every keyframe leaves
this module with them prepended — without that, a guest that joined
mid-stream has nothing describing what follows and never draws a frame.

That rewrite is pure byte manipulation with no framework call in it, so it is
compiled and unit-tested on **every** platform the feature is enabled for
rather than only on a Mac. This is not tidiness; see "Consequences".

**No reordering, ever.** `kVTCompressionPropertyKey_RealTime` is set true and
`AllowFrameReordering` false, and both are hard failures rather than
best-effort: an encoder free to reorder holds a picture back waiting for a
future one, which is tens of milliseconds no amount of work anywhere else in
the pipeline wins back. ADR 0059 made the host encode for latency, and a
picture that depends on one that has not been sent yet is exactly the delay
that decision refuses. The profile (Main, falling back to Baseline), the
expected frame rate and the keyframe interval are best-effort: an encoder that
keeps its default for any of them still produces a usable stream.

**One picture in, one picture out.** Each `VTCompressionSessionEncodeFrame` is
followed by `VTCompressionSessionCompleteFrames` up to that frame's own
timestamp. Unlike the Media Foundation drain this mirrors, it is not a
pipeline flush: it forces emission of what has already been submitted and
leaves the session streaming with its reference frames intact, so there is no
stream restart and no keyframe behind it.

**A bitrate change is a property write.** `AverageBitRate` is set on the live
session. Reopening would cost an IDR every time the adaptive controller moved
the target, which at `ABR_ADJUST_MAX_RATE_PER_SEC` is once a second — a
visible hitch coming from the mechanism meant to smooth one over.

## Consequences

`select_encoder`'s macOS arm now constructs a `VideoToolboxEncoder`, and — like
the VA-API arm and unlike the Windows one — falls through to `openh264` with a
log line if the constructor fails after a successful probe. The probe has
already encoded a frame by then, so a constructor that then fails is the
framework changing its mind between two calls a microsecond apart, and a
session is worth more than being right about that.

**What has and has not been verified.** No macOS machine was reachable while
this was written either: the test host answers to an mDNS name that did not
resolve, and it is not on the tailnet. What was done instead is stronger than
ADR 0040 could manage for VA-API at the same stage, but weaker than a session:

- the whole backend type-checks and passes `clippy -D warnings` against
  `x86_64-apple-darwin`, using the real `objc2` framework bindings, by
  cross-checking from a Windows host;
- the AVCC-to-Annex-B rewrite and its edge cases (a narrower length prefix, a
  NAL longer than the sample, a truncated prefix, a zero-length NAL) are unit
  tested and green on a non-Apple machine, which is why that code deliberately
  does not live behind `cfg(target_os = "macos")`;
- nothing has run against a real `VTCompressionSession`. The behavioural
  tests — that a session keeps encoding past the first frame, that a requested
  keyframe arrives, that the first frame starts with an Annex-B start code —
  are written and will skip on any machine without the hardware, so they prove
  nothing until a Mac runs them.

The design's own safety net is what makes shipping it in that state
acceptable, exactly as it was for ADR 0040: an honest probe that declines
costs nothing, a constructor that fails falls back, and `encode-openh264`
stays in every macOS build. The first machine with a working Mac toolchain
should run
`cargo test -p lumepeer-media --features encode-videotoolbox,encode-openh264`
and an actual session, confirm `Hardware` in the status, watch it for ten
minutes, and rewrite this section with what happened.
