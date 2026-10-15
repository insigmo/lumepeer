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
//!
//! A host that remembers its grant ([`portal::RememberedGrant`]) asks the
//! portal to keep it and offers the restore token back on the next handshake,
//! so only the first guest needs somebody at the host's screen (ADR 0160).
//! A token the portal no longer honours costs one dialog, never a session that
//! waits for nothing: the handshake forgets it and asks again.

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
    /// The handshake in flight, answered once somebody has dealt with the
    /// portal's dialog.
    #[cfg(feature = "capture-portal")]
    negotiating: Option<std::sync::mpsc::Receiver<Result<portal::PortalHandle>>>,
    /// Where the portal's grant is kept between handshakes (ADR 0160).
    #[cfg(feature = "capture-portal")]
    grant: portal::RememberedGrant,
}

impl WaylandPortalCapturer {
    /// Creates a capturer with no portal session yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a capturer and injector that share one portal session. Both
    /// negotiate lazily: nothing happens until the capturer's first `start`.
    /// Every handshake offers `grant`'s token and keeps the one it gets back
    /// (ADR 0160).
    #[cfg(feature = "capture-portal")]
    #[must_use]
    pub fn paired_with_injector(grant: portal::RememberedGrant) -> (Self, WaylandPortalInjector) {
        let shared = Arc::new(Mutex::new(None));
        (
            Self {
                shared: Arc::clone(&shared),
                stream: None,
                negotiating: None,
                grant,
            },
            WaylandPortalInjector::new(shared),
        )
    }

