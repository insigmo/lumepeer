# ADR 0144 — The host keeps up with its encoder and its link: the hardware encoder is found, a still screen costs nothing, no skipped interval after a slow frame, a session that comes back keeps its preset and comes back at once, a lossy link is used rather than abandoned, a lower bitrate while the link stalls, and no empty frames

Status: accepted
Date: 2026-10-05, extended 2026-10-07 and 2026-10-08

The owner watched a film on `beta` from the reference machine for an hour on
2026-10-03 and reported that "at some point it started to glitch and freeze,
fps fell to 0, the film was a slideshow, then it let go and again". They
tried again on v0.0.133 overnight on 2026-10-07, for four hours, and it was
worse: "fps falls terribly". The owner's bar: a film plays without freezes and
without losing quality.

The host's own log (`media: the last period of the encode loop`, every 5 s)
and the guest's log for both sessions, a diagnostic run on `beta` itself, and
eight soak runs of `e2e/matrix/soak.py` between the reference machine and
`beta` with a full-screen animation playing on `beta`, show twelve separate
causes. Eleven are this code's or the transport's, and one is a preset doing
exactly what it was told.

## Context

**The hardware encoder was never found.** Every session on `beta` was encoded
by `openh264`, in software: "no hardware encoder available, falling back to
openh264". `beta` is a Ryzen 7 5700U with Radeon graphics, and Media
Foundation lists two `AMDh264Encoder` MFTs on it. Both refuse the probe's
64x64 picture at `SetOutputType` (`MF_E_INVALIDMEDIATYPE`). Both take 128x128,
256x256, 720p and 1080p, with and without a Direct3D device manager. Intel's
AV1 MFT has the same kind of floor, which ADR 0069 had already met and fixed
for AV1 alone. The probe swallowed every candidate's error, so no log anywhere
said a hardware encoder had been there and why it was passed over. Measured on
`beta` once it was found: a 1080p frame that stays on the GPU encodes in
7–8 ms, against `openh264`'s 30–60 ms.

**A still screen cost the full bitrate on AMD.** The encoder asks for
`LowDelayVBR` and falls back to CBR. AMD's MFT accepts `LowDelayVBR` and then
pads every frame to the mean bitrate, exactly as it does under CBR: 33 KB a
frame for a still 1080p screen, every frame, 8 Mbit/s of filler on a Wi-Fi
link. Under peak-constrained VBR the same frames are 76 bytes, and frames with
motion are 3–17 KB. Intel refuses `LowDelayVBR`, runs CBR without padding,
and takes peak-constrained VBR as well.

**A skipped interval after every slow frame.** The encode loop hands each
frame to the writer task through a one-slot channel (ADR 0059) and checks the
slot with `try_reserve` before capturing the next picture. The writer is woken
by the hand-off. On tokio's multi-threaded scheduler, a task woken from a
worker goes into that worker's LIFO slot and runs once the waking task
yields. When the encode took longer than the frame interval, nothing between
the hand-off and the next `try_reserve` yields. The slot was always still
taken, the tick was counted as "skipped", and the loop slept a whole
interval. On `beta` at 1080p, a film took `openh264` 40–60 ms a frame. In the
busiest minutes the log shows about one skipped tick per encoded frame
(`fps 11, skipped 53` for a period of 5 s, `encode_ms 56.1`). The encoder alone
allowed 18–19 frames a second; the loop made 11. Reproduced in-process
(`an_encoder_slower_than_the_interval_sets_the_frame_rate_alone`): an encoder
taking 30 ms a frame delivered one every 47.6 ms.

**A session that came back lost its preset.** On 2026-10-07 the host refused
the guest's resume (it had no session to resume), and the guest started a new
session into the window it already had open. The window names its preset
once, at mount. The new session therefore ran without one, and the adaptive
controller had the bitrate. The guest counted the host's empty frames (below)
as lost, the controller halved the bitrate for the loss, and `openh264`
rebuilt itself for every change — a keyframe each time, up to four in 5 s.
At the lower bitrate it skipped more frames, which made more "loss". The
target reached the 300 kbit/s floor. The film played at 50–120 kbit/s and
6–19 frames a second, and the guest logged 9 357 empty frames in 42 minutes.

