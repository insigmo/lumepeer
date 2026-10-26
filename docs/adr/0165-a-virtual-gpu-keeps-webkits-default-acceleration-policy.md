# ADR 0165 — A virtual GPU keeps WebKit's default acceleration policy

Status: accepted
Date: 2026-10-26

Reverses the part of [ADR 0129](0129-a-virtual-gpu-composites-on-mesas-software-renderer.md)
that kept [ADR 0125](0125-the-linux-start-waits-on-nothing-and-a-virtual-gpu-draws-in-software.md)'s
`HardwareAccelerationPolicy::Never`.

## Report

On the Debian VM, reinstalled with Cinnamon on Wayland (VMware SVGA II,
`vmwgfx` with 3D off, Mesa 25.0.7, WebKitGTK 2.54.0 from `trixie-security`),
the main window of v0.0.158 stayed white for as long as the app ran, after
every start, the autostarted one included.

## Cause

Measured, not inferred:

- The page was alive. Through WebKit's remote inspector the DOM was the whole
  main window, `document.visibilityState` was `visible` and
  `requestAnimationFrame` ran at about 70 frames a second; the codec report
  reached the log. Replacing the page's body with a red block from the
  inspector changed nothing on screen.
- The window only ever showed GTK's own background. A Wayland trace showed GTK
  committing frames normally; WebKit's widget put nothing in them. On X11
  (ADR 0118) and on Wayland alike.
- ADR 0129's restart had happened (`software_gl=1`, llvmpipe in both
  processes), so this is not ADR 0129's `vmwgfx` fence loop.
- The same release binary, run in WSL with WebKitGTK 2.54.0, Mesa's software
  renderer and `/sys/class/drm` faked to show a `vmwgfx` card, was white too;
  without the fake card it drew. A debug build with the `on_page_load` hook
  that sets the policy turned off drew under the fake card; with it on it was
  white. The same build with the hook on drew under WebKitGTK 2.52.6.

So setting `HardwareAccelerationPolicy::Never` on each webview as its page
starts loading leaves the window blank on WebKitGTK 2.54. A standalone
WebKitGTK 2.54 window on the same VM drew with the policy set before, at and
after its load started, so the exact trigger inside Tauri's path is not known.

## Decision

The webviews keep WebKit's default policy on a virtual GPU too. The
`on_page_load` hook, the function behind it and the direct `webkit2gtk`
dependency it needed are gone. ADR 0129's restart on `LIBGL_ALWAYS_SOFTWARE=1`
stays and is what draws the windows there; the log line that names the driver
stays.

## Consequences

- On a virtual GPU the page's layers are painted by Skia on llvmpipe instead
  of Skia on the CPU, which ADR 0129 called the more expensive of the two. The
  page is mostly static, so this is CPU spent per change, not per frame.
- Not run on a VMware VM with 3D on. With `LIBGL_ALWAYS_SOFTWARE=1` Mesa does
  not touch `vmwgfx` either way, which is why ADR 0129's restart was enough
  there; a run with 3D on would confirm it.
- Every installed release from v0.0.113 to v0.0.159 shows a white window on a
  virtual-GPU machine once WebKitGTK 2.54 is installed.
