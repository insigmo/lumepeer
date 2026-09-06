# ADR 0058 — The guest decodes the bitstream in its own webview

Status: accepted
Date: 2026-09-06

Supersedes the transport half of §11.3 (the sandboxed decoder worker) for
webviews that can decode H.264 themselves; the worker stays as the fallback.
Answers a host report that the picture was a second behind and looked like
mud.

## Context

The guest's picture went through this: `rd/media/1` → decode in the sandboxed
worker process of §11.3 → RGBA8 in a shared-memory slot → a `watch` channel →
`view_next_frame` IPC → `putImageData` on a canvas.

Three things were wrong with the last two steps, and together they were most
of the second.

**The pixels crossed the IPC boundary.** `MAX_PICTURE_PIXELS` is 1920×1080,
so one picture is 8 294 400 bytes of RGBA. Moving that through `WebView2`'s IPC
is not free the way it is on macOS — the community's measurements put ~10 MB
at ~200 ms on Windows against ~5 ms on macOS — so a frame cost on the order of
150 ms *before anything was drawn*. The same picture as an H.264 frame at
4 Mbit/s is roughly 16 KB. Not a constant factor: three orders of magnitude.

**The window polled, serially.** `view.ts` ran `tick()` on
`requestAnimationFrame` and only scheduled the next one in `.finally()`, so the
effective frame rate was `1 / round-trip` — about six per second — and every
frame that did arrive was already a round trip old.

**It composited on the CPU.** `putImageData` writes into the canvas backing
store from JavaScript; nothing about it can stay on the GPU.

The resolution ceiling and the `SLOT_PAYLOAD_BYTES` it is asserted against
exist only to bound *that* RGBA buffer. So does the memory the worker's
shared-memory ring holds. All of it is downstream of the choice to move
pixels rather than bitstream.

## Decision

**The view window decodes the host's H.264 itself, with `VideoDecoder`, and
the IPC carries the bitstream.**

- A new command, `view_next_chunk`, hands the window encoded frames:
  `status:u8 | flags:u8 | count:u16 | reserved:u32`, then `count` frames of
  `keyframe:u8 | timestamp_us:u64 | length:u32 | bitstream`.
- It is a **long poll**, not a poll: the call does not answer until a frame
  exists, and it returns with everything that has arrived since the previous
  call. The round trip leaves the per-frame budget entirely. An empty answer
  comes back at most every 250 ms so `status`, the live `input` grant and the
  host's recording statement keep reaching the window on a still screen —
  those three ride every answer exactly as they rode every frame before.
- A **queue**, not the single slot the RGBA path uses. That is forced by the
  codec, not a preference: an RGBA picture is independent, so keeping only the
  newest is right, while an inter frame is meaningless without the frames it
  references. Dropping one silently corrupts everything after it.
- The queue is bounded (64 frames or 8 MiB). Overflow clears it and raises a
  **desync** flag; the window then throws its decoder away and asks for an
  intra frame. Losing frames loudly is the only correct behaviour a bitstream
  transport has that a pixel transport does not need.
- The window is configured from the stream's **own SPS**, parsed out of the
  first keyframe, rather than from an assumed profile — the host asks its
  hardware encoder for High and takes what the driver gives (ADR 0059).
- Painting is `drawImage(VideoFrame)` on a `desynchronized` 2D context, from
  the decoder's output callback.

**Which side decodes is the window's choice, and it makes it by which command
it calls first.** `view_next_chunk` means "I decode"; `view_next_frame` means
"you decode". Until one of them is called, the media receiver queues and
decodes nothing — so the worker process is never started for a session that
turns out not to need it (§8.1: nothing runs that the session does not
require).

## Consequences

**The fallback stays, whole.** `WebView2` is Chromium and has `VideoDecoder`;
WebKitGTK and WKWebView may or may not, depending on the version on the
machine. The window probes with `VideoDecoder.isConfigSupported` before its
first call and takes the RGBA path unchanged if the answer is no. A window
whose own decoder fails repeatedly can fall back at any point by simply going
back to `view_next_frame` — the choice is a cell the newest caller wins, not a
setting fixed at startup.

**The trust boundary does not move.** §11.3 put decoding in a sandbox because
a video bitstream is untrusted input from the network and decoders are where
that goes wrong. It still is, and it still is: Chromium's renderer sandbox
instead of the worker process, and the same platform hardware decoder
underneath. The bitstream reaching JavaScript changes nothing about who parses
it — `VideoDecoder` is the browser's own decoder, not one written here.

**On this path, three things stop existing:** the worker process, its
shared-memory ring, and the NV12→RGBA conversion inside it. The `wire` →
`window` hop is bitstream the whole way.

**`MAX_PICTURE_PIXELS` is not raised here.** It still bounds the fallback's
shared-memory slot, which is asserted against it at compile time, and raising
both would double a mapping that is already 64 MiB against a 150 MiB
active-session budget. Lifting the ceiling for hosts with screens larger than
1080p needs the guest to tell the host what it can take — a `Hello` feature
bit — and is left to its own change.

**What was measured.** Against a chunk response produced by this machine's own
Media Foundation encoder (40 frames, 1280×720, 167 KB total): the stream
describes itself as `avc1.64001f` (High profile, level 3.1), all 40 frames are
handed to the decoder in 4.9 ms, and the canvas comes back pixel-accurate —
one-pixel-tall white rules resolved cleanly against a 0x20 background, which
is exactly the content a blurry decode destroys.
