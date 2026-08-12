# ADR 0003 — Platform scope: X11 first, Wayland later

Status: accepted
Date: 2026-08-12

## Context

§11 of the design document treats Wayland via xdg-desktop-portal/PipeWire as
the primary Linux capture path and X11 as the lower-trust one. §19 phase 4
lists Wayland under platform hardening.

## Decision

Target platforms, in order:

- **Hosts:** Windows and Linux/X11 first, then macOS.
- **Viewers:** Windows, Linux, macOS, Android, iOS.
- **Wayland:** supported later; `crates/media/src/capture/linux_wayland.rs`
  exists with the normative portal call order documented, but it returns
  `CaptureUnavailable` until then.

X11 keeps its lower-trust status: a visible on-screen indicator is mandatory
for the whole session, exactly as §11 requires.

## Consequences

The Linux acceptance criterion of phase 2 is met on X11. Wayland gets its own
milestone before any Linux release is called complete, because on current
distributions Wayland is the default session and X11-only capture silently
degrades there.
