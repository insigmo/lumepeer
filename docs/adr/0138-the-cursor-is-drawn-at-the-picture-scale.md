# ADR 0138 — The cursor is drawn at the picture's scale, by its hotspot

Status: accepted
Date: 2026-10-01

Corrects [ADR 0038](0038-the-cursor-leaves-the-picture.md), which predates
every reduction of the picture a guest can ask for
([ADR 0060](0060-the-guest-names-the-picture-size-it-will-draw.md), the presets of D7).

## Context

Reported 2026-10-01: with the picture set to 1080p and a 4K host, the cursor
the guest draws does not sit on the pointer. It sits well below it, by a couple
of centimetres.

The guest draws the host's cursor itself, at its own pointer (ADR 0038). Two
things go into that, and both were wrong.

- **The Windows host never sent a hotspot.** `GetFramePointerShape` reports one
  in `DXGI_OUTDUPL_POINTER_SHAPE_INFO::HotSpot`. The backend read the shape
  info and kept everything but that field, then sent every cursor with a
  hotspot of `(0, 0)`. The guest's `cursorPlacement` puts the hotspot under the
  pointer, so every cursor was drawn with its top-left corner there instead.
  The arrow's hotspot really is `(0, 0)`, so it hid the defect. Every other
  cursor sat off down and to the right by its hotspot. On this machine at 125%
  that is `(19, 20)` for the I-beam and `(9, 22)` for the vertical resize.
  Compositing never needed the field: `PointerPosition` already names the
  shape's top-left.
- **The cursor kept the screen's scale while the picture lost it.** A capture
  backend reports the cursor in the captured surface's pixels. The guest has
  only the picture to measure against: it scales the cursor by
  `picture.width / frame.width`. That holds only while the frame is the
  screen's size. `performance` on a 4K host is the 50% floor, a 1920x1080
  picture. The 4K cursor was then drawn at twice its size, and its hotspot
  offset was doubled with it. The same happens under every reduction:
  `balance`, the guest's own box, and the `MAX_PICTURE_PIXELS` budget.

## Decision

**The hotspot travels.** `PointerShape` keeps DXGI's `HotSpot`, and both
conversions (`COLOR`/`MASKED_COLOR` and `MONOCHROME`) send it. A monochrome
hotspot is already relative to the cursor, not to the stacked masks, so the
halving of the height does not touch it. A hotspot outside the shape is
refused by the existing §14 bound, the same as any other shape that cannot
travel.

**The host sends the cursor at the picture's scale.** The guest already
assumes the cursor is in picture pixels, and the host is the only side that
knows both sizes. `scale::cursor_for_picture` box-averages the premultiplied
bitmap and scales the hotspot by the ratio between the captured frame and the
encoded one, axis by axis. A picture at full size sends the cursor untouched,
and the cursor is never enlarged.

The encode loop holds the newest shape at capture scale (`PictureCursor`). It
sends the shape after the frame is encoded, at that frame's scale, and sends
it again whenever the captured or encoded size changes. Re-sending on a size
change is the half that is easy to miss: a preset picked mid-session changes
the picture and not the cursor, and the backend reports only cursor changes.

The guest is unchanged. `cursorPlacement` was right about everything it
assumed; the host was not delivering it.

## Consequences

- A reduced picture carries a reduced cursor, as sharp as the picture under
  it, and no sharper.
- Nothing moves on the wire. `CursorShape` keeps its shape. `PROTOCOL_MINOR`
  and the golden vectors stay as they are. An older guest gets the corrected
  shape and draws it correctly.
- X11 hosts already sent a real hotspot and get the scale half of the fix.
  Wayland and macOS send no shape at all (ADR 0038) and are untouched.
- A cursor shape change on an otherwise still screen still waits for the next
  changed frame, because an unchanged frame ends the tick before the cursor is
  read. That predates this ADR and is not changed here.
- Verified by unit tests only: the hotspot conversion, the scaled bitmap and
  hotspot, and the re-send on a size change. It has not been seen live on a
  4K host.
