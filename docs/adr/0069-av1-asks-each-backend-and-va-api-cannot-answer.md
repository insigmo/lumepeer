# ADR 0069 — AV1 asks each backend, and VA-API cannot answer

Status: accepted
Date: 2026-09-09

Follows [ADR 0011](0011-windows-hardware-encoder.md),
[ADR 0040](0040-the-vaapi-encoder-and-what-a-probe-is-allowed-to-claim.md) and
[ADR 0066](0066-the-videotoolbox-encoder-and-the-frame-a-probe-has-to-produce.md),
which set the rule a probe obeys. Builds on
[ADR 0067](0067-codec-negotiation-guest-advertises-host-intersects.md), which
put a codec on the wire in the first place.

## Context

`VideoCodec::Av1` has existed since phase 2 and has never been reachable.
`probe_hardware` refused it before any backend saw it:

```rust
if config.codec != VideoCodec::H264 {
    return None;
}
```

The comment above that check argued the gate belonged in one place because no
backend implemented AV1, so a new backend would otherwise reintroduce the
"answer an AV1 question with the H.264 rehearsal" mismatch silently. That was
true while the answer was "no" everywhere. It stops being true the moment one
backend can say yes: a central gate then has to be edited in step with every
backend that learns a codec, and the thing it was protecting — that no backend
answers for a codec it never activated — is a property of the backends, not of
their caller.

§11 allows AV1 only with mutual hardware support and gives it no software
fallback. ADR 0067 made a host announce the codec it chose, and left the AV1
arm of `choose_media_codec` calling `probe_hardware`, which was guaranteed to
say no.

## Decision

**The gate moves into the backends.** `probe_hardware` refuses nothing and asks
each compiled-in backend about `config.codec`. Every backend checks the codec
itself before it claims anything: Media Foundation dispatches on it, VA-API and
`VideoToolbox` answer `false` for everything but H.264 and say in their own doc
comments why.

**Media Foundation encodes AV1 through the same MFT machinery.** One function,
`mf_subtype`, turns a `VideoCodec` into a Media Foundation output subtype, and
it is the only place that mapping exists — so enumeration (`MFTEnumEx` filtered
to `MFT_ENUM_FLAG_HARDWARE`) and the negotiated output type can never ask for
different things. The rest of the module is unchanged: NV12 in, the same
asynchronous MFT protocol, the same low-latency `ICodecAPI` tuning. Two
properties are H.264's alone and are set only for it: `MF_MT_MPEG2_PROFILE`
carries an `eAVEncH264VProfile` value, and `AVEncH264CABACEnable` names an
entropy coder AV1 does not have a choice about.

**The AV1 probe encodes a frame; the H.264 probe still does not.** ADR 0066's
reasoning, applied where it now fits: on H.264 the question is whether an
encoder exists at all, and plenty of machines have none. On AV1 the hardware is
at most a few years old and reached through driver paths much younger than the
H.264 ones beside them, so the question is whether the transform that just
enumerated actually produces a picture. `hardware_available` therefore builds an
encoder for AV1 and pushes one 64x64 frame through it, and reports `Hardware`
only when a non-empty bitstream comes back. The H.264 path is left exactly as
ADR 0011 wrote it.

**Keyframes are read per codec.** `MFSampleExtension_CleanPoint` stays the
primary signal, cross-checked against the bitstream because not every driver
sets it faithfully — but the cross-check is now `bitstream_is_random_access`,
which scans Annex-B for an IDR NAL on H.264 and walks OBU sizes for a sequence
header on AV1. The two bitstreams have nothing structurally in common; asking an
AV1 temporal unit whether it contains an H.264 IDR finds whatever the byte
pattern happens to hit.

A sequence header rather than a key frame header, deliberately: AV1 puts the
decoder configuration in the sequence header, so a decoder that has not read one
cannot decode the key frame after it either. An encoder emits one alongside
every random-access point, which makes "does this temporal unit carry a sequence
header" the same question as "can a decoder join here", and answers it by
walking lengths instead of parsing a frame header's variable-length bit fields.

**No software AV1, anywhere.** `libaom`, `rav1e` and SVT-AV1 are all out,
including "just for CI": §11 is explicit, and `ci/resource-budget.yml` would not
survive it either. `select_encoder` refuses any non-H.264 codec whose probe did
not report hardware, and refuses it a second time on the other side of the
hardware branch — the case where a probe said yes and the constructor then
failed, which on Linux and macOS falls through towards `openh264`. Encoding
H.264 into a session that has already announced AV1 on the wire would be a black
window with nothing logged anywhere.

**VA-API answers `false` for AV1, and not for want of looking.** This is the
part of the plan that could not be carried out. VA-API's AV1 encode entrypoint
cannot be driven through `cros-libva` 0.0.13 at all:
`VAEncPictureParameterBufferAV1` carries bit offsets into a frame-header OBU —
`bit_offset_qindex`, `bit_offset_segmentation`, `bit_offset_loopfilter_params`,
`bit_offset_cdef_params`, `byte_offset_frame_hdr_obu_size`,
`size_in_bits_frame_hdr_obu` — which exist so the driver can patch fields inside
a header the *application* wrote and handed over as a packed header buffer.
`cros_libva::BufferType` has no packed-header variant: it covers picture, slice,
IQ matrix, slice data, the four encode parameter buffers, the coded buffer and
the misc parameters, and nothing else. There is no call through which a frame
header OBU could be delivered.

A session opened anyway would produce a stream with no frame header in it, which
is not a picture — and it would do so with every individual libva call returning
success, which is the exact failure mode §11's mutual-hardware-support rule
exists to prevent. So the backend declines, in one line, with the reason written
next to it.

## Consequences

An Intel Arc, 12th-generation-or-newer Intel, NVIDIA 40-series or AMD RDNA 3
host on **Windows** can now negotiate and encode AV1. The same machine on
**Linux** cannot, and reports so honestly; it keeps encoding H.264 through
VA-API. macOS is unchanged: `VideoToolbox` does encode AV1 on Apple silicon that
has the hardware, through a different codec type with its own parameter sets,
and nothing here has checked it.

AV1 is not preferred by default and this ADR does not make it one.
`choose_media_codec` picks it when the guest advertised it *and* the probe
reports hardware, which is the intersection ADR 0067 defined — but whether AV1
is actually the better choice at a given bitrate and latency is a measurement,
and the measurement has not been made. See "Still open".

Renaming each backend's `hardware_h264_available` to `hardware_available` leaves
the name in ADR 0040's and ADR 0066's own text stale. Those are dated records of
what was decided then and are left as written.

## Still open

- **No machine with a hardware AV1 encoder was reachable.** The development host
  has an RTX 3070 (Ampere: NVENC encodes H.264 and HEVC, not AV1) and an older
  Intel UHD, so `hardware_available` for AV1 returns `false` here — the "no AV1
  hardware" half of the plan's acceptance, which *is* verified: the probe
  declines, negotiation stays on H.264, and the log says which codec was chosen.
  Everything on the other side of a `true` answer — that a real AV1 MFT
  activates, that the bitstream a guest gets is decodable, that the
  sequence-header scan finds what a real encoder emits — has not run.
- **No measurement.** The plan requires bitrate at equal visual quality and
  end-to-end latency, AV1 against H.264, before AV1 is made preferable. Until
  those numbers exist, "the guest asked and the hardware is there" is the whole
  of the selection rule.
- The OBU walk is unit-tested against hand-built temporal units, not against a
  real encoder's output.
