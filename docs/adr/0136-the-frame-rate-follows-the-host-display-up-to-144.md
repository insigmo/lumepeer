# ADR 0136 — The frame rate follows the host's display up to 144, each preset names its own, and the pacing is measured in real milliseconds

Status: accepted
Date: 2026-09-30

The owner's report: "It feels like the picture comes at the same frame rate
everywhere. Make it as fast as it can go." Asked for a ceiling, the owner
chose 144. Told that a preset holds the bitrate still, so more frames means
fewer bits for each, the owner chose a frame rate per preset: `performance`
the most there is, `balance` 60, `quality` 30.

Amends [ADR 0037](0037-receiver-reports-and-the-degradation-ladder.md) (where
frame-rate recovery stops) and [ADR 0064](0064-a-quality-preset-is-a-target-not-a-ceiling.md)
(what frame rate a preset pins).

## Context

Two limits stacked, and the first one made the second invisible.

**Every session ran at 30.** `ENCODE_DEFAULT_FPS = 30` was the encoder's
declared rate, the pace of the encode loop, the top of the adaptive ladder's
recovery and the frame rate a named preset pinned, on every host and every
display. A 144 Hz host and a 60 Hz one sent the same picture. And a preset is
always named: the view window sends one at mount (`toolbar.ts`
`sendQuality`), so in practice every session was pinned at 8 Mbit/s and 30 fps.

**The loop did not reach 30 either.** The loop sleeps what is left of the
frame interval with `tokio::time::sleep`. On Windows tokio's timer wakes on
the system clock tick, and the lumepeer process never raises the timer
resolution. Measured on the reference machine (Windows 11, rustc 1.98):

| requested sleep | `tokio::time::sleep` | `std::thread::sleep` |
|---|---|---|
| 1 ms | 15.8 ms | 1.5 ms |
| 8 ms | 15.8 ms | 8.4 ms |
| 11 ms | 16.0 ms | 11.4 ms |
| 16–23 ms | 31.7 ms | — |

A loop doing 5 ms of work a tick ran at 21 fps against a 30 fps target, and
would have run at 32 against 60 — raising the constant alone would have
bought almost nothing on a Windows host. `std` sleeps on a high-resolution
waitable timer and lands within half a millisecond.

**The encoder has to be told the rate frames really come at.** Media
Foundation's rate controller divides the bitrate by the frame rate it was
*declared*, not by the rate frames arrive at. Measured on the reference
machine's hardware H.264 MFT, 2560x1440 at 8 Mbit/s with frames arriving at
60 a second (`a_declared_frame_rate_sets_the_frame_budget`):

| declared | spent | per frame |
|---|---|---|
| 30 | 11.2 Mbit/s | 23.2 KB |
| 60 | 8.1 Mbit/s | 16.8 KB |
| 144 | 3.5 Mbit/s | 7.4 KB |

A blanket 144 would have made every frame on a 60 Hz host — most hosts —
2.3 times softer than it needs to be, and a 30 fps preset on an encoder
declared for 60 would spend half its bitrate. `openh264` did not care either
way (2.1–2.6 Mbit/s whatever it was told).

## Decision

1. **The session's ceiling is the host display's refresh rate, capped at
   `ENCODE_MAX_FPS` = 144** (`abr::ceiling_fps`). Desktop Duplication and
   `ScreenCaptureKit` never deliver more than the display presents, so the
   refresh rate is also the most a capture can produce. It is read once when
   the encode loop starts, from `CaptureController::current_display_mode`
   (`EnumDisplaySettingsW` on Windows, `RandR` on X11), on a blocking thread.
   A host that cannot say gets `ENCODE_DEFAULT_FPS`, now 60 instead of 30:
   macOS and Wayland report no current mode yet, and 60 Hz is what nearly
   every panel runs at. `ScreenCaptureKit`'s minimum frame interval follows
   the same constant.
2. **Each preset names its frame rate**, held under that ceiling:
   `performance` 144, `balance` 60, `quality` 30 (`toolbar.ts` `fpsFor`). The
   guest sends it as a `QualityAdjust { target_fps, target_bitrate_kbps: 0 }`
   right after its `StreamScaleRequest`. That kind has been in the protocol
   since its first minor and no host ever acted on it, so there is no minor
   bump: a host older than this ignores it (`ignoring a control message`) and
   keeps its own 30. The host checks the range and the `view` grant exactly
   as it does for the scale, and keeps the figure with the scale in
   `StreamCaps`, so a redial or a resume starts its loop at the same rate
   (ADR 0119). A guest older than this names no rate; its preset runs at the
   host's ceiling.
3. **The encoder is built for the rate frames will come at**: the session's
   ceiling at start, and rebuilt when a preset's rate differs from what it
   was built for. A rebuild costs one intra frame, which a preset change asks
   for anyway. The adaptive ladder does not rebuild — its 5 fps steps would
   pay an intra frame a second — so, as before this ADR, its lower rungs run
   an encoder declared for more frames than it gets.
4. **Without a preset, the adaptive ladder starts at and recovers to the
   session's ceiling** (`AbrController::with_max_fps`), not to a constant.
5. **The pacing sleep is `std::thread::sleep` on a blocking thread**, and the
   frame interval is computed in microseconds (whole milliseconds made 60 fps
   a 16 ms interval, 62.5 fps, and 144 a 6 ms one, 166 fps).
   `the_pacing_keeps_up_with_60_fps` pins it: 32.1 fps with the old sleep,
   about 58 with the new one.

## Consequences

- The default preset, `quality`, now delivers a real 30 frames a second of a
  moving screen on a Windows host instead of about 21, with every frame as
  sharp as before. `balance` doubles that at half the bits per frame, and
  `performance` goes to the host's refresh rate, up to 144, at the fewest.
- The bitrate a preset pins did not move: 8 Mbit/s whatever the frame rate.
  Nothing here sends more bytes; `balance` and `performance` spend the same
  bytes on more frames.
- The host does up to twice the capture and encode work of before on a 60 Hz
  display in `balance`, and up to nearly five times on a 144 Hz one in
  `performance`, while the screen is moving. A host without a hardware
  encoder (`openh264`) runs as fast as its CPU allows; the loop never sleeps
  once a tick takes longer than the interval.
- A display-mode or monitor change mid-session keeps the ceiling the loop
  started with until the next redial.
- Pixels still limit the rate where the picture has to be reduced. When the
  guest's box is smaller than the host's display, a zero-copy frame is read
  back from the GPU and box-averaged on one CPU thread (`scale::fit_within`).
  Measured on the reference machine (Intel UHD, 4K panel): reading back and
  reducing a 3840x2160 frame to 1080p costs 50 ms and a 2560x1440 one 34 ms,
  against 13 and 7 ms to encode the full-size frame on the GPU. Such a
  session stays near 16–22 fps in any preset. Scaling inside the encoder's
  video processor, which already runs on every zero-copy frame, is the change
  that would lift it; it is not part of this decision.
- Not measured end to end: a live Desktop Duplication run from the agent's
  shell was refused with `E_ACCESSDENIED` on an unlocked desktop (the three
  live `dxgi` capture tests fail there on master too), so the achieved rate of
  a real session is still to be read off one.
