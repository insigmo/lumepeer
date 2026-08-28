//! Screen capture: one trait, one backend per platform (design doc §11, §11.1).
//!
//! Capture never starts without an active viewer and stops as soon as the last
//! viewer leaves (§8.1, §11).

use std::collections::BTreeSet;

use lumepeer_core::NodeId;
use lumepeer_core::constants::MAX_CURSOR_SHAPE_PIXELS;
use lumepeer_core::protocol::{CursorShapeData, InputEventPayload};

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
#[cfg(all(
    target_os = "linux",
    not(target_os = "android"),
    feature = "capture-portal"
))]
pub(crate) mod pipewire_stream;

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

/// One monitor of this host, for the §11 `MonitorsList` announcement
/// (ADR 0028).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostMonitor {
    /// Stable id a guest passes back in [`CaptureTarget::Display`] — the
    /// platform's own enumeration index, the same order `Display` uses.
    pub id: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Whether this is the primary display.
    pub primary: bool,
}

/// Every monitor this host can capture, in the order
/// [`CaptureTarget::Display`] indexes (§11 `MonitorsList`; ADR 0028).
///
/// # Errors
/// [`MediaError::CaptureUnavailable`] when the platform cannot enumerate its
/// displays at all; the caller degrades to a single primary entry only where
/// the platform itself guarantees one.
pub fn host_monitors() -> Result<Vec<HostMonitor>> {
    #[cfg(target_os = "windows")]
    {
        windows::WindowsCapturer::attached_monitors_info()
    }
    #[cfg(all(target_os = "linux", not(target_os = "android")))]
    {
        linux_host_monitors()
    }
    #[cfg(not(any(
        target_os = "windows",
        all(target_os = "linux", not(target_os = "android"))
    )))]
    {
        // macOS is the only platform left here, and its ScreenCaptureKit
        // backend does not enumerate displays yet: report the primary as
        // the only monitor rather than an empty list, which would read as
        // "this host has no screens" (§18: degrade honestly, never lie).
        //
        // TODO(docs/tasks/12-macos-completion.md): once that backend can
        // enumerate, this arm becomes the "no backend at all" case only.
        Ok(vec![HostMonitor {
            id: 0,
            width: 0,
            height: 0,
            primary: true,
        }])
    }
}

/// The primary head's size as X reports it, or `None` when this build has no
/// X11 backend to ask.
///
/// Only ever the Wayland path's pre-negotiation estimate: on a Wayland
/// session Xwayland reports the compositor's own outputs, so this is the same
/// geometry the portal would hand back — available without raising a dialog
/// for it (ADR 0028).
#[cfg(all(
    target_os = "linux",
    not(target_os = "android"),
    feature = "capture-portal"
))]
fn x11_primary_size() -> Option<(u32, u32)> {
    #[cfg(feature = "capture-x11")]
    {
        linux_x11::host_monitors().ok().and_then(|monitors| {
            monitors
                .iter()
                .find(|monitor| monitor.primary)
                .or_else(|| monitors.first())
                .map(|monitor| (monitor.width, monitor.height))
        })
    }
    #[cfg(not(feature = "capture-x11"))]
    {
        None
    }
}

