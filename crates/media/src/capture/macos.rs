//! macOS capture via `ScreenCaptureKit` (design doc §11, §5.1; ADR 0013).
//!
//! `ScreenCaptureKit` is push-based: an `SCStream` delivers `CMSampleBuffer`s to
//! an `SCStreamOutput` delegate on a dispatch queue, while [`ScreenCapturer`] is
//! polled. The delegate therefore converts each buffer to a [`Frame`] and keeps
//! only the newest one; [`ScreenCapturer::next_frame`] takes it, and reports
//! `None` when the screen has not changed since the last frame handed out —
//! the same blake3 comparison `capture::linux_x11` uses, since a duplicate must
//! never reach the encoder (§11.1).
//!
//! Screen Recording permission is mandatory and is never worked around. The
//! first `SCShareableContent` request in a process is what makes macOS show the
//! system prompt; until the user grants it, that request fails with
//! `SCStreamErrorUserDeclined` and [`ScreenCapturer::start`] returns
//! [`MediaError::PermissionDenied`] with an actionable message. There is no
//! retry loop and no fallback capture path: an ungranted prompt ends the
//! attempt (§18, and the "no hidden capture, no bypassing OS permission
//! prompts" rule of §2).
//!
//! Losing a permission mid-session is a normal, handled event, not a crash: the
//! stream's `stream:didStopWithError:` fires and the next `next_frame` returns
//! [`MediaError::CaptureInterrupted`], which revokes the session and notifies
//! both sides (§18). The same applies to the Accessibility permission on the
//! input side, where the next `CGEvent` fails instead. iOS is viewer-only in v1,
//! so no capture backend exists there (§1.2).

#[cfg(all(target_os = "macos", feature = "capture-screencapturekit"))]
pub use self::screen_capture_kit::{MacosCapturer, MacosInjector};

#[cfg(not(all(target_os = "macos", feature = "capture-screencapturekit")))]
pub use self::unavailable::{MacosCapturer, MacosInjector};

