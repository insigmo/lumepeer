# ADR 0018 — Host-side downscale to a fixed picture budget

Status: accepted
Date: 2026-08-20

## Context

`lumepeer_media::decode::shm::SLOT_PAYLOAD_BYTES` is 8 MiB, and its own doc
comment already said what that implies — "One 4K RGBA picture does not fit and
is not meant to: the worker returns pictures capped at this size" — but
nothing anywhere capped anything. Capture handed the encoder whatever the
screen was, the decoder worker decoded to RGBA at that size, and
`SharedRing::push_return` refused the picture:

```
Error: decoder worker failed: payload of 33177600 bytes exceeds the 8388608 byte slot
```

33 177 600 bytes is 3840x2160x4. The worker then exited on that `Err`, the
guest saw its stdout close, redialed, and repeated the cycle for the whole
session while its view window sat on "Waiting for the remote screen…".

The threshold is not 4K: 8 MiB of RGBA8 is 2 097 152 pixels, so 1920x1080
(2 073 600) is the last common resolution that fits. **Every host with a
screen above 1080p was unviewable**, on every guest platform, which is what
made this look like a per-platform transport bug rather than one shared
pipeline limit.

Three ways out were considered:

- **Grow the slot.** A 4K RGBA picture needs 32 MiB; with `RING_SLOTS = 4` per
  direction that is a 288 MiB mapping, against an
  `ACTIVE_SESSION_EXTRA_RAM_BUDGET_MIB` of 150 (§15,
  `ci/resource-budget.yml`). Even at two slots it eats half the budget, and
  the 33 MiB IPC response per painted frame is still there behind it.
- **Downscale in the decoder worker**, which is what the existing comment
  hints at. It fits the slot, but the host has already spent the CPU encoding
  4K and the link has already carried it, so the cost the budget is about is
  paid anyway.
- **Downscale on the host, before encode.** One reduction upstream of
  everything: encode, wire, decode, shared memory and the guest's canvas all
  size themselves off the reduced picture.

## Decision

`crates/core/src/constants.rs` gains `MAX_PICTURE_PIXELS = 1920 * 1080`, the
largest picture the media pipeline carries in one frame, and
`crates/media/src/scale.rs` reduces anything larger before
`spawn_encode_loop` encodes it. The aspect ratio is preserved and both axes
are rounded down to even numbers (4:2:0 has no odd rows or columns).

The filter is a box average over the exact source rectangle each destination
pixel covers — no image crate, and for the 2:1 ratio a 4K screen actually hits
it is the correct filter rather than an approximation of one.

`SLOT_PAYLOAD_BYTES` is now asserted against `MAX_PICTURE_PIXELS * 4` at
compile time in `decode::shm`, so the two can no longer drift: raising the
picture budget without raising the slot fails the build instead of failing on
somebody's monitor.

Independently, the decoder worker now answers an oversized picture with its
`ERROR_BYTE` status and keeps running, instead of returning `Err` out of
`run` and taking the process down. Nothing should reach it at that size any
more; §2.4's "a failure degrades towards safety" says it must not be fatal if
something does.

## Consequences

- A 4K host is watched at 1920x1080. That is a real quality reduction on a 4K
  guest display, and it is the trade the memory budget of §15 asks for; a
  future "high resolution" mode would have to raise `MAX_PICTURE_PIXELS`,
  `SLOT_PAYLOAD_BYTES` and the §15 budget together, which the compile-time
  assertion now forces to be a deliberate, reviewable change.
- Hosts at or below 1080p are untouched: `target_size` returns `None` and the
  frame passes through without a copy.
- The box filter costs one pass over the captured frame on the host. At 4K/30
  that is measurable but small next to encoding, and it *removes* three
  quarters of the pixels from everything after it.
- Non-16:9 shapes (3440x1440, 2048x2048) are reduced to their own aspect
  ratio, not letterboxed into 1920x1080; `scale.rs`'s tests assert both the
  budget and the ratio for a spread of real monitor sizes.
