# ADR 0010 — Wayland capture and input implemented

Status: accepted
Date: 2026-08-14
Supersedes: ADR 0003 (Wayland half)

## Context

ADR 0003 deferred Wayland: the portal handshake in
`crates/media/src/capture/linux_wayland.rs` was normative but frame
consumption and input injection were stubs (`CaptureUnavailable`). See
`docs/superpowers/specs/2026-08-14-wayland-capture-design.md` for the full
design.

## Decision

- `WaylandPortalCapturer` consumes the granted PipeWire stream (via the
  `pipewire` crate, `MemPtr` buffers, one thread per stream) and produces
  real frames.
- A new `WaylandPortalInjector` injects input through the same portal
  session's `RemoteDesktop` interface (`notify_pointer_motion_absolute`,
  `notify_keyboard_keycode`, `notify_pointer_button`, `notify_pointer_axis`).
- Capture and input share one portal session — one consent dialog — because
  the `notify_*` calls require the same `Session` handle used for
  `SelectDevices`/`Start`. `platform_capturer()`/`platform_injector()` are
  replaced by a single `platform_backend()` returning the paired
  `(ScreenCapturer, InputInjector)`, with runtime session-type detection
  (`XDG_SESSION_TYPE`/`WAYLAND_DISPLAY`) choosing X11 or Wayland.
- DMA-BUF/EGL zero-copy import stays out of scope; frames are copied out of
  `MemPtr` buffers.

## Consequences

Wayland hosts get real capture and input, not a documented gap. The Linux
acceptance criterion of phase 2 (ADR 0003) now covers both session types.
CI's `capture-portal` feature build needs `libpipewire-0.3-dev` installed;
no CI job exercises live PipeWire/portal IO, matching how portal negotiation
was already treated (clippy/build only, real verification is manual).
