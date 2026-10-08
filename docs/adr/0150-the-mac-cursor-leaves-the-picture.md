# ADR 0150 — The Mac's cursor leaves the picture too

Status: accepted
Date: 2026-10-08

Extends [ADR 0038](0038-the-cursor-leaves-the-picture.md) to macOS hosts, and
closes the gap [ADR 0138](0138-the-cursor-is-drawn-at-the-picture-scale.md)
left open: a cursor that changes over a still screen.

## Context

Reported 2026-10-08 with a photo of a Windows guest's view of a Mac: two
cursors on screen, the guest's own white arrow and the Mac's black one a few
millimetres away from it, and the Mac's one "far, far behind" the guest's.

Both were one defect. ADR 0038 moved the cursor out of the picture on Windows
and X11 and left macOS out: `capture/macos.rs` configured ScreenCaptureKit with
`setShowsCursor(true)` and reported no shape, on the grounds that macOS never
hands the cursor's bitmap over. With no shape the guest draws nothing of its
own and leaves its system arrow on (ADR 0038's "two cursors are worse than one
that lags"). So the guest showed its arrow where the hand was, and the Mac's
cursor where the picture was.

How far behind the picture was, on the host in the report (the `macos_new` VM
in VMware, no GPU, a 116 ms round trip over the internet): the encode loop ran
at 5-10 frames a second, scaled to 52% by the encoder's speed cap (ADR 0144),
with 10 ms capture, 16-21 ms scaling (263 ms at worst) and 8-11 ms encoding a
frame, while `WindowServer` alone used 118% of a core. A cursor that travels
in that picture arrives a network round trip plus a capture, scale and encode
later, at 5-10 updates a second. Next to a pointer that moves at the display's
rate, it reads as a second cursor crawling after the first.

The premise of ADR 0038's macOS row does not hold. `NSCursor.currentSystemCursor`
returns the cursor on screen, whichever application set it, with its image and
hotspot. Measured on that VM (macOS 26.7.1) from a background thread of a
process started over ssh: the arrow, 28x40 points, hotspot (5, 5), image
representations up to 4x. A read cost 1.6-5 ms, once 77 ms, with
`WindowServer` loaded; drawing it at 1x took 0.2 ms. Apple has deprecated the
property, and the SDK header says it will answer nil in a future macOS.

## Decision

**The macOS backend reports the cursor and can leave it out of the picture.**
`set_cursor_embedded(false)` replaces the stream's configuration with the same
one and `showsCursor` off (`SCStream.updateConfiguration`, not waited for: it
is called on the actor). The one function `stream_configuration` builds both,
because an update replaces the whole configuration. Whether the cursor is
embedded is kept on the capturer, not the stream, so a capture restarted
mid-session keeps it.

**The cursor is read on a thread of its own, only while it is out of the
picture.** A `lumepeer-cursor` thread reads `currentSystemCursor` every
100 ms, draws it at its size in points (the unit this backend captures in)
into a premultiplied BGRA bitmap, and numbers it when it differs from the last
one. `cursor_shape` hands out a number it has not handed out yet. Not read
inside `cursor_shape`: the encode loop calls that from its async task with the
capture locked, and a 77 ms window-server stall belongs on a thread that can
afford it. 100 ms is how late an arrow may turn into an I-beam without anyone
telling it from the network's own delay.

**A changed cursor goes out without a frame.** With the cursor out of the
picture, a pointer crossing a text field changes the cursor and repaints
nothing, and the encode loop used to read the shape only after a changed
frame. ADR 0138 recorded this as a known gap. It now reads it on an unchanged
tick too and sends it at the sizes of the last frame that went out
(`PictureCursor::due_for_last_picture`). This applies to every backend: on
Windows the I-beam over a still screen used to wait for the next repaint.

**When the property answers nil**, the cursor is out of the picture and no
shape arrives. The guest then shows its own arrow, which is where the pointer
is: no shape, but no lag and no second cursor either.

## Consequences

- On a Mac host the guest draws the host's cursor at its own pointer, as on
  Windows and X11. The cursor no longer waits for the picture, however slow
  the picture is. Wayland is now the only host that burns it in.
- The real pointer on the Mac still moves one network trip after the hand:
  that is where clicks land, and the e2e test below checks it separately.
- Nothing changes on the wire. `CursorShape` and `FEATURE_CURSOR_SHAPE` are
  as they were, and every guest that draws a Windows host's cursor draws a
  Mac's.
- `objc2-app-kit` becomes a direct dependency of `lumepeer-media` on macOS
  (`NSCursor`, `NSImage`). It was already in the graph at the same version
  through tao.
- e2e `test_cursor` (scenario 3 of `e2e/matrix`) checks, for every pair: the
  host's cursor lands within a picture pixel of five aims from corner to
  corner; the guest draws the host's cursor and the picture under it does not
  change when it moves away; over the host's tracker it turns into the I-beam
  on a still screen; and through a one-second sweep the host's cursor arrives
  within 150 ms of the network's one-way delay and falls no more than 150 ms
  further behind.
