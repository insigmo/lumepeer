# ADR 0005 — Phase 2 media backends and the `unsafe` carve-out

Status: accepted
Date: 2026-08-13

## Context

Phase 2 (§19) needs a real capture backend on the development platform,
hardware H.264 encoding, ABR, and a decoder running as its own sandboxed
process. Three points of the design document collide with the phase 0 lint
setup or with what is reachable today:

- §5.1 picks `scap` as the MVP capture crate. On Linux `scap` captures through
  xdg-desktop-portal/PipeWire, which is the Wayland path that ADR 0003 defers.
  The development and first-target platform here is Linux/X11.
- §19 asks for hardware encoding in phase 2, while §5.1 puts the direct
  platform bindings (Media Foundation, `VideoToolbox`, `MediaCodec`, VA-API)
  in the hardening phase, which §19 numbers as phase 4.
- §11.3 requires the decoder to talk to the main process over a shared memory
  ring buffer. A mapping shared between two processes cannot be expressed in
  safe Rust, but phase 0 set `unsafe_code = "forbid"` crate-wide.

## Decision

**Capture.** Linux/X11 is implemented directly against `x11rb`, which is pure
safe Rust, behind the `capture-x11` feature. The `GetImage` request is used
rather than MIT-SHM: MIT-SHM needs a shared segment and therefore `unsafe`, and
nothing has measured that the copy is what breaks a §15 budget. `scap` stays in
the manifest for the Windows and macOS backends. Frame-to-frame deduplication,
which §11.1 expresses as `next_frame` returning `None`, compares a BLAKE3 hash
of the frame.

**Encode.** `probe_hardware` exists and returns `None` on every platform, so
the pipeline runs on the `openh264` software fallback that §18 prescribes. The
hardware backends land with the rest of the platform work in phase 4. AV1 is
refused outright without hardware on both sides, per §11: there is no software
AV1 in v1. Because `openh264` takes its bitrate at construction, an ABR change
rebuilds the encoder and the next frame is a keyframe; at
`ABR_ADJUST_MAX_RATE_PER_SEC` = 1 per second that is acceptable.

**Decoder IPC and `unsafe`.** `crates/media` and `crates/decoder-worker` move
from `#![forbid(unsafe_code)]` to `#![deny(unsafe_code)]`. Exactly one module,
`media::decode::shm`, opts back in with a `reason`. It contains three `unsafe`
blocks: two `MmapMut::map_mut` calls and one cast of header bytes to
`AtomicU32`. Each carries a `SAFETY:` note and each is covered by tests,
including a test that maps the same file twice and observes items crossing
between the two mappings. This is what §21 asks of `unsafe`; the alternative,
serializing every frame through a socket, is explicitly rejected by §11.3.

**Sandbox.** The Linux worker installs a seccomp-BPF filter through
`seccompiler` before it touches any bitstream. It is a deny list, not an allow
list: network and filesystem syscalls return `EPERM`, everything else is
allowed. An allow list over a modern allocator, `openh264` and `tracing` would
be long, libc-version-dependent, and would kill the process on an unrelated
syscall, which is a worse failure mode than the threat it prevents. The
filter's action is `Errno`, not `Kill`, so a refused syscall surfaces as a
decode error rather than a crash. Windows `AppContainer` and macOS
`sandbox_init` are phase 4; until then `sandbox::apply` fails there and the
worker refuses to decode, which is the §11.3 behaviour.

Added dependencies: `x11rb`, `memmap2`, `seccompiler`, `libc`.

## Consequences

The phase 2 acceptance criteria are met on Linux/X11: a real capture is
encoded, decoded in a confined separate process and comes back as a picture,
capture starts with the first viewer and stops with the last, and no frame can
be produced without one.

Windows and macOS gain capture, encode and decode in phase 4, and until then
`platform_capturer` and `sandbox::apply` fail loudly there rather than
degrading silently. The §15 budgets are not yet measured; that is phase 5, and
the `GetImage` copy and the software encoder are the first two candidates to
revisit when they are.