    /// Opens the PipeWire stream of the negotiated session.
    #[cfg(feature = "capture-portal")]
    fn start_stream(&mut self) -> Result<()> {
        let guard = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (node_id, stream_size) = match guard.as_ref() {
            Some(handle) => (handle.node_id(), handle.stream_size_handle()),
            None => {
                // Unreachable in practice (only called once a session is
                // there), but refusing here rather than assuming it beats a
                // panic if a future refactor breaks that invariant (§21).
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
}

impl ScreenCapturer for WaylandPortalCapturer {
    fn start(&mut self, _target: CaptureTarget) -> Result<()> {
        #[cfg(feature = "capture-portal")]
        {
            let negotiated = self
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some();
            if negotiated {
                return self.start_stream();
            }
            // `Start` shows the portal's dialog and does not answer until a
            // person has, while `start` runs on the network actor: waiting
            // for it there stalled every command the host had, the grant
            // itself included, for as long as the dialog stayed up. So the
            // handshake runs on a thread of its own, and `next_frame` picks
            // up its answer.
            if self.negotiating.is_none() {
                let (answer, negotiating) = std::sync::mpsc::channel();
                let grant = self.grant.clone();
                std::thread::Builder::new()
                    .name("wayland-portal".to_owned())
                    .spawn(move || {
                        // Nobody is waiting any more if the capturer was
                        // stopped meanwhile; the session is dropped here.
                        let _ = answer.send(portal::PortalHandle::negotiate(&grant));
                    })
                    .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;
                self.negotiating = Some(negotiating);
            }
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
            use std::sync::mpsc::TryRecvError;

            let answer = match self
                .negotiating
                .as_ref()
                .map(std::sync::mpsc::Receiver::try_recv)
            {
                None => None,
                // The dialog is still up: no picture yet, and no failure.
                Some(Err(TryRecvError::Empty)) => return Ok(None),
                Some(answer) => Some(answer),
            };
            if let Some(answer) = answer {
                self.negotiating = None;
                let handle = answer.map_err(|_| {
                    MediaError::CaptureUnavailable(
                        "the portal handshake ended without an answer".to_owned(),
                    )
                })??;
                *self
                    .shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);
                self.start_stream()?;
            }
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
            self.negotiating = None;
            let mut guard = self
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = None;
        }
    }

    /// Once the stream runs, `next_frame` waits up to a frame interval for
    /// its next picture (ADR 0139). While the portal's dialog is still up it
    /// answers at once, and the caller has to pace it.
    fn waits_for_change(&self) -> bool {
        #[cfg(feature = "capture-portal")]
        {
            self.stream.is_some() && self.negotiating.is_none()
        }
        #[cfg(not(feature = "capture-portal"))]
        {
            false
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

        handle.block_on(async {
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
                    // The wire's `dy` is positive up; the portal's axis is
                    // positive down, like `wl_pointer.axis`.
                    remote
                        .notify_pointer_axis(
                            session,
                            f64::from(dx),
                            -f64::from(dy),
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
        })?
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

    /// Where a host keeps the portal's grant between handshakes (ADR 0160).
    ///
    /// A Wayland host shows nothing until somebody at its screen answers the
    /// portal's dialog, so a host nobody sits at never showed a guest
    /// anything. Asked to persist, the portal hands a restore token back from
    /// `Start` once the dialog's "Remember this selection" is left ticked, and a
    /// later handshake that offers it gets the same screen and devices with no
    /// dialog at all. The token is single-use: each `Start` returns the one to
    /// offer next time, which is why every handshake writes it back.
    ///
    /// Not a secret: the portal counts every unsandboxed program of this user
    /// as one and the same app, and any of them can list the portal's stored
    /// grants itself. So it is a plain file beside the app's other state, not
    /// a keystore entry — and a locked keyring on a host that started with
    /// nobody there must not be what stops its picture.
    ///
    /// The default keeps nothing: the dialog comes up every time, as before.
    #[derive(Debug, Clone, Default)]
    pub struct RememberedGrant {
        path: Option<std::path::PathBuf>,
    }

    impl RememberedGrant {
        /// A grant kept in the file at `path`.
        #[must_use]
        pub const fn at(path: std::path::PathBuf) -> Self {
            Self { path: Some(path) }
        }

        /// What `SelectDevices` asks the portal to do with the grant.
        fn persist_mode(&self) -> PersistMode {
            if self.path.is_some() {
                PersistMode::ExplicitlyRevoked
            } else {
                PersistMode::DoNot
            }
        }

        /// The token to offer, if one was kept and still looks like one.
        ///
        /// xdg-desktop-portal refuses `SelectDevices` outright for a token
        /// that is not a UUID, so a damaged file would fail every handshake
        /// instead of costing one dialog.
        fn load(&self) -> Option<String> {
            let text = std::fs::read_to_string(self.path.as_ref()?).ok()?;
            let token = text.trim();
            is_uuid(token).then(|| token.to_owned())
        }

        /// Keeps the token `Start` returned, or forgets the old one when it
        /// returned none: the person unticked "Remember", or the portal is too
        /// old to persist a remote-desktop session.
        fn keep(&self, token: Option<&str>) {
            let (Some(path), Some(token)) = (self.path.as_ref(), token) else {
                self.forget();
                return;
            };
            let written = path
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| {
                    use std::io::Write as _;
                    use std::os::unix::fs::OpenOptionsExt as _;
                    std::fs::OpenOptions::new()
                        .write(true)
                        .create(true)
                        .truncate(true)
                        .mode(0o600)
                        .open(path)?
                        .write_all(token.as_bytes())
                });
            if let Err(error) = written {
                tracing::warn!(%error, path = %path.display(), "cannot keep the screen grant; the next guest needs somebody at this screen");
            }
        }

        /// Drops the kept token.
        fn forget(&self) {
            let Some(path) = self.path.as_ref() else {
                return;
            };
            match std::fs::remove_file(path) {
                Ok(()) => tracing::info!("forgot the remembered screen grant"),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::warn!(%error, path = %path.display(), "cannot forget the screen grant");
                }
            }
        }
    }

    /// Whether `text` is a UUID in its usual 8-4-4-4-12 hex form, the only
    /// shape of restore token xdg-desktop-portal accepts.
    fn is_uuid(text: &str) -> bool {
        text.len() == 36
            && text.char_indices().all(|(at, c)| match at {
                8 | 13 | 18 | 23 => c == '-',
                _ => c.is_ascii_hexdigit(),
            })
    }

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

    /// The private current-thread runtime the portal's D-Bus calls run on.
    ///
    /// Capture starts and input arrives on the network actor, which is a task
    /// on a worker of the app's own runtime, and tokio refuses both to
    /// `block_on` a second runtime and to drop one on such a thread. Doing
    /// either there panicked every Wayland host on its first grant and
    /// poisoned its session state. So each call runs on a plain OS thread of
    /// its own, as the Secret Service keystore does, and the runtime is shut
    /// down without waiting for anything.
    #[derive(Debug)]
    struct PortalRuntime(Option<tokio::runtime::Runtime>);

    impl PortalRuntime {
        fn new() -> Result<Self> {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map(|runtime| Self(Some(runtime)))
                .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))
        }

        fn block_on<F>(&self, future: F) -> Result<F::Output>
        where
            F: std::future::Future + Send,
            F::Output: Send,
        {
            let runtime = self.0.as_ref().ok_or_else(|| {
                MediaError::CaptureUnavailable("the portal runtime is shut down".to_owned())
            })?;
            std::thread::scope(|threads| {
                threads
                    .spawn(|| runtime.block_on(future))
                    .join()
                    .map_err(|_| {
                        MediaError::CaptureUnavailable("the portal call panicked".to_owned())
                    })
            })
        }
    }

    impl Drop for PortalRuntime {
        fn drop(&mut self) {
            if let Some(runtime) = self.0.take() {
                runtime.shutdown_background();
            }
        }
    }

    /// What one handshake got from the portal, before [`PortalHandle`] wraps
    /// it with the runtime the session is driven on.
    struct Granted {
        session: Session<RemoteDesktop>,
        node_id: Option<u32>,
        input: InputCapability,
        steps: Vec<PortalStep>,
        /// The token to offer next time, when the portal agreed to keep the
        /// grant (ADR 0160).
        restore_token: Option<String>,
    }

    /// Live portal session: the negotiated grant plus everything needed to
    /// keep injecting input and consuming frames for the session's duration.
    #[derive(Debug)]
    pub struct PortalHandle {
        runtime: PortalRuntime,
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
        /// Offers `grant`'s token first. A token the portal no longer honours
        /// is forgotten and the handshake runs once more without it, which
        /// raises the dialog: the host asks again rather than waiting for a
        /// picture that cannot come (ADR 0160).
        ///
        /// # Errors
        /// [`MediaError::PermissionDenied`] when the user dismisses the
        /// dialog, [`MediaError::CaptureUnavailable`] when no portal is
        /// reachable.
        pub fn negotiate(grant: &RememberedGrant) -> Result<Self> {
            let runtime = PortalRuntime::new()?;
            let (remote, granted) = runtime.block_on(Self::negotiate_async(grant))??;
            Ok(Self {
                runtime,
                remote,
                session: granted.session,
                node_id: granted.node_id,
                input: granted.input,
                steps: granted.steps,
                stream_size: std::sync::Arc::new(StreamSize::new()),
            })
        }

        async fn negotiate_async(grant: &RememberedGrant) -> Result<(RemoteDesktop, Granted)> {
            let (remote, screencast) = connect().await?;

            if let Some(token) = grant.load() {
                let asked = std::time::Instant::now();
                match Self::ask(&remote, &screencast, grant, Some(&token)).await {
                    Ok(granted) if granted.node_id.is_some() => {
                        // xdg-desktop-portal gives a token it still knew back
                        // unchanged, even when the desktop showed its dialog
                        // anyway (KDE does, for a grant GNOME made); a new one
                        // means it no longer knew the token and somebody here
                        // answered the dialog. Only the time tells a restored
                        // grant from a dialog answered quickly.
                        let answered_in_ms =
                            u64::try_from(asked.elapsed().as_millis()).unwrap_or(u64::MAX);
                        if granted.restore_token.as_deref() == Some(token.as_str()) {
                            tracing::info!(
                                answered_in_ms,
                                "the portal took the remembered screen grant"
                            );
                        } else {
                            tracing::info!(
                                answered_in_ms,
                                "the portal no longer knew the remembered screen grant; this screen answered its dialog"
                            );
                        }
                        grant.keep(granted.restore_token.as_deref());
                        return Ok((remote, granted));
                    }
                    // GNOME restores the devices alone once the remembered
                    // monitor is gone, and keeps the grant that way: every
                    // later guest would get input and no picture.
                    Ok(granted) => {
                        tracing::warn!(
                            "the remembered screen grant restored no screen; asking on this screen again"
                        );
                        let _ = granted.session.close().await;
                    }
                    // Only a dialog answers "cancelled", so this is a
                    // person's answer: asking again would put the same
                    // question to them twice.
                    Err(MediaError::PermissionDenied) => {
                        grant.forget();
                        return Err(MediaError::PermissionDenied);
                    }
                    Err(error) => tracing::warn!(
                        %error,
                        "the portal refused the remembered screen grant; asking on this screen again"
                    ),
                }
                grant.forget();
            }

            let granted = Self::ask(&remote, &screencast, grant, None).await?;
            grant.keep(granted.restore_token.as_deref());
            Ok((remote, granted))
        }

        /// One run of the §11 order on a session of its own, closed again if
        /// the portal refuses any step.
        async fn ask(
            remote: &RemoteDesktop,
            screencast: &Screencast,
            grant: &RememberedGrant,
            token: Option<&str>,
        ) -> Result<Granted> {
            let mut steps = Vec::with_capacity(PORTAL_CALL_ORDER.len());

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

            let answer = async {
                // 2. SelectDevices, strictly before SelectSources (§11). A
                // combined session persists here (RemoteDesktop v2) and
                // nowhere else.
                remote
                    .select_devices(
                        &session,
                        SelectDevicesOptions::default()
                            .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
                            .set_persist_mode(grant.persist_mode())
                            .set_restore_token(token),
                    )
                    .await
                    .map_err(map_portal_error)?;
                steps.push(PortalStep::SelectDevices);

                // 3. SelectSources on the same session. xdg-desktop-portal
                // refuses any persist mode but none here on a remote-desktop
                // session ("Remote desktop sessions cannot persist").
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

                // 4. Start raises the dialog, unless the token restored an
                // earlier answer, and returns what the user allowed.
                let response = remote
                    .start(&session, None, StartOptions::default())
                    .await
                    .map_err(map_portal_error)?
                    .response()
                    .map_err(map_portal_error)?;
                steps.push(PortalStep::Start);
                Ok(response)
            }
            .await;
            let response = match answer {
                Ok(response) => response,
                Err(error) => {
                    let _ = session.close().await;
                    return Err(error);
                }
            };

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

            Ok(Granted {
                session,
                node_id,
                input,
                steps,
                restore_token: response.restore_token().map(ToOwned::to_owned),
            })
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

        /// Runs `future` on the runtime the handshake ran on, reused for
        /// `notify_*` calls so the injector doesn't spin up a runtime per
        /// event. Safe to call from inside another tokio runtime.
        ///
        /// # Errors
        /// [`MediaError::CaptureUnavailable`] when the call panicked.
        pub fn block_on<F>(&self, future: F) -> Result<F::Output>
        where
            F: std::future::Future + Send,
            F::Output: Send,
        {
            self.runtime.block_on(future)
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

    /// The two portal interfaces one session talks to, on a session-bus
    /// connection of that session's own (ADR 0159).
    ///
    /// ashpd's plain constructors share one connection for the whole process,
    /// and zbus reads a connection on the runtime it was opened on. That was
    /// the first guest's [`PortalRuntime`], shut down when that guest left, so
    /// every later handshake waited for an answer nobody read and its guest
    /// never got a picture. A connection of its own lives and dies with its
    /// session, and closing it also tells the portal the session is over.
    async fn connect() -> Result<(RemoteDesktop, Screencast)> {
        let connection = ashpd::zbus::Connection::session()
            .await
            .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;
        let remote = RemoteDesktop::with_connection(connection.clone())
            .await
            .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;
        let screencast = Screencast::with_connection(connection)
            .await
            .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;
        Ok((remote, screencast))
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

    #[cfg(test)]
    mod tests {
        #![allow(clippy::unwrap_used, reason = "a failed assumption must fail the test")]

        use super::{
            PersistMode, PortalRuntime, RememberedGrant, connect, is_uuid, map_portal_error,
        };
        use crate::error::MediaError;

        /// Capture starts and input arrives on the network actor, a task on a
        /// worker of the app's runtime: the portal runtime has to run and shut
        /// down there without tokio's nested-runtime panics.
        #[tokio::test(flavor = "multi_thread")]
        async fn the_portal_runtime_runs_and_drops_inside_another_runtime() {
            let runtime = PortalRuntime::new().unwrap();
            let answer = runtime
                .block_on(async {
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    42
                })
                .unwrap();
            assert_eq!(answer, 42);
            drop(runtime);
        }

        /// A directory of the calling test's own under the system temp dir.
        fn scratch(name: &str) -> std::path::PathBuf {
            let dir = std::env::temp_dir().join(format!(
                "lumepeer-portal-grant-{name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            dir
        }

        /// xdg-desktop-portal fails `SelectDevices` for a token that is not a
        /// UUID, so only that shape may ever be offered (ADR 0160).
        #[test]
        fn only_a_uuid_counts_as_a_restore_token() {
            assert!(is_uuid("3f2c1b9a-7d4e-4c21-9a0b-5e6f7a8b9c0d"));
            assert!(is_uuid("3F2C1B9A-7D4E-4C21-9A0B-5E6F7A8B9C0D"));
            assert!(!is_uuid(""));
            assert!(!is_uuid("3f2c1b9a7d4e4c219a0b5e6f7a8b9c0d"));
            assert!(!is_uuid("3f2c1b9a-7d4e-4c21-9a0b-5e6f7a8b9c0g"));
            assert!(!is_uuid(" 3f2c1b9a-7d4e-4c21-9a0b-5e6f7a8b9c0"));
        }

        /// The token `Start` returns is the one offered next time, and a
        /// handshake that returns none drops the old one: a single-use token
        /// kept past its use would only ever cost a dialog.
        #[test]
        fn a_kept_grant_is_offered_back_until_a_handshake_returns_none() {
            let dir = scratch("kept");
            let grant = RememberedGrant::at(dir.join("nested").join("portal-grant"));
            assert_eq!(grant.persist_mode(), PersistMode::ExplicitlyRevoked);
            assert_eq!(grant.load(), None);

            grant.keep(Some("3f2c1b9a-7d4e-4c21-9a0b-5e6f7a8b9c0d"));
            assert_eq!(
                grant.load().as_deref(),
                Some("3f2c1b9a-7d4e-4c21-9a0b-5e6f7a8b9c0d")
            );
            grant.keep(Some("0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d"));
            assert_eq!(
                grant.load().as_deref(),
                Some("0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d")
            );
            {
                use std::os::unix::fs::PermissionsExt as _;
                let mode = std::fs::metadata(dir.join("nested").join("portal-grant"))
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600, "only this user reads the grant");
            }

            grant.keep(None);
            assert_eq!(grant.load(), None);
            assert!(!dir.join("nested").join("portal-grant").exists());
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// A damaged file costs one dialog, not every handshake.
        #[test]
        fn a_damaged_grant_file_offers_nothing() {
            let dir = scratch("damaged");
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("portal-grant");
            std::fs::write(&path, b"not a token").unwrap();
            assert_eq!(RememberedGrant::at(path).load(), None);
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// The default keeps nothing and asks the portal to keep nothing: the
        /// dialog comes up for every guest, as before ADR 0160.
        #[test]
        fn the_default_grant_persists_nothing() {
            let grant = RememberedGrant::default();
            assert_eq!(grant.persist_mode(), PersistMode::DoNot);
            grant.keep(Some("3f2c1b9a-7d4e-4c21-9a0b-5e6f7a8b9c0d"));
            assert_eq!(grant.load(), None);
        }

        /// Each guest's handshake runs on a runtime of its own, shut down when
        /// the guest leaves, and the next guest still has to reach the portal:
        /// a connection read on the first guest's runtime left every later
        /// handshake waiting for an answer nobody read (ADR 0159). Opt-in: it
        /// needs a desktop session bus with xdg-desktop-portal, though no
        /// dialog comes up (`CreateSession` asks nobody).
        #[test]
        fn a_second_guest_reaches_the_portal_after_the_first_one_left() {
            if std::env::var("LUMEPEER_TEST_PORTAL_BUS").as_deref() != Ok("1") {
                return;
            }
            for guest in 1..=2 {
                let runtime = PortalRuntime::new().unwrap();
                let reached = runtime
                    .block_on(async {
                        tokio::time::timeout(std::time::Duration::from_secs(5), async {
                            let (remote, screencast) = connect().await?;
                            #[allow(
                                clippy::default_trait_access,
                                reason = "ashpd keeps CreateSessionOptions private"
                            )]
                            let session = remote
                                .create_session(Default::default())
                                .await
                                .map_err(map_portal_error)?;
                            Ok::<_, MediaError>((remote, screencast, session))
                        })
                        .await
                    })
                    .unwrap();
                let Ok(Ok(kept)) = reached else {
                    panic!("guest {guest} never reached the portal: {reached:?}");
                };
                // Let go in `PortalHandle`'s order: the runtime first, then
                // the proxies, on a thread with no runtime of its own.
                drop(runtime);
                drop(kept);
            }
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

        // Negotiating triggers the real system consent dialog, on a thread of
        // its own; `next_frame` collects the answer.
        let _ = capturer.start(CaptureTarget::PrimaryDisplay);
        while capturer.negotiating.is_some() {
            let _ = capturer.next_frame();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

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

    /// The picture of a host nobody sits at (ADR 0160): after one grant, every
    /// later guest's handshake is restored from the kept token and frames flow
    /// with no dialog. Opt-in like the test above, and it needs a person once:
    /// for the first round, unless `LUMEPEER_TEST_PORTAL_GRANT` names a file
    /// an earlier run already filled. The first round also starts from a token
    /// the portal has never seen, which must cost exactly that one dialog and
    /// never a handshake that waits for nothing.
    #[cfg(feature = "capture-portal")]
    #[test]
    fn a_remembered_grant_brings_the_picture_back_without_a_dialog() {
        if std::env::var("LUMEPEER_TEST_PORTAL_RESTORE").as_deref() != Ok("1") {
            return;
        }
        let path = std::env::var_os("LUMEPEER_TEST_PORTAL_GRANT").map_or_else(
            || std::env::temp_dir().join(format!("lumepeer-portal-grant-{}", std::process::id())),
            std::path::PathBuf::from,
        );
        let unknown = "00000000-0000-4000-8000-000000000000";
        if std::fs::read_to_string(&path).is_err() {
            std::fs::write(&path, unknown).unwrap();
        }
        for guest in 1..=3 {
            let offered = std::fs::read_to_string(&path).unwrap_or_default();
            // A person may have to answer the first round's dialog; nobody
            // answers the later ones, so they must not raise it at all, and a
            // restored grant answers in well under a second.
            let patience = if offered == unknown {
                eprintln!(
                    "guest {guest}: click Share on the host's screen, with Remember this selection ticked"
                );
                std::time::Duration::from_mins(10)
            } else {
                std::time::Duration::from_secs(5)
            };
            let (mut capturer, _injector) = WaylandPortalCapturer::paired_with_injector(
                portal::RememberedGrant::at(path.clone()),
            );
            let began = std::time::Instant::now();
            capturer.start(CaptureTarget::PrimaryDisplay).unwrap();
            let frame = loop {
                match capturer.next_frame() {
                    Ok(Some(frame)) => break frame,
                    Ok(None) => {}
                    Err(error) => panic!("guest {guest}: {error}"),
                }
                assert!(
                    began.elapsed() < patience,
                    "guest {guest}: no picture after {:?} (offered {offered:?})",
                    began.elapsed()
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            };
            let kept = std::fs::read_to_string(&path).unwrap_or_default();
            eprintln!(
                "guest {guest}: {}x{} after {:?}, offered {offered:?}, kept {kept:?}",
                frame.width,
                frame.height,
                began.elapsed()
            );
            assert!(frame.width > 0 && frame.height > 0);
            assert_ne!(kept, unknown, "guest {guest}: the unknown token was kept");
            assert!(!kept.is_empty(), "guest {guest}: the portal kept no grant");
            // xdg-desktop-portal gives a token it still knew back unchanged;
            // that it was restored, not answered, is what the patience above
            // says.
            if offered != unknown {
                assert_eq!(
                    kept, offered,
                    "guest {guest}: the portal no longer knew the grant"
                );
            }
            capturer.stop();
        }
    }

    /// ADR 0161, against a real compositor: a stream that offers DMA-BUF gets
    /// a picture however the compositor answers — as a DMA-BUF where it hands
    /// over a buffer this side takes, in shared memory where it never settles
    /// on one (KWin on a GPU without explicit modifiers), once the offer is
    /// withdrawn. The offer is forced, so the withdrawal runs even on a
    /// machine whose encoder would never have asked for DMA-BUF. Opt-in like
    /// the restore test above, and it takes that test's grant file
    /// (`LUMEPEER_TEST_PORTAL_GRANT`); without one, a person answers the
    /// dialog.
    #[cfg(feature = "encode-vaapi-zero-copy")]
    #[test]
    fn a_stream_offered_dmabuf_gets_a_picture_however_the_compositor_answers() {
        if std::env::var("LUMEPEER_TEST_PORTAL_DMABUF").as_deref() != Ok("1") {
            return;
        }
        let grant = std::env::var_os("LUMEPEER_TEST_PORTAL_GRANT")
            .map_or_else(portal::RememberedGrant::default, |path| {
                portal::RememberedGrant::at(path.into())
            });
        let handle = portal::PortalHandle::negotiate(&grant).unwrap();
        let stream = crate::capture::pipewire_stream::PipeWireFrameThread::spawn_offering(
            handle.node_id().unwrap(),
            handle.stream_size_handle(),
            true,
        )
        .unwrap();
        // Generous: what this tells apart is a picture from none at all —
        // the loop of ADR 0161 never delivers one — not a fast one from a
        // slow one on a loaded machine.
        let began = std::time::Instant::now();
        let frame = loop {
            if let Some(frame) = stream.try_recv_frame() {
                break frame;
            }
            assert!(
                began.elapsed() < std::time::Duration::from_secs(30),
                "offered DMA-BUF, no picture after {:?}",
                began.elapsed()
            );
        };
        eprintln!(
            "{}x{} after {:?}, as {}",
            frame.width,
            frame.height,
            began.elapsed(),
            if frame.dmabuf.is_some() {
                "a DMA-BUF"
            } else {
                "shared memory"
            }
        );
        assert!(frame.width > 0 && frame.height > 0);
        if frame.dmabuf.is_none() {
            assert_eq!(
                frame.data.len(),
                frame.width as usize * frame.height as usize * 4
            );
        }
        drop((frame, stream, handle));
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
