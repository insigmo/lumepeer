# ADR 0007 — Phase 4 scope: what platform hardening covers, and what it does not

Status: accepted
Date: 2026-08-13

## Context

§19 phase 4 asks for the Wayland portal in the exact order of §11, macOS,
Windows, the keystore on all platforms, input adapters, and an integration test
per row of the error matrix (§18) on each declared OS.

The work was done on a Linux/X11 machine with no macOS or Windows toolchain and
no Wayland compositor. Writing platform FFI that cannot be compiled even once is
not hardening; it is unverified code with a confident comment on top.

## Decisions

**Input authorization lives in the core, not in the adapter.** §11 says every
event is checked by the core before the platform adapter. `SessionManager::
authorize_input` is that check, and it runs per event rather than per session so
that a revoke takes effect on the very next event. The `InputInjector`
implementations never look at grants.

**The `ControlLimited` allowlist is snapshotted at grant time.** §8.2 says a
policy edit applies to future grants and never modifies a running session.
Rather than relying on call-site discipline, the allowlist is copied into the
session when consent is granted, so a later `set_control_policy` cannot widen a
session that is already active. A policy file that fails to parse is an error,
never an empty-and-therefore-permissive one.

**Pointer buttons get a logical-id range.** §9.1 fixes `logical` and `scancode`
but not their namespace, and the host policy has to tell a click from a
keystroke without a per-platform scancode table.
`POINTER_BUTTON_LOGICAL_BASE = 0xF000_0000` splits the space: at or above it is
a pointer button, below it is a key. This is a protocol convention this
implementation introduces; it is visible on the wire and belongs to
`PROTOCOL_MINOR` 0 because nothing shipped before it.

**X11 input goes through XTEST.** Same trust level as the X11 capture path: any
client on the display can already do this, which is why X11 needs the visible
indicator. Guest scancodes are evdev codes, X11 keycodes are those plus 8.

**The injection test is opt-in.** `LUMEPEER_TEST_XTEST=1` enables it, and even
then it only injects a move to the position the pointer already occupies. A
developer running `cargo test` on their own desktop must not have their session
driven by the suite; CI sets the variable because it runs against Xvfb.

**Wayland: the portal negotiation is implemented, the PipeWire consumption is
not.** The normative part of §11 is the call order, and `PortalSession::
negotiate` performs `CreateSession`, `SelectDevices`, `SelectSources`, `Start`
in that order, with a test that pins it. An empty device mask is treated as the
user declining, not as a failure (§18). Turning the granted node id into frames
needs the PipeWire C bindings and a Wayland session to test against; `start`
therefore returns an explicit error after negotiating, rather than pretending.

**Keystore: Linux and macOS are real, Windows is not.** The Secret Service
backend of §11.2 is implemented and tested against a live session keyring, and
the Keychain backend (`security-framework`'s generic-password API, filed under
the Tauri bundle identifier) was added once a macOS machine was available to
build and test it against the login Keychain. The Credential Manager backend
is still not written: it cannot be compiled here, let alone tested, and
`keystore::open` refuses on Windows rather than falling back to the encrypted
file, which would quietly weaken §11.2. That refusal is the honest state, not
a placeholder that appears to work.

**Error matrix coverage.** All 17 rows of §18 have their own test in
`tests/integration/tests/error_matrix.rs`. Rows whose trigger is a platform
event that cannot be raised on demand — a real screen lock, a withdrawn macOS
accessibility permission — are driven through the same entry point the platform
layer calls, which is the part the rest of the system depends on.

## Consequences

Phase 4 is complete for Linux/X11 and partially complete overall. What remains,
each needing a machine that can build and run it:

- PipeWire frame consumption for the Wayland path.
- Windows capture (DXGI/WGC), input (`SendInput`), keystore (Credential
  Manager) and the `AppContainer` decoder sandbox.
- macOS capture (`ScreenCaptureKit`), input (`CGEvent`) and the
  `sandbox_init` decoder sandbox. (Keystore is now done — see above.)
- Running the §18 matrix on each of those OSes, which §19 requires before
  phase 4 can be called done on them.

Until then every one of those paths fails with an explicit error naming what is
missing. The rule from §24.5 applies: when in doubt, degrade towards safety and
tell the user, rather than degrade silently.
