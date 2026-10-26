# ADR 0166 — A desktop with no capture portal says so

Status: accepted
Date: 2026-10-26

Extends [ADR 0024](0024-host-media-unavailable-wire-message.md) and
[ADR 0110](0110-a-host-the-os-refused-capture-says-so.md).

## Report

A guest connected to the Debian VM, reinstalled with "Cinnamon on Wayland
(Experimental)", was admitted with full control and then showed "waiting for
the picture" for as long as its window stayed open.

## Cause

Measured on the host (v0.0.160):

- Cinnamon 6.4 on Wayland has no way for an app to capture its screen. Its
  portal backends (`xdg-desktop-portal-xapp`, `-gtk`) implement neither
  `org.freedesktop.impl.portal.ScreenCast` nor `RemoteDesktop`, so the portal
  frontend exports neither interface, and Muffin 6.4 offers no screen cast
  API of its own that another backend could use.
- The first encode loop's `next_frame` collected the handshake's answer:
  `A portal frontend implementing org.freedesktop.portal.RemoteDesktop was not
  found`, as `MediaError::CaptureUnavailable`. The loop ended in silence, as it
  does for every error without an arm of its own.
- The handshake answers once. Every redial after it found a capturer with no
  stream and no handshake in flight (`capturer not started`), while the
  controller still counted the guest as a viewer and never started it again.
- The guest heard nothing, so its recovery pass redialed the media stream:
  452 times in 6 minutes, about 1.4 a second, for as long as the window was
  open.

## Decision

- `MediaError::CaptureUnsupported` is a new variant: the desktop offers
  nothing to capture its screen with. The Wayland portal path returns it when
  ashpd reports `PortalNotFound` for either interface, and when the session
  bus answers `ServiceUnknown` for the portal frontend itself (no
  xdg-desktop-portal installed; an installed one that fails to start answers
  otherwise). Both shapes were taken from real buses, not from ashpd's
  documentation: a frontend with only `xdg-desktop-portal-gtk` behind it
  fails the proxies' construction with `PortalNotFound`, and a bus with no
  frontend lets the proxies build and fails `CreateSession` with
  `ServiceUnknown` inside `PortalError::ZBus`.
- The encode loop answers it with `MediaUnavailableReason::NoCaptureBackend`
  and ends. `NoCaptureBackend` is part of ADR 0024's first set, so every guest
  that announced `FEATURE_MEDIA_UNAVAILABLE` decodes it and no protocol minor
  changes.
- It is recorded in `MediaHealth`, unlike ADR 0110's `CaptureDenied`: nobody
  at the host can grant a portal the desktop does not have. From then on this
  host refuses a media dial with the announcement instead of starting a loop,
  tells a new guest at its grant, and its own window says it cannot capture.
- Any other portal failure stays `CaptureUnavailable`: a portal that failed a
  call is still a portal.

## Verified

- `a_desktop_without_a_capture_portal_says_so`, against real buses in WSL:
  `dbus-run-session` with xdg-desktop-portal and only its gtk backend
  (`PortalNotFound`), and the same with a bus config that lists no service
  directory (`ServiceUnknown`). The second failed before `no_portal_frontend`
  existed.
- `a_desktop_that_offers_no_capture_ends_the_loop_and_says_so` fails with the
  new arm of the encode loop switched off.
- e2e, a WSL guest against the debian VM in its Cinnamon-on-Wayland session,
  with this build as the host: one media dial, the host's `this desktop offers
  no screen capture` and `NoCaptureBackend`, the guest's window on "No
  picture from this device", and no further dial in the 51 s until the run
  ended. v0.0.160 made 452 dials in 6 minutes there.

## Consequences

- The record lasts for the life of the process. Changing the desktop session
  means logging out, which restarts the app, so a host that moves to a session
  that can capture (Cinnamon on X11) captures again.
- A desktop whose portal has `ScreenCast` but no `RemoteDesktop` (wlroots with
  `xdg-desktop-portal-wlr`) is now told apart the same way, since the §11
  handshake needs both. A view-only handshake on `ScreenCast` alone would give
  such a desktop a picture; that is a separate decision.
- The host's own warning (`status.noCapture`) says input still works. On a
  desktop with no `RemoteDesktop` portal it does not.
- A capture error that is not `CaptureUnsupported` still leaves the capturer
  dead after its one answer, and its guest redialing.
