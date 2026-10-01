# ADR 0137 — Host audio comes with the picture, and the guest plays it

Status: accepted
Date: 2026-10-01

The owner's report: "There is no sound from the host, on all three OSes."

Amends [ADR 0023](0023-phase7-catchup-decisions.md) (audio was opt-in per
session) and [ADR 0028](0028-remote-sas-and-view-toolbar.md) §5 (playback
existed only for the guest microphone).

## Context

Every piece of the host→guest audio path existed: WASAPI loopback,
PipeWire monitor and ScreenCaptureKit capture; Opus both ways; a tagged `A`
stream on the media connection; WASAPI, PipeWire and `CoreAudio` playback.
Three gaps between them meant no guest ever heard a host.

1. **Nothing started the host's audio loop.** It ran only on the
   `audio_toggle` IPC command, and no part of the UI ever called it.
   `questions.md` (2026-08-23) lists the toggle button as "not done"; it was
   never done.
2. **The guest threw the sound away.** Decoded PCM went into a `watch`
   channel whose receiver was `std::mem::forget`-ten, "placeholder until the
   playback sink lands". The sink landed for the *host* side of the guest
   microphone (ADR 0028) and was never wired to this direction.
3. **A quiet host ended capture for good.** Each capture backend returned
   `CaptureInterrupted` after `READ_TIMEOUT` (2 s) without audio, and the loop
   stops on the first error. WASAPI loopback and ScreenCaptureKit hand back
   nothing at all while nothing plays, so a session that started on a silent
   desktop — most of them — lost its audio two seconds in and never got it
   back, even had something turned it on.

## Decision

**Audio starts with every media session.** `on_media_accepted` starts the
audio loop right after the encode loop, on the same connection, replacing
any loop a previous media connection had. The `view` grant already says
"receive video **and audio**" (§8.1), and loopback capture is what the
speakers play, not the microphone. `audio_toggle` stays as it was, to turn it
off and on again; nothing in the UI calls it, and a media redial turns audio
back on.

**The audio stream opens after the picture's.** The picture's stream predates
tagging and carries no tag, so a guest takes the first stream it accepts as
the picture (`dial_media` → `accept_media_stream`). QUIC delivers streams to
`accept_uni` in the order they were opened, so the order of the two `open_uni`
calls decides which one a guest decodes as video. The encode loop raises a
latch on `EncodeControl` (`video_stream_open`) once its stream is open; the
audio loop waits on it (`video_stream_opened()`) before opening its own. This
holds for guests already in the field too: they would decode an audio stream
that won the race as video.

**A read timeout is silence, not the end.** The three desktop-mix backends
answer a wait that runs out with `PcmChunk::silence`. A stream that is
actually gone still ends capture: WASAPI's `GetNextPacketSize` fails with the
device, PipeWire's thread disconnects, ScreenCaptureKit reports a stop
reason. The microphone capturer is unchanged — a microphone always streams,
so a timeout there is still a stuck device.

**The guest plays on its own thread.** `Speakers` owns one `AudioPlayer` per
audio stream on a dedicated thread, fed by a 5-chunk (100 ms) queue that
drops when full, because audio already behind the picture is worse than a
gap (ADR 0039). Not a tokio worker: every backend's `push` blocks until the
device has room — WASAPI pre-fills its whole 200 ms buffer, so in steady
state each push waits most of a 20 ms chunk. A device that fails mid-session
(headphones unplugged) is reopened on the default one after 2 s instead of
leaving the rest of the session silent.

## Consequences

- A guest hears the host from the first second of a session, on Windows,
  Linux (PipeWire) and macOS, without anyone pressing anything.
- Audio adds about 96 kbit/s per session and one capture client per guest.
  A host without a capture backend, or whose backend refuses, logs it and the
  session stays video-only (§18), as before.
- WASAPI playback runs about 200 ms behind real time (its pre-filled buffer)
  plus the network. Not changed here; it is the same in the microphone path.
- There is no mute button on either side. The host can still stop audio for
  a session through `audio_toggle`; a guest that wants silence turns its own
  volume down.
- The host-side guest-microphone pass still pushes on a tokio worker; the
  same move to its own thread would suit it, and is not part of this change.

## Verification

`view::tests::host_audio_follows_the_picture_and_reaches_the_speakers` runs a
tone through `run_audio_loop`, real Opus and a local QUIC pair into
`spawn_audio_pass` with recording speakers: no stream appears before the
picture's latch is raised, the first stream the guest accepts is the
picture's, and the speakers receive the tone. It fails as it should with the
latch wait removed ("audio opened a stream before the picture's") and with
the speakers unplugged from the decoder ("heard only silence"). Green on
Windows and on Linux (WSL Debian).

A throwaway probe on the Windows reference machine, with nothing playing,
read WASAPI loopback four times: each read took 2.0 s and returned a
1920-sample chunk of silence — the read that used to end the audio loop.

Not yet heard on a real pair of machines, and the macOS change is not
compiled here (no Mac reachable from this checkout).
