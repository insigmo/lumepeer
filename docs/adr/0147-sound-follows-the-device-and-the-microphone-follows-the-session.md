# ADR 0147 — Sound follows the default device, the microphone follows the session, and both say what they carried

Status: accepted
Date: 2026-10-08

The owner's report: "Sometimes there is sound when I connect, sometimes there
isn't. Find out why. Then write e2e tests for Windows, macOS and Linux that
check whether there is sound, and whether my microphone reaches the host."

Amends [ADR 0137](0137-host-audio-comes-with-the-picture.md) (a capture
endpoint that goes away no longer ends the session's sound) and
[ADR 0028](0028-remote-sas-and-view-toolbar.md) (the guest's microphone).

## What the logs showed

The guest is the owner's Windows machine; the host it reached most is `beta`.
Every media session in its logs since 2026-10-01 had the host's audio stream
arrive, and beta's logs show its loopback capture starting for every one.
Two kinds of session were silent anyway:

1. **2026-10-08, all three sessions.** The guest could not open its
   speakers: `IAudioClient::Initialize` failed with `0x800706CC` ("The
   endpoint is a duplicate"). A standalone WASAPI probe, no Lumepeer code in
   it, fails the same way on that machine for any process, and its event log
   has the Windows Audio service hanging (`Application Hang`, svchost) and
   being restarted at 00:37 local, with no session running. Since then no
   program there can open an audio client. That is the machine, and only a
   restart of the audio service or of Windows brings it back. Lumepeer said
   so once, in its log, and nowhere a person looks.
2. **2026-10-07 20:54–20:57.** Right after the guest machine unlocked,
   opening its speakers took three minutes; both that session's and the
   next one's speakers opened in the same millisecond once the OS let go.

Reading the code for what else could make sound come and go found four
faults of Lumepeer's own:

3. **Capture and playback stayed on the endpoint that was the default when
   the session started.** A WASAPI client does not follow the default.
   Beta's default output is a monitor's HDMI audio beside USB speakers; when
   Windows moves the default (a display waking, headphones plugged in), every
   program follows it and the loopback left on the old endpoint sends
   silence for the rest of the session. The guest's player had the same
   blind spot in the other direction. And an endpoint that went away ended
   host capture for good: `GetNextPacketSize` failing was "the stream is
   gone" (ADR 0137).
4. **A loopback mix at any rate but 48 kHz was mangled.** WASAPI capture
   took 960 *device* frames per chunk whatever the device's rate: 21.8 ms at
   44.1 kHz (the tail thrown away, a click every chunk) and 10 ms at 96 kHz
   (half of every chunk a repeated sample, at twice real time).
5. **The guest's microphone died with the first media connection.** Its
   stream rode the connection the picture had dialed; a lost link, a codec
   change or a session coming back starts a new one, the old stream ended,
   and the map entry stayed — so the button said "on", pressing "on" again
   was a no-op, and nothing reached the host until it was pressed off and on.
6. **A Linux or macOS guest had no microphone at all**, and the button
   turned on anyway: `mic_toggle` answered before the task found there was
   no capture backend.

## Decision

**Follow the default output.** The WASAPI loopback capturer and the WASAPI
player ask once a second which endpoint is the default console output and
move to it when it changed. A loopback endpoint that fails is reopened on the
current default (after 200 ms, then every 2 s while none opens) instead of
ending capture; the loop keeps answering silence meanwhile. PipeWire's
session manager already moves an unpinned stream with the default, and
ScreenCaptureKit captures the system mix, so neither needed a change.

**Chunk by the device's own rate.** `frames_per_chunk(rate)` is 20 ms of the
device's frames; the WASAPI capturer takes exactly that per chunk, the way
the PipeWire and ScreenCaptureKit paths already did.

**The microphone belongs to the view window.** `MicSession` moved from a map
keyed by peer into `ViewState`, so it lives through a parked session and a
media redial, and ends when the window closes (or the role loses `input`).
Its loop reads the window's media-connection cell and opens a new `M` stream
on whichever live connection the picture rides now, a second after the last
one ended; each stream gets a fresh Opus encoder, because the host decodes
each with a fresh decoder. A press before the first dial is accepted and
streams once the picture's connection exists; a terminal window still
refuses (ADR 0101).

**Linux gets a microphone; macOS says it has none.** The PipeWire capture
stream without `stream.capture.sink` links to the default source, which is
the microphone. `mic_supported()` is false on macOS, and the press is refused
with its own IPC code, `NO_MICROPHONE`, so the button stays off.

**Count what went through.** `AudioMeter` (crates/media) counts, per
direction: streams, chunks, chunks that are not digital silence, chunks at
−30 dBFS or louder and the tone of the latest one, chunks the device took
and dropped, the peak, and whether the device opened or why it failed.
`connection_stats` carries it as `audio_in` (what this side plays) and
`audio_out` (what it sends) on both host and guest rows. The e2e matrix
asserts on it; no screen shows it yet.

**e2e: `test_audio.py`.** `sound`: the host plays an 880 Hz tone through its
default output (agent.py `play_tone`); the guest must decode 880 Hz ±5 % and
its speakers must take it, and a failure names the stage from the meters.
`mic`: the guest turns its real microphone on; the host must receive at
least a second, play it, and it must not be digital silence. It runs for
every pair in `hosts.toml`.

## Consequences

- A session that starts while the host's monitor sleeps, or whose output
  device changes mid-session, keeps its sound on both ends.
- A host whose only output disappears sends silence, not nothing, and picks
  up the next device that appears.
- A guest whose OS audio stack is broken still hears nothing — nothing in
  Lumepeer can open speakers the OS refuses — but `connection_stats` now says
  `device: failed` with the OS's reason, and the log says when a device opens
  again.
- The microphone keeps working across reconnects with no press.
- The host-side microphone playout still pushes on a tokio worker (ADR 0137
  noted it); unchanged here.
- macOS guests still cannot send their microphone; they now say so.

## Verification

Unit: `audio_meter` (tone within 1 %, digital silence vs a noise floor vs
sound), `frames_per_chunk` at 44.1/96/192 kHz, and
`view::tests::the_guest_microphone_follows_the_picture_onto_its_next_connection`
— which fails with the microphone ending on its first connection, the
pre-ADR behaviour ("the microphone never followed the picture onto its next
connection"). `host_audio_follows_the_picture_and_reaches_the_speakers`
now also checks both meters and that a 440 Hz tone is heard as 440 Hz.

e2e, 2026-10-08, `test_audio.py` on real machines (all PASS):

| pair | sound (host's 880 Hz tone at the guest) | mic (guest's microphone at the host) |
|------|------|------|
| win→beta | 872 Hz, 160 chunks played, 0 dropped | 136 chunks played, peak 392 |
| beta→win | 872 Hz, 158 played, 0 dropped | 122 played, peak 19 |
| linux→win | 872 Hz, 171 played | 96 played, peak 2562 (PipeWire mic) |
| linux→beta | 880 Hz, 204 played | 121 played, peak 788 |

Not run: a Linux host (the `debian` VM's GNOME session was locked, so its
screen-cast portal refused and no media connection lasted), and the Mac
(the VM has no audio device at all; BlackHole needs the owner's password).

The owner's machine's audio service hung a second time at 17:42 the same
day, three minutes after a WSL VM started; the e2e run caught it at once —
"the guest decoded the tone but its speakers never took it … device=failed
(0x800706CC)" — and passed after the service was restarted.
