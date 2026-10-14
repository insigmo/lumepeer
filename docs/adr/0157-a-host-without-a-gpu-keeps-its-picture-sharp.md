# ADR 0157 — A host without a GPU keeps its picture sharp

Status: accepted
Date: 2026-10-14

Builds on [ADR 0144](0144-the-host-keeps-up-with-its-encoder-and-its-link.md)
(the encoder-speed controller that reduces the picture),
[ADR 0139](0139-the-picture-waits-for-the-link-not-the-link-for-the-picture.md)
(GPU reduction, held to the CPU filter's bytes) and
[ADR 0141](0141-a-host-with-no-hardware-encoder-encodes-av1-in-software.md)
(the nasm check of libaom's assembly).

## Context

Reported 2026-10-14: connected to the Mac or to Linux, the picture is poor;
connected to Windows it is excellent. The Mac also lags, Linux less so, and
neither changes smoothly. Both hosts are virtual machines on `beta` with no
GPU: the Mac (`macos_new`, 4 vCPU, 8 GB, 1718x920) and `debian` (2 vCPU,
2 GB, 1280x800, GNOME on Wayland). The Windows host is `beta` itself, whose
AMD hardware encoder takes 7-11 ms a 1080p frame.

The hosts' own logs (`media: the last period of the encode loop`) said where
the picture went:

- **Linux.** `SoftwareOpenH264`, 110 ms a 1280x800 frame. Two seconds of that
  and ADR 0144's controller cut the picture to 85%, 72%, 61%, 51%, 50%:
  640x400 at 7-13 fps, sent to a guest window of 1280x800. The reduction
  itself cost 25-50 ms a frame on the CPU.
- **Mac.** `Hardware` by the log's name, but VideoToolbox's only H.264
  encoder in the VM is `com.apple.videotoolbox.videoencoder.h264`, Apple's
  software one (a session there answers `EncoderID` with it and does not
  support `UsingHardwareAcceleratedVideoEncoder`): 21-25 ms a frame at
  1718x920 with the BGRA-to-NV12 conversion included. The screen was
  mostly still, one to three frames a second, and the controller's mean of
  those was whichever one was slow: a 250 ms frame after a still minute took
  the picture to 72%, 60%, 50%. Every reduced frame then spent 20-30 ms in the
  CPU filter, more than the reduction saved the encoder.

Three faults under those numbers:

1. **openh264 shipped without its assembly on Linux.** openh264-sys2 assembles
   its SIMD when nasm runs and builds C alone when it does not, and tells
   cargo nothing that would make it decide again. nasm reached the Linux
   release runner on 2026-10-02 (ADR 0141), after `rust-cache` had saved an
   openh264-sys2 built without it, and every Linux release since reused that
   build. Searched for the machine code of openh264's nasm objects, v0.0.144's
   `/usr/bin/lumepeer-desktop` holds 0 of 87 runs; a WSL build of the same
   source holds 87 of 87. On `debian` the same test binary encodes a
   scrolling 1280x800 screen in 14 ms a frame with the assembly.
2. **openh264 ran on one thread whatever it was told.** The `openh264` crate
   initialises the encoder with a single slice, and openh264 runs
   `min(threads, slices)` threads (`InitSliceSettings`), so
   `ENCODE_MAX_SOFTWARE_THREADS` changed nothing.
3. **The speed controller read sparse frames as a slow encoder, and reduced
   with a slow filter.** The per-pixel box filter took 63 ms for 1718x920 to
   1460x782 on the reference machine.

And one cost of the Mac's own making: ScreenCaptureKit was pinned to BGRA,
which the encoder converted to NV12 on one core, after the capture had copied
and hashed the 4-byte picture.

## Decision

1. **openh264 is rebuilt against the runner's nasm on every release**
   (`cargo clean -p openh264-sys2` after the cache is restored), and the
   release checks that its assembly reached the binary on the two x86-64 rows,
   as it does for libaom's: `ci/find-openh264-asm.py` takes relocation-free
   runs of `satd_sad`, `mc_luma`, `quant`, `dct` and `vaa` from the nasm
   objects (ELF or COFF) and requires every one in `lumepeer-desktop`.
2. **openh264 cuts every picture into one slice per thread.** On the first
   picture of a size a blank picture initialises the encoder, then
   `SetOption(ENCODER_OPTION_SVC_ENCODE_PARAM_EXT)` asks for
   `SM_FIXEDSLCNUM_SLICE` and as many slices as threads; the new thread count
   resets the encoder, so the real picture is the one keyframe a new size
   starts with anyway. openh264 itself keeps a picture too small for slices in
   one. Measured on a scrolling 1280x800 screen: 6.3 ms a frame on one slice,
   4.3 on four (i7-11800H); 14.3 on one, 10.5 on two (`debian`).
3. **The speed controller counts a window as slow only when the encoder was
   busy for most of it** — at least 60% of the window, besides a mean over
   the interval. A still desktop's frame or three a second no longer reduce
   the picture however slow one of them was.
4. **The box filter is written for speed and kept to its bytes.** Spans are
   worked out once a frame, the division is an exact multiplication by
   `ceil(2^32/count)` for boxes of up to 257 samples (sums under 2^16), and a
   picture is split across up to four threads. Every output byte is the one
   the per-pixel loop made, which the GPU reduction of ADR 0139 is also held
   to; the old loop stays in the tests as the reference. 1718x920 to 1460x782:
   2.2 ms against 63 ms; 1280x800 to 640x400 on `debian`: 2.3 ms against 8.9.
   NV12 is reduced plane by plane by the same filter.
5. **A Mac captures NV12 where its window server will make it.** The picture
   stream asks ScreenCaptureKit for `420v` with the BT.601 matrix, the
   conversion the encoder used to do, so colours stay as they were; the
   window server converts, and the capture copies and hashes 1.5 bytes a
   pixel instead of 4. VideoToolbox takes the NV12 as it is, `openh264` pulls
   its chroma apart into I420 for a guest that picks it (ADR 0143), and the
   cursor of a guest that does not draw its own is blended into the NV12
   picture in BT.601's video range. The Mac VM, with no Metal, refuses to
   start such a stream (`Start stream failed (1003)`, while a BGRA stream of
   the same display starts), so a stream that will not start in NV12 is
   started again in BGRA, and the process asks for BGRA from then on. The
   audio stream keeps BGRA; nothing reads its pictures.
6. **The BGRA-to-NV12 conversion is split across the cores**, row blocks to a
   thread, for the hardware encoders that still take BGRA: the Mac VM, and
   Media Foundation and VA-API without their zero-copy paths. Byte for byte
   the one-thread result.

Not done: VideoToolbox's `PrioritizeEncodingSpeedOverQuality`. Apple's
software encoder answers it with `kVTPropertyNotSupportedErr` (-12900), and
timed on the VM it changes nothing: 14 ms median, 16 ms mean a 1718x920 frame
of scrolling text either way.

## Measured live

win → Mac VM, the e2e build (picture path optimised), a page of text
scrolling without pause on the Mac for a minute:

| | picture | frames a second | encode (scale included) |
| --- | --- | --- | --- |
| v0.0.144, the owner's sessions of 2026-10-13 | 1718x920 cut to 1460x782, 1236x662, 858x460 | 2-25 | 21-25 ms, plus 20-30 ms to reduce |
| this ADR | 1718x920 throughout, `speed_cap` 100 | 18-21 of 30 | 24-26 ms |

The frame rate is what the VM's four cores give the encoder beside a browser
scrolling in software; the picture is no longer what pays for it. `still`
(e2e 3d) is unchanged at 206/232/261 ms min/p50/max against 199/229/258 before:
a single change on a still screen was never reduced, and its time is the
VM's — the guest acknowledges the Mac's frames 30-50 ms later than the
network's round trip, where a Windows host's arrive at it.

## Consequences

- A Linux host with no hardware encoder encodes its screen several times
  faster from the first release built this way, and the picture stays at the
  screen's size.
- A host whose encoder really cannot keep up — busy for most of every second
  with frames over the interval — is still reduced, and a reduction now costs
  a few milliseconds instead of most of a frame.
- A sliced picture has the deblocking filter off across slice boundaries
  (openh264 does that for threading); on screen content that is not visible.
- The audit job's new RustSec advisories of 2026-10-14 are unrelated and left
  for their own change.