**A refused resume waited five minutes.** After a refused resume, the guest
sat out the rest of `RECONNECT_WINDOW_SECS` (300 s) before dialing a new
session: disconnect 03:10:03, refusal 03:10:11, new session 03:15:08. ADR 0084
waits so that a new session does not race the repair of the old one. A
refusal means there is nothing left to repair, and the host has just proved it
is up. ADR 0106 had already listed dialing at once as "a real improvement",
left for its own decision.

**Nothing answered the encoder's speed.** A preset pins bitrate, frame rate
and scale (ADR 0064, ADR 0136), so the adaptive ladder of ADR 0037 never runs,
and its signals are about the link anyway. A host whose processor cannot
encode the preset's picture at the preset's rate had no way to trade a smaller
picture for steadier frames. The owner's rule for the presets (2026-09-30) is
that a preset chooses the tradeoff, not whether there is one: `quality` gives
up frames before sharpness, `performance` the reverse, `balance` in between.

**Nothing answered a link that stalls under a preset.** ADR 0139 made the host
wait for the guest's acknowledgements instead of queueing frames, so every
frame shown is current. Both sessions show loss bursts all session long, both
machines on Wi-Fi. A ping to each router for 150 s lost packets on the
guest's side 62 s apart. Most bursts last a second. Some last tens of
seconds: the congestion window falls to 3–5 KB, a few packets a second are
lost, and the host spends every tick waiting (`held 45–50` per period,
`fps 0`), because a frame of 25 KB takes longer than the period to arrive.

**Empty frames on the wire.** `openh264`'s rate control skips a frame to hold
its bitrate and returns an empty bitstream. The host sent it: a nine-byte
header with no picture. The guest's parser rejects anything that short, logged
`dropping a malformed media payload`, and counted each one as lost in its
receiver report.

**`openh264` paid a keyframe for every bitrate change.** `set_bitrate`
rebuilt the encoder. A link already failing to carry the picture is the worst
place for the largest frame in the stream, and under the ladder it happened
once a second.

**The picture was held back for the link's jitter, not its queue.** ADR
0139 stops producing frames while the oldest unacknowledged one has been on
its way more than 16–50 ms longer than the link's best case. On the evening of
2026-10-07 the path between the two homes took 215–311 ms a round trip from
one second to the next, idle, and 242–429 ms when measured again later. Half
the frames were acknowledged later than best case + 50 ms by chance alone, and
the host held back after each of them: 5–8 frames a second with the link
mostly idle.

**One lost packet in 250 cut the picture's bitrate to a fifth.** On the same
evening, a single TCP transfer from `beta` reached 0.59 Mbit/s, and four at
once reached 3.18 Mbit/s between them. The bottleneck had room; each flow's
loss-based congestion control did not. The path lost about 0.4% of packets to
noise, and Cubic took 30% off its window for every loss. At a 260 ms round
trip it never grew back, and the picture's QUIC connection sat at the
single-flow figure. noq's BBRv3, the textbook answer, stalled outright on this
transport: no acknowledgement in 40 s and the writer blocked on every frame. It
is marked experimental and is not used.

**The stream opened with two keyframes.** The encoder was built for the
display's rate (59 Hz on `beta`) and rebuilt a frame later for the preset's 30.
On a fresh connection to a lossy path, the first window collapses after the
opening burst, and a second 100–200 KB keyframe behind the first held the
first picture back for seconds more.

## Decision

1. **The hardware encoder is probed at 256x256, for every codec.** One probe
   size replaces the H.264/AV1 pair; it is negotiated once and the first real
   frame reconfigures to the screen's size. Every candidate MFT that is passed
   over is logged with its name, the size and the error
   (`a hardware encoder MFT was not usable`).

2. **Peak-constrained VBR is asked for first**, then `LowDelayVBR`, then CBR,
   the first the transform takes. A still screen is a few dozen bytes a frame
   on every vendor measured (`a_still_screen_is_not_padded_to_the_bitrate`,
   run on the AMD and the Intel machine).

3. **The loop waits for the writer's slot, up to one interval**
   (`tokio::time::timeout(interval, frames_tx.reserve())` after a failed
   `try_reserve`). The wait yields, so the writer runs and frees the slot,
   and the frame rate of a slow encoder is the encoder's own: 30.7 ms a frame
   for the 30 ms encoder above. A tick is still skipped, as before, when the
   writer really holds the previous frame for a whole interval. The timeout is
   tokio's timer, which on Windows fires on the 15.6 ms system tick
   (ADR 0136). That is acceptable for a link that has already held a frame
   for an interval. It is not used for pacing.

