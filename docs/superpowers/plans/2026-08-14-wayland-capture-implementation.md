# Wayland Capture and Input Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `WaylandPortalCapturer` produce real frames from PipeWire and add a `WaylandPortalInjector` that injects input through the same portal session, replacing ADR 0003's Wayland stub.

**Architecture:** A new `PortalHandle` owns the ashpd `Session`/`RemoteDesktop` proxy and a persistent tokio runtime, shared via `Arc<Mutex<_>>` between the capturer and injector. Negotiation happens lazily on the capturer's first `start()`. Frame consumption runs on a dedicated OS thread owning a PipeWire `MainLoop`, feeding a bounded channel that `next_frame()` drains non-blockingly. `platform_capturer()`/`platform_injector()` are replaced by a single `platform_backend()` that runtime-detects X11 vs. Wayland and returns a paired `(ScreenCapturer, InputInjector)`.

**Tech Stack:** Rust, `pipewire` crate 0.8.0 (pipewire-rs — pinned by `scap`'s existing `pipewire = "^0.8.0"`, see Global Constraints), `libspa` 0.8 (re-exported as `pipewire::spa`), `ashpd` 0.13.13 (already a dependency), `tokio` current-thread runtime.

**Spec:** `docs/superpowers/specs/2026-08-14-wayland-capture-design.md`

## Global Constraints

- `pipewire` crate version: `0.8.0`, exact. **Not the initially planned `0.10`** — Task 1 discovered that `scap` (an existing, already-`optional`-declared dependency of `crates/media`, used by the unrelated `capture-scap` feature) pins `pipewire = "^0.8.0"`, and Cargo's `links = "pipewire-0.3"` uniqueness rule allows only one resolved version of a native-linking crate across the *entire workspace lockfile*, regardless of which features are active for a given build invocation. `0.8.0` is the only version satisfying `^0.8.0`, so it is not a preference, it is the only value that resolves at all. Verified directly: `pipewire::main_loop::MainLoop`, `pipewire::context::Context::new(&mainloop)` (single-argument, no properties param), `Context::connect(None)` (returns `Core`, not a `*Rc` type), and `pipewire::stream::Stream::new(&core, ..)` replace the `MainLoopRc`/`ContextRc::new(&mainloop, None)`/`connect_rc`/`StreamBox` names an earlier pass of this plan used before the conflict was found — 0.8 predates that Rc/Box smart-pointer split. Everything else this plan uses (`libspa`'s pod/video/format types, `Stream`'s `add_local_listener_with_user_data`/`param_changed`/`process`/`register`/`connect`/`dequeue_buffer`, the `pipewire::channel` module) is byte-for-byte identical between 0.8.0 and 0.10.0 — confirmed by downloading and diffing both crates' source, not by assumption.
- No DMA-BUF/EGL import — `SPA_DATA_MemPtr` buffers only, copied out (per spec's non-goals).
- Requested video format is fixed `BGRx` (`PixelFormat::Bgra8` on the `Frame` side) — no negotiation of alternate formats.
- `capture-portal` feature gates all of this; it must not affect the default `cargo build --workspace` (no platform SDK required, per the feature's existing doc comment in `crates/media/Cargo.toml`).
- CI's `capture-portal` job stays clippy/build-only, no live-portal test job, matching how portal negotiation is already treated (see `.github/workflows/ci.yml`'s `media` job).
- Every `MediaError` mapping follows the existing convention in `crates/media/src/error.rs`: `CaptureUnavailable` for "can't get going", `CaptureInterrupted` for "was going, now isn't", `InputUnavailable` for anything on the injection side, `PermissionDenied` only for an explicit user decline.

---

### Task 1: Add the `pipewire` dependency and CI system packages

**Files:**
- Modify: `crates/media/Cargo.toml`
- Modify: `.github/workflows/ci.yml:75-76`
- Test: none (this task's deliverable is verified by a clean `cargo clippy` run, folded into Task 1's own steps)

**Interfaces:**
- Produces: the `pipewire` crate becomes available under `crates/media/src/capture/` when built with `--features capture-portal`.

- [ ] **Step 1: Add the dependency**

In `crates/media/Cargo.toml`, add to the Linux-only `[target.'cfg(all(target_os = "linux", not(target_os = "android")))'.dependencies]` section (alongside the existing `ashpd` and `x11rb` entries), and wire it into the `capture-portal` feature:

```toml
[features]
# ...unchanged lines above...
# xdg-desktop-portal negotiation and PipeWire frame consumption for the
# Wayland path (§11).
capture-portal = ["dep:ashpd", "dep:tokio", "dep:pipewire"]
```

```toml
[target.'cfg(all(target_os = "linux", not(target_os = "android")))'.dependencies]
ashpd = { version = "0.13.13", default-features = false, features = [
    "tokio",
    "screencast",
    "remote_desktop",
], optional = true }
x11rb = { workspace = true, optional = true, features = ["xtest"] }
pipewire = { version = "0.8.0", optional = true }
```

**Note (already executed as of this revision):** Task 1 tried `0.10` first and hit a Cargo resolver error — `scap` (an existing, already-declared dependency of `crates/media`) pins `pipewire = "^0.8.0"`, and Cargo's `links = "pipewire-0.3"` uniqueness rule forces one resolved version workspace-wide. `0.8.0` is correct and already committed; see Global Constraints for the full explanation and its effect on Task 4's code.

- [ ] **Step 2: Install the system packages CI needs**

`pipewire-sys` links `libpipewire` via `pkg-config` and generates its bindings with `bindgen` at build time (verified by reading `pipewire-sys-0.10.0`'s `build.rs`, which calls `system_deps::Config::new().probe()` for `libpipewire` and `bindgen::Builder`). That needs both the PipeWire dev headers and libclang. In `.github/workflows/ci.yml`, extend the `media` job's system dependency install (currently at line 76):

```yaml
      - name: Install system dependencies
        run: |
          sudo apt-get update
          sudo apt-get install -y nasm xvfb libx11-dev libxtst-dev libdbus-1-dev gnome-keyring libpipewire-0.3-dev libclang-dev
```

- [ ] **Step 3: Verify it builds**

Run: `cargo clippy -p lumepeer-media --all-targets --features capture-portal -- -D warnings`

Expected: compiles clean (the `pipewire` crate is pulled in but nothing references it yet, so this only proves the dependency resolves and links). This step requires `libpipewire-0.3-dev` and `libclang-dev` (or equivalent, e.g. `clang`) installed locally — if missing, install them the same way the CI step does before running the command.

- [ ] **Step 4: Commit**

```bash
git add crates/media/Cargo.toml .github/workflows/ci.yml
git commit -m "media: add pipewire dependency for Wayland frame consumption"
```

---

### Task 2: Runtime session-type detection

**Files:**
- Modify: `crates/media/src/capture/mod.rs`
- Test: inline `#[cfg(test)] mod tests` in the same file

**Interfaces:**
- Produces: `pub enum SessionType { X11, Wayland, Unknown }` and `fn detect_session_type() -> SessionType`, both in `crate::capture` (`mod.rs`). Task 6 (`platform_backend()`) consumes `detect_session_type()`.

- [ ] **Step 1: Write the failing tests**

Add to `crates/media/src/capture/mod.rs`, inside the existing `#[cfg(test)] mod tests` block:

```rust
#[test]
fn session_type_from_xdg_session_type_wins_over_everything_else() {
    assert_eq!(
        session_type_from(Some("x11"), Some("wayland-0"), Some(":0")),
        SessionType::X11
    );
    assert_eq!(
        session_type_from(Some("wayland"), None, Some(":0")),
        SessionType::Wayland
    );
}

#[test]
fn session_type_falls_back_to_wayland_display_when_xdg_session_type_is_absent() {
    assert_eq!(
        session_type_from(None, Some("wayland-0"), None),
        SessionType::Wayland
    );
}

#[test]
fn session_type_falls_back_to_x11_display_when_nothing_else_is_set() {
    assert_eq!(session_type_from(None, None, Some(":0")), SessionType::X11);
}

#[test]
fn session_type_is_unknown_with_no_signal_at_all() {
    assert_eq!(session_type_from(None, None, None), SessionType::Unknown);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p lumepeer-media --lib session_type -- --nocapture`
Expected: FAIL with "cannot find function `session_type_from`" / "cannot find type `SessionType`" (nothing exists yet).

- [ ] **Step 3: Implement `SessionType` and `detect_session_type()`**

Add to `crates/media/src/capture/mod.rs`, near `platform_capturer` (this replaces the hardcoded X11-only dispatch that function currently has — Task 6 removes `platform_capturer`/`platform_injector` and uses this instead):

```rust
/// Which windowing session this process is running under (§11, ADR 0010).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionType {
    /// A native X11 session, or Xwayland with no compositor portal reachable.
    X11,
    /// A Wayland session: xdg-desktop-portal is the only capture path.
    Wayland,
    /// Neither `XDG_SESSION_TYPE` nor a display variable gave a signal.
    Unknown,
}

/// Pure classification, testable without touching real process environment.
fn session_type_from(
    xdg_session_type: Option<&str>,
    wayland_display: Option<&str>,
    display: Option<&str>,
) -> SessionType {
    match xdg_session_type {
        Some("wayland") => return SessionType::Wayland,
        Some("x11") => return SessionType::X11,
        _ => {}
    }
    if wayland_display.is_some() {
        return SessionType::Wayland;
    }
    if display.is_some() {
        return SessionType::X11;
    }
    SessionType::Unknown
}

/// Detects the current session type from the real process environment
/// (§11). `Unknown` is treated as Wayland by callers, since Wayland is the
/// common default on current distributions (ADR 0003).
#[must_use]
pub fn detect_session_type() -> SessionType {
    session_type_from(
        std::env::var("XDG_SESSION_TYPE").ok().as_deref(),
        std::env::var("WAYLAND_DISPLAY").ok().as_deref(),
        std::env::var("DISPLAY").ok().as_deref(),
    )
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p lumepeer-media --lib session_type`
Expected: PASS, all four tests green.

- [ ] **Step 5: Commit**

```bash
git add crates/media/src/capture/mod.rs
git commit -m "media: add runtime session-type detection for X11 vs Wayland"
```

---

### Task 3: `PortalHandle` — the shared, long-lived portal session

**Files:**
- Modify: `crates/media/src/capture/linux_wayland.rs`
- Test: `#[cfg(test)] mod tests` in the same file (existing tests must keep passing; this task does not add new automated tests, since a real portal is required to exercise negotiation — see Step 4)

**Interfaces:**
- Consumes: `MediaError`, `Result` from `crate::error`; `InputCapability` from `crate::capture`; `ashpd::desktop::remote_desktop::{RemoteDesktop, Session}` (already imported in this file).
- Produces: `pub(crate) struct PortalHandle` with:
  - `fn negotiate() -> Result<Self>` — runs the full handshake and keeps everything alive.
  - `fn node_id(&self) -> Option<u32>` — the granted PipeWire stream's node id, once negotiated.
  - `fn input_capability(&self) -> InputCapability`.
  - `fn remote(&self) -> &RemoteDesktop`, `fn session(&self) -> &Session<RemoteDesktop>`, `fn runtime(&self) -> &tokio::runtime::Runtime` — accessors Task 5's injector needs.
  - Task 4 and Task 5 both hold `Arc<Mutex<Option<PortalHandle>>>` (negotiated lazily, so `None` until the capturer's first `start()`).

- [ ] **Step 1: Replace the free-standing `negotiate()` with `PortalHandle`**

The existing `portal::PortalSession::negotiate()` builds a `tokio::runtime::Builder::new_current_thread()` runtime, uses it once, and drops everything (`remote`, `screencast`, `session`) when it returns — only the `PortalGrant` (node ids + input capability) survives. That's fine for capture alone, but `notify_*` calls need the live `Session` and `RemoteDesktop` handles, so nothing can be dropped anymore.

Replace the `portal` module's contents in `crates/media/src/capture/linux_wayland.rs` (currently lines 111-233) with:

```rust
/// The portal handshake and everything kept alive after it (§11, ADR 0010).
///
/// Capture and input share one negotiated session: `notify_*` calls need the
/// same `Session` handle `SelectDevices`/`Start` used, so nothing from the
/// handshake can be dropped once it succeeds, unlike a capture-only session.
#[cfg(feature = "capture-portal")]
pub mod portal {
    use ashpd::desktop::PersistMode;
    use ashpd::desktop::remote_desktop::{
        DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions,
    };
    use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
    use ashpd::desktop::Session;

    use super::{PortalStep, PORTAL_CALL_ORDER};
    use crate::capture::InputCapability;
    use crate::error::{MediaError, Result};

    /// The granted PipeWire stream's negotiated pixel size, published by
    /// `pipewire_stream::PipeWireFrameThread` from its `param_changed`
    /// callback once format negotiation completes. `(0, 0)` until then.
    ///
    /// Lives here (not in `pipewire_stream`) so both `WaylandPortalCapturer`
    /// (which owns the thread that writes it) and `WaylandPortalInjector`
    /// (which reads it to scale pointer coordinates into the stream's
    /// logical space, per `RemoteDesktop::notify_pointer_motion_absolute`'s
    /// contract) can reach it through the one handle they already share —
    /// no second `Arc` needs threading through the capturer/injector split.
    #[derive(Debug, Default)]
    pub struct StreamSize {
        width: std::sync::atomic::AtomicU32,
        height: std::sync::atomic::AtomicU32,
    }

    impl StreamSize {
        fn new() -> Self {
            Self::default()
        }

        /// Called from the PipeWire thread's `param_changed` callback.
        pub fn set(&self, width: u32, height: u32) {
            self.width.store(width, std::sync::atomic::Ordering::Relaxed);
            self.height.store(height, std::sync::atomic::Ordering::Relaxed);
        }

        /// Called from the injector before scaling a pointer coordinate.
        /// `(0, 0)` means no frame has arrived yet.
        #[must_use]
        pub fn get(&self) -> (u32, u32) {
            (
                self.width.load(std::sync::atomic::Ordering::Relaxed),
                self.height.load(std::sync::atomic::Ordering::Relaxed),
            )
        }
    }

    /// Live portal session: the negotiated grant plus everything needed to
    /// keep injecting input and consuming frames for the session's duration.
    #[derive(Debug)]
    pub struct PortalHandle {
        runtime: tokio::runtime::Runtime,
        remote: RemoteDesktop,
        session: Session<RemoteDesktop>,
        node_id: Option<u32>,
        input: InputCapability,
        steps: Vec<PortalStep>,
        stream_size: std::sync::Arc<StreamSize>,
    }

    impl PortalHandle {
        /// Runs the handshake in the order §11 fixes and keeps the session
        /// alive for both capture and input.
        ///
        /// # Errors
        /// [`MediaError::PermissionDenied`] when the user dismisses the
        /// dialog, [`MediaError::CaptureUnavailable`] when no portal is
        /// reachable.
        pub fn negotiate() -> Result<Self> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;
            let (remote, session, node_id, input, steps) =
                runtime.block_on(Self::negotiate_async())?;
            Ok(Self {
                runtime,
                remote,
                session,
                node_id,
                input,
                steps,
                stream_size: std::sync::Arc::new(StreamSize::new()),
            })
        }

        #[allow(clippy::type_complexity, reason = "internal handshake result, not a public signature")]
        async fn negotiate_async() -> Result<(
            RemoteDesktop,
            Session<RemoteDesktop>,
            Option<u32>,
            InputCapability,
            Vec<PortalStep>,
        )> {
            let mut steps = Vec::with_capacity(PORTAL_CALL_ORDER.len());

            let remote = RemoteDesktop::new()
                .await
                .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;
            let screencast = Screencast::new()
                .await
                .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;

            // 1. CreateSession. Its options type is private in ashpd, so the
            // default is the only thing that can be passed here.
            #[allow(
                clippy::default_trait_access,
                reason = "ashpd keeps CreateSessionOptions private"
            )]
            let session = remote
                .create_session(Default::default())
                .await
                .map_err(map_portal_error)?;
            steps.push(PortalStep::CreateSession);

            // 2. SelectDevices, strictly before SelectSources (§11).
            remote
                .select_devices(
                    &session,
                    SelectDevicesOptions::default()
                        .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
                        .set_persist_mode(PersistMode::DoNot),
                )
                .await
                .map_err(map_portal_error)?;
            steps.push(PortalStep::SelectDevices);

            // 3. SelectSources on the same session.
            screencast
                .select_sources(
                    &session,
                    SelectSourcesOptions::default()
                        .set_cursor_mode(CursorMode::Embedded)
                        .set_sources(ashpd::enumflags2::BitFlags::from(SourceType::Monitor))
                        .set_multiple(false)
                        .set_persist_mode(PersistMode::DoNot),
                )
                .await
                .map_err(map_portal_error)?;
            steps.push(PortalStep::SelectSources);

            // 4. Start raises the dialog and returns what the user allowed.
            let response = remote
                .start(&session, None, StartOptions::default())
                .await
                .map_err(map_portal_error)?
                .response()
                .map_err(map_portal_error)?;
            steps.push(PortalStep::Start);

            let devices = response.devices();
            // An empty device mask is a decision, not a failure (§18).
            let input = if devices.is_empty() {
                InputCapability::None
            } else {
                InputCapability::PortalRemoteDesktop
            };

            let node_id = response
                .streams()
                .first()
                .map(ashpd::desktop::screencast::Stream::pipe_wire_node_id);

            Ok((remote, session, node_id, input, steps))
        }

        /// The granted PipeWire stream's node id, if any stream was granted.
        #[must_use]
        pub const fn node_id(&self) -> Option<u32> {
            self.node_id
        }

        /// What this session allows on the input side (§18).
        #[must_use]
        pub const fn input_capability(&self) -> InputCapability {
            self.input
        }

        /// The steps that actually ran, for the order test and the audit log.
        #[must_use]
        pub fn steps(&self) -> &[PortalStep] {
            &self.steps
        }

        /// The `RemoteDesktop` proxy, for issuing `notify_*` calls.
        #[must_use]
        pub const fn remote(&self) -> &RemoteDesktop {
            &self.remote
        }

        /// The negotiated session, for issuing `notify_*` calls.
        #[must_use]
        pub const fn session(&self) -> &Session<RemoteDesktop> {
            &self.session
        }

        /// The tokio runtime the handshake ran on, reused for `notify_*`
        /// calls so the injector doesn't spin up a runtime per event.
        #[must_use]
        pub const fn runtime(&self) -> &tokio::runtime::Runtime {
            &self.runtime
        }

        /// A clone of the shared, atomically-updated stream size, to hand to
        /// [`crate::capture::pipewire_stream::PipeWireFrameThread::spawn`]
        /// so it can publish the negotiated size as frames start arriving.
        #[must_use]
        pub fn stream_size_handle(&self) -> std::sync::Arc<StreamSize> {
            std::sync::Arc::clone(&self.stream_size)
        }

        /// The negotiated stream's pixel size, or `(0, 0)` before the first
        /// frame's format negotiation completes.
        #[must_use]
        pub fn stream_size(&self) -> (u32, u32) {
            self.stream_size.get()
        }
    }

    /// A dismissed dialog is the user declining, everything else is the portal
    /// being unavailable (§18).
    fn map_portal_error(error: ashpd::Error) -> MediaError {
        match error {
            ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled) => {
                MediaError::PermissionDenied
            }
            other => MediaError::CaptureUnavailable(other.to_string()),
        }
    }
}
```

Note `PortalGrant` is gone — `PortalHandle` replaces it. `PortalStep`/`PORTAL_CALL_ORDER` (outside the `portal` module, at the top of the file) are unchanged; only their visibility needs `pub(super)` → they're already `pub`, so no change needed there.

- [ ] **Step 2: Update `WaylandPortalCapturer` to use `PortalHandle` instead of `PortalGrant`**

Replace the `grant: Option<PortalGrant>` field and its accessor in `WaylandPortalCapturer` (this is an interim step — Task 4 replaces this type entirely with the `Arc<Mutex<...>>`-sharing version, but keeping the file compiling after each step matters for review):

```rust
/// Portal/PipeWire capturer.
#[derive(Debug, Default)]
pub struct WaylandPortalCapturer {
    #[cfg(feature = "capture-portal")]
    handle: Option<portal::PortalHandle>,
}

impl WaylandPortalCapturer {
    /// Creates a capturer with no portal session yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            #[cfg(feature = "capture-portal")]
            handle: None,
        }
    }
}

impl ScreenCapturer for WaylandPortalCapturer {
    fn start(&mut self, _target: CaptureTarget) -> Result<()> {
        #[cfg(feature = "capture-portal")]
        {
            let handle = portal::PortalHandle::negotiate()?;
            self.handle = Some(handle);
            Err(MediaError::CaptureUnavailable(
                "the portal granted a stream, but PipeWire frame consumption is not implemented"
                    .to_owned(),
            ))
        }
        #[cfg(not(feature = "capture-portal"))]
        {
            Err(MediaError::CaptureUnavailable(
                "this build has no xdg-desktop-portal support".to_owned(),
            ))
        }
    }

    fn next_frame(&mut self) -> Result<Option<Frame>> {
        Err(MediaError::CaptureUnavailable(
            "the portal capture path produces no frames yet".to_owned(),
        ))
    }

    fn stop(&mut self) {
        #[cfg(feature = "capture-portal")]
        {
            self.handle = None;
        }
    }

    fn input_capability(&self) -> InputCapability {
        #[cfg(feature = "capture-portal")]
        {
            return self
                .handle
                .as_ref()
                .map_or(InputCapability::PortalRemoteDesktop, |h| {
                    h.input_capability()
                });
        }
        #[cfg(not(feature = "capture-portal"))]
        InputCapability::PortalRemoteDesktop
    }
}
```

(The `Err(...)` in `start()` and the stub `next_frame()` are still correct here — Task 4 is what makes frames real. This step's only job is swapping the type without changing behavior.)

- [ ] **Step 3: Update the existing tests to compile against `PortalHandle`**

The two existing tests (`select_devices_sits_between_create_session_and_select_sources`, `an_empty_device_mask_degrades_to_view_only`) reference `WaylandPortalCapturer::grant` directly, which no longer exists. The order test is untouched (it only reads `PORTAL_CALL_ORDER`, not the removed field). Rewrite the second test to match the new field name and type — since `PortalHandle` can only be constructed via a real `negotiate()` call (no public bare constructor, by design: there is no such thing as a `PortalHandle` that didn't negotiate), this test becomes a `#[cfg(feature = "capture-portal")]`-gated, opt-in-only integration check rather than a pure unit test. Replace it with:

```rust
/// §18: an empty device mask degrades to view-only instead of failing.
/// Opt-in like the X11 XTEST test: it needs a real portal and a user to
/// click through (or decline) the consent dialog, so it must not run by
/// default in CI.
#[cfg(feature = "capture-portal")]
#[test]
fn an_empty_device_mask_degrades_to_view_only() {
    if std::env::var("LUMEPEER_TEST_PORTAL").as_deref() != Ok("1") {
        return;
    }
    let mut capturer = WaylandPortalCapturer::new();
    assert_eq!(
        capturer.input_capability(),
        InputCapability::PortalRemoteDesktop
    );

    // Negotiating triggers the real system consent dialog.
    let _ = capturer.start(CaptureTarget::PrimaryDisplay);

    // Whatever the user granted, capability must be one of the two valid
    // outcomes, and stopping must always forget the session.
    assert!(matches!(
        capturer.input_capability(),
        InputCapability::PortalRemoteDesktop | InputCapability::None
    ));
    capturer.stop();
    assert_eq!(
        capturer.input_capability(),
        InputCapability::PortalRemoteDesktop
    );
}
```

This is a real behavior change from the previous test (which synthesized a `PortalGrant` by hand and needed no live portal). That's the direct consequence of `PortalHandle` no longer being constructible without a real handshake — noted here rather than silently dropped, per the "no placeholders" rule. The `select_devices_sits_between_create_session_and_select_sources` order test needs no change.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p lumepeer-media --lib --features capture-portal linux_wayland`
Expected: PASS. `an_empty_device_mask_degrades_to_view_only` short-circuits and passes trivially without `LUMEPEER_TEST_PORTAL=1` set (matching the X11 `LUMEPEER_TEST_XTEST` pattern); `select_devices_sits_between_create_session_and_select_sources` passes unconditionally.

Also run: `cargo clippy -p lumepeer-media --all-targets --features capture-portal -- -D warnings` to confirm the whole crate still compiles clean.

- [ ] **Step 5: Commit**

```bash
git add crates/media/src/capture/linux_wayland.rs
git commit -m "media: keep the portal session alive as PortalHandle instead of dropping it after negotiation"
```

---

### Task 4: PipeWire frame consumption

**Files:**
- Create: `crates/media/src/capture/pipewire_stream.rs`
- Modify: `crates/media/src/capture/linux_wayland.rs` (wire `WaylandPortalCapturer` to the new module, share `PortalHandle` via `Arc<Mutex<Option<PortalHandle>>>`)
- Modify: `crates/media/src/capture/mod.rs:26-27` (add `pub(crate) mod pipewire_stream;` next to the existing `linux_wayland` module declaration, gated the same way)
- Test: `#[cfg(test)] mod tests` in `pipewire_stream.rs` (fakeable dedup/backpressure logic) and a manual-only test in `linux_wayland.rs`

**Interfaces:**
- Consumes: `Frame`, `PixelFormat` from `crate::capture`; `MediaError`, `Result` from `crate::error`; `linux_wayland::portal::StreamSize` (Task 3 — the shared, atomically-updated stream size both the capturer's thread and the injector reach through `PortalHandle`).
- Produces: `pub(crate) struct PipeWireFrameThread` with `fn spawn(node_id: u32, stream_size: Arc<portal::StreamSize>) -> Result<Self>` and `fn try_recv_frame(&self) -> Option<Frame>`. Dropping it joins the thread. Task 3's `WaylandPortalCapturer` (finished in this task) holds one per active capture.

- [ ] **Step 1: Write the dedup/backpressure test first, against a small seam**

The real PipeWire thread can't run in CI (no compositor, no portal). What *can* be unit tested without any of that is the packing/dedup logic: given raw bytes with a stride, does it produce the right tightly-packed `Frame`, and does repeating the same bytes correctly yield `None`. Extract that into a pure function so it's testable without a `pw::stream::Stream` in the loop at all.

Create `crates/media/src/capture/pipewire_stream.rs`:

```rust
//! PipeWire frame consumption for the Wayland portal capture path (§11).
//!
//! The negotiated `Session` (Task 3, `linux_wayland::portal::PortalHandle`)
//! grants a PipeWire node id, not pixels: turning that node id into `Frame`s
//! needs its own PipeWire `MainLoop`, run on a dedicated thread because the
//! loop blocks for the life of the capture (`MainLoop::run` does not return
//! until something calls `quit()`).

use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::capture::linux_wayland::portal::StreamSize;
use crate::capture::{Frame, PixelFormat};
use crate::error::{MediaError, Result};

/// Packs a raw row-strided buffer into a tightly-packed BGRx `Frame`,
/// deduplicating against `last_hash` the same way `linux_x11.rs` does.
///
/// Returns `Ok(None)` when the frame is identical to the last one handed
/// out, or when the buffer doesn't yet carry a full frame (a short read
/// during format renegotiation) — neither is an error.
fn pack_frame(
    width: u32,
    height: u32,
    stride: usize,
    bytes: &[u8],
    started_at: std::time::Instant,
    last_hash: &mut Option<[u8; 32]>,
) -> Option<Frame> {
    if width == 0 || height == 0 {
        return None;
    }
    let row_bytes = (width as usize) * 4;
    let effective_stride = stride.max(row_bytes);
    let needed = effective_stride * (height as usize - 1) + row_bytes;
    if bytes.len() < needed {
        return None;
    }

    let mut packed = Vec::with_capacity(row_bytes * height as usize);
    for row in 0..height as usize {
        let start = row * effective_stride;
        packed.extend_from_slice(&bytes[start..start + row_bytes]);
    }

    let hash = *blake3::hash(&packed).as_bytes();
    if *last_hash == Some(hash) {
        return None;
    }
    *last_hash = Some(hash);

    Some(Frame {
        width,
        height,
        format: PixelFormat::Bgra8,
        timestamp_us: u64::try_from(started_at.elapsed().as_micros()).unwrap_or(u64::MAX),
        data: packed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_a_strided_buffer_into_a_tight_frame() {
        // 2x2 BGRx, stride padded to 12 bytes/row (row_bytes is 8).
        let mut buf = vec![0xAAu8; 12 * 2];
        buf[0..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        buf[12..20].copy_from_slice(&[9, 10, 11, 12, 13, 14, 15, 16]);

        let mut last_hash = None;
        let frame = pack_frame(2, 2, 12, &buf, std::time::Instant::now(), &mut last_hash)
            .expect("first frame must not be deduplicated");

        assert_eq!(frame.width, 2);
        assert_eq!(frame.height, 2);
        assert_eq!(frame.format, PixelFormat::Bgra8);
        assert_eq!(
            frame.data,
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
        );
    }

    #[test]
    fn identical_bytes_deduplicate_to_none() {
        let buf = vec![7u8; 8 * 3];
        let mut last_hash = None;
        assert!(pack_frame(2, 3, 8, &buf, std::time::Instant::now(), &mut last_hash).is_some());
        assert!(
            pack_frame(2, 3, 8, &buf, std::time::Instant::now(), &mut last_hash).is_none(),
            "identical bytes must dedup to None"
        );
    }

    #[test]
    fn a_short_buffer_yields_no_frame_instead_of_panicking() {
        let buf = vec![0u8; 4];
        let mut last_hash = None;
        assert!(pack_frame(4, 4, 16, &buf, std::time::Instant::now(), &mut last_hash).is_none());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail, then pass**

Run: `cargo test -p lumepeer-media --lib --features capture-portal pipewire_stream`
First run (before this file existed) would have failed with "module not found" — since the file is created with both the function and the tests together, run it now and confirm all three tests PASS. (This task deliberately writes implementation and test in the same step because `pack_frame` has no external dependency to stub — there's nothing to watch fail separately without contriving a stub.)

- [ ] **Step 3: Add the PipeWire thread around `pack_frame`**

Append to `crates/media/src/capture/pipewire_stream.rs` (this part needs `libpipewire`/`libclang` to compile, per Task 1, and is gated the same way the rest of the portal path is — behind `capture-portal`):

```rust
struct StreamUserData {
    width: u32,
    height: u32,
    sender: SyncSender<Frame>,
    started_at: std::time::Instant,
    last_hash: Option<[u8; 32]>,
    stream_size: Arc<StreamSize>,
}

/// Sent to shut the PipeWire thread down; see `pipewire::channel`, which
/// exists exactly for signaling a loop running on another thread.
struct Shutdown;

/// Owns a PipeWire `MainLoop` on a dedicated thread, feeding decoded frames
/// through a bounded channel. Dropping this joins the thread.
#[derive(Debug)]
pub(crate) struct PipeWireFrameThread {
    handle: Option<JoinHandle<()>>,
    shutdown: pipewire::channel::Sender<Shutdown>,
    frames: Receiver<Frame>,
}

impl PipeWireFrameThread {
    /// Spawns the thread and connects to `node_id`.
    ///
    /// # Errors
    /// [`MediaError::CaptureUnavailable`] if the thread itself cannot be
    /// spawned. Errors from inside the thread (PipeWire connection failure,
    /// format negotiation failure) are not reported synchronously — the
    /// thread exits and `try_recv_frame` then always returns `None`, which
    /// callers already treat as "no new frame right now", not a failure.
    ///
    /// `stream_size` is the same `Arc` `PortalHandle::stream_size_handle`
    /// returns — the thread publishes the negotiated width/height into it
    /// from `param_changed` so `WaylandPortalInjector` can scale pointer
    /// coordinates correctly (Task 5).
    pub fn spawn(node_id: u32, stream_size: Arc<StreamSize>) -> Result<Self> {
        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<Frame>(1);
        let (shutdown_tx, shutdown_rx) = pipewire::channel::channel::<Shutdown>();

        let handle = std::thread::Builder::new()
            .name("lumepeer-pipewire-capture".to_owned())
            .spawn(move || {
                if let Err(err) = Self::run(node_id, &frame_tx, shutdown_rx, &stream_size) {
                    tracing::warn!("pipewire capture thread exited: {err}");
                }
            })
            .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;

        Ok(Self {
            handle: Some(handle),
            shutdown: shutdown_tx,
            frames: frame_rx,
        })
    }

    fn run(
        node_id: u32,
        frame_tx: &SyncSender<Frame>,
        shutdown_rx: pipewire::channel::Receiver<Shutdown>,
        stream_size: &Arc<StreamSize>,
    ) -> Result<()> {
        use pipewire::spa::param::format::{MediaSubtype, MediaType};
        use pipewire::spa::param::video::{VideoFormat, VideoInfoRaw};
        use pipewire::spa::pod::serialize::PodSerializer;
        use pipewire::spa::pod::{Object, Pod, Property, Value};
        use pipewire::spa::utils::{Direction, Id, SpaTypes};
        use pipewire::stream::StreamFlags;

        pipewire::init();
        // pipewire 0.8.0 API (see Global Constraints): MainLoop is already
        // Rc-backed and Clone on its own — no separate *Rc type. Context::new
        // takes only the loop, no properties argument. connect() (not
        // connect_rc()) returns Core directly.
        let mainloop = pipewire::main_loop::MainLoop::new(None)
            .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;
        let context = pipewire::context::Context::new(&mainloop)
            .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;
        let core = context
            .connect(None)
            .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;

        // Cross-thread shutdown: dropping `PipeWireFrameThread` sends
        // `Shutdown`, which this attaches to the loop as an IO source.
        let _shutdown_listener = {
            let mainloop = mainloop.clone();
            shutdown_rx.attach(mainloop.loop_(), move |Shutdown| mainloop.quit())
        };

        let props = pipewire::properties::properties! {
            *pipewire::keys::MEDIA_TYPE => "Video",
            *pipewire::keys::MEDIA_CATEGORY => "Capture",
            *pipewire::keys::MEDIA_ROLE => "Screen",
        };
        let stream = pipewire::stream::Stream::new(&core, "lumepeer-capture", props)
            .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;

        let data = StreamUserData {
            width: 0,
            height: 0,
            sender: frame_tx.clone(),
            started_at: std::time::Instant::now(),
            last_hash: None,
            stream_size: Arc::clone(stream_size),
        };

        let _listener = stream
            .add_local_listener_with_user_data(data)
            .param_changed(|_, user_data, id, param| {
                let Some(param) = param else { return };
                if id != pipewire::spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let mut info = VideoInfoRaw::new();
                if info.parse(param).is_err() {
                    return;
                }
                let size = info.size();
                user_data.width = size.width;
                user_data.height = size.height;
                // Published for WaylandPortalInjector (Task 5), which reads
                // this through the same PortalHandle to scale pointer
                // coordinates into the stream's own logical space.
                user_data.stream_size.set(size.width, size.height);
            })
            .process(|stream, user_data| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let datas = buffer.datas_mut();
                let Some(data) = datas.first_mut() else {
                    return;
                };
                let stride = data.chunk().stride().max(0) as usize;
                let Some(bytes) = data.data() else { return };

                if let Some(frame) = pack_frame(
                    user_data.width,
                    user_data.height,
                    stride,
                    bytes,
                    user_data.started_at,
                    &mut user_data.last_hash,
                ) {
                    // A full channel means the consumer hasn't caught up:
                    // drop this frame rather than block the PipeWire thread.
                    if let Err(TrySendError::Disconnected(_)) = user_data.sender.try_send(frame) {
                        // The receiving end (WaylandPortalCapturer) is gone;
                        // nothing more to do until `stop()` tears this down.
                    }
                }
            })
            .register()
            .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;

        let mut requested = VideoInfoRaw::new();
        requested.set_format(VideoFormat::BGRx);
        let values: Vec<u8> = PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &Value::Object(Object {
                type_: SpaTypes::ObjectParamFormat.as_raw(),
                id: pipewire::spa::param::ParamType::EnumFormat.as_raw(),
                properties: vec![
                    Property::new(
                        pipewire::spa::sys::SPA_FORMAT_mediaType,
                        Value::Id(Id(MediaType::Video.as_raw())),
                    ),
                    Property::new(
                        pipewire::spa::sys::SPA_FORMAT_mediaSubtype,
                        Value::Id(Id(MediaSubtype::Raw.as_raw())),
                    ),
                    Property::new(
                        pipewire::spa::sys::SPA_FORMAT_VIDEO_format,
                        Value::Id(Id(requested.format().as_raw())),
                    ),
                ],
            }),
        )
        .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?
        .0
        .into_inner();
        let format_pod = Pod::from_bytes(&values)
            .ok_or_else(|| MediaError::CaptureUnavailable("could not build format pod".to_owned()))?;
        let mut params = [format_pod];

        stream
            .connect(
                Direction::Input,
                Some(node_id),
                StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
                &mut params,
            )
            .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;

        mainloop.run();
        Ok(())
    }

    /// Drains the next available frame, or `None` if nothing new has
    /// arrived, matching `ScreenCapturer::next_frame`'s "no change" contract.
    pub fn try_recv_frame(&self) -> Option<Frame> {
        match self.frames.try_recv() {
            Ok(frame) => Some(frame),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }
}

impl Drop for PipeWireFrameThread {
    fn drop(&mut self) {
        let _ = self.shutdown.send(Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
```

- [ ] **Step 4: Register the module**

In `crates/media/src/capture/mod.rs`, next to the existing Wayland module declaration (around line 26-27):

```rust
#[cfg(all(target_os = "linux", not(target_os = "android")))]
pub mod linux_wayland;
#[cfg(all(
    target_os = "linux",
    not(target_os = "android"),
    feature = "capture-portal"
))]
pub(crate) mod pipewire_stream;
```

- [ ] **Step 5: Wire `WaylandPortalCapturer` to the PipeWire thread**

Replace `WaylandPortalCapturer` in `crates/media/src/capture/linux_wayland.rs` (built on top of Task 3's `PortalHandle`-holding version) with the shared-handle version Task 5 also needs:

```rust
use std::sync::{Arc, Mutex};

/// Portal/PipeWire capturer. Shares its negotiated session with a
/// [`WaylandPortalInjector`] built from the same handle (§11, ADR 0010) —
/// `notify_*` calls need the same `Session` `SelectDevices`/`Start` used.
#[derive(Debug, Default)]
pub struct WaylandPortalCapturer {
    #[cfg(feature = "capture-portal")]
    shared: Arc<Mutex<Option<portal::PortalHandle>>>,
    #[cfg(feature = "capture-portal")]
    stream: Option<crate::capture::pipewire_stream::PipeWireFrameThread>,
}

impl WaylandPortalCapturer {
    /// Creates a capturer with no portal session yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a capturer and injector that share one portal session. Both
    /// negotiate lazily: nothing happens until the capturer's first `start`.
    #[cfg(feature = "capture-portal")]
    #[must_use]
    pub fn paired_with_injector() -> (Self, super::WaylandPortalInjector) {
        let shared = Arc::new(Mutex::new(None));
        (
            Self {
                shared: Arc::clone(&shared),
                stream: None,
            },
            super::WaylandPortalInjector::new(shared),
        )
    }
}

impl ScreenCapturer for WaylandPortalCapturer {
    fn start(&mut self, _target: CaptureTarget) -> Result<()> {
        #[cfg(feature = "capture-portal")]
        {
            let mut guard = self
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if guard.is_none() {
                *guard = Some(portal::PortalHandle::negotiate()?);
            }
            let handle = guard.as_ref().expect("just set above if it was None");
            let node_id = handle.node_id();
            let stream_size = handle.stream_size_handle();
            drop(guard);

            let node_id = node_id.ok_or_else(|| {
                MediaError::CaptureUnavailable(
                    "the portal granted no PipeWire stream".to_owned(),
                )
            })?;
            self.stream = Some(crate::capture::pipewire_stream::PipeWireFrameThread::spawn(
                node_id,
                stream_size,
            )?);
            Ok(())
        }
        #[cfg(not(feature = "capture-portal"))]
        {
            Err(MediaError::CaptureUnavailable(
                "this build has no xdg-desktop-portal support".to_owned(),
            ))
        }
    }

    fn next_frame(&mut self) -> Result<Option<Frame>> {
        #[cfg(feature = "capture-portal")]
        {
            let stream = self.stream.as_ref().ok_or_else(|| {
                MediaError::CaptureUnavailable("capturer not started".to_owned())
            })?;
            return Ok(stream.try_recv_frame());
        }
        #[cfg(not(feature = "capture-portal"))]
        Err(MediaError::CaptureUnavailable(
            "this build has no xdg-desktop-portal support".to_owned(),
        ))
    }

    fn stop(&mut self) {
        #[cfg(feature = "capture-portal")]
        {
            self.stream = None;
            let mut guard = self
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = None;
        }
    }

    fn input_capability(&self) -> InputCapability {
        #[cfg(feature = "capture-portal")]
        {
            let guard = self
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            return guard
                .as_ref()
                .map_or(InputCapability::PortalRemoteDesktop, |h| {
                    h.input_capability()
                });
        }
        #[cfg(not(feature = "capture-portal"))]
        InputCapability::PortalRemoteDesktop
    }
}
```

Note `#[derive(Default)]` still works on `WaylandPortalCapturer` because `Arc<Mutex<Option<_>>>` and `Option<_>` both implement `Default`. The bare `WaylandPortalCapturer::new()` (no injector) is kept for the case where only capture is needed, e.g. in `platform_capturer`-style call sites that don't want input — Task 6's `platform_backend()` uses `paired_with_injector()` instead.

The `an_empty_device_mask_degrades_to_view_only` test from Task 3 keeps working unmodified: `WaylandPortalCapturer::new()` still produces a capturer whose `shared` starts as `Arc::new(Mutex::new(None))`.

- [ ] **Step 6: Run the full test suite for this crate**

Run: `cargo clippy -p lumepeer-media --all-targets --features capture-portal -- -D warnings && cargo test -p lumepeer-media --lib --features capture-portal`
Expected: clean clippy, all tests pass (the `pipewire_stream` unit tests plus the unmodified/updated `linux_wayland` tests).

- [ ] **Step 7: Commit**

```bash
git add crates/media/src/capture/pipewire_stream.rs crates/media/src/capture/linux_wayland.rs crates/media/src/capture/mod.rs
git commit -m "media: consume PipeWire frames for Wayland capture"
```

---

### Task 5: `WaylandPortalInjector`

**Files:**
- Modify: `crates/media/src/capture/linux_wayland.rs`
- Test: `#[cfg(test)] mod tests` in the same file (pure mapping logic tested directly; the actual `notify_*` calls are opt-in like Task 3's portal test, since they need a live session)

**Interfaces:**
- Consumes: `PortalHandle` (Task 3); `InputEventPayload`, `InputDetail`, `POINTER_BUTTON_LOGICAL_BASE` from `lumepeer_core::protocol` (same imports `linux_x11.rs` already uses); `InputInjector`, `InputCapability` from `crate::capture`.
- Produces: `pub struct WaylandPortalInjector` implementing `InputInjector`, constructed via `WaylandPortalCapturer::paired_with_injector()` (Task 4) or `WaylandPortalInjector::new(shared)` directly.

- [ ] **Step 1: Write the failing tests for the pure mapping functions**

Add to the `#[cfg(test)] mod tests` block in `crates/media/src/capture/linux_wayland.rs`:

```rust
#[cfg(feature = "capture-portal")]
#[test]
fn normalized_coordinates_map_onto_the_stream() {
    assert_eq!(WaylandPortalInjector::to_stream(0, 1920), 0.0);
    assert_eq!(WaylandPortalInjector::to_stream(u16::MAX, 1920), 1920.0);
    assert!((WaylandPortalInjector::to_stream(32_767, 1920) - 959.5).abs() < 1.0);
}

#[cfg(feature = "capture-portal")]
#[test]
fn the_first_three_pointer_buttons_map_to_evdev_left_middle_right() {
    use lumepeer_core::protocol::POINTER_BUTTON_LOGICAL_BASE;

    assert_eq!(
        WaylandPortalInjector::evdev_button(POINTER_BUTTON_LOGICAL_BASE).unwrap(),
        0x110 // BTN_LEFT
    );
    assert_eq!(
        WaylandPortalInjector::evdev_button(POINTER_BUTTON_LOGICAL_BASE + 1).unwrap(),
        0x112 // BTN_MIDDLE
    );
    assert_eq!(
        WaylandPortalInjector::evdev_button(POINTER_BUTTON_LOGICAL_BASE + 2).unwrap(),
        0x111 // BTN_RIGHT
    );
    assert!(WaylandPortalInjector::evdev_button(POINTER_BUTTON_LOGICAL_BASE + 3).is_err());
}

#[cfg(feature = "capture-portal")]
#[test]
fn injecting_before_negotiation_refuses_rather_than_silently_dropping() {
    use lumepeer_core::protocol::{InputDetail, InputEventPayload};

    let shared = std::sync::Arc::new(std::sync::Mutex::new(None));
    let mut injector = WaylandPortalInjector::new(shared);
    assert_eq!(
        injector.capability(),
        InputCapability::PortalRemoteDesktop
    );
    let result = injector.inject(&InputEventPayload {
        logical: 0,
        scancode: 30,
        modifiers: 0,
        detail: InputDetail::Press,
    });
    assert!(matches!(result, Err(MediaError::InputUnavailable(_))));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p lumepeer-media --lib --features capture-portal linux_wayland`
Expected: FAIL — `WaylandPortalInjector` doesn't exist yet.

- [ ] **Step 3: Implement `WaylandPortalInjector`**

Add to `crates/media/src/capture/linux_wayland.rs`, after `WaylandPortalCapturer`'s `impl ScreenCapturer` block:

```rust
/// Full range of a normalized pointer coordinate (§9.1), same constant as
/// `linux_x11.rs::POINTER_RANGE`.
#[cfg(feature = "capture-portal")]
const POINTER_RANGE: u32 = 65_535;

/// Evdev codes for the first three pointer buttons (`linux/input-event-codes.h`).
#[cfg(feature = "capture-portal")]
const BTN_LEFT: i32 = 0x110;
#[cfg(feature = "capture-portal")]
const BTN_RIGHT: i32 = 0x111;
#[cfg(feature = "capture-portal")]
const BTN_MIDDLE: i32 = 0x112;

/// Input injection through the portal's `RemoteDesktop` interface (§11,
/// ADR 0010). Shares its session with a [`WaylandPortalCapturer`] — both
/// hold the same [`Arc<Mutex<Option<portal::PortalHandle>>>`], since
/// `notify_*` needs the `Session` that capture's negotiation produced.
#[cfg(feature = "capture-portal")]
#[derive(Debug)]
pub struct WaylandPortalInjector {
    shared: std::sync::Arc<std::sync::Mutex<Option<portal::PortalHandle>>>,
}

#[cfg(feature = "capture-portal")]
impl WaylandPortalInjector {
    /// Wraps a portal handle shared with a capturer. Use
    /// [`WaylandPortalCapturer::paired_with_injector`] rather than calling
    /// this directly, so the two always share the same session.
    #[must_use]
    pub const fn new(
        shared: std::sync::Arc<std::sync::Mutex<Option<portal::PortalHandle>>>,
    ) -> Self {
        Self { shared }
    }

    /// Maps a normalized 0..=65535 coordinate onto the stream's pixel space.
    fn to_stream(value: u16, extent: u32) -> f64 {
        f64::from(value) * f64::from(extent) / f64::from(POINTER_RANGE)
    }

    /// Evdev button code for a pointer button carried as a logical id.
    /// Only left/middle/right are mapped, matching the three buttons
    /// `linux_x11.rs::X11Injector::button` actually covers.
    fn evdev_button(logical: u32) -> Result<i32> {
        match logical.saturating_sub(lumepeer_core::protocol::POINTER_BUTTON_LOGICAL_BASE) {
            0 => Ok(BTN_LEFT),
            1 => Ok(BTN_MIDDLE),
            2 => Ok(BTN_RIGHT),
            _ => Err(MediaError::InputUnavailable(
                "button outside the range the portal path supports".to_owned(),
            )),
        }
    }
}

#[cfg(feature = "capture-portal")]
impl crate::capture::InputInjector for WaylandPortalInjector {
    fn inject(&mut self, event: &lumepeer_core::protocol::InputEventPayload) -> Result<()> {
        use ashpd::desktop::remote_desktop::{
            KeyState, NotifyKeyboardKeycodeOptions, NotifyPointerAxisOptions,
            NotifyPointerButtonOptions, NotifyPointerMotionAbsoluteOptions,
        };
        use lumepeer_core::protocol::{InputDetail, POINTER_BUTTON_LOGICAL_BASE};

        let guard = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let handle = guard.as_ref().ok_or_else(|| {
            MediaError::InputUnavailable(
                "no portal session negotiated yet: capture must start first".to_owned(),
            )
        })?;
        let stream_id = handle.node_id().ok_or_else(|| {
            MediaError::InputUnavailable("the portal granted no stream to inject into".to_owned())
        })?;
        let remote = handle.remote();
        let session = handle.session();

        let (stream_width, stream_height) = handle.stream_size();

        handle.runtime().block_on(async {
            match event.detail {
                InputDetail::PointerMove { x, y } => {
                    // notify_pointer_motion_absolute wants coordinates in
                    // the stream's own logical pixel space, not the wire
                    // protocol's normalized 0..=65535 range. PortalHandle's
                    // stream_size is published by PipeWireFrameThread's
                    // param_changed callback (Task 4) once format
                    // negotiation completes; before the first frame it's
                    // (0, 0), which scales any motion to (0.0, 0.0) — a
                    // brief, harmless no-op rather than a wrong position.
                    remote
                        .notify_pointer_motion_absolute(
                            session,
                            stream_id,
                            Self::to_stream(x, stream_width),
                            Self::to_stream(y, stream_height),
                            NotifyPointerMotionAbsoluteOptions::default(),
                        )
                        .await
                }
                InputDetail::Wheel { dx, dy } => {
                    remote
                        .notify_pointer_axis(
                            session,
                            f64::from(dx),
                            f64::from(dy),
                            NotifyPointerAxisOptions::default().set_finish(true),
                        )
                        .await
                }
                InputDetail::Press | InputDetail::Release => {
                    let state = if matches!(event.detail, InputDetail::Press) {
                        KeyState::Pressed
                    } else {
                        KeyState::Released
                    };
                    if event.logical >= POINTER_BUTTON_LOGICAL_BASE {
                        let button = Self::evdev_button(event.logical)?;
                        remote
                            .notify_pointer_button(
                                session,
                                button,
                                state,
                                NotifyPointerButtonOptions::default(),
                            )
                            .await
                    } else {
                        remote
                            .notify_keyboard_keycode(
                                session,
                                i32::try_from(event.scancode).map_err(|_| {
                                    MediaError::InputUnavailable(
                                        "scancode outside the portal's range".to_owned(),
                                    )
                                })?,
                                state,
                                NotifyKeyboardKeycodeOptions::default(),
                            )
                            .await
                    }
                }
            }
            .map_err(|e| MediaError::InputUnavailable(e.to_string()))
        })
    }

    fn capability(&self) -> InputCapability {
        let guard = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .as_ref()
            .map_or(InputCapability::PortalRemoteDesktop, |h| {
                h.input_capability()
            })
    }
}
```

- [ ] **Step 4: Wire `paired_with_injector` (finishes Task 4's forward reference)**

`WaylandPortalCapturer::paired_with_injector` (written in Task 4, Step 5) references `super::WaylandPortalInjector` — confirm it now resolves (it will, since this task just defined that type in the same file). No code change needed here; this step is just running the build to confirm the forward reference closes.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p lumepeer-media --lib --features capture-portal linux_wayland`
Expected: PASS — the three new tests from Step 1, plus everything from Task 3/4.

Run: `cargo clippy -p lumepeer-media --all-targets --features capture-portal -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add crates/media/src/capture/linux_wayland.rs
git commit -m "media: inject input through the portal RemoteDesktop interface on Wayland"
```

---

### Task 6: `platform_backend()` replacing `platform_capturer`/`platform_injector`

**Files:**
- Modify: `crates/media/src/capture/mod.rs`
- Test: `#[cfg(test)] mod tests` in the same file

**Interfaces:**
- Consumes: `detect_session_type` (Task 2); `WaylandPortalCapturer::paired_with_injector` (Task 4); `linux_x11::{X11Capturer, X11Injector}`.
- Produces: `pub fn platform_backend() -> Result<(Box<dyn ScreenCapturer>, Box<dyn InputInjector>)>`, replacing `platform_capturer()` and `platform_injector()` (removed — confirmed unused outside this module and its own tests by `grep -rn "platform_capturer\|platform_injector"` across the repo).

- [ ] **Step 1: Write the failing test**

Add to `crates/media/src/capture/mod.rs`'s `#[cfg(test)] mod tests`:

```rust
#[cfg(all(
    target_os = "linux",
    not(target_os = "android"),
    feature = "capture-x11",
    feature = "capture-portal"
))]
#[test]
fn platform_backend_picks_x11_when_the_session_says_so() {
    // This only exercises the branch selection, not a live connection —
    // X11Capturer::new()/X11Injector::connect() may still fail with no
    // display, which is fine and unrelated to what this test checks.
    match platform_backend_for(SessionType::X11) {
        Ok(_) | Err(MediaError::InputUnavailable(_)) => {}
        Err(other) => panic!("unexpected error for the X11 branch: {other}"),
    }
}

#[cfg(all(
    target_os = "linux",
    not(target_os = "android"),
    feature = "capture-portal"
))]
#[test]
fn platform_backend_picks_wayland_for_wayland_and_unknown_sessions() {
    for session in [SessionType::Wayland, SessionType::Unknown] {
        let (_, _) = platform_backend_for(session).expect(
            "the Wayland path only negotiates lazily on start(), so building it never fails here",
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p lumepeer-media --lib --features capture-x11,capture-portal platform_backend`
Expected: FAIL — `platform_backend_for` doesn't exist yet.

- [ ] **Step 3: Implement `platform_backend()`**

Replace `platform_capturer` and `platform_injector` (currently at lines 140-159 and 193-219 of `crates/media/src/capture/mod.rs`) with:

```rust
/// Opens the platform capture and input backend together (§11, ADR 0010).
///
/// Capture and input are no longer independently constructible on Wayland:
/// input injection needs the same portal `Session` capture negotiated, so
/// this replaces the old `platform_capturer`/`platform_injector` split with
/// one call that returns a matched pair.
///
/// # Errors
/// [`MediaError::CaptureUnavailable`] when no capture backend is compiled in
/// for this target; [`MediaError::InputUnavailable`] when the X11 branch
/// can't reach an input-capable display (capture may still be usable there
/// even when input isn't — callers that only need capture should construct
/// `linux_x11::X11Capturer`/`WaylandPortalCapturer` directly instead of
/// going through this pairing).
pub fn platform_backend() -> Result<(Box<dyn ScreenCapturer>, Box<dyn InputInjector>)> {
    platform_backend_for(detect_session_type())
}

fn platform_backend_for(
    session: SessionType,
) -> Result<(Box<dyn ScreenCapturer>, Box<dyn InputInjector>)> {
    match session {
        #[cfg(all(
            target_os = "linux",
            not(target_os = "android"),
            feature = "capture-x11"
        ))]
        SessionType::X11 => {
            let capturer = Box::new(linux_x11::X11Capturer::new()) as Box<dyn ScreenCapturer>;
            let injector =
                Box::new(linux_x11::X11Injector::connect()?) as Box<dyn InputInjector>;
            Ok((capturer, injector))
        }
        #[cfg(all(target_os = "linux", not(target_os = "android"), feature = "capture-portal"))]
        SessionType::Wayland | SessionType::Unknown => {
            let (capturer, injector) = linux_wayland::WaylandPortalCapturer::paired_with_injector();
            Ok((
                Box::new(capturer) as Box<dyn ScreenCapturer>,
                Box::new(injector) as Box<dyn InputInjector>,
            ))
        }
        #[cfg(not(all(
            target_os = "linux",
            not(target_os = "android"),
            any(feature = "capture-x11", feature = "capture-portal")
        )))]
        _ => Err(MediaError::CaptureUnavailable(
            "no capture backend is compiled in for this target".to_owned(),
        )),
        #[cfg(all(
            target_os = "linux",
            not(target_os = "android"),
            feature = "capture-x11",
            not(feature = "capture-portal")
        ))]
        SessionType::Wayland | SessionType::Unknown => Err(MediaError::CaptureUnavailable(
            "this build has no xdg-desktop-portal support".to_owned(),
        )),
        #[cfg(all(
            target_os = "linux",
            not(target_os = "android"),
            feature = "capture-portal",
            not(feature = "capture-x11")
        ))]
        SessionType::X11 => Err(MediaError::CaptureUnavailable(
            "this build has no X11 capture support".to_owned(),
        )),
    }
}
```

This is a wider `cfg` matrix than the previous two functions had, because `platform_backend_for` now has to handle four build configurations (both features, X11-only, portal-only, neither) instead of one feature toggling a single hardcoded branch. If the `cfg`-gated match arms produce an "unreachable pattern" or "non-exhaustive match" warning under a specific feature combination when this is actually built, resolve it by collapsing the always-present catch-all arm's `cfg` condition rather than adding more arms — the intent is exactly "unsupported session/feature combination for this target" for anything not explicitly handled above.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p lumepeer-media --lib --features capture-x11,capture-portal platform_backend`
Expected: PASS.

Run each feature combination clippy needs to stay clean for (matching what CI's `media` job already builds):
```
cargo clippy -p lumepeer-media --all-targets --features capture-x11,encode-openh264 -- -D warnings
cargo clippy -p lumepeer-media --all-targets --features capture-portal -- -D warnings
```

- [ ] **Step 5: Commit**

```bash
git add crates/media/src/capture/mod.rs
git commit -m "media: replace platform_capturer/platform_injector with paired platform_backend"
```

---

### Task 7: Full-crate verification and CI parity

**Files:** none created or modified — this task only runs checks.

- [ ] **Step 1: Run the exact commands CI's `media` job runs, in order**

```bash
cargo clippy -p lumepeer-media --all-targets --features capture-x11,encode-openh264 -- -D warnings
cargo build -p lumepeer-decoder-worker
cargo clippy -p lumepeer-media --all-targets --features capture-portal -- -D warnings
cargo clippy -p lumepeer-net --all-targets --features secret-service -- -D warnings
xvfb-run -a env LUMEPEER_TEST_XTEST=1 cargo test -p lumepeer-media --features capture-x11,encode-openh264
```

Expected: all clean/passing, unchanged from before this plan (the X11 path was not touched).

- [ ] **Step 2: Run the new Wayland-path tests explicitly**

```bash
cargo test -p lumepeer-media --lib --features capture-portal
```

Expected: PASS, including `pipewire_stream`'s dedup/backpressure tests and `linux_wayland`'s mapping tests. The `LUMEPEER_TEST_PORTAL=1`-gated live test stays skipped (no compositor in this environment) — that's expected, not a gap in this step.

- [ ] **Step 3: Manual verification note for the user**

This plan cannot exercise a live portal/PipeWire session in an automated way (same limitation the existing X11 `LUMEPEER_TEST_XTEST` test has for XTEST, just with no CI equivalent at all for portal since there's no virtual Wayland compositor in CI). Before calling this feature done, run on a real Wayland desktop:

```bash
LUMEPEER_TEST_PORTAL=1 cargo test -p lumepeer-media --lib --features capture-portal an_empty_device_mask_degrades_to_view_only -- --ignored --nocapture
```

and separately, a manual smoke test calling `platform_backend()`, driving `start()`/`next_frame()` in a loop, and confirming real frames arrive and dedup correctly when the screen is idle. Report the result back rather than assuming green CI means the PipeWire path works — CI only proves it compiles.

- [ ] **Step 4: No commit for this task** (verification only; nothing to add to git).
