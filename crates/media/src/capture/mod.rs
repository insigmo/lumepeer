//! Screen capture: one trait, one backend per platform (design doc §11, §11.1).
//!
//! Capture never starts without an active viewer and stops as soon as the last
//! viewer leaves (§8.1, §11).

use std::collections::BTreeSet;

use lumepeer_core::NodeId;
use lumepeer_core::protocol::InputEventPayload;

use crate::error::{MediaError, Result};

#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub mod macos;

#[cfg(all(
    target_os = "linux",
    not(target_os = "android"),
    feature = "capture-x11"
))]
pub mod linux_x11;

#[cfg(all(target_os = "linux", not(target_os = "android")))]
pub mod linux_wayland;

/// What the platform allows besides pixels (§11.1, §18).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputCapability {
    /// Full keyboard and pointer injection.
    Full,
    /// Injection only through the desktop portal's `RemoteDesktop` interface.
    PortalRemoteDesktop,
    /// No injection at all: the session degrades to view-only and the UI says
    /// so explicitly (§18).
    None,
}

/// What to capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureTarget {
    /// The primary display.
    PrimaryDisplay,
    /// A specific display, as indexed by the platform backend.
    Display(u32),
}

/// Pixel format of a captured frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 8 bits per channel, BGRA order.
    Bgra8,
    /// Planar 4:2:0, 8 bits per sample.
    Nv12,
}

/// One captured frame.
#[derive(Debug, Clone)]
pub struct Frame {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Pixel format of `data`.
    pub format: PixelFormat,
    /// Monotonic capture timestamp in microseconds.
    pub timestamp_us: u64,
    /// Raw pixels.
    pub data: Vec<u8>,
}

/// Platform screen capture backend (§11.1).
pub trait ScreenCapturer: Send + std::fmt::Debug {
    /// Starts capturing `target`.
    ///
    /// # Errors
    /// [`MediaError::PermissionDenied`] if the user declines the system
    /// prompt, [`MediaError::CaptureUnavailable`] if the backend cannot run.
    fn start(&mut self, target: CaptureTarget) -> Result<()>;

    /// Returns the next frame, or `None` when it is identical to the previous
    /// one (§11.1).
    ///
    /// # Errors
    /// [`MediaError::CaptureInterrupted`] on screen lock, user switch or
    /// desktop change (§18).
    fn next_frame(&mut self) -> Result<Option<Frame>>;

    /// Stops capturing. Idempotent.
    fn stop(&mut self);

    /// What this backend allows on the input side.
    fn input_capability(&self) -> InputCapability;
}

/// Platform input adapter (§11).
///
/// The adapter is the last step, never the first: `lumepeer-core` authorizes
/// every event before it reaches an implementation of this trait, and the
/// adapter itself never consults grants. Guests send logical keys and physical
/// scancodes, never raw OS handles (§11).
pub trait InputInjector: Send + std::fmt::Debug {
    /// Injects one already authorized event.
    ///
    /// # Errors
    /// [`MediaError::InputUnavailable`] if the platform refuses the injection,
    /// which on macOS means the accessibility permission was withdrawn during
    /// the session and the caller must revoke (§18).
    fn inject(&mut self, event: &InputEventPayload) -> Result<()>;

    /// What this adapter can do, mirroring [`ScreenCapturer::input_capability`].
    fn capability(&self) -> InputCapability;
}

/// Injector that refuses everything, for a session that degraded to view-only
/// because the platform gave no input capability (§18).
#[derive(Debug, Default)]
pub struct NoInputInjector;

impl InputInjector for NoInputInjector {
    fn inject(&mut self, _event: &InputEventPayload) -> Result<()> {
        Err(MediaError::InputUnavailable(
            "this session has no input capability".to_owned(),
        ))
    }

    fn capability(&self) -> InputCapability {
        InputCapability::None
    }
}

/// Opens the input adapter of the current platform.
///
/// # Errors
/// [`MediaError::InputUnavailable`] when no adapter is compiled in. The caller
/// continues view-only and says so in the UI rather than failing the session
/// (§18).
pub fn platform_injector() -> Result<Box<dyn InputInjector>> {
    #[cfg(all(
        target_os = "linux",
        not(target_os = "android"),
        feature = "capture-x11"
    ))]
    {
        linux_x11::X11Injector::connect().map(|i| Box::new(i) as Box<dyn InputInjector>)
    }
    #[cfg(not(all(
        target_os = "linux",
        not(target_os = "android"),
        feature = "capture-x11"
    )))]
    {
        Err(MediaError::InputUnavailable(
            "no input adapter is compiled in for this target".to_owned(),
        ))
    }
}

