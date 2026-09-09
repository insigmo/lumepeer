# ADR 0073 — A captured frame can stay on the GPU on Windows

Status: accepted
Date: 2026-09-09

Extends [ADR 0012](0012-windows-screen-capture.md), which
chose DXGI Desktop Duplication, and
[ADR 0011](0011-windows-hardware-encoder.md), which chose the
Media Foundation encoder MFT. Both of them, until now, met in main memory.
Follows [ADR 0059](0059-the-host-encodes-for-latency-and-measures-congestion-honestly.md), whose
frame budget is what this shortens.

## Context

The two halves of the Windows host pipeline are already on the GPU. Desktop
Duplication hands back an `ID3D11Texture2D`; a hardware encoder MFT is a piece
of the same GPU. Between them, every frame took this route:

1. `CopyResource` into a `D3D11_USAGE_STAGING` texture,
2. `Map`, and a row-by-row repack into an owned `Vec<u8>` — 8 MiB at 1080p,
   33 MiB at 4K,
3. `blake3` over all of it, to answer §11.1's "identical to the previous one",
4. a CPU BGRA→NV12 conversion into a second owned buffer,
5. `MFCreateMemoryBuffer` and a copy into *that*.

Steps 2, 4 and 5 are the picture crossing the bus and main memory for no
reason other than that the two APIs were never introduced. Measured here, in
release, at 1920x1080 on this machine's integrated GPU, that route costs
**5.6-5.9 ms of CPU per frame**. A 60 fps frame is 16.7 ms.

## Decision

**A captured frame may carry a GPU texture instead of bytes, and the Media
Foundation encoder may take it that way.** Behind a feature,
`encode-mf-zero-copy`, which is in no release build.

- `capture::Frame` grows one field, `gpu: Option<Arc<GpuTexture>>`, and it
  exists **only** on a Windows build with that feature. Every other backend
  and every other platform compiles against exactly the struct they compiled
  against before, and neither `capture::macos` nor `capture::linux_x11` nor
  `capture::pipewire_stream` was touched by this change.
- Everything that works on pixels — `scale`, `encode::nv12`, the software
  encoder, the tests — reads them through `Frame::as_cpu()`, which is a borrow
  of `data` on every ordinary frame and a readback, remembered, on a
  GPU-resident one. Nothing downstream had to learn what a texture is.
- The encoder adopts the **capture** device rather than making one of its own:
  `GpuTexture` carries the `ID3D11Device` it was made on, and the encoder
  wraps that device in the `IMFDXGIDeviceManager` it binds the transform to.
  Two devices would be a copy through main memory wearing a texture's clothes,
  and this makes that failure mode unrepresentable instead of merely checked
  for.
- The BGRA-to-NV12 conversion moves to a Direct3D 11 video processor on the
  same device. No encoder MFT takes BGRA, so without this step there is no
  path at all.
- Sharing one immediate context between the capture thread and the encode
  thread is legal only with `ID3D11Multithread::SetMultithreadProtected` on.
  Capture turns it on at `Active::open` and hands out **no** GPU frame at all
  on a device that refuses it.

### When the path is actually live

Both of these have to hold, and the second one is a real limitation, not a
detail:

- **The guest draws its own cursor** (`FEATURE_CURSOR_SHAPE`, ADR 0028).
  While the host composites the cursor into the picture, that is a CPU pass
  over the frame, and a frame that has to be in main memory for the cursor may
  as well be encoded from there.
- **Nothing is being downscaled.** `scale::scale_to_percent`,
  `fit_within` and `fit_within_budget` (ADR 0018, 0037, 0060) reduce pictures
  on the CPU, and reducing a GPU frame means reading it back — which is the
  cost this exists to remove. So the path is live at native size and gives way
  the moment the adaptive ladder or a guest's size request asks for less.

The batch this came from offered the alternative — scale on the GPU in the
same video processor, which it can do in the same blit. It is not taken here,
and the reason is structural rather than technical: scaling is decided in
`view.rs` and applied by `scale`, one step *before* the encoder, and moving it
onto the video processor means moving the decision into the encoder and giving
`VideoEncoder::encode` a target size. That is a change to the pipeline's
shape, not to this path, and it belongs to whoever takes gap-tasks `12`.

### What is given up

**Duplicate-frame suppression.** §11.1's "return `None` when the frame is
identical to the previous one" is answered on the readback path by hashing the
picture, precisely because neither `LastPresentTime` nor DXGI's dirty-rect
metadata means "the pixels changed" (ADR 0012 measured 13 consecutive
byte-identical presents). Hashing a frame that is still on the GPU means
reading it back. So on this path the only filter left is the compositor's own
`LastPresentTime`, and a screen repainted with identical pixels is encoded
where the readback path would have dropped it.

That is a genuine regression, and it is the reason this is a feature and not a
default: on a desktop that repaints without changing, the frames it lets
through cost encoder time and bitrate that the hash used to save. What it
buys is on the other side of the same trade, measured below.

## Consequences

Measured on this machine (Windows 11, integrated GPU, release build, 120
frames of 1920x1080 through the real hardware H.264 MFT, three runs), by
`encode::windows::tests::zero_copy_costs_less_per_frame`:

| | CPU per frame | Wall per frame | Peak RSS |
|---|---|---|---|
| readback | 5.6-5.9 ms | 8.5-8.8 ms | 171.5 MiB |
| zero-copy | 0.5-1.2 ms | 4.6-4.7 ms | 179.9 MiB |

CPU per frame falls by about **85%**, wall time per frame by about **46%** —
at 60 fps, from roughly a third of a core to a twentieth of one. Resident set
does **not** improve: it is ~8 MiB *higher*, because the video processor, its
NV12 target and the driver's own mappings are resident for the session, while
the readback path's per-frame buffers were transient and reused by the
allocator. Anyone hoping this would reduce memory should read the table again.

The measurement is a test rather than a number in this document alone, because
the answer is a property of the machine: a GPU with a fast readback and a slow
video processor would come out the other way, and the honest response there is
to leave the feature off. It is `#[ignore]`d, since a measurement that fails
the build on a busy machine is a flake.

The fallback is per session and silent to the user: an MFT that is not
`MF_SA_D3D11_AWARE`, a device manager that refuses the device, a video
processor that will not convert — each of them logs once and puts the session
on the readback path, which is the path every build without the feature is on
anyway. Nothing about a session's picture changes when it happens.

## Still open

- Linux (`capture-portal` DMA-BUF into VA-API) is the other half of backlog
  item 33 and is gap-tasks `11`; it was deliberately not started here.
- The path has been measured on one machine, driving a real encoder, but not
  yet across a session with a guest on it. Nothing in the frame's route to the
  wire changes past the encoder, so the expectation is that the host-side
  numbers above are the whole effect — but that is an expectation, not a
  measurement.
- Scaling on the GPU, as above.
