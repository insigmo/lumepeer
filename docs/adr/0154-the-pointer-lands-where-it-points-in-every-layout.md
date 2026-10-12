# ADR 0154 — The pointer lands where it points, in every layout and at every resolution

Status: accepted
Date: 2026-10-12

Builds on [ADR 0038](0038-the-cursor-leaves-the-picture.md),
[ADR 0138](0138-the-cursor-is-drawn-at-the-picture-scale.md) and
[ADR 0150](0150-the-mac-cursor-leaves-the-picture.md).

## Context

Reported 2026-10-12: connected to the Mac, the host's cursor and the guest's
do not match; and whenever the host's resolution differs from the guest's,
they do not match either. The owner asked for it to be fixed so that it does
not happen again, with e2e tests that check it from now on.

ADR 0150's e2e `cursor` scenario was green. It aims at five spots from corner
to corner, but only in the view's default layout, and in a window whose shape
happened to leave no free space above and below the picture. The view maps a
pointer position through `pictureBox`, the box the layout math says the
picture occupies; the page drew it where CSS put it, and the two agreed in
exactly that case. Measured with new scenarios (below) against v0.0.142,
reference machine (3840x2160 at 250 %) → `beta` (1920x1080):

| layout | spot sent to the host, worst |
|---|---|
| fit over the whole window | right |
| fit, window narrowed (bars above and below) | 1507 px off |
| 1:1 | 1493 px off |
| zoomed past the window, centred or panned | 378 px off |
| fit, after `beta` switched to 4096x2160 mid-session | 48 px off, vertically |

Three causes on the guest, one on a macOS host:

1. **The grid made rows of empty siblings.** `#view` is a grid with
   `place-items: center`, and besides the canvas it holds two in-flow `div`s
   (the status overlay and the recording badge), empty nearly always. Each
   is an implicit row, and the rows share the window's free height. A
   picture narrower in shape than the window (bars above and below) was
   centred in the top third of the free space, not in the window: in a
   768 px window with a 1.867 picture, 73 px higher than the mapping
   assumed. The Mac in the report is 1718x920 — wider than 16:9 — so on a
   16:9-ish window it is always width-limited, and every session to it was
   off by this from the first frame. Any host whose shape is wider than the
   guest window's is.
2. **A picture larger than the window went to the corner.** At 1:1 or zoomed
   in past the window, the grid's track grows to the picture and starts at
   the window's left and top, while `pictureBox` has it centred. Pointer
   positions and the drawn cursor were off by half of what did not fit, and
   the pan could reach only half of the picture.
3. **The cursor layer was placed by a logical property.** `#cursor` had
   `inset-inline-start: 0`, which in the Arabic (right-to-left) window is
   `right: 0`; with `left` set from the pointer and a width, `left` loses, and
   the host's cursor was drawn against the window's right edge.
4. **A Mac host never followed a display that changed size.** A
   ScreenCaptureKit stream keeps the size it was made with and scales the
   display into it. The VM in the report changes resolution whenever its
   VMware window is resized or made full screen (1718x920 windowed,
   1920x1080 full screen). The picture then kept its old shape, letterboxed,
   while input — re-read per move since 2026-10-07 — went to the display as
   it now was.
5. **The guest's decoder held a Mac's pictures back.** VideoToolbox's SPS,
   read out of a live stream: Main, level 3.0, two reference frames, a VUI
   with HRD and no `bitstream_restriction`. A decoder told nothing about
   reordering assumes the worst and holds pictures until later ones push
   them out. The Windows encoder's SPS says `max_num_reorder_frames` 0. With
   4 fixed, `beta` → Mac showed each new screen size one step late (972x608
   on screen while the Mac had already sent 778x582 of 1024x768), sessions
   to a still Mac showed no picture for 40-90 s, and a live Mac desktop at
   5-10 frames a second is shown several frames late.
6. **A keyframe asked for on a still screen never came** (found on the way):
   the encode loop took a keyframe request only with a changed frame.

## Decision

**The picture is placed where the layout says, and pointer positions are
mapped against where it actually is.** `#screen` is positioned absolutely
from the window's corner, and `applyLayout` translates it to `pictureBox`
(which already holds the pan); the grid no longer decides anything about it.
`ViewInput` and the cursor layer read the canvas's own bounding rectangle
(`pictureOnScreen`) instead of recomputing the box, so whatever CSS does, a
position maps to the pixel drawn under it. Both, because either alone leaves
a gap: a measured box with grid placement would map correctly onto a picture
the pan cannot centre, and a placed picture with a computed box would break
again the day something else moves it.

**Physical `left`/`top` for both layers.** Pointer coordinates are physical;
the window's text direction has nothing to say about where a pixel is.

**The macOS capturer starts its stream again when the display changes size.**
Every half second `next_frame` compares `CGDisplayBounds` of the captured
display with its size when the stream started, and on a difference stops and
starts the stream on the same target, which reads the new size. A start, not
`updateConfiguration`: a start is what every session already does, and an
update is what took `WindowServer` down on the Mac without Metal (ADR 0152).

**A keyframe request on a still screen is served by a snapshot.** When the
capture reports no change and a keyframe is pending, the encode loop asks the
backend for the screen as it is (`ScreenCapturer::snapshot`, ADR 0141's
mechanism for a loop that starts on a running capture) and encodes that as
the keyframe. The macOS backend now has a snapshot too: the stream's newest
picture, with the cursor drawn in when the host draws it. A backend without
one (Wayland) is where it was: the next change carries the keyframe.

## Consequences

- At every layout and every host resolution, the spot under the guest's
  pointer, the host's own cursor and the cursor the guest draws are one
  spot. Measured after the fix, reference machine → `beta`: 0 px worst in all
  seven layouts and after `beta` switched to 1920x1200 and 1600x1200.
- The pan now reaches the whole picture at 1:1 and zoomed in.
- A resolution change on a Mac costs a restart of the stream (a few hundred
  ms without frames) and a keyframe, the same as on Windows, where DXGI
  answers a mode change with `ACCESS_LOST`.
- Still not covered: input on every OS goes to the host's primary display,
  while a guest that picked another monitor sees that one. Picking a
  monitor needs the injector to follow the capture target, across the
  service's desktop injector on Windows too.
- e2e (`e2e/matrix`), for every pair:
  - `layouts`: fit over the whole window, narrowed, flattened and smaller
    than the host's screen, 1:1, zoomed past the window and zoomed and
    panned; five spots each; the spot sent, the host's cursor, the drawn
    cursor against the pointer and against the host's cursor must agree
    within a picture pixel and a CSS pixel, and the picture must keep the
    screen's shape.
  - `resize`: the host switches to two screens of other shapes mid-session,
    by the product's own switch (ADR 0048) or the machine's own (the Mac
    VM's `vmware-resolutionSet`), and back; the picture has to follow within
    15 s and `layouts`' checks hold in fit. Skipped for a host marked
    `resize = false` (the machine the run is driven from).