/// Capturer that produces nothing. Used in phase 0/1, where the consent and
/// transport paths are exercised without any platform SDK.
#[derive(Debug, Default)]
pub struct StubCapturer {
    running: bool,
}

impl ScreenCapturer for StubCapturer {
    fn start(&mut self, _target: CaptureTarget) -> Result<()> {
        self.running = true;
        Ok(())
    }

    fn next_frame(&mut self) -> Result<Option<Frame>> {
        if self.running {
            Ok(None)
        } else {
            Err(MediaError::CaptureUnavailable(
                "capturer not started".to_owned(),
            ))
        }
    }

    fn stop(&mut self) {
        self.running = false;
    }

    fn input_capability(&self) -> InputCapability {
        InputCapability::None
    }
}

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

/// Opens the capture backend of the current platform.
///
/// # Errors
/// [`MediaError::CaptureUnavailable`] when no backend is compiled in for this
/// target.
pub fn platform_capturer() -> Result<Box<dyn ScreenCapturer>> {
    #[cfg(all(
        target_os = "linux",
        not(target_os = "android"),
        feature = "capture-x11"
    ))]
    {
        // X11 first, Wayland later (ADR 0003). On a Wayland session `start`
        // fails unless Xwayland is reachable, and the error says so.
        Ok(Box::new(linux_x11::X11Capturer::new()))
    }
    #[cfg(all(target_os = "windows", feature = "capture-windows"))]
    {
        Ok(Box::new(windows::WindowsCapturer::new()))
    }
    #[cfg(all(target_os = "macos", feature = "capture-screencapturekit"))]
    {
        Ok(Box::new(macos::MacosCapturer::new()))
    }
    #[cfg(not(any(
        all(
            target_os = "linux",
            not(target_os = "android"),
            feature = "capture-x11"
        ),
        all(target_os = "windows", feature = "capture-windows"),
        all(target_os = "macos", feature = "capture-screencapturekit"),
    )))]
    {
        Err(MediaError::CaptureUnavailable(
            "no capture backend is compiled in for this target".to_owned(),
        ))
    }
}

/// Viewer-gated owner of a capturer (§8.1, §11).
///
/// Capture starts when the first viewer holding a `view` grant appears and
/// stops with the last one. There is no path that produces a frame with no
/// viewer attached: that is the "no capture without viewer" rule of §19 phase 2
/// and it is enforced here rather than by convention at the call sites.
#[derive(Debug)]
pub struct CaptureController {
    capturer: Box<dyn ScreenCapturer>,
    target: CaptureTarget,
    viewers: BTreeSet<NodeId>,
    capturing: bool,
}

impl CaptureController {
    /// Wraps `capturer`; nothing is captured until a viewer is added.
    #[must_use]
    pub fn new(capturer: Box<dyn ScreenCapturer>, target: CaptureTarget) -> Self {
        Self {
            capturer,
            target,
            viewers: BTreeSet::new(),
            capturing: false,
        }
    }

    /// Registers a viewer, starting capture if it is the first one.
    ///
    /// Idempotent per peer: the same viewer twice does not start a second
    /// capture and does not keep it alive after its single revoke.
    ///
    /// # Errors
    /// Propagates the backend error from [`ScreenCapturer::start`]; the viewer
    /// is not registered if capture could not start.
    pub fn add_viewer(&mut self, peer: NodeId) -> Result<()> {
        if self.viewers.contains(&peer) {
            return Ok(());
        }
        if self.viewers.is_empty() {
            self.capturer.start(self.target)?;
            self.capturing = true;
        }
        self.viewers.insert(peer);
        Ok(())
    }

    /// Removes a viewer, stopping capture with the last one.
    pub fn remove_viewer(&mut self, peer: &NodeId) {
        if !self.viewers.remove(peer) {
            return;
        }
        if self.viewers.is_empty() {
            self.stop();
        }
    }

    /// Drops every viewer and stops capture: revoke, screen lock, user switch
    /// or license expiry (§8.1, §18).
    pub fn stop(&mut self) {
        self.viewers.clear();
        if self.capturing {
            self.capturer.stop();
            self.capturing = false;
        }
    }

