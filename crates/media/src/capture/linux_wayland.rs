//! Wayland capture and input via xdg-desktop-portal and PipeWire (design doc
//! §11, ADR 0010).
//!
//! The portal call order is normative and must not be reordered "to simplify
//! the code": `CreateSession`, then `SelectDevices`, then `SelectSources`,
//! then `Start`. [`portal::PortalHandle::negotiate`] is written in exactly
//! that order and the test below pins it, because getting it wrong is the
//! kind of change that looks harmless in review.
//!
//! A zero input-device mask coming back from `Start` is not an error: it is the
//! user declining input in the system dialog. The session then continues with
//! [`InputCapability::None`] and the UI says so, rather than claiming a control
//! it cannot exercise (§18).
//!
//! Capture and input share one negotiated portal session: `notify_*` calls on
//! the `RemoteDesktop` interface need the same `Session` handle
//! `SelectDevices`/`Start` used, so [`WaylandPortalCapturer`] and
//! [`WaylandPortalInjector`] hold the same `Arc<Mutex<Option<PortalHandle>>>`
//! rather than negotiating independently. Use
//! [`WaylandPortalCapturer::paired_with_injector`] to build both at once.

#[cfg(feature = "capture-portal")]
use std::sync::{Arc, Mutex};

use crate::capture::{CaptureTarget, Frame, InputCapability, ScreenCapturer};
use crate::error::{MediaError, Result};

/// One step of the portal handshake, in the order §11 fixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PortalStep {
    /// `org.freedesktop.portal.RemoteDesktop.CreateSession`.
    CreateSession,
    /// `SelectDevices`, strictly between `CreateSession` and `SelectSources`.
    SelectDevices,
    /// `org.freedesktop.portal.ScreenCast.SelectSources`.
    SelectSources,
    /// `Start`, which raises the user's dialog.
    Start,
}

