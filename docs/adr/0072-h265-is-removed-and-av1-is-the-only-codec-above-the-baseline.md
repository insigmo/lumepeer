# ADR 0072 — H.265 is removed, and AV1 is the only codec above the baseline

Status: accepted
Date: 2026-09-09

Supersedes [ADR 0071](0071-h265-is-a-licensing-switch-not-a-default.md), which
built the H.265 path and put it behind a flag nothing turned on. Follows
[ADR 0067](0067-codec-negotiation-guest-advertises-host-intersects.md), which
assigned the wire byte, and
[ADR 0069](0069-av1-asks-each-backend-and-va-api-cannot-answer.md), which made
`probe_hardware` a per-backend question.

## Context

ADR 0071 answered "may this project ship an H.265 encoder?" with "not until
someone with the authority decides", and made that decision executable as one
build flag. The decision has now been taken, and it is no.

The reasoning is the one ADR 0071 already set out, followed to its end. H.265
is covered by three competing patent pools — Access Advance, Via LA and Vectis
— plus holders in none of them, and the pools charge per unit for a product
that ships an installer. AV1 is royalty-free by AOMedia's own patent licence,
granted to any implementer rather than only to members. H.264's core patents
have expired, and the encoder comes from the operating system rather than from
this workspace. H.265 is the only codec here that carries a real bill, and the
only one whose obstacle is not technical.

A flag that nothing turns on is not free. It is a second set of parameter
buffers in the VA-API backend, a second subtype and profile in the Media
Foundation one, a second bitstream scanner, a second `VideoDecoder` config
string, a third arm in every negotiation match, and two `#[cfg]` branches on
each of them — around 600 lines carried, compiled in one configuration, and
never once run against a decoder (ADR 0071's own "Still open" says so of both
backends). Keeping that against a decision already made is cost with no
remaining option attached to it.

## Decision

**H.265 is removed, not disabled.** `VideoCodec::H265`, `MediaCodec::H265`,
`FEATURE_CODEC_H265`, `WireCodec.H265`, the `encode-h265` feature in both
`Cargo.toml`s, the Media Foundation HEVC subtype and profile, the whole
`hevc_*` VA-API parameter-buffer family, the Annex-B IRAP scanner and the
`hev1` decoder string are all gone.

**Wire byte 2 stays retired rather than reused.** `MediaCodec::try_from`
refuses it as malformed, exactly as it refuses any byte it has never assigned,
and both the Rust enum and `WireCodec` carry a comment saying why the number is
skipped. Reusing 2 for a future codec would mean a peer built before this
change reads that codec's frames as HEVC and fails on the pictures rather than
on the configuration — the failure mode ADR 0071 took care to avoid in the
`hev1`/`hvc1` choice, arrived at from the other direction.

**AV1 is the only codec above the baseline, and it is preferred whenever both
ends can manage it.** `choose_media_codec` asks the guest's advertisement and
`probe_hardware` and returns AV1 when both say yes; H.264 is what remains
otherwise. That behaviour is unchanged — ADR 0071 already put AV1 ahead of
H.265 in the same function — but the ordering is now the whole rule rather
than the first of two.

**H.264 remains the mandatory baseline and is not going anywhere.** It is the
only codec with an encoder on every platform this project targets: VA-API has
no drivable AV1 encode entrypoint through `cros-libva` (ADR 0069), no Mac has
an AV1 hardware encoder at all, and there is no software AV1 encoder in this
workspace. A build that dropped H.264 would stream on Windows hosts with a
recent GPU and nowhere else.

## Consequences

A host and guest that can both do AV1 negotiate AV1, as before. Everything
else negotiates H.264, as before. No build can reach H.265, and none could
before this either — no release row ever enabled the feature — so no shipped
behaviour changes.

The latency measurement in ADR 0071 survives this ADR and is worth restating,
because it is the one fact that argues against the direction taken here: on
the development machine's Intel AV1 MFT, AV1 costs two to three times an
H.264 frame, with a p50 above the 33 ms a 30 fps session has for everything.
Where that holds, preferring AV1 buys a licence-free bitstream at the cost of
the latency §11 puts first. It is one machine and one possibly-hybrid iGPU,
and it is the strongest reason to keep `choose_media_codec`'s preference under
review rather than to treat it as settled.

## Still open

- **No guest has decoded an AV1 stream from this host.** Unchanged from ADR
  0071's first open item, minus the H.265 half.
- **Whether AV1 should be preferred on hardware where it is slower.** The
  measurement above says the preference is not free. Nothing yet measures it
  per machine or falls back on it.
- **No quality-matched bitrate comparison.** This workspace still has no AV1
  decoder to build one with.
