# ADR 0013 — macOS screen capture via ScreenCaptureKit

Status: accepted
Date: 2026-08-18

## Context

§19 phase 2 asks for a real capture backend per platform.
`crates/media/src/capture/macos.rs` had been a deliberate stub since phase 2
opened: every method returned `CaptureUnavailable("phase 2: macOS capture not
implemented yet")`. ADR 0007 recorded macOS as a documented gap because no
Mac was reachable from the development machine at the time. One is now
(macOS 26.3, x86_64), so the gap has no excuse left, the same way ADR 0011
argued for Windows hardware encoding.

§5.1 names `ScreenCaptureKit` as the macOS capture path. It is also the only
one that is not deprecated: `CGDisplayStream` and `CGWindowListCreateImage`
are both gone as of macOS 15, and both predate the Screen Recording TCC
prompt being enforced the way §2 requires.

## Decision

`crates/media/src/capture/macos.rs` implements `ScreenCapturer` on top of
`SCShareableContent`/`SCContentFilter`/`SCStreamConfiguration`/`SCStream`,
behind a new `capture-screencapturekit` feature
(`crates/media/Cargo.toml`, matching the `capture-x11`/`encode-mf`
convention: `cargo build --workspace` still needs no platform SDK by
default, on macOS or anywhere else).

### Which binding, and why

**`objc2-screen-capture-kit` 0.3.2**, with `objc2` 0.6, `objc2-foundation`,
`objc2-core-media`, `objc2-core-video`, `objc2-core-graphics`, `block2` and
`dispatch2` — the `objc2` framework family, the de-facto standard for
macOS/Rust interop.

The decisive argument is that this adds no new ecosystem. Those exact
versions (`objc2` 0.6.4, `objc2-foundation` 0.3.2, `objc2-core-foundation`
0.3.2, `objc2-core-graphics` 0.3.2, `block2` 0.6.2, `dispatch2` 0.3.1) are
already in `Cargo.lock`, resolved through `apps/desktop/src-tauri`'s macOS
dependency tree (tauri, tao, objc2-app-kit). Enabling the feature adds three
framework crates to a graph that already contains a dozen of their siblings,
rather than a second, parallel set of Apple bindings. It is also pure Rust:
`cargo audit`, `cargo deny` and the CycloneDX SBOM of the `supply-chain` CI
job see all of it.

### Alternatives considered

- **`screencapturekit` 8.0.1** (`doom-fish/screencapturekit-rs`, 1.2M
  downloads) offers a genuinely safe, well-documented API — `SCShareableContent::get()`
  is synchronous, `SCStreamOutputTrait` hides the Objective-C delegate, and
  `CVPixelBufferLockGuard::as_slice()` hands back pixels without a single
  `unsafe` block on our side. On the face of it that suits a crate whose
  ground rule is `deny(unsafe_code)` better than raw bindings do, and it was
  the first choice.

  It was rejected after actually building it on the Mac. Since 5.0 the crate
  is not an `objc2` wrapper at all: it vendors a **Swift package** and
  compiles it from `build.rs`, pulling in `apple-cf` and `apple-metal`, which
  vendor Swift bridges of their own. A cold `cargo build` of nothing but this
  crate had not finished after 12 minutes on the test Mac, still running
  `swift-frontend -O -whole-module-optimization` over five bridge modules
  including a Metal one this project has no use for. That buys three things
  we do not want: a Swift toolchain as a hard build requirement for anyone
  enabling the feature, a large amount of Swift source that the
  `supply-chain` job's Rust-only tooling cannot audit, and a second Apple
  bindings ecosystem alongside the `objc2` one Tauri already drags in.
  A safe API is not worth an unauditable one.

- **Hand-written FFI.** No advantage over `objc2-screen-capture-kit`, which
  is generated from the same headers and gets the `msg_send` ABI, ownership
  and `method_family` rules right by construction. Rejected without
  prototyping.

- **`scap`** is already an optional dependency (`capture-scap`, §5) and wraps
  ScreenCaptureKit on macOS, but it owns the whole capture loop rather than
  implementing our trait, gives no access to the frame-status or stop-error
  signals §18 needs, and its macOS path is itself a `screencapturekit`
  wrapper. Rejected.

### The fourth `unsafe` carve-out

`capture::macos` joins `decode::shm` (ADR 0005), `decode::windows_sandbox`
(ADR 0007) and `encode::windows` (ADR 0011) as a module allowed
`unsafe_code`; `crates/media/src/lib.rs`'s crate comment now lists four
rather than three. Two things force it, neither avoidable through a
different binding:

1. Every Objective-C entry point in the generated bindings is an `unsafe fn`,
   because a `msg_send` crosses into a language with no borrow checker —
   exactly the situation ADR 0011 documented for the `windows` crate's COM
   bindings.
2. `SCStreamOutput` is a delegate protocol, so receiving frames at all means
   defining a real Objective-C class (`objc2::define_class!`).

