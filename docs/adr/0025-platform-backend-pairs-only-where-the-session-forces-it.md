# ADR 0025 — `platform_backend()` pairs capture and input only where the session forces it

Status: accepted
Date: 2026-08-24
Amends: ADR 0010 (Wayland capture and input)

## Context

ADR 0010 landed the Wayland portal capture and input design and said:

> `platform_capturer()`/`platform_injector()` are replaced by a single
> `platform_backend()` returning the paired `(ScreenCapturer, InputInjector)`

That decision was written when Linux X11 was the only implemented branch of
either function. The code it described was never committed; it sat as
uncommitted work in a worktree while `master` moved on, and `master` has since
given both `platform_capturer()` and `platform_injector()` real Windows and
macOS branches (ADR 0013 for ScreenCaptureKit, the Windows Media Foundation
and duplication work after it).

Applying ADR 0010 literally now would delete those branches: the replacement
`platform_backend()` it specifies knows only X11 and Wayland. That is a
regression on two platforms in exchange for a refactor whose actual
requirement is narrower than "always pair".

The real requirement is only this: on the Wayland portal path the
`RemoteDesktop` `notify_*` calls need the very `Session` handle that
`SelectDevices`/`Start` ran on, so an injector built independently there would
raise a second consent dialog and then inject into a session capture never
claimed. No other platform has that coupling — Windows and macOS build an
injector from process-wide APIs that know nothing about the capturer.

## Decision

`platform_backend()` is **added**, not substituted:

```rust
pub type PlatformBackend = (Box<dyn ScreenCapturer>, Option<Box<dyn InputInjector>>);
pub fn platform_backend() -> Result<PlatformBackend>;
```

It returns `Some` injector only on the Wayland portal branch, and `None`
everywhere else. `platform_capturer()` and `platform_injector()` stay, keeping
their Windows and macOS branches; `platform_capturer()` gains a portal branch
for a Linux build that has `capture-portal` without `capture-x11`, which
previously reported "no capture backend is compiled in".

The optional injector is deliberately `Option<Box<dyn InputInjector>>` rather
than a [`NoInputInjector`]: "this platform builds input separately" and "this
platform has no input at all" are different facts, and only the first leaves
`platform_injector()` worth calling.

`HostMedia` carries the paired injector through to the actor, which seeds
`Actor::injector` from it. When it is `None` the actor keeps the lazy path it
already had — building an injector on the first input event and degrading the
session to view-only if that fails (§18). So Windows and macOS behave exactly
as before this change, including not touching an input API at startup on a
machine that may only ever be a guest.

`detect_session_type() == Unknown` goes to the portal, not to X11: with no
signal either way the portal is the path that asks the user, and guessing X11
on a Wayland desktop captures nothing.

## Consequences

Wayland gets the single-consent-dialog pairing ADR 0010 requires, and Windows
and macOS keep the backends they gained in the meantime. The cost is two
construction paths for input instead of one — the pairing seam and the lazy
seam — which is the honest shape of the problem: one platform couples capture
and input, the others do not.

One coverage regression comes with the ported code and is recorded here rather
than left to be discovered. The old
`an_empty_device_mask_degrades_to_view_only` unit test synthesized a
`PortalGrant` by hand. `PortalHandle` replaces it and cannot be constructed
without a real handshake — by design, since there is no such thing as a
portal handle that did not negotiate — so that test is now gated behind
`LUMEPEER_TEST_PORTAL=1` and needs a live portal and a user to click through
the dialog, like the X11 `LUMEPEER_TEST_XTEST` test. Default CI therefore
covers the portal call *order* and nothing else on this path.

The Wayland code in this change has never been compiled: it is gated behind
`capture-portal`, which needs Linux plus the PipeWire and xdg-desktop-portal
development libraries, and it was ported on Windows. Every symbol it uses was
checked to exist in the current tree and the files parse, but "it builds" is
still an open claim — `cargo clippy -p lumepeer-media --all-targets --features
capture-portal` on a Linux host is what closes it.

## Alternatives considered

- **Apply ADR 0010 literally.** Rejected: deletes the Windows and macOS
  branches of `platform_capturer`/`platform_injector`, which is a regression
  on two shipping platforms.
- **Pair on every platform**, returning a real injector everywhere. Rejected:
  it would build a Windows or macOS injector at startup on a machine that may
  never host a session, and it would turn "no input adapter" from a per-event
  view-only degradation into a startup-time condition. §18 wants the former.
- **Keep only `platform_capturer`/`platform_injector` and let the Wayland
  injector negotiate its own portal session.** Rejected: two consent dialogs
  for one session, and the second session's `notify_*` calls target a handle
  that never ran `SelectSources` — input that silently does nothing, which is
  exactly what §18 forbids.
