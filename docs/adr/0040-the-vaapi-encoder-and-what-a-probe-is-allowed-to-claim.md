# ADR 0040 — The VA-API encoder, and what a probe is allowed to claim

Status: accepted
Date: 2026-08-28

Follows [ADR 0011](0011-windows-hardware-encoder.md), which set the rule this
one obeys on a second platform.

## Context

ADR 0011 established that `probe_hardware` is not a capability query but a
rehearsal: on Windows it enumerates hardware H.264 MFTs, activates one, and
negotiates NV12 in / H.264 out, reporting `Hardware` only when all of that
actually succeeds. "Never a hopeful guess."

Every other platform returned `None`, so `select_encoder`'s non-Windows arm
was an honest error guarding an unreachable state. Linux hosts therefore
always encoded with `openh264`.

## Decision

**VA-API for Linux, H.264 only, behind `encode-vaapi`.** Built on
`cros-libva` 0.0.13 (BSD-3-Clause, already on `deny.toml`'s allowlist), which
is a safe wrapper, so this module needs no `unsafe` of its own — unlike the
Media Foundation one.

**The probe is the constructor.** `hardware_h264_available` calls
`VaapiEncoder::open` and reports whether it succeeded. Not "is libva
installed", not "does the driver list `VAEntrypointEncSlice`" — the whole
sequence: open a DRM display, create the config, allocate NV12 surfaces,
create the encode context, find an NV12 image format to upload through. A
driver that lists an entrypoint and a driver that will give you a context are
different populations, and a machine with libva installed and no
encode-capable GPU behind it is the common case, not the exotic one.

Because the probe *is* the constructor, the "probe does not lie" property
holds by construction rather than by care, and the test asserting it can only
fail if the two calls are made to diverge deliberately.

**AV1 is refused once, centrally.** The check moved from inside the Windows
branch up into `probe_hardware` itself. No backend implements an AV1 probe,
and the failure this guards — handing back an H.264 answer to an AV1 question,
which is exactly what §11's mutual-hardware-support rule exists to prevent —
is one a new backend would reintroduce silently if the check lived inside each
of them.

**A probe that succeeds and a constructor that then fails falls back, it does
not fail the session.** On Linux `select_encoder` logs and drops through to
`openh264` rather than propagating. The two calls are microseconds apart and a
driver changing its mind between them is rare, but a session is worth more
than being right about it (§18).

**`encode-vaapi` is deliberately not in the shipped Linux feature set.** A
`.deb` must install and run on a machine with no VA-API at all. Turning it on
by default would make the package's usefulness depend on the GPU in the
machine, and the fallback already handles the negative case correctly.

**NVIDIA is out of scope, explicitly.** NVIDIA's Linux encode path is NVENC: a
different SDK, different licence terms, a different `cargo deny` question.
`nvidia-vaapi-driver` bridges VA-API to NVDEC — decode — and exposes no encode
entrypoint, so an NVIDIA host correctly probes `false` here and falls back
rather than half-working. Whether to add NVENC is a separate decision.

**BGRA→NV12 moved to `encode/nv12.rs`.** All three hardware backends take NV12
while the capturers hand out BGRA8. The conversion is pure arithmetic with no
platform API in it, and a copy per backend is three chances to get BT.601
subtly different on one of them — with the tests for each copy only ever
running on the machine that built it. One copy, tested everywhere.

## Consequences

- `set_bitrate` costs nothing: the rate-control buffer travels with each
  picture, so an `AbrController` adjusting up to `ABR_ADJUST_MAX_RATE_PER_SEC`
  times a second rebuilds nothing. An encoder needing a new session per change
  would be useless here, which is why the bitrate lives in the config.
- One reference frame, one slice per picture, no B-frames, CAVLC, Constrained
  Baseline. Every one of those is a latency choice, not a limitation of the
  API: a deeper DPB and more slices buy compression and error resilience that
  a live viewer on a reliable QUIC stream does not want to pay for.
- Coded pictures are macroblock-aligned and cropped back with the SPS crop
  offsets, so a 1080-line screen codes as 1088 and the eight lines come off in
  the bitstream rather than in the picture.

## Verification

This is where this ADR has to be careful, because the decision above claims a
standard it can only partly demonstrate.

**Verified.** Compiles and lints clean at `-D warnings` for `encode-vaapi`
alone and combined with `encode-openh264`. `cargo build --workspace` still
needs no platform SDK. `cargo deny check licenses bans` passes with
`cros-libva` in the tree. The Windows encoder still builds and its 95 tests
still pass after the NV12 move — including `probe_hardware_agrees_with_...` in
its *positive* case, on a machine that does have a hardware MFT.

Run on a live Debian 13 host with libva 2.22 and a real DRM render node whose
GPU (VMware SVGA) has no video driver:

```
libva info: Trying to open .../vmwgfx_drv_video.so
libva info: va_openDriver() returns -1
probe_hardware(H264) = None
probe_hardware(AV1)  = None
select_encoder(H264) -> SoftwareOpenH264
OK: AV1 refused -- AV1 needs hardware support on both sides ...
```

That is the case that matters most and it behaves correctly: libva present,
no encoder behind it, probe declines, session continues on `openh264`.

**Not verified: a single frame has ever been encoded by this code.** No
machine reachable from this project has a VA-API encode entrypoint — the
Debian VM's GPU is VMware SVGA with no video driver, and WSL exposes `/dev/dxg`
but no DRM node for Mesa's VA driver to bind. So the sequence, picture and
slice parameter buffers, the NV12 upload's stride handling, the coded-buffer
readback, the IDR/P-frame bookkeeping and the crop offsets are all written
against the specification and reviewed, and none of them has met a driver.

The measurements ADR 0011's sibling task asks for — latency, CPU and quality
against `openh264` at equal bitrate — are therefore also missing, and the
"enable by default" question they were meant to settle is answered
conservatively in the meantime: not enabled by default. That is the right
default regardless, but it should be re-examined on hardware.

The design's own safety net is what makes shipping this acceptable in that
state: the feature is off in every shipped build, the probe declines on every
machine without an encoder, and a constructor that fails after a successful
probe falls back rather than breaking the session. The first machine with an
Intel or AMD GPU should run
`cargo test -p lumepeer-media --features encode-vaapi,encode-openh264` and an
actual session, and this section should be rewritten with what happened.

## Still open

VideoToolbox on macOS, the third backend the same task asked for, is not
implemented. It could not be compiled, let alone run: no macOS machine was
reachable while this work was done. Writing it would have meant several
hundred lines of `VTCompressionSession` code that no compiler had ever seen,
which is a different proposition from the VA-API module above — that one at
least type-checks against real headers and runs its probe correctly. macOS
hosts continue to encode with `openh264`, which is what `MACOS_MEDIA_FEATURES`
already ships and says.
