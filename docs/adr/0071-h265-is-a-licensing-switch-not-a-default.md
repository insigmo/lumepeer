# ADR 0071 — H.265 is a licensing switch, not a default

Status: accepted
Date: 2026-09-09

Follows [ADR 0069](0069-av1-asks-each-backend-and-va-api-cannot-answer.md),
which made `probe_hardware` a per-backend question, and
[ADR 0067](0067-codec-negotiation-guest-advertises-host-intersects.md), which
reserved wire byte 2 for H.265 before anything could produce it.

## Context

`MediaCodec::H265` has had a wire byte and a `Hello.features` string since ADR
0067 and no encoder behind either. H.265 is the one optional codec whose
obstacle is not technical: it is covered by the Access Advance and Via LA
patent pools, and this project ships an installer. Whether a distributed build
may carry an H.265 encoder is a question for someone with the authority to
answer it, not for whoever writes the encoder.

What that means for the code is narrow and specific: the support has to exist,
have to be complete, and have to be reachable by exactly one flag — so that
the decision, once taken either way, is executable without a patch.

## Decision

**Everything is behind `encode-h265`, and nothing turns it on.** The feature
is off in `crates/media`'s defaults, off in `apps/desktop/src-tauri`'s
defaults, absent from every row of `.github/workflows/release.yml`'s matrix
and from `Taskfile.yml`'s `*_MEDIA_FEATURES`. It adds no dependency — both
backends already carry the bindings they need — so it is a marker that gates
code and nothing else.

**Turning it on for a distributed build is a legal decision, not a technical
one.** That sentence is the point of this ADR. A build with the feature is a
build whose publisher has accepted whatever obligations follow; nothing in the
code can make that judgement, and nothing in the code should make it easy to
enable by accident. Both `Cargo.toml`s say so where the feature is declared.

**`VideoCodec::H265` exists unconditionally; only the answers are gated.** The
enum variant is not feature-gated, so every `match` over the codec set reads
the same in both builds and a new backend cannot forget a codec that exists
only sometimes. What the feature gates is whether any backend answers for it:
without it `mf_subtype` returns `None` and `va_profile`/`coding_block` return
an error *before* Media Foundation or libva is touched at all, so a probe
answers `false` having made no platform call.

**Windows: the same MFT machinery, a different subtype.**
`MFVideoFormat_HEVC` through the same `MFTEnumEx` +
`MFT_ENUM_FLAG_HARDWARE` path, Main profile 4:2:0 8-bit via
`MF_MT_MPEG2_PROFILE` — which carries an `eAVEncH265VProfile` for an HEVC MFT
and an `eAVEncH264VProfile` for an H.264 one, so the value is chosen per codec
by `mf_profile` rather than shared. The probe encodes a frame before claiming
hardware, for the reason ADR 0069 gives.

**The stream is Annex-B, and the guest's config string says so.** The MFT
emits a byte stream with the VPS/SPS/PPS in band ahead of every IRAP picture —
verified, not assumed: the probe's own output is asserted to start with a
start code and to contain NAL types 32, 33 and 34. The guest configures
`VideoDecoder` with `hev1.1.6.L120.B0` and **no** `description`, and WebCodecs
reads a missing `description` as "this bitstream is Annex-B". `hev1` rather
than `hvc1` for the matching reason: in ISOBMFF `hvc1` promises the parameter
sets are out of band and never change, `hev1` allows them in the bitstream,
and in the bitstream is where they are. The batch plan named `hvc1`; the
stream this produces is what `hev1` describes, and a test asserts the pairing.

Level 4.0 (`L120`) rather than the plan's 3.1 (`L93`) for the same
stream-must-match-string reason: 3.1 stops at 720p, this pipeline's default
picture is 1080p, and the VA-API encoder writes level 4.0 into its own
sequence header. A decoder asked for less than the stream carries may accept
the configuration and then fail on the pictures, which is a black window
instead of an honest fallback to H.264.

**Linux: `VAProfileHEVCMain` with its own parameter buffers.** Sequence,
picture and slice parameters are built by `hevc_*` functions that share
nothing with the H.264 ones but the rate-control buffer, which is codec
agnostic. This is not tidiness: H.265 numbers `slice_type` the other way round
from H.264 (I is 2 in both, but P is 1 rather than 0), counts picture geometry
in 32-pixel coding tree units rather than 16-pixel macroblocks, and counts
levels in thirtieths rather than tenths. Every one of those is a value the
driver would accept and no decoder would agree with.