/// Linux monitor enumeration, which is two different answers because Linux is
/// two different capture paths (ADR 0028).
///
/// On a Wayland session the list is always exactly **one** entry, and that is
/// not a limitation to be fixed later: a portal session grants one stream,
/// chosen by the user in the portal's own dialog, and the guest's
/// `MonitorSelect` cannot move it. Announcing three monitors there would
/// promise a choice `CaptureTarget::Display` has no way to honour — the
/// contract this list carries is "these are the ids `Display` indexes", so a
/// path where `Display` is inert must announce one.
///
/// On an X11 session it is the `RandR` head list, in the order
/// `CaptureTarget::Display` indexes.
///
/// # Errors
/// [`MediaError::CaptureUnavailable`] when the X11 path is the one in use and
/// there is no display to connect to.
#[cfg(all(target_os = "linux", not(target_os = "android")))]
#[allow(
    clippy::unnecessary_wraps,
    reason = "the X11 arm below genuinely errors; a portal-only build is the one               configuration where every arm succeeds, and the signature has to               match `host_monitors` across all of them"
)]
fn linux_host_monitors() -> Result<Vec<HostMonitor>> {
    #[cfg(feature = "capture-portal")]
    if matches!(
        detect_session_type(),
        SessionType::Wayland | SessionType::Unknown
    ) {
        // Preference order, all three honest: the size the portal actually
        // negotiated; failing that the primary head as Xwayland's `RandR`
        // reports it, which is the same compositor's own geometry and needs
        // no dialog; failing that zeros, which is what "nothing has told us
        // yet" looks like.
        let size = linux_wayland::portal::last_stream_size().or_else(x11_primary_size);
        let (width, height) = size.unwrap_or((0, 0));
        if width == 0 || height == 0 {
            tracing::debug!(
                "no portal stream negotiated yet and no X fallback: reporting an unsized monitor"
            );
        }
        return Ok(vec![HostMonitor {
            id: 0,
            width,
            height,
            primary: true,
        }]);
    }

    #[cfg(feature = "capture-x11")]
    {
        linux_x11::host_monitors()
    }
    #[cfg(not(feature = "capture-x11"))]
    {
        Ok(vec![HostMonitor {
            id: 0,
            width: 0,
            height: 0,
            primary: true,
        }])
    }
}

/// How many displays this host can capture — the exclusive upper bound of
/// [`CaptureTarget::Display`].
///
/// # Errors
/// [`MediaError::CaptureUnavailable`] when the platform cannot enumerate its
/// displays.
pub fn host_display_count() -> Result<usize> {
    #[cfg(target_os = "windows")]
    {
        windows::WindowsCapturer::display_count()
    }
    #[cfg(all(target_os = "linux", not(target_os = "android")))]
    {
        // Derived from the same list rather than counted separately: the two
        // must agree, because this is the bound `MonitorSelect` is range
        // checked against and that list is what the guest picked from.
        linux_host_monitors().map(|monitors| monitors.len())
    }
    #[cfg(not(any(
        target_os = "windows",
        all(target_os = "linux", not(target_os = "android"))
    )))]
    {
        Ok(1)
    }
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

    /// The host's cursor, if this backend can report it *and* it changed since
    /// the last call (§11's `CursorShape`).
    ///
    /// Only when it changed: a cursor bitmap is up to
    /// `MAX_CURSOR_SHAPE_PIXELS` pixels, and sending it with every frame would
    /// be a second video channel for a picture that changes when someone
    /// hovers a text field.
    ///
    /// The default is `None`, and that is the honest answer for a platform
    /// whose compositor burns the cursor into the frame and never hands the
    /// bitmap over — Wayland's `CursorMode::Embedded` and macOS's
    /// `setShowsCursor(true)` both do. A made-up shape would be worse than
    /// none: the guest would draw a second cursor next to the real one.
    ///
    /// No position travels with it. Where the cursor is is something the guest
    /// already knows — it is the one moving the pointer — and a message per
    /// mouse move would cost more than the latency it saves (§11).
    fn cursor_shape(&mut self) -> Option<CursorShapeData> {
        None
    }

    /// Asks the backend to stop drawing the cursor into the frames it
    /// produces, because the guest is about to draw it itself.
    ///
    /// Ignored by default, which is again the honest answer where the
    /// compositor owns the decision. A backend that ignores it keeps
    /// compositing, and a guest that sees no [`Self::cursor_shape`] therefore
    /// draws nothing — two cursors is worse than one that lags.
    fn set_cursor_embedded(&mut self, _embedded: bool) {}
}

