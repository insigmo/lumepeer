# ADR 0161 — A portal stream offers DMA-BUF only where it is taken

Status: accepted
Date: 2026-10-15

Amends [ADR 0088](0088-a-portal-frame-can-stay-on-the-gpu-on-linux.md)
(a portal frame can stay on the GPU), whose zero-copy feature has been in every
Linux release build since v0.0.117 (`08c9c5f`, 2026-09-30) although that ADR
still says it is in none. Closes what
[ADR 0160](0160-a-wayland-host-remembers-its-screen-grant.md) left open on KDE.

## Context

On the `debian` VM under Plasma 6.3.5 on Wayland (KWin 6.3.6, PipeWire 1.4.2,
VMware SVGA, no VA-API driver), every build with `encode-vaapi-zero-copy`
gave its guest no picture. The portal granted the stream, then
`pipewire_stream.rs` logged `the portal stream negotiated its buffers (ADR
0088) dmabuf=true modifier=0` 3,351 times in two minutes and never delivered
a frame. A build with `capture-portal` alone got its first frame in
0.14–0.30 s. The installed v0.0.152 did the same to a trusted device's guest
admitted at 11:05 UTC. It was still at it half an hour later, with the
PipeWire daemon at 99% CPU, which slowed every other stream on the machine
about fivefold.

What the two sides said, logged on the stream (every `param_changed`, every
buffer) and with `pw-mon` on the graph:

- The stream offers `BGRx`/`BGRA` with modifier `{0}` (linear), marked
  `MANDATORY | DONT_FIXATE`, then shared memory. KWin offers
  `{DRM_FORMAT_MOD_INVALID, 0}`, marked the same way, then shared memory.
- The link lands on modifier `{0}`, still unfixated. KWin test-allocates a
  1920x1080 buffer for `[0]`. GBM honours it, but a driver that cannot report
  modifiers reports the buffer as `DRM_FORMAT_MOD_INVALID`, and KWin's
  allocator records what GBM reports. KWin then fixates that: it announces
  `{INVALID}` (`MANDATORY`, not `DONT_FIXATE`) ahead of its list.
- `{INVALID}` does not meet the stream's `{0}`, so the link falls back to the
  next pair and negotiates the unfixated `{0}` again. KWin's
  `onStreamParamChanged` re-tests whenever its fixated modifier is not in the
  offered list, and drops modifiers from its offer only when an allocation
  *fails*, which this one never does. So it fixates `INVALID` again, about 25
  times a second (every 0.3 s in a debug build), forever. No buffer is ever
  added to the stream. Read in KWin 6.3.6's `screencaststream.cpp` and
  `gbmgraphicsbufferallocator.cpp`.

So the loop is KWin's re-fixation, not the stream's `Buffers` answer:
answering an unchanged format less often would not have stopped it. And on this
machine a DMA-BUF had nothing to go to anyway. With no VA-API encoder,
openh264 encodes, and it could only have read every DMA-BUF frame back through
`Frame::as_cpu()`, a copy the memfd path does on the compositor's side.
GNOME's mutter on the same VM never fixated the linear modifier at all: it
negotiated shared memory on the first format.

## Decision

1. **DMA-BUF is offered only where an encoder imports it.**
   `PipeWireFrameThread::spawn` asks `linux_vaapi::takes_dmabuf()`: the same
   rehearsal `select_encoder` builds the VA-API encoder on (ADR 0040, ADR
   0088: open the encoder, encode a 64x64 frame, want an IDR back), for H.264,
   asked once per process at the first stream. The stream starts before its
   session's encoder exists, and the answer belongs to the machine. Where it
   says no, the stream is the shared-memory stream of a build without the
   feature: one `EnumFormat`, and no `Buffers` param of its own. That is what
   the `capture-portal` build ran on KDE and GNOME. Before, it also answered
   each shared-memory format with `MemFd | MemPtr`, which cost a buffer
   renegotiation for nothing. That answer is now made only to replace an
   earlier request for DMA-BUF buffers.
2. **An offer that never settles is withdrawn.** PipeWire's procedure takes
   two formats before a buffer: the one the producer fixates, then the fixated
   one. A stream that negotiates more than four DMA-BUF formats with no
   DMA-BUF buffer arriving between them replaces its `EnumFormat`s with the
   shared-memory one alone (`pw_stream_update_params`). The next negotiation
   lands on the producer's own shared-memory format. This is logged once as a
   warning: `the compositor never settled on a DMA-BUF this stream can take`.
   This covers a VA-API machine whose compositor fixates a modifier the
   stream did not offer. It loses zero-copy there, not the picture.
