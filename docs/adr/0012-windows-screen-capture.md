# ADR 0012 — Windows screen capture via DXGI Desktop Duplication

Status: accepted
Date: 2026-08-18

## Context

§19 phase 2 asks for a real capture backend per platform. Linux got one
(X11, ADR 0003; the portal path, ADR 0010), but
`crates/media/src/capture/windows.rs` stayed a stub that returned
`CaptureUnavailable("phase 2: windows capture not implemented yet")` from
every method — so a Windows host could complete a consent cycle and then
show a guest nothing. Real Windows hardware with a live desktop session was
available here, so unlike the platforms ADR 0007 records as genuinely
unbuildable on this machine, there was no excuse for it to stay a documented
gap.

## Decision

`crates/media/src/capture/windows.rs`, behind a new `capture-windows`
feature (`crates/media/Cargo.toml`, matching the `capture-x11`/`encode-mf`
convention: `cargo build --workspace` still needs no platform SDK by
default), implements `ScreenCapturer` on **DXGI Desktop Duplication**
(`IDXGIOutputDuplication`), not Windows.Graphics.Capture and not the `scap`
crate the original stub comment anticipated.

Desktop Duplication was chosen because its shape already matches the
`ScreenCapturer` contract instead of having to be bent into it:

- `AcquireNextFrame` is a synchronous poll with a timeout, so `next_frame`
  needs no `WinRT` dispatcher, no callback thread and no frame-pool
  bookkeeping. WGC's `Direct3D11CaptureFramePool` would have needed either a
  message pump or the free-threaded variant plus its own frame queue, to
  reach the same place.
- `DXGI_ERROR_ACCESS_LOST` is documented as the error for a desktop switch,
  a session lock or a mode change — §18's `CaptureInterrupted` verbatim,
  reported by the OS rather than inferred from a stalled stream.
- Adapter/output enumeration is native to DXGI, so `CaptureTarget::Display`
  is a real monitor index (`EnumAdapters1` × `EnumOutputs`, attached outputs
  only) and `PrimaryDisplay` is the output whose desktop rectangle sits at
  the virtual-screen origin. That check needs no `GetMonitorInfoW` and no
  `Win32_UI_WindowsAndMessaging` bindings.
- WGC also draws a yellow capture border that is not reliably suppressible
  across Windows versions; §11's indicator requirement is the host UI's job,
  not an OS decoration this crate cannot control.

Frames arrive as `DXGI_FORMAT_B8G8R8A8_UNORM`, which is the existing
`PixelFormat::Bgra8` — the same variant X11 produces and both encoders
already consume, so nothing on the encode side changed and no new variant
was added.

