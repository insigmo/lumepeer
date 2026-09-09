# ADR 0074 — The picture ceiling is 5K, and the frame bound did not move

Status: accepted
Date: 2026-09-09

Amends [ADR 0060](0060-the-guest-names-the-picture-size-it-will-draw.md),
which introduced `StreamSizeRequest` and made `MAX_STREAM_PIXELS` the ceiling
on it. Reads against [ADR 0073](0073-a-captured-frame-can-stay-on-the-gpu-on-windows.md),
which is what makes the new ceiling affordable in main memory.

## Context

Backlog item 34 asks for "a picture above 4K, and frames heavier than 8 MiB",
and names two constants to raise: `MAX_STREAM_PIXELS` (3840x2160) and
`MAX_MEDIA_FRAME_BYTES` (8 MiB). The batch's own instruction is to measure
first and then raise **only what actually binds**.

Measured on the reference machine (Windows 11, integrated GPU, release build,
blocky pseudo-noise so the encoder has real residuals to spend bits on) by
`encode::windows::tests::what_a_picture_above_4k_costs`, at
`ABR_MAX_BITRATE_KBPS` and 60 fps:

| Codec | Size | Negotiated | Intra frame | Largest later frame | Encode time |
|---|---|---|---|---|---|
| H.264 | 3840x2160 | yes | 368 KiB (4.4% of the bound) | 79 KiB | 181 ms intra, 30 ms/frame |
| H.264 | 5120x2880 | **refused** | — | — | — |
| H.264 | 6016x3384 | **refused** | — | — | — |
| H.264 | 7680x4320 | **refused** | — | — | — |
| AV1 | 3840x2160 | yes | 1.3 MiB (15.8% of the bound) | 448 KiB | 467 ms intra, 85 ms/frame |
| AV1 | 5120x2880 | **refused** | — | — | — |
| AV1 | 6016x3384 | **refused** | — | — | — |
| AV1 | 7680x4320 | **refused** | — | — | — |

Three things fall out of that table, and none of them is the one the backlog
item assumed.

**The encoded frame was never the limit.** Not at 4K, and not at any size:
what bounds an encoded frame is rate control, not pixel count. At
`ABR_MAX_BITRATE_KBPS` a whole *second* of video is 3.1 MiB, so a single frame
cannot approach 8 MiB without the encoder having ignored its own bitrate. The
measured intra frames — 4.4% and 15.8% of the bound at 4K — say the same thing
from the other side.

**The hardware encoder is the limit.** Both MFTs on this machine negotiate
3840x2160 and refuse everything above it. That is not a property of this one
GPU either: hardware H.264 encoders are commonly capped at 4096 pixels on the
long axis, and a codec that reaches 8K in hardware is generally HEVC — removed
by [ADR 0072](0072-h265-is-removed-and-av1-is-the-only-codec-above-the-baseline.md).

**Main memory is the other limit**, and it binds before 8K by a wide margin.
On the readback capture path a session holds one BGRA frame, the cursor-free
copy of it, and the NV12 conversion buffer:

| Size | Frame buffers resident |
|---|---|
| 1920x1080 | 18.8 MiB |
| 3840x2160 | 75.1 MiB |
| 5120x2880 | 133.7 MiB |
| 7680x4320 | 534 MiB |

§15's `active_extra_rss_mib` budget is 150 MiB for the whole session.

## Decision

**`MAX_STREAM_PIXELS` becomes 5120x2880. `MAX_MEDIA_FRAME_BYTES` stays at
8 MiB. `MAX_PICTURE_PIXELS` is untouched.**

5K and not 8K, for the two limits above: 8K is refused by every encoder in
reach and would need 534 MiB of frame buffers against a 150 MiB budget.
Raising the budget to fit it is what §15 forbids in as many words, so the
ceiling is what moves.

5K only just fits, and only on one path: 133.7 MiB of the 150 MiB budget goes
to frame buffers on the readback path, leaving 16 MiB for everything else a
session does. A host actually reaching this size inside its budget is a host
on ADR 0073's zero-copy path, where all three buffers are GPU textures and
none of them is resident in main memory at all. That is recorded in
`ci/resource-budget.yml` next to the number, so the next person to raise a
ceiling sees what it spends.

`MAX_MEDIA_FRAME_BYTES` gains a compile-time assertion rather than a new
value: one frame may not be asked to carry more than a second at
`ABR_MAX_BITRATE_KBPS`. That is the invariant that makes the bound
independent of the picture size, and it is now something the build checks
instead of something a doc comment claims.

### A ceiling above what the encoder takes is a ceiling that can blank a session

Before this, `MAX_STREAM_PIXELS` was exactly 4K, so a 5K host always
downscaled to something its encoder would accept. Raising the ceiling removes
that accident: a 5K host, a guest whose window is 5K, and an encoder that caps
at 4K now meet, and `MediaFoundationEncoder::reconfigure` refuses every frame
for the rest of the session — a session that stays connected and shows
nothing.

So the encode loop gives the guest's size up rather than the picture. The
first frame the encoder refuses while a `StreamSizeRequest` size is in force
withdraws that size for the rest of the session, once, with one log line, and
the loop continues at the ADR 0018 picture budget. §18: degrade honestly,
never silently, and never to nothing.

Withdrawn for the session and not retried per frame, deliberately: the
encoder's answer about a size is a property of the hardware, and asking it
again every 16 ms would be a per-frame reconfiguration storm on the one path
that is already failing.

## Consequences

- A guest with a 5K panel gets a 5K picture from a host that can encode one,
  where before it got 4K and upscaled.
- `SLOT_PAYLOAD_BYTES` and the sandboxed decoder worker are untouched: the
  slot is sized by `MAX_PICTURE_PIXELS`, which did not move, so the RGBA
  fallback path of §11.3 is bit-for-bit what it was. `resource_budget.rs`
  measures that worker and needed no change.
- The protocol's own range check is unchanged in shape — `check_limits` still
  rejects an oversized `StreamSizeRequest` before anything is allocated
  (§3.2), only against a larger number.
- Nothing downstream buffers per resolution: the writer queue is one frame
  deep (ADR 0059), and the guest's bitstream queue is capped in bytes as well
  as in frames, so a bigger picture does not become a bigger backlog.

## Still open

- Measured on a machine whose panel is not 5K, so the numbers above are the
  encoder's and the arithmetic's, not a live 5K session's. What is untested
  is exactly the end-to-end claim: a 5K host, a 5K guest window, a picture at
  native size. The failure mode that claim would exercise — the encoder
  refusing the size — is the one this ADR added a fallback for, and that
  fallback has not been seen firing on real hardware either.
- 8K stays out of reach until both an encoder that negotiates it and a memory
  path that fits it exist. ADR 0073's zero-copy is half of the second one; the
  first has no candidate in this workspace.