    /// Number of viewers currently holding a `view` grant.
    #[must_use]
    pub fn viewer_count(&self) -> usize {
        self.viewers.len()
    }

    /// Whether the backend is currently capturing.
    #[must_use]
    pub const fn is_capturing(&self) -> bool {
        self.capturing
    }

    /// What this backend allows on the input side (§11.1).
    #[must_use]
    pub fn input_capability(&self) -> InputCapability {
        self.capturer.input_capability()
    }

    /// Next frame, or `None` when the screen has not changed (§11.1).
    ///
    /// # Errors
    /// [`MediaError::CaptureUnavailable`] when no viewer is attached, plus
    /// whatever the backend reports.
    pub fn next_frame(&mut self) -> Result<Option<Frame>> {
        if !self.capturing {
            return Err(MediaError::CaptureUnavailable(
                "no viewer holds a view grant: capture must not run".to_owned(),
            ));
        }
        self.capturer.next_frame()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// Capturer that counts starts and stops and always yields the same frame.
    #[derive(Debug, Default)]
    struct CountingCapturer {
        starts: usize,
        stops: usize,
        running: bool,
    }

    impl ScreenCapturer for CountingCapturer {
        fn start(&mut self, _target: CaptureTarget) -> Result<()> {
            self.starts += 1;
            self.running = true;
            Ok(())
        }

        fn next_frame(&mut self) -> Result<Option<Frame>> {
            assert!(self.running, "frames must never be produced while stopped");
            Ok(Some(Frame {
                width: 2,
                height: 1,
                format: PixelFormat::Bgra8,
                timestamp_us: 0,
                data: vec![0; 8],
            }))
        }

        fn stop(&mut self) {
            self.stops += 1;
            self.running = false;
        }

        fn input_capability(&self) -> InputCapability {
            InputCapability::Full
        }
    }

    fn peer(n: u8) -> NodeId {
        iroh_base::SecretKey::from_bytes(&[n; 32]).public()
    }

    #[test]
    fn nothing_is_captured_without_a_viewer() {
        let mut controller = CaptureController::new(
            Box::new(CountingCapturer::default()),
            CaptureTarget::PrimaryDisplay,
        );
        assert!(!controller.is_capturing());
        assert!(matches!(
            controller.next_frame(),
            Err(MediaError::CaptureUnavailable(_))
        ));
    }

    #[test]
    fn capture_starts_with_the_first_viewer_and_stops_with_the_last() {
        let mut controller = CaptureController::new(
            Box::new(CountingCapturer::default()),
            CaptureTarget::PrimaryDisplay,
        );

        controller.add_viewer(peer(1)).unwrap();
        assert!(controller.is_capturing());
        assert!(controller.next_frame().unwrap().is_some());

        controller.add_viewer(peer(2)).unwrap();
        // A second viewer must not start a second capture.
        assert_eq!(controller.viewer_count(), 2);

        controller.remove_viewer(&peer(1));
        assert!(controller.is_capturing(), "one viewer is still watching");

        controller.remove_viewer(&peer(2));
        assert!(!controller.is_capturing());
        assert_eq!(controller.viewer_count(), 0);
        assert!(matches!(
            controller.next_frame(),
            Err(MediaError::CaptureUnavailable(_))
        ));
    }

    #[test]
    fn adding_the_same_viewer_twice_does_not_outlive_its_revoke() {
        let mut controller = CaptureController::new(
            Box::new(CountingCapturer::default()),
            CaptureTarget::PrimaryDisplay,
        );
        controller.add_viewer(peer(1)).unwrap();
        controller.add_viewer(peer(1)).unwrap();
        assert_eq!(controller.viewer_count(), 1);
        controller.remove_viewer(&peer(1));
        assert!(!controller.is_capturing());
    }

    #[test]
    fn stop_drops_every_viewer_at_once() {
        let mut controller = CaptureController::new(
            Box::new(CountingCapturer::default()),
            CaptureTarget::PrimaryDisplay,
        );
        controller.add_viewer(peer(1)).unwrap();
        controller.add_viewer(peer(2)).unwrap();
        controller.stop();
        assert_eq!(controller.viewer_count(), 0);
        assert!(!controller.is_capturing());
    }

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
}
