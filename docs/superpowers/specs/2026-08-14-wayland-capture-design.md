# Wayland capture + input — design

Status: proposed
Date: 2026-08-14

## Context

ADR 0003 deferred Wayland: `crates/media/src/capture/linux_wayland.rs` has the
normative xdg-desktop-portal handshake (`CreateSession`, `SelectDevices`,
`SelectSources`, `Start`, in that order, per §11) but `start()` always returns
`CaptureUnavailable` after negotiating — no PipeWire frame consumption exists,
and there is no Wayland `InputInjector` at all. On current distributions
Wayland is the default session, so Linux hosts silently degrade to X11-only
(via Xwayland, when reachable) or fail outright.

This spec implements the deferred half: PipeWire frame consumption and portal
`RemoteDesktop` input injection, replacing ADR 0003's Wayland stub.

## Goals

- `WaylandPortalCapturer::next_frame` produces real `Frame`s from the
  PipeWire stream granted by the portal.
- A new `WaylandPortalInjector` injects keyboard/pointer events through the
  same portal session's `RemoteDesktop` interface.
- Capture and input share one portal session (one consent dialog), since
  `notify_*` calls require the same `Session` handle used for
  `SelectDevices`/`Start`.
- Runtime session detection picks X11 vs. Wayland instead of the current
  compile-time-only dispatch.

## Non-goals

- DMA-BUF/EGL zero-copy import. Frames are consumed via `MemPtr` buffers and
  copied; GPU-path import is a future optimization, not required for
  correctness.
- Multi-monitor / multi-stream selection UI. `SelectSourcesOptions` already
  pins `set_multiple(false)`; this stays as-is.
- Wayland decode/encode hardware paths — out of scope, this is capture/input
  only.

## Architecture

### Paired construction

`platform_capturer()` and `platform_injector()` are removed (grep confirms no
callers outside `capture/mod.rs` and its own tests). They're replaced by:

```rust
pub fn platform_backend() -> Result<(Box<dyn ScreenCapturer>, Box<dyn InputInjector>)>
```

`detect_session_type()` reads `XDG_SESSION_TYPE`, falling back to
`WAYLAND_DISPLAY`/`DISPLAY` presence, and returns `X11 | Wayland | Unknown`.
`Unknown` routes to the Wayland path (Wayland is the common default; ADR 0003
already reasons about this).

On the Wayland path, `platform_backend()` builds one
`Arc<Mutex<PortalHandle>>` and returns `(WaylandPortalCapturer, WaylandPortalInjector)`
both holding it. Negotiation happens lazily on the capturer's first `start()`
(matching `CaptureController`'s "no capture without a viewer" rule); the
injector returns `MediaError::InputUnavailable` if it's called before
negotiation completed.

### PipeWire consumption

New optional dependency `pipewire` (pipewire-rs), added under the existing
`capture-portal` feature — the feature is dead weight without it. CI's
`capture-portal` clippy/build job gets `libpipewire-0.3-dev` added to its
`apt-get install` line (`.github/workflows/ci.yml` around the existing
`libx11-dev libxtst-dev` install). No new `cargo test --features
capture-portal` job: negotiation needs a live portal, same as today.

Per granted `node_id` (in practice one, since `set_multiple(false)`),
`start()` spawns an OS thread owning a `pw::MainLoop` + `pw::stream::Stream`.
The thread:

1. Connects to the node id, negotiates `SPA_FORMAT` for `BGRx`/`BGRA` via
   `MemPtr` buffers.
2. On each `process` callback, copies the buffer into a
   `Frame { format: PixelFormat::Bgra8, .. }`.
3. Pushes it through a `std::sync::mpsc::sync_channel(1)`; a full channel
   means the consumer hasn't caught up, so the stale frame is replaced, not
   queued (`try_send`, drop-and-retry-once on `Full`).

`next_frame()` does a non-blocking `try_recv`, then the same blake3
last-hash dedup `linux_x11.rs` already uses, returning `None` when unchanged.

`stop()` signals thread shutdown (drop a flag / close the loop), joins the
thread, then drops the ashpd `Session` — matches the existing "stop forgets
the grant" contract.

### Input injection

`WaylandPortalInjector` maps `InputEventPayload` to
`ashpd::desktop::remote_desktop::RemoteDesktop` calls on the shared session:

| `InputDetail` | Portal call |
|---|---|
| `PointerMove { x, y }` | `notify_pointer_motion_absolute`, `x`/`y` scaled from the normalized 0..=65535 range against the negotiated stream's width/height (same scaling shape as `X11Injector::to_screen`) |
| `Press`/`Release`, `logical < POINTER_BUTTON_LOGICAL_BASE` | `notify_keyboard_keycode` with `event.scancode` (raw evdev keycode, no keysym translation) |
| `Press`/`Release`, `logical >= POINTER_BUTTON_LOGICAL_BASE` | `notify_pointer_button` |
| `Wheel { dx, dy }` | `notify_pointer_axis` (portal has real axes; no per-notch button loop like X11) |

`capability()` mirrors `WaylandPortalCapturer::input_capability`:
`PortalRemoteDesktop` optimistically pre-grant, `None` once a grant with an
empty device mask lands (§18 — a declined dialog degrades to view-only, it's
not an error).

### Error handling

- No portal reachable → `CaptureUnavailable`, same as today.
- Dialog dismissed → `PermissionDenied`, same as today (`map_portal_error`).
- `inject()` before negotiation → `InputUnavailable`, matching
  `NoInputInjector`'s "refuse, not downgrade" posture.
- PipeWire connect/format-negotiation failure → `CaptureUnavailable` from
  `start()`, session-level, not per-frame.

## Testing

- Existing `linux_wayland.rs` tests (portal call order, empty-mask
  degradation) stay as-is.
- New unit tests exercise the PipeWire consumption logic (dedup, channel
  backpressure, thread shutdown-on-stop) against a fake buffer source behind
  a small trait, not a real PipeWire connection — no CI job attempts real
  PipeWire/portal IO, matching the current treatment of portal negotiation
  (clippy/build only).
- Real end-to-end capture+input on a live Wayland session is manual
  verification, same as portal negotiation is today.

## Documentation follow-up

ADR 0003 gets superseded by a new ADR recording that Wayland capture+input
is implemented, referencing this spec.