The feature enables only the `windows` crate's Direct3D 11/DXGI binding
modules (`Win32_Graphics_Direct3D`, `_Direct3D11`, `_Dxgi`, `_Dxgi_Common`,
`_Gdi`). The `windows` dependency itself is already non-optional on Windows
(ADR 0007's decoder sandbox needs it unconditionally), so the default build
is untouched: `capture::windows` still compiles, as a stub that names the
feature in its error message, and anything referring to
`capture::windows::WindowsCapturer` keeps compiling either way.

This is the fourth module allowed `unsafe_code` (after `decode::shm`,
ADR 0005; `decode::windows_sandbox`, ADR 0007; `encode::windows`, ADR 0011):
every `IDXGIOutputDuplication`/`ID3D11Device` call in the `windows` crate is
`unsafe fn` because it crosses into COM. Every block carries a `SAFETY:`
note per §21, and the crate-level carve-out list in `lib.rs` names it.

Two things this module deliberately does *not* do, both departures from
`encode::windows`: it never calls `CoInitializeEx` (DXGI factory creation
and `D3D11CreateDevice` are plain DLL exports needing no apartment, so
joining one could only disturb a caller that already has), and it needs no
`unsafe impl Send` (windows-rs already marks the D3D11/DXGI interfaces used
here `Send`, because they are agile).

### What running on a real screen caught that a clean compile did not

The implementation compiled and passed clippy on the first attempt. Both of
the following were found only by capturing an actual desktop:

1. **The first frame published was a solid black picture of the host's
   screen.** `next_frame` originally force-delivered the first acquired
   frame regardless of `LastPresentTime`, reasoning that a newly attached
   viewer has nothing to diff against and needs a full picture. Measured,
   the very first `AcquireNextFrame` after `DuplicateOutput` returns
   `LastPresentTime == 0, AccumulatedFrames == 0` and a surface that is
   uniformly zero — not a stale image, an uninitialized one. There is no
   such thing as a valid desktop image without a present behind it, so the
   rule is now unconditional: `LastPresentTime == 0` means `None`. A viewer
   attaching to an idle desktop sees nothing for a few milliseconds instead
   of seeing a lie, and the first real present delivers a complete frame.
   This is the failure mode worth being loudest about: it is silent,
   it looks like a working capture, and what it leaks to the guest is a
   wrong picture of the host's screen.
2. **`LastPresentTime` and the dirty-rect metadata do not mean "the pixels
   changed".** §11.1 requires `None` when a frame is identical to the
   previous one, and the OS signal looked like the cheap, exact answer.
   Measured over 599 acquires on this machine, 14 frames arrived with
   `LastPresentTime != 0` *and* non-empty dirty rects while being
   byte-identical to the frame before them; on a settled desktop, 13
   consecutive presents were bit-for-bit identical. Windows repaints
   regions that end up visually unchanged. So the backend now uses the OS
   signal as a free first filter and then hashes the frame with `blake3`,
   exactly as the X11 backend does. Hashing a 4K frame is far cheaper than
   encoding and shipping a duplicate of it, which is what the OS signal
   alone would have done.

A third, smaller finding: `DuplicateOutput` returns `E_INVALIDARG` — not
`DXGI_ERROR_NOT_CURRENTLY_AVAILABLE` — when the calling process already
duplicates that output. One duplication per output per process is a hard
limit, so `start` drops any previous one before opening a new one, the
error is mapped to a message that says so, and the two tests that duplicate
a real screen serialize on a mutex rather than racing each other into a
false "no display available" skip.

Both of the first two are recorded here rather than only in a commit
message because they are the same class of "looked right in review, wrong
against the real thing" mistake ADR 0011 documented for the encoder, and
§24.5 asks to degrade towards safety and say so.

### What is still not covered

`capture::windows` is implemented but not yet selected: `platform_capturer`
in `crates/media/src/capture/mod.rs` still only returns the X11 backend,
because that function is shared with the parallel macOS capture and desktop
pipeline work and is theirs to wire. Nothing else in the crate reaches this
backend until that one-line arm exists.

Multi-monitor is enumerated but only ever captures one output at a time;
there is no combined virtual-desktop target. The mouse cursor is not
composited into the frame — Desktop Duplication reports pointer position
and shape separately, and drawing it is left to a later change. Input
injection on Windows is unimplemented: `input_capability` reports `Full`
because `SendInput` can do it, but no `InputInjector` exists yet, so
`platform_injector` still returns `InputUnavailable` on Windows and a
session degrades to view-only per §18.

## Consequences

A Windows host can capture its screen for real: verified end to end on this
machine against a live desktop session, capturing 3840x2160 BGRA8 frames,
tightly packed, with real desktop content — not merely compiled. Monitor
enumeration, primary-display selection, out-of-range display indices,
frame-identity suppression and the refuse-before-start/after-stop contract
are covered by tests that degrade gracefully (skip, not fail) on a headless
or session-0 runner, matching the X11 backend's test style.

The default `cargo build --workspace` is unchanged: no new dependency, no
new `windows` crate features, the stub still compiles. Windows capture only
exists in a build that asks for `capture-windows`.

## Alternatives considered

- **Windows.Graphics.Capture (WGC).** The newer API, per-window as well as
  per-monitor, and the one Microsoft steers new code towards. Rejected for
  this pass: it needs `WinRT` activation and either a dispatcher thread or
  the free-threaded frame pool plus a queue to satisfy a synchronous
  `next_frame`, it draws a capture border that is not reliably suppressible
  across versions, and it gives no equivalent of `DXGI_ERROR_ACCESS_LOST`
  as a first-class interruption signal. Worth revisiting if per-window
  capture becomes a requirement — Desktop Duplication cannot do it.
- **The `scap` crate**, which the stub's own comment named as the MVP path.
  Rejected on the same ground §5/§20 rejects a beta `quinn` dependency:
  it is pinned at `0.1.0-beta.1` (ADR 0002), and screen capture is a
  security-critical path that should not sit on a beta crate when the
  platform API underneath it is directly reachable through a dependency the
  crate already carries unconditionally.
- **GDI `BitBlt` of the desktop window.** Simple, no new bindings, works
  everywhere. Rejected: it is a CPU-side blit of the whole screen every
  frame with no change signal at all, it misses hardware-composited and
  layered content, and it is markedly slower at 4K than a GPU copy plus a
  staging read-back.
