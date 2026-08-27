# 0038 — The cursor leaves the picture

Status: accepted
Date: 2026-08-27

## Context

`MessageKind::CursorShape` and `MAX_CURSOR_SHAPE_PIXELS` have been in the
protocol since `PROTOCOL_MINOR` 1, and its doc comment says what they are for:
the guest draws the host's cursor locally, over the video, so the pointer moves
with the hand instead of with the frame rate. Nothing sent one, nothing
received one, and no capture backend could produce one.

What each platform did instead:

| Backend | Cursor |
|---|---|
| Windows (`capture/windows.rs`) | composited into the frame, with a cursor-free background cache so motion leaves no trail |
| Wayland (`capture/linux_wayland.rs`) | `CursorMode::Embedded` — the compositor burns it in |
| macOS (`capture/macos.rs`) | `setShowsCursor(true)` — same |
| **X11 (`capture/linux_x11.rs`)** | **nothing at all** |

The X11 row is a plain defect and unrelated to the rest: `XGetImage` has never
contained the pointer, and this backend never drew one, so an X11 host shared a
screen with no cursor on it.

At 150 ms round trip a cursor that travels in the video lags the hand far
enough that the whole session reads as broken, even while the picture itself is
fine. That is the latency this channel exists to remove.

## Decision

**X11 draws its cursor, from XFIXES.** `xfixes_get_cursor_image` gives the
bitmap, the hotspot and the position; the backend alpha-blends it onto the
frame with the premultiplied formula the XFIXES payload actually uses, and
hashes the pointer's position and serial alongside the pixels so cursor motion
over a still screen counts as a change (§11.1). A display without XFIXES gets a
`tracing::debug` and frames without a cursor — never a refused capture (§18).

It needs none of the "clean background" cache the Windows backend keeps. That
cache exists because DXGI hands the same surface back between presents, so
compositing onto it twice leaves a trail; every `GetImage` here is a fresh,
cursor-free buffer.

**`ScreenCapturer` gains two methods, both defaulted.**
`cursor_shape()` returns the bitmap only when it changed — a cursor is up to
`MAX_CURSOR_SHAPE_PIXELS` and changes whenever a pointer crosses a text field,
so sending it per frame would be a second video channel. `set_cursor_embedded()`
asks a backend to stop drawing it in. Both default to "this platform cannot",
which is the honest answer for Wayland and macOS: a made-up shape would put a
second cursor next to the real one.

**No position travels.** The guest is the one moving the pointer, and drawing
at its own position instant is the entire point. A message per mouse move would
cost more than it saves.

**The channel is all-or-nothing, and it takes two things of every viewer.**
One capture backend feeds them all, so the decision cannot be per session. The
cursor leaves the picture only when *every* peer currently receiving a frame

- advertised `FEATURE_CURSOR_SHAPE`, so it can draw one; and
- holds the `input` grant, so the pointer it draws is the one it is moving.

The second condition is the one worth arguing. A view-only guest is watching
someone else work: its own pointer has nothing to do with the cursor on the
host's screen, so drawing there would be actively wrong, while the embedded
cursor is exactly right. Both conditions are re-checked whenever a media
session starts or ends and whenever a role changes.

**An inverting cursor is rendered as opaque black.** DXGI's `MASKED_COLOR` and
`MONOCHROME` encodings can ask for XOR against the background, which is a
function of the pixels underneath — and the guest draws on a layer *above* the
video, where those do not exist. Every remote-desktop client has to choose;
this one chooses black, because the common inverting cursor is the text I-beam
and it is used over a light background almost every time. The alternative is
not better colours, it is keeping the cursor composited into the video, which
is what a host without this channel already does.

**The bitmap reaches the window by polling, not by frame.** A separate
`view_cursor` IPC call, binary like `view_next_frame`, answered from the same
lock-free view feed and carrying a sequence number: the window asks with the
one it has and gets pixels back only when the host has since announced a
different shape. Position is local, so a quarter-second poll is invisible.

**Receiving a shape *is* the signal.** No message says "the picture no longer
contains a cursor". A host only sends `CursorShape` once it has stopped
compositing, so the guest's rule is simply: a shape has arrived, therefore draw
one; none has, therefore do not. That keeps the two facts from ever
disagreeing, and it means minor 1 covers the whole feature — nothing new goes
on the wire.

**The field name stays `rgba` and the comment becomes the authority.** Every
backend that can report a cursor produces premultiplied BGRA: DXGI's `COLOR` is
a 32bpp BGRA bitmap, and XFIXES returns premultiplied ARGB packed into a
`CARD32`, which is the same four bytes little-endian. Renaming the field would
change the postcard-encoded shape of the message and break the golden vectors
of §17.2, so the contradiction is resolved in the direction that costs nothing.

## Consequences

- An X11 host shows a cursor for the first time. That alone is worth the
  change, and it lands whether or not any guest speaks the new feature string.
- `PROTOCOL_MINOR` does not move. `CursorShape` has existed since minor 1;
  `FEATURE_CURSOR_SHAPE` gates a change of *behaviour* on the host, not a new
  discriminant, so an older guest keeps a cursor in its picture and notices
  nothing.
- On Wayland and macOS the cursor stays in the frame no matter what the guest
  asks for, and the toolbar says so as a fact about the other machine rather
  than offering a switch that would do nothing.
- The guest draws only at its own pointer. When the *host's* user moves the
  mouse during a controlled session, the guest sees no cursor move — accepted,
  because the alternative is a message per mouse move, and because the session
  where that matters is the view-only one, which keeps the embedded cursor.
- The premultiplied bytes are un-premultiplied again for the canvas, which
  wants straight RGBA. Skipping that leaves every antialiased edge too dark.
