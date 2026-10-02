# ADR 0139 — The picture waits for the link instead of queueing in front of it, a still screen is watched without a gap, and a smaller picture is made on the GPU

Status: accepted
Date: 2026-10-01

The owner asked how to make the remote screen respond faster, and then:
"Quality must not get worse, but do everything to make this as fast as
possible." Read against the code, three things stood between a click on the
guest and the picture of its result. Each is fixed here without touching
bitrate, picture size or the presets.

## Context

**Frames queued in front of the link.** `MediaFrameWriter::write_frame`
returns as soon as QUIC has taken the bytes into its send buffer. noq's
defaults let that buffer hold up to the 1.25 MB stream window (10 MB across
the connection) of frames that have not been sent yet. The encode loop's
one-slot channel (ADR 0059) found out that the link was busy only once that
buffer was full. A named preset pins 8 Mbit/s (ADR 0064), and the guest always
names one, so on a link slower than that nothing adapts. While the screen
moved, the guest was shown pictures that had waited in the buffer for
seconds, and a menu opened late by the same amount. Measured in-process
(`an_acknowledging_guest_is_shown_fresh_pictures_on_a_slow_link`): with a
guest that takes one frame every 50 ms, the picture it was shown was 1.37 s
old after 40 frames.

**A still screen was watched with a gap.** On Windows, `AcquireNextFrame`
waits up to 16 ms for the compositor to present something. When nothing came,
the loop then slept out the rest of the frame interval. At the `quality`
preset's 30 fps that left 17 ms of every 33 in which a change sat unseen. That
is exactly the case of a click after a pause, the one where a person notices
latency most.

**A smaller picture was made on the CPU.** When the guest's window is smaller
than the host's screen, or a preset reduces the picture, `scale::fit_within`
read the frame back from the GPU (ADR 0073 kept it there for the encoder) and
box-filtered it on one CPU thread. Measured on the reference machine (Intel
UHD), readback plus filter:

| reduction | CPU |
|---|---|
| 1920x1080 → 1280x720 | 18 ms |
| 2560x1440 → 1920x1080 | 40 ms |
| 3840x2160 → 1920x1080 | 52–59 ms |
| 3840x2160 → 1600x900 | 53 ms |

That is three to four times the hardware encoder's own 13 ms at 4K, on every
frame.

## Decision

1. **The guest acknowledges every frame, and the host produces a frame only
   while the link keeps up** (`FrameAcks`). As each frame arrives, before
   anything else is done with it, the guest writes how many frames it has
   received so far on a stream of its own on the media connection. That stream
   is tagged `K` (`STREAM_ACKS`), and each entry is a `u64`, little endian
   (`encode_frame_ack`). The host times each acknowledgement against the
   moment it handed that frame to the writer. It keeps the quickest of the last
   `MEDIA_ACK_BEST_WINDOW_MS` (5 s) as the link's best case: its round trip
   with nothing queued. Before capturing, the loop checks the oldest frame not
   yet acknowledged. If it has been on its way longer than that best case plus
   a slack, the loop waits for the next acknowledgement and captures what is
   on screen then. The slack is two frame intervals, held between
   `MEDIA_QUEUE_SLACK_MIN_MS` (16) and `MEDIA_QUEUE_SLACK_MAX_MS` (50): a frame
   big enough to take a while to send is not a link falling behind. The same
   in-process measurement shows a picture 91–183 ms old instead of 1.37 s.

   Nothing about the picture changes. Bitrate, scale and the encoder are
   exactly what the preset pinned. A link that cannot carry the picture now
   shows fewer frames, each of them current, instead of every frame late.

   There is no minor bump, and none is needed in either direction:
   - A host that predates this has a microphone pass that accepts guest
     streams looking for `M`. It reads the `K` tag, skips the stream and
     drops it, and the guest's next write ends its acknowledgement task.
   - A guest that predates this opens no such stream. The host's
     `FrameAcks` stays inert: it keeps nothing and holds nothing back.

   On the host, every stream the guest opens now goes through one acceptor,
   `spawn_guest_streams`, which serves each by its tag. The microphone's pass
   used to be that acceptor, and it dropped every stream that was not its own.

2. **A capture backend that waits for a change is asked again at once**
   (`ScreenCapturer::waits_for_change`). The loop's pace is now a frame
   interval after the last picture it captured, not a clock that started when
   the screen was last looked at. When Desktop Duplication answers "nothing
   changed" after its own wait, the loop goes straight back into
   `AcquireNextFrame`. The other backends answer at once rather than waiting,
   so they say `false` (the default) and keep their sleep. For X11 that is
   also the only thing that stops its full-screen `GetImage` from running
   back to back.

3. **A frame that is on the GPU is reduced on the GPU** (`GpuScaler`). It is
   a pixel shader, compiled once per capture device with `d3dcompiler_47`,
   that is `scale::box_downscale` step for step: the same source rectangles,
   whole bytes summed, and the same truncating division. The test
   `a_gpu_frame_is_reduced_on_the_gpu_into_the_cpu_filters_very_picture`
   requires the result to be byte-identical to the CPU filter's on all four
   ratios above, on a picture of one-pixel rules, text-like strokes, a
   gradient and noise. Submitting the pass takes 5–12 µs of CPU. What comes
   out is again a `GpuTexture`, so the encoder's zero-copy path takes it as it
   is. A host with a software encoder reads back the reduced picture instead
   of the full one: 6–12 ms in total where it was 18–59. `GpuTexture::pixels`
   now keeps its staging texture, so that path no longer allocates one per
   frame either.

   The Direct3D 11 video processor can scale too, and was tried first. Its
   filter is the driver's choice, and the one measured here sharpened: a white
   page next to text came out brighter than white. Against the exact area
   average it scored 22.2 dB where the box filter scores 58.8 dB. The quality
   bar was "no worse", so it was not used.

## Consequences

- The host's `media: the last period of the encode loop` line gains `held`,
  the ticks spent waiting for acknowledgements, and `link_best_ms`, the
  link's best case. Read together with the guest's statistics overlay
  (Ctrl+Alt+Shift+S), `queue delay` should now stay near zero on a slow link
  while `held` rises.
- A session moved onto a slower path (ADR 0134) is held to the old path's
  round trip until its best case leaves the 5 s window, and shows fewer frames
  until then.
- If the guest stops acknowledging but keeps the stream open, the picture
  stops. That guest could not have shown the frames anyway, and the QUIC idle
  timeout ends a peer that is truly gone.
- A guest's microphone stream that ends and is opened again on the same
  connection is now played again; the old pass played only the first one.
- Not changed, and why:
  - The input path, which crosses each side's actor and the shared control
    stream: nothing measured says it is slow.
  - The congestion controller: the acknowledgements already keep the queue
    near the link's own round trip.
  - The default preset: 60 fps at the same pinned bitrate would halve each
    frame's bits.
  - The capture wait on macOS and Wayland: both hand frames over through a
    slot that answers at once, and making it wait is its own change.
- Not yet run on a real pair over the internet.