**A probe asks at 256x256, not 64x64.** Discovered rather than designed: the
64x64 rehearsal ADR 0011 uses for H.264 is refused by both the "NVIDIA HEVC
Encoder MFT" and the "Intel Hardware H265 Encoder MFT" on the development
machine, and by its AV1 MFT as well. The probe reported "no encoder" where the
true answer was "not at that size" — which means ADR 0069 shipped an AV1 path
that could never have been selected on hardware that has one. `probe_dims`
now answers per codec, and the same size is used for the VA-API probe.

**AV1 is asked about before H.265.** Where a host and a guest can do both, the
royalty-free one wins.

## Consequences

A build made with `--features encode-h265` on a Windows host with an HEVC
encoder will negotiate and encode H.265 for a guest whose WebView decodes it
(ADR 0070). Every other build behaves exactly as before, and a test asserts
that: without the feature the probe is `false` and `choose_media_codec` returns
H.264 whatever the guest advertised.

The 64x64 probe fix changes AV1's story from ADR 0069. That ADR's "Still open"
said no machine with an AV1 encoder was reachable; the machine was in front of
it all along, answering a question that was being asked at a size it declines.
See the numbers below.

### What was measured

150 frames of a synthetic 1920x1080 desktop — flat ground, a static block of
text-like stripes, one 500x400 window moving four pixels a frame — encoded
through `select_encoder` on a machine with an RTX 3070 and an Intel UHD, whose
registered MFTs are "NVIDIA HEVC Encoder MFT", "Intel Hardware H265 Encoder
MFT" and "Intel Hardware Accelerated AV1 Encoder MFT". Per-frame time includes
the BGRA-to-NV12 conversion, which is identical for all three.

| target | codec | achieved | encode p50 | encode p95 |
|---|---|---|---|---|
| 2 Mbps | H.264 | 1 600 kbps | 22.2 ms | 36.9 ms |
| 2 Mbps | AV1 | 195 kbps | 69.9 ms | 103.9 ms |
| 2 Mbps | H.265 | 1 600 kbps | 17.9 ms | 31.6 ms |
| 8 Mbps | H.264 | 6 400 kbps | 15.3 ms | 34.3 ms |
| 8 Mbps | AV1 | 285 kbps | 48.2 ms | 98.3 ms |
| 8 Mbps | H.265 | 6 400 kbps | 18.6 ms | 30.7 ms |
| 30 Mbps | H.264 | 26 250 kbps | 23.6 ms | 40.8 ms |
| 30 Mbps | AV1 | 419 kbps | 66.7 ms | 117.8 ms |
| 30 Mbps | H.265 | 24 000 kbps | 22.7 ms | 38.0 ms |

Read carefully, because two of the three columns say less than they appear to.

The **bitrate** column is not a quality comparison. H.264 and H.265 are driven
in a constant-ish rate-control mode and fill the target on content that needs
a fraction of it; AV1's MFT evidently does not, which is why its achieved rate
barely moves with the target. Nothing here says AV1 needs a twentieth of the
bits at equal quality, and nothing here could: measuring that needs a decode
and a distortion metric, and this workspace has no AV1 or H.265 decoder to
build one with. That comparison remains unmade.

The **latency** column does say something, and it is the important one. AV1
costs two to three times an H.264 or H.265 frame on this machine, and its p50
at 1080p is above the 33 ms a 30 fps session has for everything — capture,
convert, encode, send. H.265 is level with H.264, sometimes slightly ahead.
On this hardware AV1 is not a latency win, and §11's remote-desktop priorities
are latency first. That is a measurement, on one machine, of one Intel iGPU
whose AV1 support may well be partly hybrid; it is not a verdict on AV1. It is
enough to say that neither optional codec becomes preferred by default here.

## Still open

- **No guest has decoded either stream.** The Windows encoders are exercised
  by tests that assert a picture comes back and, for H.265, that it is
  Annex-B with its parameter sets in band. Nothing has fed one to a
  `VideoDecoder`, because that needs a second machine and a session.
- **The VA-API H.265 path has never run.** It compiles and passes
  `clippy -D warnings` on Linux against real libva 1.22 headers, and its
  geometry and profile choices are unit tested — but no VA-API encode-capable
  GPU was reachable, exactly as ADR 0040 records for the H.264 path beside it.
  The parameter buffers are written from the specification and the H.264
  sibling, and are unverified.
- **No quality-matched bitrate comparison**, for the reason given above.
- The synthetic sequence is far more compressible than a real desktop; the
  latency figures generalize better than the bitrate ones.