3. Not done:
   - Offering `DRM_FORMAT_MOD_INVALID` as well. An implicit modifier may be
     tiled; ADR 0088 reads back only linear buffers, and importing one is a
     guess about the driver.
   - Asking KWin to drop a modifier its allocation answered with another
     modifier. That is KWin's fix to make.

## Consequences

- A KDE host on a GPU whose GBM does not report modifiers (this VM's vmwgfx
  among them) shows its picture again in release builds. Every release since
  v0.0.117 had lost it there.
- A host without a VA-API encoder (most VMs, any machine without libva's
  driver) no longer asks for DMA-BUF it would only have read back.
- A host with one keeps ADR 0088's path unchanged wherever the compositor
  settles. The guard costs it nothing, since it only counts while no buffer
  arrives.
- The first stream of a process pays for one VA-API rehearsal before it
  connects: tens of milliseconds on Intel (ADR 0088), a failed
  `va_openDriver()` here.

## Verified

Unit tests (`pipewire_stream::dmabuf::tests`, every Linux CI row with the
feature):

- PipeWire's procedure keeps the offer however often a stream goes through
  it.
- KWin's loop gives it up at the fifth unsettled format.
- The shared-memory format carries no modifier.

Live on `debian` (Plasma 6.3.5 on Wayland), 2026-10-15, release feature set
(`capture-x11,capture-portal,encode-vaapi-zero-copy,encode-openh264`), the
grant ADR 0160 left in the e2e app's `portal-grant`:

- Before the fix: `a_remembered_grant_brings_the_picture_back_without_a_dialog`
  fails its first guest after 5 s, and the stream's log and `pw-mon` show the
  loop above.
- The e2e build of the app (`e2e/matrix/deploy.sh linux`, the feature set
  above), as a host. `python -m pytest e2e/matrix --only win,linux -k
  "win_to_linux and connect"`: **PASS**, and the picture reached the Windows
  guest. The host logged `offer_dmabuf=false`, one `negotiated its buffers
  … dmabuf=false` where the release had logged thousands, and openh264.
- `a_stream_offered_dmabuf_gets_a_picture_however_the_compositor_answers`
  (opt-in, `LUMEPEER_TEST_PORTAL_DMABUF=1` with the grant file): offered
  DMA-BUF regardless of the encoder, KWin looped as before. The stream gave
  the offer up and got its first frame (1920x1080) **in shared memory**.
  `pw-mon` shows its `EnumFormat` reduced to `BGRx` alone and the node
  running.

All of these ran while the installed v0.0.152 on the same machine was still
in the loop with its own guest and held the PipeWire daemon at 90–110% CPU.
Every PipeWire step took about a second instead of milliseconds, so the times
measured that, not the fix: 8 s to the stream's first format in the e2e
host, 7.9–15.3 s to the forced test's first frame. For the same reason the
restore test above, whose 5 s bound tells a restored grant from a dialog,
still timed out. It is to be run again once that host is stopped.

GNOME, without the owner at the screen (the VM's seat runs Plasma): a
headless gnome-shell 48.7 (`--headless --virtual-monitor 1600x900`, private
session bus) and a cast of its monitor through mutter's own
`org.gnome.Mutter.ScreenCast`, which asks nobody, fed to
`a_screen_cast_node_gives_a_picture_offered_dmabuf_or_not`
(`LUMEPEER_TEST_PIPEWIRE_NODE`). It got a 1600x900 picture both from the
stream as the product spawns it (no offer here) and from one offering
DMA-BUF, which mutter answered with shared memory.

## Not verified

- A VA-API machine on KDE: there is none here (ADR 0088's Tiger Lake is not
  reachable). That KWin allocates and reports a linear buffer there, and the
  offer stays, is read from KWin's source, not run.
- GNOME on a real seat through the portal: the headless shell above shares
  the stream code and mutter's producer, not the GPU or the portal dialog.
- `a_portal_stream_delivers_dmabuf_only_when_it_is_offered` (ADR 0088's
  headless sway) was not run again. Its path changes only for a stream that
  never settles.