/// Builds a [`CursorShapeData`] after checking it against the bound of §14.
///
/// The check happens here, before the `Vec` is handed on, because this is the
/// point where a platform's own numbers become a wire payload: `width *
/// height` over `MAX_CURSOR_SHAPE_PIXELS`, a pixel buffer that contradicts the
/// geometry, a zero axis or a hotspot outside the shape are all a cursor to
/// drop rather than a frame to send. `MessageEnvelope::check_limits` rejects
/// the same shapes on the way in; this is the matching refusal on the way out.
///
/// `pixels` is premultiplied BGRA, which is what every backend that can report
/// a cursor at all produces (see [`CursorShapeData::rgba`]).
#[must_use]
pub fn cursor_shape(
    width: u16,
    height: u16,
    hotspot_x: u16,
    hotspot_y: u16,
    pixels: Vec<u8>,
) -> Option<CursorShapeData> {
    let area = usize::from(width).checked_mul(usize::from(height))?;
    if area == 0 || area > MAX_CURSOR_SHAPE_PIXELS {
        return None;
    }
    if pixels.len() != area.checked_mul(4)? {
        return None;
    }
    if hotspot_x >= width || hotspot_y >= height {
        return None;
    }
    Some(CursorShapeData {
        width,
        height,
        hotspot_x,
        hotspot_y,
        rgba: pixels,
    })
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
    #[cfg(all(target_os = "windows", feature = "capture-windows"))]
    {
        windows::WindowsInjector::connect().map(|i| Box::new(i) as Box<dyn InputInjector>)
    }
    #[cfg(all(
        any(target_os = "macos", target_os = "ios"),
        feature = "capture-screencapturekit"
    ))]
    {
        macos::MacosInjector::connect().map(|i| Box::new(i) as Box<dyn InputInjector>)
    }
    #[cfg(not(any(
        all(
            target_os = "linux",
            not(target_os = "android"),
            feature = "capture-x11"
        ),
        all(target_os = "windows", feature = "capture-windows"),
        all(
            any(target_os = "macos", target_os = "ios"),
            feature = "capture-screencapturekit"
        ),
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

/// The platform's capture backend and, where the two cannot be built apart,
/// the injector that shares its session (§11, ADR 0010).
///
/// The injector is optional rather than a [`NoInputInjector`]: "this platform
/// builds input separately" and "this platform has no input" are different
/// facts, and only the first one leaves [`platform_injector`] worth calling.
pub type PlatformBackend = (Box<dyn ScreenCapturer>, Option<Box<dyn InputInjector>>);