4. **A session that comes back is told again what its window asked for.** The
   guest keeps, per view, the last preset (scale and frame rate), the box it
   draws into, and the encoder it picked (`PictureAsks`). It sends them again
   when a session comes back into the window. A host that resumed already
   knows them, and a repeated value changes nothing there. A host that did
   not resume learns them before the first picture.

5. **A refused resume dials a new session at once** (`ReconnectWait::refused`).
   A wait still inside the window for a session the host has not refused
   keeps waiting, as ADR 0084 decided.

6. **An encoder that cannot make the frame rate gets a smaller picture**
   (`lumepeer_media::abr::EncodeSpeed`). The loop measures what reducing and
   encoding take, as the mean over each second. Two seconds in a row over the
   interval the preset defends reduce the share of the captured picture the
   encoder is handed by 15% of each side (28% of the pixels). The reduction
   starts from what was actually encoded, which a guest's smaller window can
   already have reduced, and stops at `ABR_MIN_SCALE_PERCENT`. The size comes
   back a step at a time after ten seconds in a row in which the next step up
   is predicted to fit 80% of the interval, with the time scaled by the
   pixels. Ten seconds, not two, because each change of size is a keyframe,
   and a film that alternates calm and busy scenes must not be resized at
   every cut. A preset picked anew resets it. With a hardware encoder this
   never triggers; it is for hosts that have none.

   The interval defended is `defended_fps(asked, rate)`. A preset that asks
   for 30 frames (`quality`) keeps its full size down to two thirds of them,
   20. One that asks for 120 or more (`performance`) gives up size before any
   frame. In between it is linear: `balance`'s 60 defends 46. It is never
   above what the session runs at, so a 60 Hz host is not held to 144, and
   never under `ABR_MIN_FPS`.

7. **A link that keeps stalling under a preset lowers the bitrate**
   (`lumepeer_media::abr::LinkPressure`). The loop counts the time it spends
   waiting on the link: on the guest's acknowledgements (ADR 0139) and on the
   writer's slot. A second at least half spent waiting is pressed. The first
   pressed second does nothing: a Wi-Fi hiccup is over before anything could
   act on it, and a softer picture would outlast it by seconds. From the
   second one on, each pressed second halves the bitrate. It halves from what
   was actually sent in that second, not from the target, and stops at
   `ABR_MIN_BITRATE_KBPS`. After two clear seconds (at most 10% waiting) the
   bitrate steps back up by half each clear second, about ten seconds from the
   floor to the preset's 8 Mbit/s. The cap is gone once it reaches the
   preset's own figure. Frame rate and scale stay pinned. Smaller frames get
   through a lossy link where large ones stall, so the picture keeps moving,
   softer, instead of freezing. Without a preset the adaptive controller owns
   the bitrate, and the cap is released.

8. **`openh264` changes its bitrate live.** `OpenH264Encoder::set_bitrate`
   calls `SetOption(ENCODER_OPTION_BITRATE)` for the stream and
   `ENCODER_OPTION_MAX_BITRATE` for its one spatial layer. `openh264` refuses
   a target above the layer's maximum, so the maximum moves first on the way
   up and last on the way down. The stream carries on with no keyframe.

   The encoder is still rebuilt before its first frame, when nothing is
   initialised to set the option on, and when `openh264` refuses the option.
   The crate re-initialises for a new picture size from the settings it was
   built with, which would undo a live bitrate. So a new size with a live
   bitrate in effect rebuilds the encoder at the current bitrate; the new size
   starts on a keyframe anyway. `openh264-sys2` becomes a direct dependency of
   `lumepeer-media` under `encode-openh264`, at the version `openh264` already
   links, for the option's constants and struct.

9. **An empty frame is not sent.** The host drops an encoded frame with no
   bytes before the cursor, the stats, the acknowledgement count or the
   writer see it. The guest treats a payload that is exactly the header as a
   frame an older host's encoder skipped: not malformed, and not lost.

10. **The hold's slack follows the link's jitter.** The guest's
    acknowledgements feed RFC 3550's interarrival jitter: a moving average of
    how much each one's delay differs from the one before. A queue that builds
    moves every delay up together and barely moves it; a path whose round trip
    swings moves it by the swing. The slack is ADR 0139's two frame intervals,
    widened to twice that jitter, up to `MEDIA_QUEUE_JITTER_SLACK_MAX_MS`
    (200 ms). ADR 0139's own measurement still holds: a guest that reads more
    slowly than the host encodes is shown pictures under 400 ms old.

