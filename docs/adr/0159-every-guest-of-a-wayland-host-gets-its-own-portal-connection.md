# ADR 0159 — Every guest of a Wayland host gets its own portal connection

Status: accepted
Date: 2026-10-15

Builds on [ADR 0010](0010-wayland-capture-and-input.md) (capture and input
through xdg-desktop-portal) and
[ADR 0088](0088-a-portal-frame-can-stay-on-the-gpu-on-linux.md) (the portal
stream's buffers).

## Context

Reported 2026-10-15: connecting to Linux on GNOME, the session comes up but
there is no picture. The host is `debian` (GNOME on Wayland, v0.0.144 then).
Its logs, one desktop process from 2026-10-13 08:05 UTC on:

- 08:06:26, the first guest after start: the portal's request objects appear,
  and at 08:06:45 `the portal stream negotiated its buffers` — somebody
  clicked *Share* — and the encode loop reports frames.
- 21:04 and 21:16 that day, 20:15 on 10-14 (eight minutes), 08:38 on 10-15:
  `media connection accepted; starting the encode loop`, then nothing from
  the portal at all. No request object, no dialog on the host's screen, no
  frame, until the guest gave up.

So only the first guest of a process ever reached the portal. The cause is
two crates' defaults meeting our per-session runtime:

- ashpd's plain constructors (`RemoteDesktop::new`, `Screencast::new`) share
  one session-bus connection for the whole process, kept in a static.
- zbus, with its `tokio` backend active, spawns a connection's socket reader
  on the tokio runtime the connection was opened on.
- Every handshake runs on a `PortalRuntime` of its own (the nested-runtime
  panics of 2026-09-24), shut down when its guest leaves.

The first guest's runtime took the shared connection's reader with it. Every
later handshake wrote its first call (`version`, read by the proxy
constructor) into a connection nobody read, and waited forever on the
`wayland-portal` thread; the capturer answered "no frame yet" for as long as
the guest stayed. The same static kept the first guest's socket open, so the
portal never learned that guest had left either: its session was closed only
when the app quit (measured below).

## Decision

`portal::connect` opens a session-bus connection for each handshake and
builds both proxies on it (`RemoteDesktop::with_connection`,
`Screencast::with_connection`). The `Session` and every request ashpd makes
take the proxy's own connection, so nothing touches the static any more. The
connection is opened inside the handshake's runtime and lives exactly as long
as the `PortalHandle` that owns that runtime; dropping it closes the socket,
and xdg-desktop-portal closes that peer's session.

## Consequences

- Every guest gets the portal's dialog, not only the first one after start.
- A guest leaving ends the screen-cast session on the compositor at once,
  instead of when the app quits.
- Unchanged: on GNOME a person at the host still has to click *Share* for
  every guest (`PersistMode::DoNot`); an unattended Wayland host shows no
  picture until somebody does. Restore tokens are a separate decision.

## Verified

`a_second_guest_reaches_the_portal_after_the_first_one_left` (opt-in,
`LUMEPEER_TEST_PORTAL_BUS=1`; needs a session bus with xdg-desktop-portal,
raises no dialog): two handshakes' worth of `CreateSession`, each on its own
runtime that is shut down before the next, dropped in `PortalHandle`'s order.
On `debian`'s real GNOME session bus:

- with ashpd's shared connection: guest 1 reached the portal, guest 2 timed
  out after 5 s. `dbus-monitor` showed guest 1's `CreateSession` at
  `:29.20` and its `Close` only at `:34.24`, when the test process exited;
- with a connection per handshake: green 3 runs of 3, and every guest's
  `Close` followed its own disconnect, before the next guest's
  `CreateSession`.

## Not verified

A live guest-to-GNOME session twice in a row: the dialog needs a person at
the host for each guest.
