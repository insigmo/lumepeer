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
pub use self::screen_capture_kit::MacosCapturer;

#[cfg(not(all(target_os = "macos", feature = "capture-screencapturekit")))]
pub use self::unavailable::MacosCapturer;

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
    use objc2_core_graphics::CGMainDisplayID;
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

    use crate::capture::{CaptureTarget, Frame, InputCapability, PixelFormat, ScreenCapturer};
    use crate::error::{MediaError, Result};

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
            // SAFETY: `sample` is the buffer ScreenCaptureKit just handed to
            // the delegate method below; it is valid for that call, and
            // `image_buffer` only reads it. Idle frames, which is how
            // ScreenCaptureKit says "nothing changed", carry no image buffer
            // and land in the `None` arm.
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
    use crate::capture::{CaptureTarget, Frame, InputCapability, ScreenCapturer};
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
}