11. **The obfuscated transport's congestion controller is Cubic, told only
    about losses that look like a queue** (`lumepeer_net::loss_tolerant`).
    It passes on a loss event when more than 2% of the bytes of the last few
    seconds were lost, when one event lost eight packets or more, on an ECN
    mark, and on persistent congestion. Sporadic losses are retransmitted as
    ever, but they no longer shrink the window. The window is held under twice
    what the link delivered over the last one to two seconds, times its
    quickest round trip, and never under 128 KB. Without that ceiling, Cubic
    grew to 10 MB in a measured run and paced bursts by it, and one burst lost
    hundreds of packets and stalled the picture for five seconds. The
    picture's own hold (ADR 0139, point 10) guards against the queue a
    braver window could otherwise build. `noq-proto` becomes a direct
    dependency of `lumepeer-net`, at the version `noq` resolves, for the one
    type the controller trait names that `noq` does not re-export.

12. **A preset named before the stream opens is the rate the encoder is built
    for**, so the stream opens with one keyframe, not two.

Each change is logged. The new log lines are `the encoder's speed moved the
picture's size` and `the link moved the picture's bitrate`. The 5 s encode
report gains three fields: `speed_cap`, `link_cap_kbps` and `link_slack_ms`.

No protocol change and no minor bump. The repeated requests are messages
every host since minor 10 (scale, size) and 23 (encoder) already reads, each
sent only to a host that speaks it.

## Consequences

Measured on 2026-10-07/08 with `e2e/matrix/soak.py`, guest = the reference
machine, host = `beta` (hardware encoder found, 1080p, `quality` preset),
a full-screen animation with motion and fine detail in every frame on `beta`,
over a path of 250–300 ms. Runs back to back, so the network is as alike as it
gets:

| run | median fps | mean bitrate | picture sharpness (guest canvas) |
|---|---|---|---|
| Cubic | 13 | 0.9 Mbit/s | ~1.1 |
| loss-tolerant Cubic, no window ceiling | 20 | 3.5 Mbit/s | ~1.3, one 5 s stall |
| loss-tolerant Cubic with the ceiling, one keyframe at start | 22 | 4.5 Mbit/s | ~5 |


- `beta`, and any host whose hardware encoder refuses small pictures, encodes
  in hardware: 7–8 ms a 1080p frame and full quality at the preset's bitrate,
  instead of the software fallback.
- A still screen on AMD costs a few dozen bytes a frame instead of the
  preset's whole bitrate, which leaves the Wi-Fi link to the frames that
  carry something.
- A session that comes back after a refused resume is back within seconds,
  with its preset, instead of five minutes later without it.
- A host with a slow software encoder runs as fast as its encoder allows, and
  below the preset's defended rate it runs at a smaller picture.
- During a long stall on a lossy link, the picture degrades to a soft but
  moving one within about two seconds, instead of staying frozen while the
  loss lasts. It is back to full quality about ten seconds after the link
  clears. A one-second hiccup is untouched by design: it is the network's,
  and nothing on the host can shorten it.
- The periodic loss itself is not this code's. A burst about every 60 s on a
  Wi-Fi link is the signature of a background scan by the Windows WLAN
  service. A cable, or finding what requests the scans on the machine that
  shows them, is the remedy.

- The picture's connection now takes more of a lossy link than one TCP
  transfer would: about 1.5% of its packets were lost at 4.5 Mbit/s in the
  last run. Its own hold keeps that from becoming a queue the person at the guest
  feels, but other traffic on the same uplink gets less of it.

## Not done

- Sessions over the iroh transport (the fallback when the obfuscated one does
  not connect) keep iroh's own Cubic; point 11 is the obfuscated transport's.
- On that path, the first picture still takes 15–20 s: the opening keyframe
  collapses a fresh connection's window, and Cubic climbs out of it slowly.
- A film is sent the way a desktop is: always the newest picture, never a
  queue. A loss burst on the link is therefore a pause on the guest. Hiding
  one completely would take a playback buffer on the guest, a mode that trades
  the latency a desktop needs for the smoothness a film wants. That is its own
  decision.
