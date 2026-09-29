# ADR 0129 — A virtual GPU composites on Mesa's software renderer

Status: accepted
Date: 2026-09-29

Amends section 2 of [ADR 0125](0125-the-linux-start-waits-on-nothing-and-a-virtual-gpu-draws-in-software.md),
whose fix was written without a VMware VM to run it on and did not work there.

## Report

On the Debian VM (VMware SVGA II, `vmwgfx`, GNOME on Xorg, WebKitGTK 2.52.6,
Mesa 25.0.7) every Lumepeer window was still white on v0.0.113, which carries
ADR 0125: the main window and the host session bar alike. The run's log said
`a virtual GPU: the windows are drawn in software`, so ADR 0125's branch had
fired.

## Cause

Measured on the VM, not inferred:

- The page never ran. `main.ts` reports the webview's codecs right after its
  first render, and `this webview reported what it can decode` never reached
  the log.
- The main thread of each `WebKitWebProcess` sat in `futex_wait` for as long
  as the app ran, while the app's own main thread was idle in `poll`.
- In a `gdb` backtrace of that web process, its `ThreadedCompositor` thread was
  inside `libEGL_mesa` → `dri_create_fence_fd` → `libgallium` (the `svga`
  driver) → `usleep`, and its kernel time kept growing (~3% of a CPU). That is
  Mesa's `vmwgfx` submit loop, which resubmits for as long as the kernel
  answers busy. Creating the fence WebKit asks for after a frame never
  returned, so the compositor never finished its first frame and the page's
  thread waited on it for ever.

`HardwareAccelerationPolicy::Never` does not avoid any of this: WebKitGTK 2.52
paints the page's layers on the CPU under it but still composites them
through EGL on the GPU. A small standalone WebKitGTK window on the same VM
drew normally under every setting tried, so this is not every WebView on
`vmwgfx`; it is what Lumepeer's windows hit there, in every run measured.

Started with `LIBGL_ALWAYS_SOFTWARE=1`, the same v0.0.113 binary drew its
window, reported its codecs 1.7 s after start, and its web process's main
thread idled in `poll`.

## Decision

When the display is a virtual GPU (the same driver list as ADR 0125) and
`LIBGL_ALWAYS_SOFTWARE` is not set, the process starts itself over as the
first thing `main` does on Linux: `exec` of `/proc/self/exe` with the same
arguments and `LIBGL_ALWAYS_SOFTWARE=1`. Mesa then gives WebKit its software
renderer, llvmpipe, and no command ever reaches `vmwgfx`.

- An environment variable is the only thing that reaches the web processes
  WebKit starts, and setting one in a running process is `unsafe` in edition
  2024, which this crate forbids. `exec` sets it on a fresh image of the same
  process instead, with the same PID, before a log file, a thread or a lock
  exists, so nothing is done twice.
- `/proc/self/exe`, not the binary's path, because after a package upgrade the
  running binary's file is gone and a path would start the new one.
- A `LIBGL_ALWAYS_SOFTWARE` that is already set, to any value, is left alone
  and there is no restart: the user's choice, or the restarted run itself.
- ADR 0125's `HardwareAccelerationPolicy::Never` stays. It moves the painting
  of layers to Skia on the CPU, which is cheaper than Skia on llvmpipe.
- The log line that names the driver now also says what
  `LIBGL_ALWAYS_SOFTWARE` was, so a report shows whether the restart happened.

## Consequences

- On a virtual GPU all of Lumepeer's GL work is done by the CPU. The page only
  draws a 2D canvas, so nothing that used to work is lost.
- Every process Lumepeer starts inherits the variable, and on such a VM so
  does the remote shell (ADR 0079) and anything run from it: a GL program
  started from a guest's terminal renders in software there. A shell started
  on the machine itself is not affected.
- If `exec` fails, the reason goes to stderr and the run goes on unchanged,
  white windows included.
