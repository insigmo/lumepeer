# ADR 0162 — libaom is the one software codec, and a host follows the screen its guest picked

Status: accepted
Date: 2026-10-16

Supersedes the `openh264` fallback of §5.1/§18 and the software-AV1
measurement of [ADR 0142](0142-the-software-av1-budget-is-the-frame-interval.md)
and [ADR 0143](0143-the-guest-can-pick-the-hosts-encoder.md); amends
[ADR 0141](0141-a-host-with-no-hardware-encoder-encodes-av1-in-software.md)
(software AV1), [ADR 0055](0055-an-always-on-top-session-bar-for-the-host.md)
and [ADR 0118](0118-the-session-bar-docks-on-linux-too.md) (the host's session
bar), and the injectors of [ADR 0028](0028-remote-sas-and-view-toolbar.md)'s
monitor picker.

The owner's list of 2026-10-16, in their order:

1. The session bar on macOS and Linux has a white outline instead of a
   transparent one.
2. The bar does not stay at the screen edge; it should be bound to the right.
3. Remove `openh264` and put software AV1 (libaom) in its place.
4. Software AV1 does not work on a Mac.
5. After switching to the second monitor the picture follows, but the cursor
   stays on the first.
6. A monitor in portrait orientation is shown landscape.
7. Connecting to `beta`, there is no session bar — which went away with
   v0.0.153, as the owner found, and is not touched here.

## Decisions

### 1. The session bar's window is transparent on macOS and Linux, and has no border of its own

The card is drawn with an 8 px radius on a window that was opaque, so the
corners outside the radius showed the window's white background, plus a 1 px
`rgb(255 255 255 / 14%)` border over it. Windows 11 rounds an undecorated
window itself and was never affected. `open_host_bar` now builds the window
`transparent(true)` off Windows, and the card has no CSS border.

On macOS a transparent window is behind Tauri's private-API switch: the
workspace's `tauri` dependency gets the `macos-private-api` feature and
`tauri.conf.json` sets `macOSPrivateApi`. Both are set for every platform,
where off macOS they do nothing, because `tauri-build` checks the two against
each other and reads only the main dependency table. The app is not
distributed through the App Store, which is the only place that switch
matters.

### 2. The session bar is held against the right edge of its screen

ADR 0055 let the host drag the bar anywhere. Now `keep_host_bar_docked` checks
every 250 ms, for as long as the bar is up, that it is flush with the right
edge of the monitor it is on, and moves it there when it is not and its
position held still since the previous check — so a drag in progress is never
fought over, but a bar let go in the middle of the screen goes back to the
edge, and one left behind when the screen changed size (a VM going full
screen, a resolution switched) follows the new edge. The host can still slide
it up and down the edge. Positions are read in the frame `set_position`
writes, as `host_bar_expand` reads them (ADR 0118).

### 3. libaom replaces `openh264`, as the host's one software encoder and the decoder worker's decoder

`openh264` was built from source here (`openh264-sys2`), so Cisco's royalty
cover for its own binary never applied to it. It is removed: the
`encode-openh264` and `decode-openh264` features, the encoder module, the
decoder worker's H.264 decoder, the release workflow's rebuild and assembly
check for it (ADR 0157) and `ci/find-openh264-asm.py`.

What takes its place:

- **H.264 comes only from a hardware encoder.** `select_encoder` refuses H.264
  with no hardware encoder for it, and `select_software_encoder` builds libaom
  for AV1 and nothing for H.264.
- **The codec rule** (`choose_media_codec`), in order: AV1 when the guest's
  `WebView` decodes it and the host has a hardware AV1 encoder; H.264 when the
  host has a hardware H.264 encoder; AV1 in software for a guest that decodes
  AV1 at all; H.264 otherwise, which such a host then cannot build, and the
  guest is told the host has no encoder. A hardware H.264 encoder comes before
  libaom because it is cheaper on the host's processor and every `WebView`
  decodes it.
- **No measurement, no refusal, no demotion.** ADR 0141-0145 timed libaom
  against `openh264` once per process, chose it only for the 30 fps preset on
  a screen of at most 1080p, and gave it up when a session fell behind — each
  time in favour of `openh264`. With nothing left to fall back to, all of it is
  gone (`software_av1::Readiness`, the measurement, the `SoftwareAv1Settled`
  event, `SoftwareAv1Watch`, the encode loop's 1080p exit and the actor's
  captured-size bookkeeping). Software AV1 still runs at 30 fps at most
  (`SOFTWARE_AV1_MAX_FPS`), and a host that cannot keep up shrinks the picture
  through ADR 0144's encoder-speed controller instead.
- **libaom's input** is the hardware encoders' BT.601 NV12 conversion with its
  chroma split into I420's two planes (`nv12::to_i420`) — the same arithmetic
  as the `openh264` converter ADR 0141 measured — so a Mac's NV12 capture
  (ADR 0157) is split without a conversion.
- **The decoder worker decodes AV1 with libaom.** `lumepeer-aom-sys` now
  builds libaom's decoder too (`CONFIG_AV1_DECODER=1`) and its shim gains
  `aom_shim_dec_*`; `lumepeer_media::decode::aom` turns an 8-bit 4:2:0 picture
  into RGBA and refuses anything else, inside the worker's sandbox.
