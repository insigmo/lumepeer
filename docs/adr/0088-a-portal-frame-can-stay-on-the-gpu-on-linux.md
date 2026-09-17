# ADR 0088 — A portal frame can stay on the GPU on Linux

Status: accepted
Date: 2026-09-17

The Linux half of backlog item 33, whose Windows half is
[ADR 0073](0073-a-captured-frame-can-stay-on-the-gpu-on-windows.md).
Amends [ADR 0040](0040-the-vaapi-encoder-and-what-a-probe-is-allowed-to-claim.md),
whose encoder had never run on hardware, and leaves
[ADR 0039](0039-the-linux-client-ships-both-session-types-and-its-audio.md)'s
two session types as they were: the X11 path is not touched.

## Context

On a Wayland session the picture comes from the ScreenCast portal as a
PipeWire stream. Until now `capture::pipewire_stream` asked for exactly one
format, `BGRx` with no modifier, which PipeWire can only satisfy with shared
memory: the compositor renders on the GPU, copies the picture into a memfd,
and this side packs it into a `Vec<u8>`, hashes it, converts it to NV12 on the
CPU and uploads it into a VA-API surface. Three trips across main memory for a
picture that started and ends on the same GPU.

Before any of that could be measured, the VA-API encoder had to work at all.
On the first machine it met (Intel Tiger Lake, `iHD` driver) it did not:

- The driver offers H.264 encode only through `VAEntrypointEncSliceLP`; the
  encoder asked for `VAEntrypointEncSlice` and refused to open.
- Once open, it produced nothing a decoder could read. The driver writes no
  SPS or PPS; `last_picture` was set on every picture, which makes the driver
  end the sequence after each one; the reference and the reconstruction were
  one surface.
- The driver offers only constant QP, so every bitrate was ignored.
- A picture that is not a whole number of macroblocks failed to upload.

## Decision

### The encoder, as it actually runs

- The low-power entrypoint is used when the full one is absent.
- The encoder writes the SPS (with a VUI naming the colour matrix, sRGB
  transfer and no reordering) and PPS itself, from the same values it gives
  the driver, and puts them in front of any IDR that came back without them.
  `cros-libva` 0.0.13 has no packed-header buffer to hand them to the driver.
- `last_picture` is never set. Two reconstruction surfaces alternate.
- The rate control mode is the first of CBR, VBR and CQP the driver offers.
  Under CQP a controller moves the quantizer between 18 and 44 from the
  smoothed size of the frames it gets back, with a 15% dead band
  (`VAAPI_CQP_*` in `constants.rs`).
- The upload fills the margin up to whole macroblocks by repeating the edge;
  the SPS crops it off.