/// The real backend. Kept in an inline module so the `unsafe_code` carve-out
/// this file needs cannot leak onto the stub below (§21, ADR 0013).
#[cfg(all(target_os = "macos", feature = "capture-screencapturekit"))]
mod screen_capture_kit {
    #![allow(
        unsafe_code,
        reason = "every ScreenCaptureKit/CoreMedia/CoreVideo entry point in the objc2 bindings is an `unsafe fn` because it crosses into Objective-C, and the SCStreamOutput delegate must be a real Objective-C class. Every block below carries a SAFETY note, per §21. See ADR 0013."
    )]

    use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
    use std::time::{Duration, Instant};

    use block2::RcBlock;
    use dispatch2::DispatchQueue;
    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2::{AnyThread, DefinedClass, define_class, msg_send};
    use objc2_core_foundation::{CFRetained, CGPoint};
    use objc2_core_graphics::{
        CGDisplayPixelsHigh, CGDisplayPixelsWide, CGEvent, CGEventTapLocation, CGEventType,
        CGKeyCode, CGMainDisplayID, CGMouseButton, CGScrollEventUnit,
    };
    use objc2_core_media::{CMSampleBuffer, CMTime};
    use objc2_core_video::{
        CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
        CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
        CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    };
    use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
    use objc2_screen_capture_kit::{
        SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration,
        SCStreamDelegate, SCStreamOutput, SCStreamOutputType, SCWindow,
    };

    use lumepeer_core::constants::ENCODE_DEFAULT_FPS;
    use lumepeer_core::protocol::{InputDetail, InputEventPayload, POINTER_BUTTON_LOGICAL_BASE};

    use crate::capture::{
        CaptureTarget, Frame, InputCapability, InputInjector, PixelFormat, ScreenCapturer,
    };
    use crate::error::{MediaError, Result};

    /// Full range of a normalized pointer coordinate (§9.1), matching
    /// `capture::linux_x11::POINTER_RANGE`.
    const POINTER_RANGE: u32 = 65_535;

    /// `kCVPixelFormatType_32BGRA`, the packed little-endian BGRA format this
    /// backend pins on the stream so frames arrive in the one layout
    /// [`PixelFormat::Bgra8`] describes. Left to its own devices
    /// `ScreenCaptureKit` picks a bi-planar YCbCr format on recent macOS
    /// releases, which the rest of the pipeline does not expect.
    const PIXEL_FORMAT_32BGRA: u32 = u32::from_be_bytes(*b"BGRA");

    /// `kCVPixelBufferLock_ReadOnly`. Named rather than inlined because the
    /// lock and the matching unlock must be passed the identical flags:
    /// `CoreVideo` documents non-symmetrical use as undefined behavior.
    const LOCK_READ_ONLY: CVPixelBufferLockFlags = CVPixelBufferLockFlags(1);

    /// `kCVReturnSuccess`.
    const CV_RETURN_SUCCESS: i32 = 0;

    /// Bytes per pixel in [`PIXEL_FORMAT_32BGRA`].
    const BGRA_BYTES_PER_PIXEL: usize = 4;

    /// How many frames `ScreenCaptureKit` may buffer for us. Apple's documented
    /// range is 3..=8; the low end is deliberate here, because this backend
    /// only ever hands out the newest frame and a deep queue would just add
    /// latency to a live remote-desktop session.
    const STREAM_QUEUE_DEPTH: isize = 3;

    /// Bound on the wait for `ScreenCaptureKit`'s asynchronous completion
    /// handlers. Nothing here waits on a human: the content request fails
    /// immediately when Screen Recording is not granted rather than blocking
    /// until the prompt is answered, so this only has to cover a wedged
    /// `WindowServer`, and a stall must fail one call rather than hang the
    /// session (§18).
    const COMPLETION_TIMEOUT: Duration = Duration::from_secs(10);

    // `SCStreamError` codes from Apple's `SCError.h`. Matched by value because
    // the objc2 bindings expose the domain as an opaque `NSString`, and because
    // the §18 error matrix distinguishes "the user said no" from "the capture
    // was interrupted" from "this backend cannot run at all".
    /// `SCStreamErrorUserDeclined`: Screen Recording was not granted.
    const SC_ERROR_USER_DECLINED: isize = -3801;
    /// `SCStreamErrorMissingEntitlements`.
    const SC_ERROR_MISSING_ENTITLEMENTS: isize = -3803;
    /// `SCStreamErrorFailedApplicationConnectionInvalid`.
    const SC_ERROR_CONNECTION_INVALID: isize = -3804;
    /// `SCStreamErrorFailedApplicationConnectionInterrupted`.
    const SC_ERROR_CONNECTION_INTERRUPTED: isize = -3805;
    /// `SCStreamErrorUserStopped`: the user ended the capture from the menu bar.
    const SC_ERROR_USER_STOPPED: isize = -3817;
    /// `SCStreamErrorSystemStoppedStream`: screen lock, user switch, or the
    /// system otherwise pulling the stream out from under us.
    const SC_ERROR_SYSTEM_STOPPED: isize = -3821;

    /// Takes a lock without ever panicking on a poisoned mutex: a panic in a
    /// dispatch-queue callback must not take the capture path with it (§21).
    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The parts of an `NSError` that are plain data, so a completion handler
    /// running on a dispatch queue can hand them to the waiting thread.
    #[derive(Debug, Clone)]
    struct ErrorInfo {
        code: isize,
        message: String,
    }

    impl ErrorInfo {
        fn from_ns(error: &NSError) -> Self {
            Self {
                code: error.code(),
                message: error.localizedDescription().to_string(),
            }
        }

        /// Maps an `SCStreamError` onto the §18 error matrix. Permission is the
        /// case that matters: it must surface as [`MediaError::PermissionDenied`]
        /// so the caller reports "grant Screen Recording", never as a generic
        /// failure the UI might retry behind the user's back.
        fn into_media_error(self, context: &str) -> MediaError {
            let Self { code, message } = self;
            match code {
                SC_ERROR_USER_DECLINED | SC_ERROR_MISSING_ENTITLEMENTS => {
                    MediaError::PermissionDenied
                }
                SC_ERROR_USER_STOPPED
                | SC_ERROR_SYSTEM_STOPPED
                | SC_ERROR_CONNECTION_INVALID
                | SC_ERROR_CONNECTION_INTERRUPTED => {
                    MediaError::CaptureInterrupted(format!("{context}: {message} ({code})"))
                }
                _ => MediaError::CaptureUnavailable(format!("{context}: {message} ({code})")),
            }
        }
    }

    /// Carries an `SCShareableContent` off the dispatch queue its completion
    /// handler runs on and onto the thread blocked in
    /// [`ScreenCapturer::start`].
    struct ContentHandoff(Retained<SCShareableContent>);

    // SAFETY: `Retained<T>` is only `Send` when `T` is, and the objc2 bindings
    // do not claim thread-safety for any ScreenCaptureKit class. This one
    // hand-off is sound anyway: `SCShareableContent` is an immutable snapshot
    // of the displays/windows visible at the moment it was produced, with no
    // interior mutability and no main-thread affinity (the framework only ever
    // delivers it on one of its own background queues in the first place), and
    // `Retained`'s reference count is manipulated with atomic ObjC retain and
    // release. Exactly one thread observes the value: the sender moves it into
    // the channel and never touches it again.
    unsafe impl Send for ContentHandoff {}

    /// State shared between the dispatch-queue delegate and the polling side.
    #[derive(Debug)]
    struct Shared {
        /// Newest frame the delegate produced and nobody has taken yet. Only
        /// the newest is kept: a remote viewer wants the current screen, not a
        /// backlog (§11.1).
        frame: Mutex<Option<Frame>>,
        /// blake3 of the last frame published, so an unchanged screen yields
        /// `None` instead of a duplicate (§11.1).
        last_hash: Mutex<Option<[u8; 32]>>,
        /// Why the stream stopped, once it has. Sticky: the first reason is the
        /// real one, and it must keep being reported until `stop` (§18).
        stopped: Mutex<Option<String>>,
        started_at: Instant,
    }

    impl Shared {
        fn new() -> Self {
            Self {
                frame: Mutex::new(None),
                last_hash: Mutex::new(None),
                stopped: Mutex::new(None),
                started_at: Instant::now(),
            }
        }

        /// Converts one sample buffer and publishes it if the screen changed.
        fn accept(&self, sample: &CMSampleBuffer) {
            // A sample with no image buffer carries no pixels to send: that is
            // how an idle frame usually arrives. It is not the only way one
            // can, though, so `publish` still compares hashes rather than
            // trusting this to be the whole story.
            //
            // SAFETY: `sample` is the buffer ScreenCaptureKit just handed to
            // the delegate method below; it is valid for that call, and
            // `image_buffer` only reads it.
            let Some(pixels) = (unsafe { sample.image_buffer() }) else {
                return;
            };
            match copy_bgra(&pixels, self.started_at) {
                Ok(frame) => self.publish(frame),
                Err(e) => tracing::debug!("dropping a ScreenCaptureKit frame: {e}"),
            }
        }

        fn publish(&self, frame: Frame) {
            let hash = *blake3::hash(&frame.data).as_bytes();
            {
                let mut last = lock(&self.last_hash);
                if *last == Some(hash) {
                    return;
                }
                *last = Some(hash);
            }
            *lock(&self.frame) = Some(frame);
        }

        fn take_frame(&self) -> Option<Frame> {
            lock(&self.frame).take()
        }

        fn record_stop(&self, reason: String) {
            let mut slot = lock(&self.stopped);
            if slot.is_none() {
                *slot = Some(reason);
            }
        }

        fn stop_reason(&self) -> Option<String> {
            lock(&self.stopped).clone()
        }
    }

    define_class!(
        // SAFETY:
        // - `NSObject` imposes no subclassing requirements.
        // - This type does not implement `Drop` itself; `define_class!`
        //   generates the `dealloc` that drops the `Arc` ivar.
        #[unsafe(super(NSObject))]
        #[ivars = Arc<Shared>]
        struct FrameSink;

        unsafe impl NSObjectProtocol for FrameSink {}

        /// Receives frames on the dispatch queue passed to `addStreamOutput`.
        unsafe impl SCStreamOutput for FrameSink {
            #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
            fn did_output_sample_buffer(
                &self,
                _stream: &SCStream,
                sample: &CMSampleBuffer,
                kind: SCStreamOutputType,
            ) {
                if kind == SCStreamOutputType::Screen {
                    self.ivars().accept(sample);
                }
            }
        }

        /// Receives the stream's lifecycle errors: screen lock, user switch,
        /// a permission withdrawn mid-session (§18).
        unsafe impl SCStreamDelegate for FrameSink {
            #[unsafe(method(stream:didStopWithError:))]
            fn did_stop_with_error(&self, _stream: &SCStream, error: &NSError) {
                let info = ErrorInfo::from_ns(error);
                self.ivars()
                    .record_stop(format!("{} ({})", info.message, info.code));
            }
        }
    );

    impl FrameSink {
        fn new(shared: Arc<Shared>) -> Retained<Self> {
            let this = Self::alloc().set_ivars(shared);
            // SAFETY: `NSObject`'s designated initializer, called exactly once
            // on a freshly allocated instance whose ivars are already set.
            unsafe { msg_send![super(this), init] }
        }
    }

    /// Copies a locked-down `CVPixelBuffer` into a tightly packed BGRA frame.
    ///
    /// The copy is not an optimization opportunity to skip: the pixel buffer is
    /// owned by `ScreenCaptureKit` and recycled as soon as the delegate returns,
    /// so nothing may outlive this call.
    fn copy_bgra(pixels: &CVPixelBuffer, started_at: Instant) -> Result<Frame> {
        let format = CVPixelBufferGetPixelFormatType(pixels);
        if format != PIXEL_FORMAT_32BGRA {
            return Err(MediaError::CaptureUnavailable(format!(
                "ScreenCaptureKit delivered pixel format {format:#010x}, not 32BGRA"
            )));
        }

        // SAFETY: locking the base address is CoreVideo's documented
        // precondition for reading it, and `pixels` is the live buffer of the
        // sample currently being delivered. The matching unlock below runs on
        // every path out of this function, with the identical flags CoreVideo
        // requires.
        let status = unsafe { CVPixelBufferLockBaseAddress(pixels, LOCK_READ_ONLY) };
        if status != CV_RETURN_SUCCESS {
            return Err(MediaError::CaptureUnavailable(format!(
                "locking the captured pixel buffer failed: CVReturn {status}"
            )));
        }

        let frame = read_locked(pixels, started_at);

        // SAFETY: balances the successful lock above, with the same flags.
        let _ = unsafe { CVPixelBufferUnlockBaseAddress(pixels, LOCK_READ_ONLY) };

        frame
    }

    /// Reads a pixel buffer whose base address the caller has already locked.
    fn read_locked(pixels: &CVPixelBuffer, started_at: Instant) -> Result<Frame> {
        let width = CVPixelBufferGetWidth(pixels);
        let height = CVPixelBufferGetHeight(pixels);
        let stride = CVPixelBufferGetBytesPerRow(pixels);
        let base = CVPixelBufferGetBaseAddress(pixels).cast::<u8>();

        if base.is_null() || width == 0 || height == 0 {
            return Err(MediaError::CaptureUnavailable(
                "the captured pixel buffer is empty".to_owned(),
            ));
        }
        let row_bytes = width
            .checked_mul(BGRA_BYTES_PER_PIXEL)
            .ok_or_else(|| MediaError::CaptureUnavailable("frame width overflows".to_owned()))?;
        // A stride shorter than one row of pixels would make the reads below
        // walk off the end of the buffer, so refuse rather than trust it.
        if stride < row_bytes {
            return Err(MediaError::CaptureUnavailable(format!(
                "pixel buffer stride {stride} is shorter than one {row_bytes}-byte row"
            )));
        }
        let total = row_bytes
            .checked_mul(height)
            .ok_or_else(|| MediaError::CaptureUnavailable("frame size overflows".to_owned()))?;

        let mut data = Vec::with_capacity(total);
        for row in 0..height {
            // SAFETY: `base` is the locked base address of an image CoreVideo
            // reports as `height` rows of `stride` bytes, so the whole buffer
            // is `height * stride` bytes long. `row < height` and
            // `row_bytes <= stride` (checked above), so
            // `row * stride + row_bytes <= height * stride`: both the offset
            // and the slice stay inside the buffer. The slice is only read,
            // and only before the unlock in `copy_bgra`.
            let source = unsafe { std::slice::from_raw_parts(base.add(row * stride), row_bytes) };
            data.extend_from_slice(source);
        }

        Ok(Frame {
            width: u32::try_from(width).map_err(|_| {
                MediaError::CaptureUnavailable("frame width out of range".to_owned())
            })?,
            height: u32::try_from(height).map_err(|_| {
                MediaError::CaptureUnavailable("frame height out of range".to_owned())
            })?,
            format: PixelFormat::Bgra8,
            timestamp_us: u64::try_from(started_at.elapsed().as_micros()).unwrap_or(u64::MAX),
            data,
        })
    }

    /// Runs one of `ScreenCaptureKit`'s `completionHandler:` methods and blocks
    /// until it fires, returning the error it reported, if any.
    fn wait_for_completion(
        what: &str,
        invoke: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>),
    ) -> Result<Option<ErrorInfo>> {
        let (tx, rx) = mpsc::sync_channel::<Option<ErrorInfo>>(1);
        let handler = RcBlock::new(move |error: *mut NSError| {
            // SAFETY: ScreenCaptureKit passes either null or a valid NSError
            // that outlives this call; the null case is handled by `is_null`,
            // and `ErrorInfo::from_ns` only reads the error.
            let info = if error.is_null() {
                None
            } else {
                Some(ErrorInfo::from_ns(unsafe { &*error }))
            };
            // A second delivery, which the framework does not do, would find
            // the bounded channel full; dropping it is correct either way.
            let _ = tx.try_send(info);
        });
        invoke(&handler);
        rx.recv_timeout(COMPLETION_TIMEOUT).map_err(|_| {
            MediaError::CaptureUnavailable(format!(
                "ScreenCaptureKit did not answer {what} within {}s",
                COMPLETION_TIMEOUT.as_secs()
            ))
        })
    }

    /// Asks `ScreenCaptureKit` what is capturable.
    ///
    /// This is the call that makes macOS show the Screen Recording prompt, and
    /// the call that fails while it is unanswered or denied. Both outcomes are
    /// final here: there is no second attempt and no alternative capture path.
    fn shareable_content() -> Result<Retained<SCShareableContent>> {
        let (tx, rx) = mpsc::sync_channel::<std::result::Result<ContentHandoff, ErrorInfo>>(1);
        let handler = RcBlock::new(
            move |content: *mut SCShareableContent, error: *mut NSError| {
                // SAFETY: ScreenCaptureKit passes exactly one of the two
                // pointers as non-null, each valid for this call. `retain`
                // takes our own reference to the content so it outlives the
                // handler; `ErrorInfo::from_ns` only reads the error.
                let retained = unsafe {
                    if content.is_null() {
                        None
                    } else {
                        Retained::retain(content)
                    }
                };
                let outcome = match retained {
                    Some(content) => Ok(ContentHandoff(content)),
                    // SAFETY: as above; reached only when `content` was null,
                    // so ScreenCaptureKit passed an error instead.
                    None if !error.is_null() => Err(ErrorInfo::from_ns(unsafe { &*error })),
                    None => Err(ErrorInfo {
                        code: 0,
                        message: "ScreenCaptureKit reported neither content nor an error"
                            .to_owned(),
                    }),
                };
                let _ = tx.try_send(outcome);
            },
        );

        // SAFETY: the block outlives the call because this thread blocks on
        // `recv_timeout` below until the handler has run; the two `false`s ask
        // for every display and window, which is what display enumeration
        // needs.
        unsafe {
            SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
                false, false, &handler,
            );
        }

        match rx.recv_timeout(COMPLETION_TIMEOUT) {
            Ok(Ok(content)) => Ok(content.0),
            Ok(Err(info)) => Err(info.into_media_error(
                "ScreenCaptureKit refused to enumerate capturable content; grant Screen Recording \
                 to this application in System Settings > Privacy & Security > Screen & System \
                 Audio Recording, then start it again",
            )),
            Err(_) => Err(MediaError::CaptureUnavailable(format!(
                "ScreenCaptureKit did not answer the content request within {}s",
                COMPLETION_TIMEOUT.as_secs()
            ))),
        }
    }

    /// Resolves a [`CaptureTarget`] against the displays `ScreenCaptureKit`
    /// actually reports.
    ///
    /// [`CaptureTarget::Display`] carries the stable `CGDirectDisplayID` the
    /// host UI shows, so it is matched against `SCDisplay.displayID` first; only
    /// when no display carries that id is it treated as a position in the
    /// enumeration, which is what the X11 backend means by the same variant.
    fn select_display(
        content: &SCShareableContent,
        target: CaptureTarget,
    ) -> Result<Retained<SCDisplay>> {
        // SAFETY: `displays` only reads the snapshot's own array; both it and
        // `displayID` are `unsafe fn` solely because they cross into ObjC.
        let displays = unsafe { content.displays() };
        if displays.is_empty() {
            return Err(MediaError::CaptureUnavailable(
                "ScreenCaptureKit reports no capturable display".to_owned(),
            ));
        }

        let wanted = match target {
            CaptureTarget::PrimaryDisplay => CGMainDisplayID(),
            CaptureTarget::Display(id) => id,
        };
        for display in &displays {
            // SAFETY: as above.
            if unsafe { display.displayID() } == wanted {
                return Ok(display);
            }
        }

        match target {
            CaptureTarget::PrimaryDisplay => displays
                .firstObject()
                .ok_or_else(|| MediaError::CaptureUnavailable("no primary display".to_owned())),
            CaptureTarget::Display(n) => displays
                .iter()
                .nth(usize::try_from(n).unwrap_or(usize::MAX))
                .ok_or_else(|| {
                    MediaError::CaptureUnavailable(format!(
                        "no display with id or index {n} among the {} ScreenCaptureKit reports",
                        displays.len()
                    ))
                }),
        }
    }

    /// A running stream and everything that has to stay alive alongside it.
    struct Active {
        stream: Retained<SCStream>,
        /// The delegate. `SCStream` holds it weakly, the way Cocoa delegates
        /// always are, so dropping this would silently stop every callback.
        _sink: Retained<FrameSink>,
        shared: Arc<Shared>,
    }

    /// `ScreenCaptureKit` capturer.
    pub struct MacosCapturer {
        active: Option<Active>,
    }

    // SAFETY: the objc2 bindings do not claim thread-safety for any
    // ScreenCaptureKit class, so `Retained<SCStream>` is not `Send` by default.
    // The framework is nonetheless free-threaded by construction: every one of
    // its APIs is asynchronous, it delivers frames on a dispatch queue the
    // caller nominates rather than the main queue, and none of the classes used
    // here is documented as main-thread-only (unlike, say, AppKit's views).
    // `Retained`'s reference counting is atomic. The only mutable state this
    // type reaches across a `Send` is `Arc<Shared>`, which is `Send + Sync`
    // through its own mutexes.
    unsafe impl Send for MacosCapturer {}

    // Mirrors `encode::windows`'s `Debug`: the Objective-C state is not
    // printable and must never be logged, only whether capture is running.
    impl std::fmt::Debug for MacosCapturer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("MacosCapturer")
                .field("capturing", &self.active.is_some())
                .finish_non_exhaustive()
        }
    }

    impl Default for MacosCapturer {
        fn default() -> Self {
            Self::new()
        }
    }

    impl MacosCapturer {
        /// Creates a capturer that opens the stream on
        /// [`ScreenCapturer::start`].
        #[must_use]
        pub const fn new() -> Self {
            Self { active: None }
        }

        /// Frames per second the stream is configured to cap at; the encoder
        /// paces itself below this (§14).
        #[must_use]
        pub const fn suggested_fps() -> u8 {
            ENCODE_DEFAULT_FPS
        }
    }

    impl ScreenCapturer for MacosCapturer {
        fn start(&mut self, target: CaptureTarget) -> Result<()> {
            // Starting twice must not leave an orphaned stream running: that
            // would be capture with no viewer attached to it (§8.1).
            self.stop();

            let content = shareable_content()?;
            let display = select_display(&content, target)?;

            // SAFETY: `display` came from the snapshot just fetched, the
            // exclusion list is a live empty array, and both `alloc`/`init`
            // pairs below follow ObjC's ownership rules through
            // `Allocated`/`Retained`. The setters only write the
            // configuration's own properties.
            let (filter, config, width, height) = unsafe {
                let width = display.width();
                let height = display.height();
                let excluded: Retained<NSArray<SCWindow>> = NSArray::new();
                let filter = SCContentFilter::initWithDisplay_excludingWindows(
                    SCContentFilter::alloc(),
                    &display,
                    &excluded,
                );

                let config = SCStreamConfiguration::new();
                config.setWidth(usize::try_from(width).unwrap_or(0));
                config.setHeight(usize::try_from(height).unwrap_or(0));
                config.setPixelFormat(PIXEL_FORMAT_32BGRA);
                config.setQueueDepth(STREAM_QUEUE_DEPTH);
                config.setShowsCursor(true);
                config.setCapturesAudio(false);
                config.setMinimumFrameInterval(CMTime::new(1, i32::from(ENCODE_DEFAULT_FPS)));
                (filter, config, width, height)
            };
            if width <= 0 || height <= 0 {
                return Err(MediaError::CaptureUnavailable(format!(
                    "ScreenCaptureKit reports a {width}x{height} display"
                )));
            }

            let shared = Arc::new(Shared::new());
            let sink = FrameSink::new(Arc::clone(&shared));
            let queue = DispatchQueue::new("dev.lumepeer.capture.macos", None);

            // SAFETY: `filter` and `config` are the objects just built, `sink`
            // outlives the stream because `Active` holds it, and the queue is a
            // fresh serial queue owned for the same span.
            let stream = unsafe {
                let stream = SCStream::initWithFilter_configuration_delegate(
                    SCStream::alloc(),
                    &filter,
                    &config,
                    Some(ProtocolObject::from_ref(&*sink)),
                );
                stream
                    .addStreamOutput_type_sampleHandlerQueue_error(
                        ProtocolObject::from_ref(&*sink),
                        SCStreamOutputType::Screen,
                        Some(&queue),
                    )
                    .map_err(|e| {
                        ErrorInfo::from_ns(&e).into_media_error("attaching the capture output")
                    })?;
                stream
            };

            if let Some(info) = wait_for_completion("the stream start", |handler| {
                // SAFETY: the handler outlives the call, since
                // `wait_for_completion` blocks until it has run.
                unsafe { stream.startCaptureWithCompletionHandler(Some(handler)) };
            })? {
                return Err(info.into_media_error("starting the ScreenCaptureKit stream"));
            }

            self.active = Some(Active {
                stream,
                _sink: sink,
                shared,
            });
            Ok(())
        }

        fn next_frame(&mut self) -> Result<Option<Frame>> {
            let active = self
                .active
                .as_mut()
                .ok_or_else(|| MediaError::CaptureUnavailable("capturer not started".to_owned()))?;

            // Sticky on purpose: once the stream is gone the caller must revoke
            // rather than keep polling a dead capture (§18).
            if let Some(reason) = active.shared.stop_reason() {
                return Err(MediaError::CaptureInterrupted(reason));
            }
            Ok(active.shared.take_frame())
        }

        fn stop(&mut self) {
            let Some(active) = self.active.take() else {
                return;
            };
            let stream = active.stream;
            // A failed stop still drops the stream, which releases it; there is
            // nothing left for the caller to do about it, so the result is
            // deliberately discarded rather than logged as an error.
            let _ = wait_for_completion("the stream stop", |handler| {
                // SAFETY: as in `start`; the handler outlives the call.
                unsafe { stream.stopCaptureWithCompletionHandler(Some(handler)) };
            });
        }

        fn input_capability(&self) -> InputCapability {
            // `CGEvent` injection can drive any application once the
            // Accessibility permission is granted; losing it mid-session
            // surfaces as `InputUnavailable` on the next event (§11.1, §18).
            InputCapability::Full
        }
    }

    /// macOS virtual key codes for the named keys the guest can send
    /// (`Carbon.framework`/`HIToolbox`'s `kVK_*` constants, `Events.h`, read
    /// from the SDK header directly rather than transcribed from memory) —
    /// not bound by `objc2-core-graphics`, so listed here by hand, the same
    /// way `capture::linux_x11` hand-lists its XTEST event type codes.
    /// Matches `apps/desktop/src/view-window.ts`'s `NAMED_KEYS` table one to
    /// one, with the two renames Apple's naming inverts relative to the web:
    /// `kVK_Delete` is the key left of Return (the web's `Backspace`),
    /// `kVK_ForwardDelete` is the web's `Delete`. `Insert` (0xe008) and
    /// `F21`..`F24` (0xe115..=0xe118) have no `kVK_` constant at all and so
    /// have no entry here; `key` reports those explicitly rather than
    /// guessing a code.
    fn named_key_vk(logical: u32) -> Option<CGKeyCode> {
        Some(match logical {
            0x08 => 0x33,   // Backspace -> kVK_Delete
            0x09 => 0x30,   // Tab -> kVK_Tab
            0x0d => 0x24,   // Enter -> kVK_Return
            0x1b => 0x35,   // Escape -> kVK_Escape
            0x7f => 0x75,   // Delete -> kVK_ForwardDelete
            0xe000 => 0x7B, // ArrowLeft
            0xe001 => 0x7E, // ArrowUp
            0xe002 => 0x7C, // ArrowRight
            0xe003 => 0x7D, // ArrowDown
            0xe004 => 0x73, // Home
            0xe005 => 0x77, // End
            0xe006 => 0x74, // PageUp
            0xe007 => 0x79, // PageDown
            0xe010 => 0x38, // Shift
            0xe011 => 0x3B, // Control
            0xe012 => 0x3A, // Alt -> kVK_Option
            0xe013 => 0x37, // Meta -> kVK_Command
            0xe014 => 0x39, // CapsLock
            0xe101 => 0x7A, // F1
            0xe102 => 0x78, // F2
            0xe103 => 0x63, // F3
            0xe104 => 0x76, // F4
            0xe105 => 0x60, // F5
            0xe106 => 0x61, // F6
            0xe107 => 0x62, // F7
            0xe108 => 0x64, // F8
            0xe109 => 0x65, // F9
            0xe10a => 0x6D, // F10
            0xe10b => 0x67, // F11
            0xe10c => 0x6F, // F12
            0xe10d => 0x69, // F13
            0xe10e => 0x6B, // F14
            0xe10f => 0x71, // F15
            0xe110 => 0x6A, // F16
            0xe111 => 0x40, // F17
            0xe112 => 0x4F, // F18
            0xe113 => 0x50, // F19
            0xe114 => 0x5A, // F20
            _ => return None,
        })
    }

    /// Input injection through `CGEvent`, posted at the HID tap so it reaches
    /// every application, the same reach `SendInput` has on Windows and
    /// XTEST has on X11 (§11).
    ///
    /// Screen Recording (this file's own permission, for capture) and
    /// Accessibility (this type's permission, for posting events) are
    /// separate macOS grants. Losing Accessibility mid-session is a normal,
    /// handled event: the next `inject` returns [`MediaError::InputUnavailable`]
    /// and the caller revokes rather than crashing (§18).
    #[derive(Debug)]
    pub struct MacosInjector {
        /// Primary display size in points, read once at `connect` so every
        /// `PointerMove` scales the guest's normalized 0..=65535 coordinate
        /// without re-querying `CGDirectDisplay` per event.
        width: f64,
        height: f64,
        /// Wherever `PointerMove` last put the pointer. `Press`/`Release`
        /// carry no coordinate of their own (`view-window.ts`'s `sink.press`
        /// takes no x/y at all — the guest always moves, then clicks), and
        /// `CGEventCreateMouseEvent` requires *some* position for a button
        /// event, unlike XTEST/`SendInput`, which post a click at wherever
        /// the pointer already is without being told.
        last_position: CGPoint,
        /// The button a `Press` last reported down, cleared on its matching
        /// `Release`. Drives whether the next `PointerMove` posts
        /// `MouseMoved` or a `*Dragged` variant: unlike X11/`SendInput`,
        /// Quartz event delivery genuinely distinguishes the two, and most
        /// `AppKit` views only receive `mouseMoved:` when a window has opted
        /// in, while `mouseDragged:` always fires while a button is down —
        /// so getting this wrong silently breaks drag-select and window
        /// dragging rather than just mislabeling an event.
        held: Option<CGMouseButton>,
    }

    impl MacosInjector {
        /// # Errors
        /// [`MediaError::InputUnavailable`] if the primary display's size
        /// cannot be read — implausible on a running desktop, but this keeps
        /// `PointerMove` from ever scaling against a size it never checked.
        pub fn connect() -> Result<Self> {
            let display = CGMainDisplayID();
            let width = CGDisplayPixelsWide(display);
            let height = CGDisplayPixelsHigh(display);
            if width == 0 || height == 0 {
                return Err(MediaError::InputUnavailable(
                    "cannot read the primary display's size".to_owned(),
                ));
            }
            // A real display's pixel width/height is nowhere near f64's
            // 2^52 exact-integer ceiling, so the precision this could lose
            // in principle never actually happens.
            #[allow(
                clippy::cast_precision_loss,
                reason = "display pixel counts stay far below f64's 52-bit exact-integer range"
            )]
            Ok(Self {
                width: width as f64,
                height: height as f64,
                last_position: CGPoint { x: 0.0, y: 0.0 },
                held: None,
            })
        }

        fn post(event: Option<CFRetained<CGEvent>>) -> Result<()> {
            let Some(event) = event else {
                return Err(MediaError::InputUnavailable(
                    "CGEvent creation failed".to_owned(),
                ));
            };
            CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
            Ok(())
        }

        fn key_vk(vk: CGKeyCode, pressed: bool) -> Result<()> {
            Self::post(CGEvent::new_keyboard_event(None, vk, pressed))
        }

        /// One code point, sent by value rather than by virtual key: the
        /// guest's `logical` is a Unicode code point, not a layout-dependent
        /// key, and `CGEventKeyboardSetUnicodeString` is the escape hatch
        /// macOS gives for that (matches any layout, any language) — the
        /// same role `KEYEVENTF_UNICODE` plays in the Windows backend.
        /// `virtual_key` 0 (`kVK_ANSI_A`) is an arbitrary placeholder: the
        /// Unicode string overrides whatever character it would have typed.
        fn key_unicode(unit: u16, pressed: bool) -> Result<()> {
            let Some(event) = CGEvent::new_keyboard_event(None, 0, pressed) else {
                return Err(MediaError::InputUnavailable(
                    "CGEvent creation failed".to_owned(),
                ));
            };
            let units = [unit];
            // SAFETY: `units` is a live, one-element `[u16]` for the length
            // (1) passed alongside it; `CGEvent` copies the string out before
            // this call returns and keeps no pointer into `units`.
            unsafe {
                CGEvent::keyboard_set_unicode_string(Some(&event), 1, units.as_ptr());
            }
            CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
            Ok(())
        }

        fn key(logical: u32, pressed: bool) -> Result<()> {
            if let Some(vk) = named_key_vk(logical) {
                return Self::key_vk(vk, pressed);
            }
            let ch = char::from_u32(logical).ok_or_else(|| {
                MediaError::InputUnavailable(format!(
                    "logical key {logical} is neither a named key nor a valid code point"
                ))
            })?;
            let mut buf = [0u16; 2];
            for unit in ch.encode_utf16(&mut buf) {
                Self::key_unicode(*unit, pressed)?;
            }
            Ok(())
        }

        fn button(&mut self, logical: u32, pressed: bool) -> Result<()> {
            let index = logical.saturating_sub(POINTER_BUTTON_LOGICAL_BASE);
            let (event_type, mouse_button) = match (index, pressed) {
                (0, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left),
                (0, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left),
                (1, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center),
                (1, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center),
                (2, true) => (CGEventType::RightMouseDown, CGMouseButton::Right),
                (2, false) => (CGEventType::RightMouseUp, CGMouseButton::Right),
                _ => {
                    return Err(MediaError::InputUnavailable(format!(
                        "pointer button {index} is not supported"
                    )));
                }
            };
            self.held = pressed.then_some(mouse_button);
            Self::post(CGEvent::new_mouse_event(
                None,
                event_type,
                self.last_position,
                mouse_button,
            ))
        }

        /// `x`/`y` are normalized to 0..=65535 of the captured surface
        /// (§9.1); `CGEvent`'s mouse position is in points from the primary
        /// display's top-left, so this scales by the size cached in
        /// `connect` — the same role `X11Injector::to_screen` plays.
        fn point(&self, x: u16, y: u16) -> CGPoint {
            CGPoint {
                x: f64::from(x) * self.width / f64::from(POINTER_RANGE),
                y: f64::from(y) * self.height / f64::from(POINTER_RANGE),
            }
        }

        fn wheel(dx: i16, dy: i16) -> Result<()> {
            if dx == 0 && dy == 0 {
                return Ok(());
            }
            Self::post(CGEvent::new_scroll_wheel_event2(
                None,
                CGScrollEventUnit::Pixel,
                2,
                i32::from(dy),
                i32::from(dx),
                0,
            ))
        }
    }

    impl InputInjector for MacosInjector {
        fn inject(&mut self, event: &InputEventPayload) -> Result<()> {
            match event.detail {
                InputDetail::PointerMove { x, y } => {
                    self.last_position = self.point(x, y);
                    let (event_type, button) = match self.held {
                        Some(button @ CGMouseButton::Left) => {
                            (CGEventType::LeftMouseDragged, button)
                        }
                        Some(button @ CGMouseButton::Right) => {
                            (CGEventType::RightMouseDragged, button)
                        }
                        Some(button) => (CGEventType::OtherMouseDragged, button),
                        None => (CGEventType::MouseMoved, CGMouseButton::Left),
                    };
                    Self::post(CGEvent::new_mouse_event(
                        None,
                        event_type,
                        self.last_position,
                        button,
                    ))
                }
                InputDetail::Wheel { dx, dy } => Self::wheel(dx, dy),
                InputDetail::Press | InputDetail::Release => {
                    let pressed = matches!(event.detail, InputDetail::Press);
                    if event.logical >= POINTER_BUTTON_LOGICAL_BASE {
                        self.button(event.logical, pressed)
                    } else {
                        Self::key(event.logical, pressed)
                    }
                }
            }
        }

        fn capability(&self) -> InputCapability {
            InputCapability::Full
        }
    }

    #[cfg(test)]
    mod tests {
        #![allow(clippy::unwrap_used)]

        use super::*;

        /// The four-char code must be the one `CoreVideo` means by
        /// `kCVPixelFormatType_32BGRA`, or the stream would silently hand back
        /// a format the rest of the pipeline misreads.
        #[test]
        fn bgra_four_char_code_matches_core_video() {
            assert_eq!(PIXEL_FORMAT_32BGRA, 0x4247_5241);
        }

        /// Named keys map to the `kVK_*` code the guest actually means,
        /// including the two spots Apple's naming inverts relative to the
        /// web (`Backspace` -> `kVK_Delete`, `Delete` ->
        /// `kVK_ForwardDelete`). Anything outside the table falls through to
        /// `key`'s Unicode path instead.
        #[test]
        fn named_keys_map_to_the_matching_virtual_key() {
            assert_eq!(named_key_vk(0x08), Some(0x33)); // Backspace -> kVK_Delete
            assert_eq!(named_key_vk(0x7f), Some(0x75)); // Delete -> kVK_ForwardDelete
            assert_eq!(named_key_vk(0x0d), Some(0x24)); // Enter -> kVK_Return
            assert_eq!(named_key_vk(0xe000), Some(0x7B)); // ArrowLeft
            assert_eq!(named_key_vk(0xe101), Some(0x7A)); // F1
            assert_eq!(named_key_vk(0xe114), Some(0x5A)); // F20, the last named one
            // A plain character code point is not a named key: `key` sends it
            // through `CGEventKeyboardSetUnicodeString` instead.
            assert_eq!(named_key_vk(u32::from(b'a')), None);
            // Insert and F21..F24 have no `kVK_` constant at all.
            assert_eq!(named_key_vk(0xe008), None);
            assert_eq!(named_key_vk(0xe115), None);
        }

        /// The real thing, against the real window server. `connect` cannot
        /// fail on a missing permission (reading the display size needs
        /// none), but posting the event can: skipped rather than failed when
        /// Accessibility is not granted to this test binary, the normal
        /// state for a run over SSH (matches this file's own
        /// `capture_produces_a_frame_when_screen_recording_is_granted`, one
        /// permission over from this one). Even when it runs, this only
        /// moves the pointer to the position it already has — read back from
        /// a fresh null `CGEvent`, which still carries the live HID location
        /// — so nothing visible happens (matches `capture::linux_x11`'s
        /// XTEST test).
        #[test]
        fn cgevent_injection_works_when_explicitly_enabled() {
            if std::env::var("LUMEPEER_TEST_CGEVENT").as_deref() != Ok("1") {
                return;
            }
            let mut injector = MacosInjector::connect().unwrap();
            assert_eq!(injector.capability(), InputCapability::Full);

            let probe = CGEvent::new(None).unwrap();
            let current = CGEvent::location(Some(&probe));
            #[allow(
                clippy::cast_possible_truncation,
                reason = "test-only: the u16::try_from right after already guards the range"
            )]
            let normalize = |value: f64, extent: f64| -> u16 {
                u16::try_from(((value / extent) * f64::from(POINTER_RANGE)) as i64)
                    .unwrap_or(u16::MAX)
            };
            let payload = InputEventPayload {
                logical: 0,
                scancode: 0,
                modifiers: 0,
                detail: InputDetail::PointerMove {
                    x: normalize(current.x, injector.width),
                    y: normalize(current.y, injector.height),
                },
            };
            match injector.inject(&payload) {
                Ok(()) => {}
                Err(MediaError::InputUnavailable(reason)) => {
                    eprintln!(
                        "skipped: Accessibility is not granted to this test binary: {reason}"
                    );
                }
                Err(e) => panic!("injection failed for an unexpected reason: {e}"),
            }
        }

        /// The §18 matrix rows this backend can produce. Permission is the one
        /// that must never be reported as a generic failure.
        #[test]
        fn stream_errors_map_onto_the_error_matrix() {
            let info = |code| ErrorInfo {
                code,
                message: "test".to_owned(),
            };
            assert!(matches!(
                info(SC_ERROR_USER_DECLINED).into_media_error("x"),
                MediaError::PermissionDenied
            ));
            assert!(matches!(
                info(SC_ERROR_MISSING_ENTITLEMENTS).into_media_error("x"),
                MediaError::PermissionDenied
            ));
            for interrupted in [
                SC_ERROR_USER_STOPPED,
                SC_ERROR_SYSTEM_STOPPED,
                SC_ERROR_CONNECTION_INVALID,
                SC_ERROR_CONNECTION_INTERRUPTED,
            ] {
                assert!(matches!(
                    info(interrupted).into_media_error("x"),
                    MediaError::CaptureInterrupted(_)
                ));
            }
            assert!(matches!(
                info(-1).into_media_error("x"),
                MediaError::CaptureUnavailable(_)
            ));
        }

        /// An unchanged screen must yield `None`, not a duplicate frame
        /// (§11.1).
        #[test]
        fn an_identical_frame_is_not_published_twice() {
            let shared = Shared::new();
            let frame = || Frame {
                width: 2,
                height: 1,
                format: PixelFormat::Bgra8,
                timestamp_us: 0,
                data: vec![7; 8],
            };
            shared.publish(frame());
            assert!(shared.take_frame().is_some());
            shared.publish(frame());
            assert!(shared.take_frame().is_none(), "the screen did not change");

            let mut changed = frame();
            changed.data[0] = 9;
            shared.publish(changed);
            assert!(shared.take_frame().is_some());
        }

        /// The first stop reason wins and keeps being reported: a later,
        /// vaguer error must not paper over why the capture really died.
        #[test]
        fn the_first_stop_reason_is_the_one_reported() {
            let shared = Shared::new();
            assert!(shared.stop_reason().is_none());
            shared.record_stop("screen locked".to_owned());
            shared.record_stop("something else".to_owned());
            assert_eq!(shared.stop_reason().as_deref(), Some("screen locked"));
        }

        /// No frame may ever come out of a capturer that was never started —
        /// the "no capture without a viewer" rule of §19 phase 2 relies on it.
        #[test]
        fn nothing_is_produced_before_start() {
            let mut capturer = MacosCapturer::new();
            assert!(matches!(
                capturer.next_frame(),
                Err(MediaError::CaptureUnavailable(_))
            ));
        }

        /// `stop` is idempotent, including before any `start`.
        #[test]
        fn stop_is_idempotent() {
            let mut capturer = MacosCapturer::new();
            capturer.stop();
            capturer.stop();
            assert!(capturer.active.is_none());
        }

        /// The real thing, against the real window server. Skipped rather than
        /// failed when Screen Recording is not granted — which is the normal
        /// state for a test binary run over SSH, where macOS has no GUI session
        /// to show the prompt in. That skip is the point: the backend must
        /// report a clear `PermissionDenied` there, never capture anyway.
        #[test]
        fn capture_produces_a_frame_when_screen_recording_is_granted() {
            let mut capturer = MacosCapturer::new();
            match capturer.start(CaptureTarget::PrimaryDisplay) {
                Ok(()) => {}
                Err(MediaError::PermissionDenied) => {
                    eprintln!("skipped: Screen Recording is not granted to this test binary");
                    return;
                }
                Err(MediaError::CaptureUnavailable(reason)) => {
                    eprintln!("skipped: no usable ScreenCaptureKit session here: {reason}");
                    return;
                }
                Err(e) => panic!("capture failed for an unexpected reason: {e}"),
            }

            // The stream is asynchronous: the first frame arrives on the
            // dispatch queue a moment after `start` returns.
            let mut frame = None;
            for _ in 0..50 {
                match capturer.next_frame() {
                    Ok(Some(f)) => {
                        frame = Some(f);
                        break;
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                    Err(e) => panic!("capture failed on a live display: {e}"),
                }
            }
            let Some(frame) = frame else {
                panic!("a live stream must produce a frame within 2.5s");
            };

            assert!(frame.width > 0 && frame.height > 0);
            assert_eq!(frame.format, PixelFormat::Bgra8);
            assert_eq!(
                frame.data.len(),
                (frame.width as usize) * (frame.height as usize) * BGRA_BYTES_PER_PIXEL
            );

            capturer.stop();
            assert!(capturer.next_frame().is_err());
        }
    }
}

