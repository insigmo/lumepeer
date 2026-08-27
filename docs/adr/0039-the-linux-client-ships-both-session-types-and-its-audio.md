# ADR 0039 — The Linux client ships both session types, and its audio

Status: accepted
Date: 2026-08-28

Supersedes the deferral in [ADR 0003](0003-x11-first-wayland-later.md).
Builds on [ADR 0010](0010-wayland-capture-and-input.md) (the portal handshake)
and [ADR 0025](0025-platform-backend-pairs-only-where-the-session-forces-it.md)
(why capture and input come from one session).

## Context

ADR 0003 shipped X11 first and deferred Wayland deliberately. ADR 0010 then
recorded the portal path as implemented. Both were true about the source tree
and neither was true about the product:

- `capture-portal` appeared in no build. `Taskfile.yml`'s
  `LINUX_MEDIA_FEATURES` and both Linux rows of the release matrix listed
  `capture-x11,encode-openh264`, so every `.deb` and `.rpm` ever published
  contained the X11 backend and nothing else. On a Wayland session — the
  default on current GNOME and KDE — that client had no capture path at all.
- The portal code **did not compile**. `cargo clippy -p lumepeer-media
  --features capture-portal`, which `ci.yml` has run since the feature landed,
  failed with two borrow errors in `capture/pipewire_stream.rs` and two
  unresolved trait methods in `capture/linux_wayland.rs`'s tests, plus seven
  lints under the `-D warnings` the same line asks for. A feature no build
  turns on is a feature no build breaks, so this sat green-looking and broken.
- `host_monitors()` was implemented for Windows only. Every other platform got
  one monitor of `width: 0, height: 0`. The comment there said so honestly and
  said it was temporary.
- `capture_audio/linux_pipewire.rs` was a stub: `start()` opened a PipeWire
  core, dropped it, and returned `CaptureUnavailable` unconditionally. It had
  never been compiled either — it called `MainLoop::new()` with no argument
  against a 0.8 API that takes one.
- `audio-capture-pipewire = []` declared no dependency. It compiled only when
  `capture-portal` happened to pull `pipewire` in, so it could not be enabled
  on its own.
- `playout::platform_player()` was Windows-only, so a Linux guest heard
  nothing from the host either.

The net effect: a Linux client that could not capture a Wayland desktop, could
not enumerate its monitors, and was deaf in both directions.

## Decision

**One binary carries both session types.** `capture-portal` joins
`capture-x11` in `LINUX_MEDIA_FEATURES` and in both Linux rows of the release
matrix. `capture::detect_session_type()` chooses at run time, and `Unknown`
goes to the portal — with no signal either way, the portal is the path that
asks the user, and a wrong guess towards X11 captures nothing on a Wayland
desktop.

**Monitor enumeration is `RandR`, not X screens.** A multi-head X display is
one X screen spanning every head, so `setup.roots` reports one monitor on a
three-monitor machine. `RandR` 1.5's `GetMonitors` reports the heads
themselves, and the order it replies in is the order `CaptureTarget::Display`
indexes. `X11Capturer` crops the root image to the chosen head's rectangle and
offsets the composited cursor to match.

`RandR` does not promise a primary head — Xvfb and Xwayland both report
`primary: false` for their only monitor — so when none is marked, the first
head takes the flag. Exactly one entry always carries it; a `MonitorsList`
with no primary leaves the guest with no monitor to open on.

**On Wayland the list is always exactly one entry, and that is not a
limitation to be fixed later.** A portal session grants one stream, chosen by
the user in the portal's own dialog, and the guest's `MonitorSelect` cannot
move it. Announcing three monitors there would promise a choice
`CaptureTarget::Display` has no way to honour. Its size comes from the
negotiated stream when one exists, and otherwise from the primary head as
Xwayland's `RandR` reports it — the same compositor's geometry, available
without raising a second dialog to ask for it.

**Linux audio is PipeWire in both directions.** The monitor capturer is
implemented for real, against the default sink's monitor port
(`STREAM_CAPTURE_SINK`, which is what distinguishes "what the speakers are
playing" from "the microphone"). A matching `Direction::Output` player lands
in `playout::linux_pipewire`. Both convert through the existing `to_wire_pcm`
/ `to_device_pcm` pair rather than growing a second converter, and both read
back the format PipeWire actually negotiated instead of assuming the one they
asked for.

The two directions differ in one deliberate way. Capture blocks when its queue
is full; playback drops. A dropped frame of video is worth less than the
latency of waiting for it, and a dropped chunk of audio is an audible click —
but audio already 160 ms behind is worse than a gap, so the playback queue is
shallower and sheds rather than grows.

**`audio-capture-pipewire` declares `dep:pipewire`.** A feature that compiles
only because another feature happened to be on is a feature that will break
the first time someone enables it alone.

**Package dependencies are declared.** `pipewire-sys` links
`libpipewire-0.3.so`, so it is a `NEEDED` entry and the binary will not start
without it: `depends` on `libpipewire-0.3-0` (`pipewire-libs` on RPM), and
`recommends` `xdg-desktop-portal` and a running `pipewire`, which are needed
at run time but not linked.

## Consequences

- A Linux `.deb` is bigger and pulls a runtime dependency it did not before.
  That is the price of the client working on the desktops people actually run.
- `CaptureTarget::Display(n)` now means a `RandR` head, not an X screen. On
  the single-screen multi-head setups that make up essentially all of them,
  this is a fix, not a change: it is the difference between "monitor 2" being
  selectable and being silently ignored.
- CI gained a line that clippies the whole shipped Linux feature set together,
  not only one feature at a time. The one-at-a-time lines catch a missing
  `dep:`; the combined line catches a module that only compiles when some
  other feature happens to be on, which is exactly how the audio feature
  reached this state.

## Verification

Compiled and linted clean at `-D warnings` for `capture-x11`,
`capture-portal`, both together, `audio-capture-pipewire`, and the full
shipped set. 96 tests pass under Xvfb with `LUMEPEER_TEST_XTEST=1`.

Run against a live Debian 13 KDE/Wayland session with a real PipeWire graph:

- **Audio, both directions, end to end.** A 440 Hz tone written through
  `PipewirePlayout` came back through `PipewireMonitorCapturer`'s sink
  monitor: 100 chunks, every one exactly one Opus frame, peak amplitude 12000
  — the value the generator wrote. This is the whole path, playout through a
  real sink through the monitor back into capture.
- **Monitor enumeration.** `host_monitors()` returns `1718x920 primary=true`
  on both a Wayland session (via the Xwayland fallback) and an X11 session
  (directly). Before this change it returned `0x0`.
- **The portal handshake reaches its dialog.** `platform_backend()` returns a
  paired injector and `start()` raises the portal's own consent dialog.

**Not yet verified:** the portal path past the dialog — frames arriving,
injection landing, consent revocation stopping capture, the session being
closed from the portal side. Each needs a human to accept the dialog on that
desktop, and nobody was at it. The code is the same code ADR 0010 described,
now compiling and shipped; that it *runs* is still unproven and this ADR
should be updated when it is.

Also unverified: the `.deb`/`.rpm` dependency declarations have not been
exercised by an actual package install, only written.