- The probe (ADR 0040's rehearsal) now encodes a 64x64 frame and wants an IDR
  back, as the VideoToolbox one does. An opened context proved nothing here.

### The zero-copy path

Behind a new feature, `encode-vaapi-zero-copy`
(`encode-vaapi` + `capture-portal` + `libc`), which is in no release build,
for the same reason `encode-vaapi` is in none: a `.deb` has to run on a
machine without libva.

- **Negotiation.** With the feature, the stream offers two `EnumFormat`s:
  first `BGRx`/`BGRA` with a `modifier` property marked
  `MANDATORY | DONT_FIXATE`, then the old shared-memory format. When the
  negotiated format carries a modifier, `param_changed` answers with a
  `Buffers` param whose data type is `SPA_DATA_DmaBuf`; otherwise with
  `MemFd | MemPtr`. This is PipeWire's documented DMA-BUF procedure; a
  compositor that does not offer DMA-BUF simply lands on the second format.
- **The frame.** `capture::Frame` grows `dmabuf: Option<Arc<DmaBuf>>`, which
  exists only on a Linux build with the feature, as `gpu` does on Windows.
  `DmaBuf` owns a duplicate of the descriptor plus format, modifier, offset,
  stride and size. The PipeWire buffer stays dequeued while any frame holds
  it; dropping the last holder sends a token back to the stream's loop
  thread, which requeues the buffer. Tokens carry a generation, so a buffer
  that was removed and whose address was reused is never queued by a stale
  token.
- **The buffer's size comes from the kernel** (`lseek(fd, 0, SEEK_END)`), not
  from `spa_data.maxsize` or the chunk's `size`: `xdg-desktop-portal-wlr`
  0.7.1 was seen sending `0` and `9` for a 1280x720 picture. Chunks flagged
  `CORRUPTED`, which the same portal sends when a copy fails, are given back
  without becoming a frame.
- **Import.** The encoder imports the buffer as a `VA_RT_FORMAT_RGB32`
  surface (`VA_FOURCC_BGRX`/`BGRA`) through `VASurfaceAttribExternalBuffers`
  with memory type `DRM_PRIME_2`, and encodes it directly. The driver does the
  RGB to YUV conversion, and measured on `iHD` it uses **BT.709**, where
  `encode::nv12` uses BT.601. The SPS names the matrix of the pictures it
  covers, so a change between the two input kinds starts a new IDR.
- **Same GPU, or nothing.** The kernel names each DMA-BUF's exporter in
  `/proc/self/fdinfo` (`exp_name`), and sysfs names each render node's
  driver. The encoder opens a node of the exporter's driver, reopening on
  one if its current node is another GPU's. If no such node encodes, the
  frame is not imported through main memory in disguise: the encoder marks
  the session `gpu_refused`, logs once, and every later frame is read back
  and uploaded as before. Nothing is shown to the user.
- **Everything else still works.** `Frame::as_cpu()` reads a DMA-BUF frame
  back (an mmap bracketed by `DMA_BUF_IOCTL_SYNC`, linear modifier only) and
  remembers the result, so scaling, the software encoder and the tests need
  no change. A frame that has to be scaled is read back, as on Windows.

## Consequences

Measured on this machine (Intel Tiger Lake, `iHD`, release build, 600 frames
of 1920x1080) by
`encode::linux_vaapi::tests::cpu_per_frame_of_each_encode_path`:

| | CPU per frame |
|---|---|
| VA-API from a DMA-BUF | 0.33 ms |
| VA-API from main memory (NV12 conversion and upload) | 4.82 ms |
| openh264 | 56.22 ms |

The main-memory row starts from pixels already in main memory, so the
compositor's copy into the memfd, which the DMA-BUF path also removes, is not
in it. The measurement is an `#[ignore]`d test, because the answer belongs to
the machine.

What was checked on hardware:

- A stream from each input kind decodes back with openh264 above 35 dB luma
  PSNR, crop included, against BT.601 for uploads and BT.709 for imports.
- A real portal stream (headless sway 1.10, `xdg-desktop-portal-wlr` 0.7.1,
  PipeWire 1.4) delivers DMA-BUF frames when offered, the encoder imports
  them without a readback, and none arrive when not offered
  (`a_portal_stream_delivers_dmabuf_only_when_it_is_offered`, opt-in through
  `LUMEPEER_TEST_PORTAL_SCREENCAST=1`).

Found on the way: that compositor offers its shared memory only as `RGBx`, so
the fixed `BGRx` request of §11 gets no frames from it at all without this
feature. Taking `RGBx` is a change to §11's non-goals and is not made here.

**Duplicate-frame suppression** is given up on this path, as on Windows:
hashing the picture means reading it back. The portal already sends frames
only on damage, which is a weaker filter than the hash.

## Still open

- A session with a guest on it, and the before/after CPU of the whole host
  process during one, has not been measured.
- Only `iHD` has been tried. AMD's `radeonsi` exports and imports DMA-BUFs
  the same way, but its conversion matrix is unmeasured; if it is not
  BT.709 the SPS will name the wrong one.
- GNOME and KDE portals usually negotiate non-linear modifiers. Import does
  not care, but `as_cpu()` refuses those buffers: on such a session a frame
  that should be scaled goes out unscaled, and a session whose encoder
  refused the GPU path has no readback to fall back to, so its frames fail.
  Offering only the linear modifier, or reading back through the GPU, is the
  fix; neither is made here, because neither has a compositor to be tried on.
