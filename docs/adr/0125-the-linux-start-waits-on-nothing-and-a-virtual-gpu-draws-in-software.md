# ADR 0125 — The Linux start waits on no resolver, and a virtual GPU draws in software

Status: accepted
Date: 2026-09-28
Amended: 2026-09-29 by [ADR 0129](0129-a-virtual-gpu-composites-on-mesas-software-renderer.md), which found that section 2's fix alone left the windows white on VMware.

Two reports from the Debian VM, both about the first seconds of a run.

## 1. The start waited on the system resolver

**Report.** Lumepeer took a very long time to start on Linux, and its window
was blank until it did.

**Cause.** Measured in the VM's own logs: 17–22 s between `transport: direct
IP paths preferred` and the first `Mainline DHT started`, then another
10–22 s before the second, while the same two lines are ~70 ms apart on
Windows. Both DHTs — the address lookup of ADR 0062 and the rendezvous of
ADR 0113 — are built inside `spawn_actor`, which Tauri's `setup` hook blocks
the main thread on, so the window could not draw for the whole wait.

`n0_mainline` resolves its four bootstrap hostnames while the DHT is being
built, one after another, with `ToSocketAddrs` — the blocking system
resolver, which asks for `A` and `AAAA` together and waits for both. The DNS
server behind VMware's NAT does not answer `AAAA`, so every name cost the
resolver's full timeout. The DHT's socket is IPv4-only and discards every
`AAAA` answer it would have got.

**Decision.** The endpoint's bind looks the bootstrap nodes up itself, `A`
records only, all four at once, through iroh's resolver, with a 3 s bound
(`dns::resolve_mainline_bootstrap`), alongside the relay measurement it
already waits for. Both DHTs are then built on those addresses
(`dns::mainline_bootstrap`), which parse without a lookup. When nothing
resolves, they are handed the names as before, and behave as before.

## 2. A virtual GPU left every window white

**Report.** With 3D acceleration switched on in VMware, the app showed a
white window and nothing else.

**Cause.** WebKitGTK composites pages on the GPU and hands the frames to GTK
as DMA-BUFs; a virtual GPU with 3D acceleration takes the first half and not
the second. It is the well-known blank-webview failure of WebKitGTK in VMs,
for which the usual answer is `WEBKIT_DISABLE_COMPOSITING_MODE=1` or
`WEBKIT_DISABLE_DMABUF_RENDERER=1`. This crate forbids `unsafe`, and setting
an environment variable is `unsafe` in edition 2024.

**Decision.** When the kernel reports a virtual GPU's driver on a display
device (`/sys/class/drm/*/device/driver`: `vmwgfx`, `vboxvideo`,
`virtio_gpu`, `qxl`, `bochs-drm`, `cirrus`, `hyperv_drm`), every webview is
set to `HardwareAccelerationPolicy::Never` as its page starts loading — the
setting `WEBKIT_DISABLE_COMPOSITING_MODE` makes, through WebKit's own API. The
run's log says so once. A machine with a real GPU, including a VM with one
passed through, is untouched, and the WebKit environment variables still work
for anyone who sets them.

## Consequences

- On a virtual GPU the windows are painted by the CPU. The remote picture is
  decoded and drawn by the page either way; what changes is compositing,
  which on a virtual GPU was not buying anything that worked.
- The two DHTs no longer depend on the order the platform resolver asks its
  questions in, on any OS.
