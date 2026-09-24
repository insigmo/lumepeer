# ADR 0110 — A host the OS refused capture says so

Status: accepted
Date: 2026-09-24

Extends [ADR 0024](0024-host-media-unavailable-wire-message.md). Found by the
e2e matrix (`e2e/matrix/`): a Mac host without Screen Recording left every
guest on "Waiting for the remote screen" for as long as the window stayed
open, redialing its media stream about every 0.8 s.

## Context

On macOS the first viewer's `CaptureController::add_viewer` calls
`ScreenCapturer::start`, and without Screen Recording that returns
`MediaError::PermissionDenied`. The actor only logged it. The guest then dialed
media, the host's encode loop found no capture running, ended, closed the
stream, and the guest's recovery pass dialed again, without end. The Wayland
portal path does the same when someone dismisses the portal's dialog, except
that the refusal arrives from `next_frame` in the encode loop.

ADR 0024's closed set had no fitting reason. `NoCaptureBackend` would tell the
guest the host *cannot* capture, which is false: someone at the host can grant
the permission.

## Decision

- `MediaUnavailableReason::CaptureDenied` is appended after
  `SecureDesktopActive`. `PROTOCOL_MINOR` goes to 19.
- The host announces it when `add_viewer` fails with `PermissionDenied`
  (macOS), and when the encode loop's `next_frame` does (Wayland portal).
- It is sent only to a guest that advertised `FEATURE_MEDIA_UNAVAILABLE`
  **and** is at minor 19 or later. An older guest would decode the unknown
  variant as malformed and close the connection (§9.1), so it gets what it got
  before: nothing.
- It is terminal for the session on the guest (`ViewStatus::CaptureDenied`,
  code 7): the receiver stops, and the window says the host's system did not
  allow screen recording and that someone there has to allow it.
- It is **not** recorded in `MediaHealth`. A missing backend or encoder is a
  fact about the build; a permission is one click away, and the next session
  must ask the operating system again.
- On the guest, the first terminal reason of a view stands. A guest that
  dialed media before the grant-time announcement arrived can hear a second
  reason from the encode loop that dial started, and that one names the wrong
  cause.

## Consequences

- Fixing the gate exposed that `host_handshake` never stored the guest's
  `Hello` minor: `peer_minor` read 0 on every host-side connection. That also
  kept ADR 0077's directory offers and large-file ceiling (read "in both
  directions") off every host-to-guest transfer. It is stored now.
- A guest below minor 19 watching a host that the OS refused capture still
  waits and redials, as before this ADR.