/// The stub this file carries on every target the `capture-screencapturekit`
/// feature is not built for, so the type exists unconditionally on macOS and
/// `cargo build --workspace` needs no platform SDK.
#[cfg(not(all(target_os = "macos", feature = "capture-screencapturekit")))]
mod unavailable {
    use lumepeer_core::protocol::InputEventPayload;

    use crate::capture::{CaptureTarget, Frame, InputCapability, InputInjector, ScreenCapturer};
    use crate::error::{MediaError, Result};

    /// `ScreenCaptureKit` capturer, not built in.
    #[derive(Debug, Default)]
    pub struct MacosCapturer {
        _private: (),
    }

    impl MacosCapturer {
        /// Creates a capturer that refuses to start.
        #[must_use]
        pub const fn new() -> Self {
            Self { _private: () }
        }
    }

    impl ScreenCapturer for MacosCapturer {
        fn start(&mut self, _target: CaptureTarget) -> Result<()> {
            Err(MediaError::CaptureUnavailable(
                "macOS capture needs the `capture-screencapturekit` feature".to_owned(),
            ))
        }

        fn next_frame(&mut self) -> Result<Option<Frame>> {
            Err(MediaError::CaptureUnavailable(
                "macOS capture needs the `capture-screencapturekit` feature".to_owned(),
            ))
        }

        fn stop(&mut self) {}

        fn input_capability(&self) -> InputCapability {
            InputCapability::Full
        }
    }

    /// `CGEvent` injector, not built in.
    #[derive(Debug, Default)]
    pub struct MacosInjector {
        _private: (),
    }

    impl MacosInjector {
        /// Refuses: rebuild with `capture-screencapturekit` to get real
        /// injection.
        ///
        /// # Errors
        /// Always.
        pub fn connect() -> Result<Self> {
            Err(MediaError::InputUnavailable(
                "macOS input injection needs the `capture-screencapturekit` feature".to_owned(),
            ))
        }
    }

    impl InputInjector for MacosInjector {
        fn inject(&mut self, _event: &InputEventPayload) -> Result<()> {
            Err(MediaError::InputUnavailable(
                "macOS input injection needs the `capture-screencapturekit` feature".to_owned(),
            ))
        }

        fn capability(&self) -> InputCapability {
            InputCapability::Full
        }
    }
}