The carve-out is an inner `#![allow(unsafe_code, reason = ...)]` on an inline
module, so it cannot leak onto the stub the file still carries for builds
without the feature. Every block has a `SAFETY:` note, per §21. Two of them
are `unsafe impl Send` (`MacosCapturer`, and a one-shot hand-off of
`SCShareableContent` off the completion handler's queue): ScreenCaptureKit is
free-threaded by construction — every API is asynchronous, frames are
delivered on a dispatch queue the caller nominates, and none of the classes
used here is main-thread-only — and `Retained`'s reference counting is
atomic.

### Contract details worth recording

- **Push versus poll.** `SCStream` pushes `CMSampleBuffer`s at a delegate;
  `ScreenCapturer` is polled. The delegate converts each buffer to an owned
  `Frame` and keeps only the newest: a remote viewer wants the current
  screen, not a backlog. The copy is not optional — the `CVPixelBuffer` is
  recycled the moment the delegate returns.
- **`None` when nothing changed** (§11.1) is decided by the same blake3
  comparison `capture::linux_x11` uses, rather than by
  `SCStreamFrameInfoStatus`. Idle frames do not carry an image buffer and are
  dropped before that, so the hash only has to catch the residual case, and
  reusing the X11 backend's mechanism keeps one definition of "unchanged" in
  the crate instead of two.
- **`CaptureInterrupted`** (§18) comes from `stream:didStopWithError:`, which
  is what fires on screen lock, fast user switching and a permission
  withdrawn mid-session. The reason is sticky: the first one is the real one,
  and it keeps being reported until `stop`, so a caller cannot poll its way
  past a dead capture.
- **Pixel format** is pinned to `kCVPixelFormatType_32BGRA`
  (`PixelFormat::Bgra8`, which already existed). Left to itself
  ScreenCaptureKit picks a bi-planar YCbCr format on recent macOS releases,
  which the rest of the pipeline would misread.
- **`CaptureTarget::Display(n)`** is matched against `SCDisplay.displayID`
  first — that is the stable `CGDirectDisplayID` a host UI would show — and
  only falls back to treating `n` as a position in the enumeration, which is
  what the X11 backend means by the same variant. `PrimaryDisplay` resolves
  through `CGMainDisplayID`.
- **Constants.** `ENCODE_DEFAULT_FPS` comes from `crates/core`'s §14 table.
  The rest (queue depth, completion timeout, the `SCStreamError` codes, the
  BGRA FourCC) are module-local named constants with doc comments, following
  `encode::windows`: they are Apple ABI values and macOS-only tuning, not §14
  tunables, and none duplicates a number that already lives in §14.

### Screen Recording permission

Mandatory, and never worked around. The first `SCShareableContent` request in
a process is what makes macOS show the system prompt; until the user grants
it, that request fails with `SCStreamErrorUserDeclined` (-3801) and `start`
returns `MediaError::PermissionDenied` — an existing variant, so no new one
was needed — carrying the System Settings path the user has to visit. There
is no retry loop, no preflight that could be used to capture without asking,
and no alternative capture path: an ungranted prompt ends the attempt. That
is the §2 rule ("no unattended access, no hidden capture, no bypassing OS
permission prompts") applied literally, not a best effort.

### What the Mac actually did

Verified on the macOS 26.6.2 / x86_64 test VM, not just compiled:
`cargo build`, `cargo test` (25 passed), `cargo clippy --all-targets -D
warnings` and `cargo fmt --check` all pass with the feature on, and the
default `cargo build -p lumepeer-media` still builds on macOS with the stub.

Screen Recording is **not** granted to a `cargo test` binary run over SSH,
which is the more interesting half of the result. No prompt appeared — TCC
has no GUI session to raise one in, and a test binary has no bundle identity
to attach a grant to — and the request did not hang waiting for one either.
`SCShareableContent` came back immediately with

```
domain=com.apple.ScreenCaptureKit.SCStreamErrorDomain code=-3801
desc=The user declined TCCs for application, window, display capture
```

which is the mapping this module already had: `start` returned
`MediaError::PermissionDenied`, and the live-capture test printed
`skipped: Screen Recording is not granted to this test binary` and passed
rather than failing the suite, the way `capture::linux_x11`'s tests skip on a
headless runner. That is exactly the intended shape — the backend refuses and
says why, instead of finding another way to the pixels — but it does mean the
frame-copy path (`copy_bgra`/`read_locked`) has been compiled and reviewed,
not executed. Exercising it needs a granted, bundled application, which is
`apps/desktop`'s job and therefore the parallel pipeline effort's, not this
one's. The `-3801` mapping, the change-detection and the stop-reason logic
are covered by tests that do run.

Worth recording for whoever wires this up: the grant attaches to the
*application bundle*, so the desktop app will need
`NSScreenCaptureUsageDescription` in its `Info.plist` and a signed bundle for
macOS to offer the prompt at all — a bare binary can only ever be denied.

### What is still not covered

Window and application capture (`SCContentFilter`'s other initializers),
audio (`setCapturesAudio` is explicitly off — audio is a separate grant this
backend has no business enabling), `SCContentSharingPicker`, and the
`VideoToolbox` hardware encoder ADR 0011 lists as an open gap. Input
injection on macOS is unchanged: `input_capability` still reports `Full`,
and `platform_injector` still has no macOS adapter, so §11's `CGEvent` path
remains open work.

## Consequences

macOS hosts can capture for real, and the "no capture without a viewer" rule
of §19 phase 2 now has a second platform actually exercising it rather than
returning a stub error. `platform_capturer` is deliberately left alone: this
change is one of three parallel efforts (Windows capture, macOS capture, the
desktop pipeline), and wiring the backend selection belongs to whichever one
merges last, not to this one. Until then `MacosCapturer` is constructed
explicitly by tests.

Anyone enabling `capture-screencapturekit` needs macOS 12.3+ and Xcode
Command Line Tools, and their build gains the `objc2` framework crates listed
above — no Swift toolchain, and no second Apple bindings ecosystem.