/// Opens the capture backend, together with an input injector when the
/// platform requires the two to come from one session (§11, ADR 0010).
///
/// Only the Wayland portal path returns `Some` injector, and it has to: the
/// `RemoteDesktop` `notify_*` calls need the very `Session` handle that
/// `SelectDevices`/`Start` ran on, so building an injector separately there
/// would raise a second consent dialog and then inject into a session capture
/// never claimed. Every other platform returns `None` and keeps building its
/// injector lazily through [`platform_injector`] — which is what lets a host
/// with no input adapter still run view-only instead of failing the session
/// (§18).
///
/// # Errors
/// [`MediaError::CaptureUnavailable`] when no capture backend is compiled in
/// for this target.
pub fn platform_backend() -> Result<PlatformBackend> {
    #[cfg(all(
        target_os = "linux",
        not(target_os = "android"),
        feature = "capture-portal"
    ))]
    {
        // `Unknown` goes to the portal too: with no signal either way the
        // portal is the path that asks the user, and a wrong guess towards
        // X11 would capture nothing on a Wayland desktop (§18).
        if matches!(
            detect_session_type(),
            SessionType::Wayland | SessionType::Unknown
        ) {
            let (capturer, injector) = linux_wayland::WaylandPortalCapturer::paired_with_injector();
            return Ok((
                Box::new(capturer) as Box<dyn ScreenCapturer>,
                Some(Box::new(injector) as Box<dyn InputInjector>),
            ));
        }
    }
    Ok((platform_capturer()?, None))
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
    // Portal-only Linux build: no X11 to prefer, so the portal is the
    // capture path. Callers that also need input must go through
    // [`platform_backend`] instead — an injector built separately here would
    // negotiate its own portal session (ADR 0010).
    #[cfg(all(
        target_os = "linux",
        not(target_os = "android"),
        feature = "capture-portal",
        not(feature = "capture-x11")
    ))]
    {
        Ok(Box::new(linux_wayland::WaylandPortalCapturer::new()))
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
        all(
            target_os = "linux",
            not(target_os = "android"),
            feature = "capture-portal"
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

    /// Points capture at a different monitor (§11 `MonitorSelect`; ADR 0028).
    ///
    /// A live capture is restarted on the new target immediately, so the
    /// encode loop's very next frame shows the new display; an idle
    /// controller only remembers the choice for its next `add_viewer`.
    ///
    /// # Errors
    /// Whatever [`ScreenCapturer::start`] reports for the new target — an id
    /// past the display count is the caller's malformed request, not a
    /// silent keep-capturing-the-old-screen.
    pub fn set_target(&mut self, target: CaptureTarget) -> Result<()> {
        self.target = target;
        if self.capturing {
            // `start` drops the previous duplication first on the platforms
            // that cap them, so this cannot compete with itself.
            self.capturer.start(target)?;
        }
        Ok(())
    }

    /// What this controller is pointed at right now.
    #[must_use]
    pub const fn target(&self) -> CaptureTarget {
        self.target
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

    /// The host's cursor, when the backend can report it and it changed
    /// (§11's `CursorShape`).
    ///
    /// Gated on `capturing` exactly like [`Self::next_frame`]: "no viewer, no
    /// capture" (§8.1) covers the pointer too, and a cursor read from an idle
    /// controller would be the one thing this gate does not stop.
    pub fn cursor_shape(&mut self) -> Option<CursorShapeData> {
        if !self.capturing {
            return None;
        }
        self.capturer.cursor_shape()
    }

    /// Stops or resumes drawing the cursor into captured frames (§11).
    ///
    /// Whether it takes effect is the backend's answer, not this one's: a
    /// compositor that owns the decision ignores it, and the guest then never
    /// receives a shape and never draws one.
    pub fn set_cursor_embedded(&mut self, embedded: bool) {
        self.capturer.set_cursor_embedded(embedded);
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
        /// One cursor, handed out once — the shape of a real backend, which
        /// answers `None` for a cursor that has not changed.
        cursor_taken: bool,
    }

    impl ScreenCapturer for CountingCapturer {
        fn start(&mut self, _target: CaptureTarget) -> Result<()> {
            self.starts += 1;
            self.running = true;
            Ok(())
        }

        fn cursor_shape(&mut self) -> Option<CursorShapeData> {
            if self.cursor_taken {
                return None;
            }
            self.cursor_taken = true;
            cursor_shape(1, 1, 0, 0, vec![0, 0, 0, 255])
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

    /// §8.1 covers the pointer too: an idle controller reports no cursor, for
    /// the same reason it refuses a frame.
    #[test]
    fn no_viewer_means_no_cursor_either() {
        let mut controller = CaptureController::new(
            Box::new(CountingCapturer::default()),
            CaptureTarget::PrimaryDisplay,
        );
        assert!(controller.cursor_shape().is_none());

        let watcher = peer(7);
        controller.add_viewer(watcher).unwrap();
        assert!(
            controller.cursor_shape().is_some(),
            "a live capture reports its cursor"
        );
        assert!(
            controller.cursor_shape().is_none(),
            "an unchanged cursor is reported once and not again"
        );

        controller.remove_viewer(&watcher);
        assert!(controller.cursor_shape().is_none());
    }
}
