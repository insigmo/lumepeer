# ADR 0060 — The guest names the picture size it will draw

Status: accepted
Date: 2026-09-07

Amends ADR 0018 (the host-side downscale) and D7 / `docs/bugs/
13-stream-resolution.md` (the guest's manual scale ceiling). Follows ADR 0058,
which is what makes the ceiling it removes obsolete.

## Context

A guest watching a 1440p host saw a picture in which no glyph had a hard
edge. The first suspicion was bitrate, and it was wrong: the wallpaper
gradient behind the icons was clean, with no banding and no blocking. A
starved H.264 stream loses a large smooth gradient first, not last. Nothing
about the picture was a compression artefact. It had simply been resampled —
twice.

**The host reduced every picture to 1920x1080 before encoding it.**
`MAX_PICTURE_PIXELS` was introduced by ADR 0018 to bound one thing: the RGBA
picture the sandboxed decoder worker of §11.3 returns through a shared-memory
slot. One 1080p RGBA8 frame is 8 MiB, which is exactly `SLOT_PAYLOAD_BYTES`,
and the two are asserted against each other at compile time. That reasoning
was sound and it is still sound — for a guest that decodes through that
worker.

Since ADR 0058 the ordinary guest does not. The bitstream crosses to the
webview and `VideoDecoder` turns it into a `VideoFrame` that never exists as
RGBA outside the renderer's own decoder. The slot is not on the path, and the
ceiling sized for the slot was reducing a 2560x1440 desktop by a factor of
1.33 — a non-integer box filter over every glyph on the screen — to fit a
buffer nothing was going to use.

**Then the guest's canvas resampled what arrived, a second time.** The canvas
backing store is the frame's own size and its CSS size is whatever the window
is, so a 1080p frame drawn into a 1440p window is a bilinear stretch, and a
1080p frame drawn into a smaller window is a bilinear shrink. Either way a
second filter ran over a picture that had already been through one.

**And nothing on the wire could have prevented it.** The only resolution
control a guest had was `StreamScaleRequest { scale_percent }` — a percentage
of *the host's own captured size*. The host is the only side that knows its
screen; the guest is the only side that knows its window. A percentage is
computed from one and applied to the other, so it agrees with the number that
matters only by coincidence. Every disagreement is a resample.

## Decisions

**The guest names the size, in its own device pixels.** A new minor-10
message, `StreamSizeRequest { width, height }`, gated on `FEATURE_STREAM_SIZE`
with the same shape `FEATURE_STREAM_SCALE` uses — a guest-to-host message,
advertised by the guest, since `HelloAck` carries no feature list to read the
other way. The host fits its captured frame inside that box, preserving its
own aspect ratio.

Device pixels, not CSS pixels and not a percentage of anything, because that
is the only number for which "one frame pixel lands on one device pixel" is a
statement that can be true.

**Never enlarged.** A guest asking for a box bigger than the host's screen
gets the host's own size. Upscaling on the host would invent detail and then
spend bitrate transmitting it, and the guest's canvas can stretch a sharp
picture for free. This is also what makes "ask for everything" expressible: a
zoomed-in window, or one larger than the host's screen, asks for the ceiling
and gets whatever the host actually has.

**Sending the message is itself the claim that this guest decodes for
itself.** That is why it lifts `MAX_PICTURE_PIXELS` in favour of the new
`MAX_STREAM_PIXELS` (4K). A guest on the fallback decoder is still bound by
the slot, and keeps asking for no more than the smaller ceiling — the view
window starts at that ceiling and raises it only once
`nativeDecodingAvailable()` has answered. A guest that never sends the
message at all keeps ADR 0018's behaviour exactly, which is the behaviour its
decoder still needs.

**The request is debounced, not rate-limited.** Each distinct size the host
accepts costs an encoder type renegotiation and the keyframe after it — the
most expensive frame in the stream. A window dragged across the screen emits
a `resize` per animation frame, and every size on the way is one nobody will
look at. 250 ms of stillness, then one request for where it landed.

**The requested size deliberately cannot see the current frame size.** The
view window reacts to a frame arriving at a new size by re-running its
layout, and the layout is also where the request is made from. If
`streamSizeFor` read `frameSize`, the two would chase each other. It reads
the layout mode, the viewport and the device pixel ratio, and nothing else.

**A ceiling, not a target.** ABR (ADR 0037) still owns the three knobs and
stays free to sit below the box. The two compose rather than compete: this
bounds the pixels, that scales what is left.

**The bitrate ladder moved with it.** `ENCODE_DEFAULT_BITRATE_KBPS` 4 Mbit ->
8 Mbit and `ABR_MAX_BITRATE_KBPS` 12 Mbit -> 25 Mbit. This is not a separate
improvement, it is what keeps this change honest: the same figure now covers
a 1440p or 4K picture rather than a downscaled 1080p one, and recovery climbs
5% per adjustment, so a 1440p session starting where a 1080p one used to
start meant most of a minute of soft picture before the ladder caught up. The
rate control of ADR 0059 is variable, so the target is a ceiling the encoder
does not have to spend: a still desktop reaches a small fraction of it, and
only a moving one costs what the number says. `MAX_MEDIA_FRAME_BYTES` goes
4 MiB -> 8 MiB for the same reason, still at or under `SLOT_PAYLOAD_BYTES`;
a 4K intra frame is the one frame in the stream that can genuinely approach
a megabyte or three, and dropping it hides the picture entirely until the
next one, because everything after it references it.

## Consequences

A 1440p host watched full-screen from a 1440p guest now encodes 2560x1440 and
draws it at scale 1.0, where it previously encoded 1920x1080 and stretched it
by 1.33. Two filters leave the pipeline; `imageRenderingFor` answers
`pixelated` at exactly 1:1, where nothing is interpolated at all.

It is also less work, not more, on the two paths that matter. The host's box
downscale — a full pass over several megabytes per frame — does not run when
the guest's box is at or above the captured size. And a guest in a window
*smaller* than the host's screen now has the host encode fewer pixels rather
than encoding all of them and shrinking the result in the canvas.

The cost is a keyframe and an encoder renegotiation per settled window
resize, which is new: a resize used to change nothing about the stream.

What this does **not** address is the quality of a picture that is not
moving. A still desktop stays at whatever quality the rate controller gave
the last intra frame, because P-frames over an unchanged screen cost almost
nothing and improve nothing. Spending an idle link on progressively
refreshing a static picture is the obvious next move and is left to its own
change — it is a rate-control question, not a resolution one, and it needs
measurement on real hardware rather than reasoning.