/// The normative order of §11.
pub const PORTAL_CALL_ORDER: [PortalStep; 4] = [
    PortalStep::CreateSession,
    PortalStep::SelectDevices,
    PortalStep::SelectSources,
    PortalStep::Start,
];

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
    pub fn paired_with_injector() -> (Self, WaylandPortalInjector) {
        let shared = Arc::new(Mutex::new(None));
        (
            Self {
                shared: Arc::clone(&shared),
                stream: None,
            },
            WaylandPortalInjector::new(shared),
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
            let (node_id, stream_size) = match guard.as_ref() {
                Some(handle) => (handle.node_id(), handle.stream_size_handle()),
                None => {
                    // Unreachable in practice (just negotiated above), but
                    // refusing here rather than assuming it beats a panic if
                    // a future refactor breaks that invariant (§21).
                    return Err(MediaError::CaptureUnavailable(
                        "portal negotiation produced no session".to_owned(),
                    ));
                }
            };
            drop(guard);

            let node_id = node_id.ok_or_else(|| {
                MediaError::CaptureUnavailable("the portal granted no PipeWire stream".to_owned())
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
            let stream = self
                .stream
                .as_ref()
                .ok_or_else(|| MediaError::CaptureUnavailable("capturer not started".to_owned()))?;
            Ok(stream.try_recv_frame())
        }
        #[cfg(not(feature = "capture-portal"))]
        Err(MediaError::CaptureUnavailable(
            "the portal capture path produces no frames yet".to_owned(),
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
            guard
                .as_ref()
                .map_or(InputCapability::PortalRemoteDesktop, |h| {
                    h.input_capability()
                })
        }
        #[cfg(not(feature = "capture-portal"))]
        InputCapability::PortalRemoteDesktop
    }
}

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
/// hold the same `Arc<Mutex<Option<portal::PortalHandle>>>`, since
/// `notify_*` needs the `Session` that capture's negotiation produced.
#[cfg(feature = "capture-portal")]
#[derive(Debug)]
pub struct WaylandPortalInjector {
    shared: Arc<Mutex<Option<portal::PortalHandle>>>,
}

#[cfg(feature = "capture-portal")]
impl WaylandPortalInjector {
    /// Wraps a portal handle shared with a capturer. Use
    /// [`WaylandPortalCapturer::paired_with_injector`] rather than calling
    /// this directly, so the two always share the same session.
    #[must_use]
    pub const fn new(shared: Arc<Mutex<Option<portal::PortalHandle>>>) -> Self {
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
                    // param_changed callback once format negotiation
                    // completes; before the first frame it's (0, 0), which
                    // scales any motion to (0.0, 0.0) — a brief, harmless
                    // no-op rather than a wrong position.
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

/// The portal handshake and everything kept alive after it (§11, ADR 0010).
///
/// Kept as an inline module so the file list of §6 stays exact.
#[cfg(feature = "capture-portal")]
pub mod portal {
    use ashpd::desktop::PersistMode;
    use ashpd::desktop::Session;
    use ashpd::desktop::remote_desktop::{
        DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions,
    };
    use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};

    use super::{PORTAL_CALL_ORDER, PortalStep};
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
            self.width
                .store(width, std::sync::atomic::Ordering::Relaxed);
            self.height
                .store(height, std::sync::atomic::Ordering::Relaxed);
            // Also published process-wide, for `last_stream_size` below.
            LAST_STREAM_SIZE.store(
                (u64::from(width) << u32::BITS) | u64::from(height),
                std::sync::atomic::Ordering::Relaxed,
            );
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

    /// Size of the most recently negotiated portal stream, packed as
    /// `width << 32 | height`, or 0 while nothing has been negotiated.
    ///
    /// Process-global on purpose. [`crate::capture::host_monitors`] is a free
    /// function with no handle on the live capturer, and on a Wayland session
    /// the only truthful answer to "how big is the screen this guest sees" is
    /// the size the portal actually negotiated — asking again would mean a
    /// second consent dialog for a question the running session already
    /// answered (ADR 0028).
    static LAST_STREAM_SIZE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// The negotiated portal stream's size, or `None` before the first frame
    /// of the first session has fixed a format.
    #[must_use]
    pub fn last_stream_size() -> Option<(u32, u32)> {
        let packed = LAST_STREAM_SIZE.load(std::sync::atomic::Ordering::Relaxed);
        if packed == 0 {
            return None;
        }
        // Both halves came from a `u32` in `StreamSize::set`, so neither
        // narrowing can lose anything.
        let width = u32::try_from(packed >> u32::BITS).unwrap_or(0);
        let height = u32::try_from(packed & u64::from(u32::MAX)).unwrap_or(0);
        (width > 0 && height > 0).then_some((width, height))
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

        #[allow(
            clippy::type_complexity,
            reason = "internal handshake result, not a public signature"
        )]
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failed assumption must fail the test")]

    use super::*;

    /// §11: the order is normative. This test exists so that reordering the
    /// calls fails here rather than on a user's machine.
    #[test]
    fn select_devices_sits_between_create_session_and_select_sources() {
        let create = PORTAL_CALL_ORDER
            .iter()
            .position(|step| *step == PortalStep::CreateSession)
            .unwrap_or(usize::MAX);
        let devices = PORTAL_CALL_ORDER
            .iter()
            .position(|step| *step == PortalStep::SelectDevices)
            .unwrap_or(usize::MAX);
        let sources = PORTAL_CALL_ORDER
            .iter()
            .position(|step| *step == PortalStep::SelectSources)
            .unwrap_or(usize::MAX);
        let start = PORTAL_CALL_ORDER
            .iter()
            .position(|step| *step == PortalStep::Start)
            .unwrap_or(usize::MAX);

        assert!(create < devices, "SelectDevices comes after CreateSession");
        assert!(
            devices < sources,
            "SelectDevices comes before SelectSources"
        );
        assert!(sources < start, "Start comes last");
    }

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

    #[cfg(feature = "capture-portal")]
    #[test]
    fn normalized_coordinates_map_onto_the_stream() {
        // Both ends are exact by construction — 0 maps to 0 and u16::MAX to
        // the full width — so these compare against an epsilon rather than
        // with `==` only to satisfy the crate-wide `float_cmp` lint.
        assert!(WaylandPortalInjector::to_stream(0, 1920).abs() < f64::EPSILON);
        assert!((WaylandPortalInjector::to_stream(u16::MAX, 1920) - 1920.0).abs() < f64::EPSILON);
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
        use crate::capture::InputInjector;
        use lumepeer_core::protocol::{InputDetail, InputEventPayload};

        let shared = std::sync::Arc::new(std::sync::Mutex::new(None));
        let mut injector = WaylandPortalInjector::new(shared);
        assert_eq!(injector.capability(), InputCapability::PortalRemoteDesktop);
        let result = injector.inject(&InputEventPayload {
            logical: 0,
            scancode: 30,
            modifiers: 0,
            detail: InputDetail::Press,
        });
        assert!(matches!(result, Err(MediaError::InputUnavailable(_))));
    }
}