- **A guest says its worker decodes AV1.** Every guest of this build adds the
  `Hello` feature `codec-av1-worker` (`FEATURE_CODEC_AV1_WORKER`), whatever its
  `WebView` said. It is not a reason to prefer AV1 — the worker's pictures
  cross the IPC boundary as RGBA (ADR 0058) — only what lets a host with
  nothing but libaom send AV1 to a guest whose `WebView` cannot decode it, and
  it is what tells such a host the guest understands `MediaCodec` at all. A
  host from before this ADR ignores the string.
- **The view window moves to the worker** when a chunk arrives in a codec its
  own decoder did not say it decodes (`needsWorker`): the native loop stops
  and the RGBA loop takes over for the rest of the window, at the RGBA slot's
  smaller picture ceiling (ADR 0060).
- **`EncoderChoice::H264Software` stays on the wire** so the discriminants do
  not move; no host offers it any more, and a guest still labels it when an
  older host does.

### 4. libaom is built for every release target

`lumepeer-aom-sys` used to refuse everything but x86-64 Windows and Linux,
the machines ADR 0141 measured. It now builds for x86-64 and 64-bit Arm on
Windows, Linux and macOS: on x86-64 with nasm's kernels as before, on Arm with
NEON intrinsics, which need no assembler; on macOS with `CMAKE_SYSTEM_NAME`,
`CMAKE_SYSTEM_PROCESSOR` and `CMAKE_OSX_ARCHITECTURES` named, because both Mac
release rows build on Apple silicon. The configuration check asks for each
processor's kernels and for the decoder. `encode-aom` is in all six release
rows and every Taskfile feature set; the x86-64 Mac row installs nasm from
Homebrew. The AVX2 gate of ADR 0141 is gone with the measurement it guarded:
libaom picks its kernels at run time, and an older processor runs the SSE
ones rather than having no picture.

That was item 4: a Mac had no software AV1 at all, because the feature was
inert there.

### 5. A guest's pointer lands on the monitor it is looking at

Every injector mapped a guest's normalized position onto the primary monitor,
whichever monitor `MonitorSelect` had moved the capture to. Now:

- **Windows.** The DXGI capturer records the desktop rectangle of the monitor
  it duplicates (`CAPTURED_MONITOR`), and the host maps a position onto the
  whole virtual desktop in its own process before any injector sees it
  (`capture::for_injection`, called at the top of the actor's `inject` and by
  the session agent). Every Windows `SendInput` move — the in-process
  injector, the `LocalSystem` desktop injector of ADR 0114, and the service's
  secure-desktop injector — sends `MOUSEEVENTF_ABSOLUTE |
  MOUSEEVENTF_VIRTUALDESK`. The mapping is done where the capture is because
  the desktop injector is another process with no capture to ask. On a single
  monitor the virtual desktop is the monitor, and nothing moves. The
  logon-screen worker of ADR 0088 still sends positions over the primary
  monitor; its host, `crates/host`, has never shipped.
- **macOS** maps onto `CGDisplayBounds` of the display the capturer last
  started on (`CAPTURED_DISPLAY`).
- **X11** maps onto the `RandR` monitor the capturer last started on
  (`CAPTURED_TARGET`). Wayland injects through the portal's stream, which is
  already the picked monitor.

### 6. A turned monitor is turned back upright

Desktop Duplication hands a monitor's desktop back as the panel scans it out,
so a monitor turned to portrait arrived landscape, on its side. The capturer
reads `DXGI_OUTDUPL_DESC.Rotation`, and on the readback path turns the picture
upright (`turn_upright`, in 64-pixel tiles) before the cursor is composited on
it in desktop coordinates; the first-frame GDI snapshot is taken at the
upright size. Following Microsoft's duplication sample, `ROTATE90` is one
quarter turn clockwise and `ROTATE270` three. A turned monitor stays off the
zero-copy GPU path (ADR 0073), which hands the encoder the texture as it is.
Monitor sizes announced to the guest were already the desktop's, so the
picker and the pointer mapping need nothing more.

## Consequences

- A host with no hardware encoder sends AV1 to every guest built from this
  release. A guest from before it that decodes only H.264 gets no picture from
  such a host — it is told the host has no encoder — where it used to get
  `openh264`'s H.264.
- Software AV1 now also runs on the presets and screen sizes ADR 0141 kept it
  off: 60 fps presets run at 30, and a screen above 1080p is shrunk until the
  encoder keeps up.
- A guest whose `WebView` cannot decode AV1 — Safari's on a Mac without AV1
  hardware, WebKitGTK without a working GStreamer AV1 decoder — shows a
  software host's picture through the decoder worker, at the RGBA slot's
  ceiling and its per-frame IPC cost. A guest whose `WebView` cannot decode
  H.264 at all, which the worker used to cover, has no picture from a host
  with a hardware H.264 encoder.
- The VA-API tests that decoded the hardware's stream with `openh264` to check
  it are gone with it.
- Verified on this Windows machine: unit tests for the pointer mapping and
  the rotation, libaom encode and decode, the codec rule, and the actor tests
  that stream software AV1 from a host with no hardware H.264 to a guest that
  decodes it in its `WebView` and to one that cannot. On Linux (WSL Debian):
  the same, and the integration test that captures, encodes with libaom and
  decodes in the decoder worker under its seccomp filter. On the Intel Mac
  VM: libaom with its AVX2 kernels, the same integration test under the
  worker's `sandbox_init` profile, and the desktop app with its transparent
  bar compiling. For Windows on Arm: libaom cross-builds with NEON and its
  decoder. Not verified: any of it on a real second monitor, a real portrait
  monitor, an Arm machine, or a live session. The macOS link warns that
  nasm's objects carry no platform load command; `ld` assumes macOS, which is
  right.
