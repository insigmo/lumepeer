# ADR 0156 — A Mac sends its sound and its microphone, and never hands a guest its own voice back

Status: accepted
Date: 2026-10-14

The owner's report: "Sound on the Mac doesn't work, it needs fixing." Then:
"I want the microphone to work from the Mac, and the sound too, and my own
microphone to reach it."

Amends [ADR 0137](0137-host-audio-comes-with-the-picture.md) (macOS host
capture) and [ADR 0147](0147-sound-follows-the-device-and-the-microphone-follows-the-session.md)
("macOS says it has no microphone").

## What was found

1. **The test Mac had no audio device at all.** The Mac is a VMware VM
   (`macos_new` on `beta`); `system_profiler SPAudioDataType` listed no
   devices, so nothing on it could play, and the session faithfully carried
   silence. The owner installed BlackHole (a virtual output and input) and
   `coreaudiod` was restarted to load it. That is the test machine, not the
   product: every real Mac has at least its built-in speaker.
2. **A Mac host's sound never reached a guest, on any Mac.** With an output
   device in place, ScreenCaptureKit delivered an audio buffer every 20 ms,
   and `extract_audio` rejected every one:
   `CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer` answered
   `-12737` (`kCMSampleBufferError_ArrayTooSmall`) for the fixed 136-byte
   list it was handed. The name misleads: the list was not too small.
   CoreMedia accepts only the exact size it needs — measured with a
   synthesized buffer of ScreenCaptureKit's format (two planar float32
   channels): 40 bytes needed, 40 accepted, 48 and 136 refused with -12737.
   The rejection was logged at debug level and the capture
   loop then answered silence on its read timeout, so a session looked
   exactly like a Mac with nothing playing. The path had never been heard
   since it was written (ADR 0137 noted the macOS edit was not even
   compiled then).
3. **A Mac guest could not send its microphone.** There was no macOS
   microphone backend; `mic_supported()` was false and the button answered
   `NO_MICROPHONE` (ADR 0147).
4. **A guest's voice could come back to it from a Mac.** The host plays the
   guest's microphone itself (ADR 0028), and ScreenCaptureKit's desktop
   audio includes every process's sound unless told otherwise.

## Decision

**Ask CoreMedia how big the buffer list is, and pass exactly that.**
`extract_audio` first calls
`CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer` with no list to
get the size it needs, then hands it storage of at least that size and
that exact size as `bufferListSize`. A refusal now says how many bytes were
needed and given, and the channel count and format flags. The fixed
`AudioBufferListStorage` is gone.

**Say once whether the capture hears anything.** The first ScreenCaptureKit
audio buffer of a stream is logged at info with its rate and channels, or as
a warning when it cannot be read. A capture that hears nothing no longer
looks like a quiet Mac in the host log.

**A macOS microphone: an `AudioQueue` input queue.** `capture_audio::
macos_mic::CoreAudioMicCapturer` opens the default input device at its own
nominal rate (the playout's "ask, don't assume", through the playout's
`default_device_sample_rate`, now shared), asks the queue for mono f32,
copies each filled 20 ms buffer out in the callback and gives the buffer
straight back, and cuts what arrives into wire chunks with `to_wire_pcm`.
A reader that falls 320 ms behind loses the oldest audio rather than
stalling the queue; nothing at all for `READ_TIMEOUT` is a stuck device, as
for every microphone. It lives under `audio-playout-coreaudio`, the same
binding set, the way `audio-capture-pipewire` carries both directions on
Linux. `mic_supported()` is true on macOS; `NO_MICROPHONE` remains for a
build without a backend.

**The bundle may ask for the microphone.** `apps/desktop/src-tauri/
Info.plist` (merged by Tauri) declares `NSMicrophoneUsageDescription`:
without it macOS ends the process the moment it touches an input device.
The release build runs under the hardened runtime, which refuses the
microphone without `com.apple.security.device.audio-input`; that is in
`Entitlements.plist`, wired through `bundle.macOS.entitlements`. Neither
changes the designated requirement a Screen Recording grant is filed
against (ADR 0128). macOS asks the person the first time; until they allow
it, what arrives is silence, which `AudioMeter` shows as chunks with
nothing non-zero.

