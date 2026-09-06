# ADR 0059 — The host encodes for latency, and measures congestion honestly

Status: accepted
Date: 2026-09-06

Amends ADR 0011 (hardware H.264 through Media Foundation) and ADR 0015
(host-local ABR). The guest half of the same host report is ADR 0058.

## Context

Four separate things on the host side spent latency or quality, and none of
them was visible as a single bug.

**1. The encoder was drained and restarted for every frame.** `encode()`
issued `MFT_MESSAGE_COMMAND_DRAIN`, waited for `METransformDrainComplete`, and
sent `MFT_MESSAGE_NOTIFY_START_OF_STREAM` — a full pipeline flush and a driver
round trip, thirty times a second. It was there for a real reason: the event
loop *discarded* every asynchronous-MFT event it was not currently waiting
for, so a `METransformNeedInput` consumed while collecting output was a credit
the next frame then waited two seconds for. Restarting the stream was the only
thing that made the driver issue a fresh one.

**2. The encoder was never told this is a live stream.** No
`CODECAPI_AVLowLatencyMode`, no rate-control mode, no reference-frame or
B-picture limit, no profile. An encoder left at its defaults is free to hold
one to three pictures back for reordering and lookahead, which is 30–100 ms
that nothing downstream can win back, and to encode Baseline where High costs
the same to decode and gives back CABAC and the 8×8 transform — real sharpness
on exactly the content a desktop is made of.

**3. Every adaptive bitrate change reset the encoder.** `set_bitrate`
renegotiated the output type and called `start_streaming`, which flushes.
Reference frames gone, next picture forced to IDR. The controller may move the
target once a second, so a link that was not perfectly steady got a
keyframe-sized spike and a visible hitch every second — from the mechanism
that exists to smooth exactly that over.

**4. The congestion signal was a stopwatch on `write_frame().await`.**
ADR 0015 needed a host-local stand-in for loss on a stream that cannot lose
bytes, and used "the write took longer than the frame budget". On a reliable
ordered QUIC stream that duration is the congestion window filling, the peer's
receive window, the OS scheduler and the encoder's own variance. Two frame
intervals of it reported **total loss**, which saturates the controller's
multiplicative-decrease branch. One hiccup walked bitrate, frame rate and
resolution toward their floors — 300 kbit/s, 10 fps, half of each axis — and
the picture the user then complained about was mud produced by an adaptation
reacting to nothing.

And the whole tick was serial: capture → scale → encode → **write** → sleep.
The frame budget was the sum of the stages rather than the largest of them,
and a momentarily slow link stalled the *capture* of the next picture, so
delay compounded instead of being absorbed.

## Decisions

**Drive the asynchronous MFT the way MSDN describes it, and stop draining.**
Events are credits and are now *counted* (`EventPump`), not discarded. With
`CODECAPI_AVLowLatencyMode` set the transform emits one picture per picture it
is given, so `encode()` submits and collects with the stream left running. An
encoder that will not do that says so once — by holding a frame back past a
200 ms probe — and is driven with the per-frame drain from then on, which is
the old behaviour unchanged.

**Ask the encoder for a live stream.** Through `ICodecAPI`, all best-effort,
because these properties are optional per MFT and a refusal means "this
encoder keeps its default", never "this session cannot run":
`AVLowLatencyMode`, `AVEncCommonLowLatency`, `AVEncCommonRealTime`;
`LowDelayVBR` falling back to CBR; mean bitrate at the target and a peak 50%
above it; `MaxNumRefFrame = 1` and no B-pictures, so nothing depends on a
picture that has not been sent; CABAC on; a 10-second GOP. Plus
`MF_LOW_LATENCY` on the transform's own attribute store before type
negotiation, and High profile on the output type.

The peak above the mean is the one that is about *quality* rather than
latency. A desktop is still most of the time and interesting exactly when it
is not; constant bitrate spends the same bits on both, which is backwards.

**Move the bitrate with `ICodecAPI::SetValue`,** and renegotiate only if the
driver refuses it.

**Put the write on its own task, one frame deep.** The loop finds out about a
busy link by not being able to reserve the slot — which is both the correct
back-pressure signal and the right moment to skip a *source* frame. Skipping
before capture is the only safe place to skip: an encoded frame cannot simply
be dropped later, because everything after it references it.

**Measure congestion as frames the link would not take.** Over a window of
`ABR_FEEDBACK_INTERVAL_MS` — the same cadence the guest measures its own
arrivals across — the host reports the share of frame slots it had to skip. A
window with nothing in it reports nothing, because an idle screen is not a
congested link. One skipped frame is not a measurement: the controller halves
the bitrate above `HEAVY_LOSS`, so a per-frame reading would turn a scheduling
hiccup into half the picture quality, which is precisely the failure being
removed. While the guest is reporting, the local window is cleared rather than
carried into the next silence.

**Run `encode()` off the runtime.** It is a blocking platform call — a
hardware MFT round trip, or a full software encode where there is none — and
it had no more business on a tokio worker thread than `next_frame` does.

**One pass for BGRA→NV12.** The conversion read the 8 MiB source twice (once
for luma, once for chroma) and copied both planes into a third buffer. It now
walks the picture in 2×2 blocks, reading each pixel once while it is still in
L1 from the luma calculation, and writes straight into the buffer the encoder
is handed.

## Consequences

Measured on this machine's hardware encoder, 1080p, desktop-like content:
p50 10.4 ms per `encode()` call, of which ~4.7 ms is the colour conversion and
the rest is the MFT. The drain removal is not visible in that number on this
particular driver — it was measurably the same before — but it removes a
two-second stall class (the discarded-credit path) and it is what makes the
one-in/one-out contract honest rather than bought with a flush.

The colour conversion is still the largest single host-side cost and is now
memory-bandwidth-shaped rather than allocation-shaped; the way past it is not
a better scalar loop but not doing it on the CPU at all — feeding the encoder
a D3D11 NV12 texture through `MFCreateDXGISurfaceBuffer` and letting the GPU's
video processor do the conversion and the downscale. That also removes the
staging-texture readback in the capturer and the CPU box filter in
`scale.rs`. It is a shared-device change across `capture/windows.rs` and
`encode/windows.rs` and is left to its own change.

`write_congestion_feedback` and its five tests are gone, replaced by `Backlog`
and its six. Nothing about ADR 0015's shape changes: the guest's own report
still wins whenever there is a fresh one, and the local signal still speaks
only for a link nobody is reporting on.