**The capture leaves out Lumepeer's own sound.** The audio stream sets
`excludesCurrentProcessAudio`: what the host process plays is the guest's
microphone, and captured with the rest it would be sent straight back.
Windows (WASAPI loopback) and Linux (PipeWire monitor) still capture it;
see Consequences.

**The Windows microphone chunks by its own rate.** It took 960 device frames
per chunk whatever the device's rate — the fault ADR 0147 fixed for loopback
only — so a 44.1 kHz microphone lost the tail of every chunk. It takes
`frames_per_chunk(rate)` now; at 48 kHz, the common rate, nothing changes.

## Consequences

- A Mac host's sound reaches its guests.
- A Mac guest can send its microphone; the person allows it once.
- A guest talking to a Mac host does not hear itself delayed.
- A guest talking to a Windows or Linux host still does: their desktop-mix
  capture includes the host's playback of that guest's microphone. Process
  loopback with the host's own process excluded (Windows) and a monitor that
  leaves Lumepeer's own stream out (PipeWire) would fix it; not done here.

## Verification

Unit, on the Mac: `a_screencapturekit_audio_buffer_is_read` builds a
`CMSampleBuffer` in ScreenCaptureKit's audio format and reads it through
`extract_audio`; with the list size passed as before (storage size, not the
needed size) it fails with -12737. 27 macOS media tests pass; clippy
`-D warnings` clean on macOS (media, ScreenCaptureKit + CoreAudio features)
and Windows.

e2e, 2026-10-14, `test_audio.py`, guest = this Windows machine, host = the
Mac VM with BlackHole 64ch as its output and input (both PASS, on fresh
builds of both sides):

| pair | sound (host's 880 Hz tone at the guest) | mic (guest's microphone at the host) |
|------|------|------|
| win→mac | 878 Hz, 151 loud chunks, 307 played, 0 dropped | 123 chunks reached the Mac, 122 played |

Echo: with the guest's microphone playing on the Mac (291 non-silent chunks
played in 6 s), the Mac sent 306 chunks back, none of them non-zero.

The macOS microphone, on its own (the session path needs a host the Mac can
reach as a guest; see below): `CoreAudioMicCapturer` in a bundle with the
e2e app's identity read BlackHole's input while an 880 Hz tone played into
its output: 50 chunks a second, 879–880 Hz, peak 11499 — the tone's own
-9 dBFS. Started from ssh instead, it delivers the same 50 chunks a second
of zeros: macOS keeps the microphone from a process it cannot ask about.
Under the hardened runtime it heard the tone with and without
`com.apple.security.device.audio-input` on this VM (SIP is off there), so
the entitlement is kept because Apple requires it, not because this
machine showed its absence failing.

Not run: a Mac *guest* end to end. The hosts it could reach all needed a
person: the Linux VM's Wayland screen-cast dialog, or this machine's
installed (elevated) client stopped. What the Mac does as a guest is the
same two backends verified above (CoreAudio playback played the guest's
microphone as a host; the microphone capturer heard the tone) behind code
shared with Windows and Linux.

Test-rig findings, not product faults:

- BlackHole 64ch is a 64-channel output, and ScreenCaptureKit hands the app
  a stereo downmix: a stereo tone arrived 30 dB under (peak 359), the same
  tone on all 64 channels at its own level. `agent.py`'s `play_tone` now
  writes the tone on every channel of the Mac's default output.
- The Mac VM's `coreaudiod` twice stopped starting output (any `afplay`
  hung in `HALC_ProxyIOContext::_TellServerAboutStreamUsage`) and was
  restarted; a step-by-step run — app start, session with capture, the
  guest's microphone playing, session end, app killed — did not reproduce
  it. Cause unknown.
