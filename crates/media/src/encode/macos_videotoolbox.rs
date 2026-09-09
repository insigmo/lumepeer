//! macOS `VideoToolbox` hardware H.264 encoder (design doc §5.1, §11,
//! §18/§19 phase 4; ADR 0066).
//!
//! The third sibling of [`super::windows`] (Media Foundation, ADR 0011) and
//! [`super::linux_vaapi`] (VA-API, ADR 0040), built to the same rule the other
//! two follow: [`hardware_available`] is not a capability query, it is a
//! rehearsal. Every Mac since 2011 has a Quick Sync or Apple-silicon H.264
//! encoder behind `VideoToolbox`, so "does this machine have one" is nearly
//! always yes and nearly always useless to ask — the question that decides
//! whether a session shows a picture is whether a session opens *and produces
//! a frame*, which is what the probe here actually does.
//!
//! Two things about the output side are not optional:
//!
//! - `VideoToolbox` emits AVCC — each NAL unit prefixed by its big-endian
//!   length — while the guest's decoder (`apps/desktop/src/view-decoder.ts`)
//!   walks Annex-B start codes to derive its own `avc1.PPCCLL` codec string.
//!   The rewrite happens here, in [`avcc_to_annex_b`], because it is the
//!   backend's own output format that differs, not the protocol's.
//! - `VideoToolbox` keeps the sequence and picture parameter sets in the
//!   sample's `CMFormatDescription` rather than in the bitstream. A guest that
//!   joined mid-stream would never see an SPS, so every keyframe gets them
//!   prepended (see `parameter_sets_annex_b`).
//!
//! No B-frames and no reordering window: ADR 0059 makes the host encode for
//! latency, and a picture that depends on one that has not been sent yet is
//! exactly the delay that decision exists to refuse.

use crate::error::{MediaError, Result};

/// Annex-B four-byte start code. Three-byte start codes are legal too, but a
/// fixed four-byte one is what every encoder in this crate emits and what
/// keeps the rewrite below a straight substitution.
const ANNEX_B_START_CODE: [u8; 4] = [0x00, 0x00, 0x00, 0x01];

/// Mask selecting `nal_unit_type` out of an H.264 NAL unit header byte.
const H264_NAL_TYPE_MASK: u8 = 0x1F;

/// `nal_unit_type` of an IDR slice — the one NAL that makes an access unit
/// decodable on its own.
const H264_NAL_TYPE_IDR: u8 = 5;

/// Widest NAL length prefix AVCC defines (ISO/IEC 14496-15: 1, 2 or 4 bytes,
/// with 3 legal but unused in practice).
const MAX_NAL_LENGTH_SIZE: usize = 4;

/// Rewrites one AVCC access unit as Annex-B and reports whether it carries an
/// IDR slice.
///
/// `avcc` is a run of NAL units, each preceded by its length in
/// `length_size` big-endian bytes — the layout `VideoToolbox` hands back and
/// the one `CMVideoFormatDescriptionGetH264ParameterSetAtIndex` reports the
/// prefix width for. The output is the same NAL units behind
/// [`ANNEX_B_START_CODE`], which is what the guest's decoder expects.
///
/// Pure byte manipulation with no platform API in it, so it is compiled and
/// tested on every platform the feature is enabled for rather than only on a
/// Mac: a Mac is the one machine this repository does not have on hand, and
/// the conversion is the part of this backend most likely to be wrong in a way
/// a compiler cannot see.
///
/// # Errors
/// [`MediaError::Encode`] if `length_size` is not a legal AVCC prefix width,
/// or if a length field runs off the end of the sample. Neither is something
/// a healthy encoder produces, so both are reported rather than papered over.
fn avcc_to_annex_b(avcc: &[u8], length_size: usize) -> Result<(Vec<u8>, bool)> {
    if length_size == 0 || length_size > MAX_NAL_LENGTH_SIZE {
        return Err(MediaError::Encode(format!(
            "VideoToolbox reported a {length_size}-byte NAL length prefix, which AVCC does not define"
        )));
    }

    // Every NAL swaps a `length_size`-byte prefix for a four-byte start code,
    // so this is exact when the prefix is four bytes wide and short by at most
    // three bytes per NAL otherwise.
    let mut out = Vec::with_capacity(avcc.len());
    let mut keyframe = false;
    let mut at = 0usize;

    while at < avcc.len() {
        let prefix_end = at
            .checked_add(length_size)
            .ok_or_else(|| MediaError::Encode("AVCC sample offset overflowed".to_owned()))?;
        let prefix = avcc.get(at..prefix_end).ok_or_else(|| {
            MediaError::Encode("AVCC sample ends inside a NAL length prefix".to_owned())
        })?;
        let length = prefix
            .iter()
            .fold(0usize, |acc, byte| (acc << 8) | usize::from(*byte));
        if length == 0 {
            return Err(MediaError::Encode(
                "AVCC sample declares a zero-length NAL unit".to_owned(),
            ));
        }

        let nal_end = prefix_end.checked_add(length).ok_or_else(|| {
            MediaError::Encode("AVCC NAL length overflowed the sample".to_owned())
        })?;
        let nal = avcc.get(prefix_end..nal_end).ok_or_else(|| {
            MediaError::Encode(
                "AVCC sample declares a NAL unit longer than the sample itself".to_owned(),
            )
        })?;

        if nal
            .first()
            .is_some_and(|header| header & H264_NAL_TYPE_MASK == H264_NAL_TYPE_IDR)
        {
            keyframe = true;
        }
        out.extend_from_slice(&ANNEX_B_START_CODE);
        out.extend_from_slice(nal);
        at = nal_end;
    }

    Ok((out, keyframe))
}

/// The real backend. Kept in an inline module so the `unsafe_code` carve-out
/// it needs cannot leak onto [`avcc_to_annex_b`] above, which needs none
/// (§21, and the same shape `capture::macos` uses).
#[cfg(target_os = "macos")]
pub use self::video_toolbox::VideoToolboxEncoder;

#[cfg(target_os = "macos")]
pub(super) use self::video_toolbox::hardware_available;

#[cfg(target_os = "macos")]
mod video_toolbox {
    #![allow(
        unsafe_code,
        reason = "every VideoToolbox/CoreMedia/CoreVideo entry point in the objc2 bindings is an `unsafe fn` because it crosses into a C framework, and VTCompressionSessionCreate takes a plain C function pointer for its output callback. Every block below carries a SAFETY note, per §21. See ADR 0066."
    )]

    use std::ffi::c_void;
    use std::ptr::NonNull;
    use std::sync::{Arc, Mutex};

    use objc2_core_foundation::{
        CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType,
        kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks,
    };
    use objc2_core_media::{
        CMFormatDescription, CMSampleBuffer, CMTime,
        CMVideoFormatDescriptionGetH264ParameterSetAtIndex, kCMVideoCodecType_H264,
    };
    use objc2_core_video::{
        CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddressOfPlane,
        CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetPlaneCount,
        CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
        kCVReturnSuccess,
    };
    use objc2_video_toolbox::{
        VTCompressionSession, VTEncodeInfoFlags, VTSessionSetProperty,
        kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
        kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxKeyFrameInterval,
        kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
        kVTEncodeFrameOptionKey_ForceKeyFrame, kVTProfileLevel_H264_Baseline_AutoLevel,
        kVTProfileLevel_H264_Main_AutoLevel,
    };

    use super::avcc_to_annex_b;
    use crate::capture::{Frame, PixelFormat};
    use crate::encode::nv12::bgra_to_nv12;
    use crate::encode::{EncodedFrame, EncoderConfig, EncoderKind, VideoCodec, VideoEncoder};
    use crate::error::{MediaError, Result};

    /// Apple's `noErr`: the `OSStatus` every call below returns on success.
    const NO_ERR: i32 = 0;

    /// Probe geometry, matching both sibling backends': a real 4x4 macroblock
    /// grid, small enough that opening a session costs nothing.
    const PROBE_WIDTH: u32 = 64;
    /// See [`PROBE_WIDTH`].
    const PROBE_HEIGHT: u32 = 64;

    /// Bits per kilobit, for the kbps of §14 against the bps of
    /// `AverageBitRate`.
    const BITS_PER_KBIT: u32 = 1_000;

    /// Seconds between two unrequested intra frames, matching the Media
    /// Foundation backend's `GOP_SECONDS`.
    ///
    /// Long, not infinite. A keyframe is the largest frame in the stream, and
    /// the guest asks for an intra frame at the moment it actually needs one
    /// (§11's `KeyframeRequest`); what this bounds is the drift a stream
    /// accumulates when no request ever comes.
    const GOP_SECONDS: u32 = 10;

    /// `kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange`, the bi-planar NV12
    /// layout [`bgra_to_nv12`] produces and the one every hardware H.264
    /// encoder takes. Spelled the way `capture::macos` spells its own pixel
    /// format constant, from the four-character code itself.
    const PIXEL_FORMAT_NV12: u32 = u32::from_be_bytes(*b"420v");

    /// Luma plane index of an NV12 `CVPixelBuffer`.
    const PLANE_LUMA: usize = 0;
    /// Interleaved chroma plane index of an NV12 `CVPixelBuffer`.
    const PLANE_CHROMA: usize = 1;
    /// Planes an NV12 `CVPixelBuffer` must have.
    const NV12_PLANE_COUNT: usize = 2;

    /// Lock flags for writing into a pixel buffer this module owns.
    ///
    /// Zero — the absence of `kCVPixelBufferLock_ReadOnly`. Named rather than
    /// inlined for the same reason `capture::macos` names its read-only
    /// counterpart: `CoreVideo` documents a lock and its unlock passed
    /// different flags as undefined behaviour.
    const LOCK_FOR_WRITING: CVPixelBufferLockFlags = CVPixelBufferLockFlags(0);

    /// Timescale of the presentation timestamps handed to `VideoToolbox`:
    /// microseconds, so a [`Frame`]'s `timestamp_us` passes through unscaled.
    const PTS_TIMESCALE: i32 = 1_000_000;

    /// Whether a genuinely usable `VideoToolbox` H.264 encoder exists right
    /// now (§18, ADR 0066).
    ///
    /// Opens a real `VTCompressionSession` *and encodes a frame through it*,
    /// which is one step further than the Windows and VA-API probes go, and
    /// deliberately so: on macOS the interesting failures are not "no encoder
    /// is installed" but "the session opened and then produced nothing", and
    /// a probe that stops at `VTCompressionSessionCreate` cannot tell the two
    /// apart. Answering yes without that evidence is the defect that shipped
    /// a blank screen in v0.0.14 (see the comment above the release matrix in
    /// `.github/workflows/release.yml`).
    ///
    /// H.264 only. `VideoToolbox` encodes AV1 on hardware that has it, but
    /// through a different codec type with its own parameter sets and its own
    /// `kVTProfileLevel`; answering an AV1 question with an H.264 rehearsal is
    /// exactly the mismatch §11's mutual-hardware-support rule exists to
    /// prevent, so the codec is checked here — in the backend that would
    /// otherwise answer for a codec it never opened a session for (ADR 0069).
    pub(in crate::encode) fn hardware_available(config: EncoderConfig) -> bool {
        if config.codec != VideoCodec::H264 {
            return false;
        }
        let Ok(mut encoder) = VideoToolboxEncoder::new(config) else {
            return false;
        };
        encoder
            .encode(&probe_frame())
            .is_ok_and(|frame| !frame.data.is_empty())
    }

    /// A flat grey [`PROBE_WIDTH`]x[`PROBE_HEIGHT`] picture for the probe to
    /// push through a session it just opened.
    fn probe_frame() -> Frame {
        Frame {
            width: PROBE_WIDTH,
            height: PROBE_HEIGHT,
            format: PixelFormat::Bgra8,
            timestamp_us: 0,
            data: vec![0x80; (PROBE_WIDTH as usize) * (PROBE_HEIGHT as usize) * 4],
        }
    }

    /// One Annex-B access unit as the compression callback produced it.
    struct Compressed {
        /// Bitstream bytes, with the parameter sets prepended on a keyframe.
        data: Vec<u8>,
        /// Whether it carries an IDR slice.
        keyframe: bool,
    }

    /// The one-slot mailbox `VideoToolbox`'s output callback writes into.
    ///
    /// A callback rather than a block because `VTCompressionSessionCreate`
    /// takes a plain C function pointer and a client reference value, which
    /// needs no `block2` closure to keep alive and no lifetime that outlives
    /// this module's own reasoning about the session.
    #[derive(Default)]
    struct Output {
        /// The frame the callback produced, if it produced one.
        frame: Option<Compressed>,
        /// Why it did not, if it did not.
        error: Option<String>,
    }

    /// Everything one open compression session owns.
    ///
    /// Kept in one struct because the teardown order matters and Rust's
    /// declaration order is what enforces it: [`Drop`] invalidates the session
    /// before the `output` mailbox the session holds a raw pointer to can be
    /// freed.
    struct Session {
        /// The compression session itself.
        handle: CFRetained<VTCompressionSession>,
        /// The buffer every frame is uploaded into. Reused rather than
        /// allocated per frame: `encode_one` completes each frame before it
        /// returns, so `VideoToolbox` is done reading it by the time the next
        /// one is written.
        pixels: CFRetained<CVPixelBuffer>,
        /// Shared with the raw pointer `VTCompressionSessionCreate` was given.
        output: Arc<Mutex<Output>>,
        /// Coded dimensions of this session.
        dims: (u32, u32),
    }

    impl Drop for Session {
        fn drop(&mut self) {
            // SAFETY: `VTCompressionSessionInvalidate` is documented as the
            // deterministic teardown for a session the caller created, and
            // this runs before any field is dropped, so the `output` mailbox
            // the session was handed a raw pointer to is still alive while
            // the encoder is being torn down.
            unsafe { self.handle.invalidate() };
        }
    }

    /// Hardware H.264 encoder backed by a `VTCompressionSession`.
    pub struct VideoToolboxEncoder {
        session: Session,
        config: EncoderConfig,
        /// Set by [`VideoEncoder::request_keyframe`], cleared once the frame
        /// carrying `kVTEncodeFrameOptionKey_ForceKeyFrame` has been
        /// submitted.
        force_keyframe: bool,
        /// Presentation timestamp of the last submitted frame.
        /// `VTCompressionSessionEncodeFrame` documents each timestamp as
        /// having to be greater than the one before it, and a capturer that
        /// hands out two frames within the same microsecond would otherwise
        /// stall the session.
        last_pts: i64,
    }

    // Mirrors both sibling backends' `Debug`: the session state is neither
    // printable nor safe to log, only the settings that matter for a log line.
    impl std::fmt::Debug for VideoToolboxEncoder {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("VideoToolboxEncoder")
                .field("config", &self.config)
                .field("dims", &self.session.dims)
                .finish_non_exhaustive()
        }
    }

    // SAFETY: `CFRetained` is `!Send` by inference because the CoreFoundation
    // types it wraps carry raw pointers. A `VTCompressionSession` is not
    // thread-affine — `VideoToolbox` is documented as callable from any
    // thread, and its own output callback runs on a thread of the framework's
    // choosing — what it forbids is *concurrent* use of one session, and
    // `VideoEncoder: Send` (not `Sync`) is exactly the promise that only one
    // thread touches this at a time. The encoder owns its whole session:
    // nothing inside `Session` is cloned out of it except the `Arc` the
    // callback reads through, which is itself synchronized by its `Mutex`.
    // The same reasoning both sibling backends record for their handles.
    unsafe impl Send for VideoToolboxEncoder {}

    impl VideoToolboxEncoder {
        /// Opens a session at the probe geometry; the first real frame
        /// reopens it at the screen's size, the same way the Media Foundation
        /// and VA-API backends do (`EncoderConfig` carries no dimensions).
        ///
        /// # Errors
        /// [`MediaError::EncoderUnavailable`] if `VideoToolbox` will not open
        /// a session with these settings, or if `config.codec` is not H.264 —
        /// this backend implements no other.
        pub fn new(config: EncoderConfig) -> Result<Self> {
            Ok(Self {
                session: Session::open(PROBE_WIDTH, PROBE_HEIGHT, config)?,
                config,
                force_keyframe: false,
                last_pts: -1,
            })
        }

        /// The strictly increasing presentation timestamp for the next frame.
        fn next_pts(&mut self, timestamp_us: u64) -> i64 {
            let wanted = i64::try_from(timestamp_us).unwrap_or(i64::MAX);
            let pts = wanted.max(self.last_pts.saturating_add(1));
            self.last_pts = pts;
            pts
        }
    }

    impl VideoEncoder for VideoToolboxEncoder {
        fn encode(&mut self, frame: &Frame) -> Result<EncodedFrame> {
            let (nv12, width, height) = bgra_to_nv12(frame)?;
            if self.session.dims != (width, height) {
                // A resolution change is a rare event, and a compression
                // session's frame size is fixed at creation, so this reopens
                // rather than reconfigures — the same trade the Media
                // Foundation backend's `reconfigure` makes, and it costs the
                // same thing: the next picture is a keyframe.
                self.session = Session::open(width, height, self.config)?;
                self.last_pts = -1;
            }
            self.session.upload(&nv12, width, height)?;

            let force_keyframe = std::mem::take(&mut self.force_keyframe);
            let pts = self.next_pts(frame.timestamp_us);
            let compressed = self
                .session
                .encode_one(pts, self.config.fps, force_keyframe)?;

            Ok(EncodedFrame {
                keyframe: compressed.keyframe,
                timestamp_us: frame.timestamp_us,
                data: compressed.data,
            })
        }

        fn set_bitrate(&mut self, bitrate_kbps: u32) -> Result<()> {
            if bitrate_kbps == self.config.bitrate_kbps {
                return Ok(());
            }
            // A live property change, not a new session: the adaptive
            // controller may move the target once a second
            // (`ABR_ADJUST_MAX_RATE_PER_SEC`, §11/§14), and reopening would
            // put a keyframe-sized spike into the stream every one of those
            // — the very hitch adaptation exists to smooth over (ADR 0059).
            let bits = CFNumber::new_i32(
                i32::try_from(bitrate_kbps.saturating_mul(BITS_PER_KBIT)).unwrap_or(i32::MAX),
            );
            // SAFETY: reading a framework's `CFString` constant.
            let key = unsafe { kVTCompressionPropertyKey_AverageBitRate };
            set_property(&self.session.handle, key, &bits)
                .map_err(|error| MediaError::Encode(error.to_string()))?;
            self.config.bitrate_kbps = bitrate_kbps;
            Ok(())
        }

        fn request_keyframe(&mut self) -> Result<()> {
            // `kVTEncodeFrameOptionKey_ForceKeyFrame` is a per-frame option,
            // not a session property, so the request is recorded here and
            // spent by the next `encode` — which is exactly the "at the next
            // opportunity" the trait promises. The
            // `KEYFRAME_MIN_INTERVAL_MS` budget is the caller's, not this
            // backend's.
            self.force_keyframe = true;
            Ok(())
        }

        fn kind(&self) -> EncoderKind {
            EncoderKind::Hardware
        }
    }

    impl Session {
        /// Opens and configures one compression session at `width`x`height`.
        fn open(width: u32, height: u32, config: EncoderConfig) -> Result<Self> {
            if config.codec != VideoCodec::H264 {
                return Err(MediaError::EncoderUnavailable(
                    "the VideoToolbox backend only implements H.264".to_owned(),
                ));
            }
            let coded_width = i32::try_from(width).map_err(|_| {
                MediaError::EncoderUnavailable("frame width out of range".to_owned())
            })?;
            let coded_height = i32::try_from(height).map_err(|_| {
                MediaError::EncoderUnavailable("frame height out of range".to_owned())
            })?;

            let output = Arc::new(Mutex::new(Output::default()));
            // The callback is handed the address of the `Mutex` inside this
            // `Arc`, which is stable for as long as the `Arc` lives; the
            // `Session` holds one and `Drop` invalidates the session before
            // releasing it.
            let ref_con: *mut c_void = Arc::as_ptr(&output).cast::<c_void>().cast_mut();

            let mut raw: *mut VTCompressionSession = std::ptr::null_mut();
            // SAFETY: `on_compressed` matches `VTCompressionOutputCallback`'s
            // signature and reads `ref_con` only as the `Mutex<Output>` it
            // actually is; `raw` is a live local for the duration of the call
            // and receives an owned (+1) session on success.
            let status = unsafe {
                VTCompressionSession::create(
                    None,
                    coded_width,
                    coded_height,
                    kCMVideoCodecType_H264,
                    None,
                    None,
                    None,
                    Some(on_compressed),
                    ref_con,
                    NonNull::from(&mut raw),
                )
            };
            if status != NO_ERR {
                return Err(MediaError::EncoderUnavailable(format!(
                    "VTCompressionSessionCreate failed: OSStatus {status}"
                )));
            }
            let raw = NonNull::new(raw).ok_or_else(|| {
                MediaError::EncoderUnavailable(
                    "VTCompressionSessionCreate reported success with no session".to_owned(),
                )
            })?;
            // SAFETY: `VTCompressionSessionCreate` returns a +1 reference,
            // which `CFRetained::from_raw` takes ownership of.
            let session = unsafe { CFRetained::from_raw(raw) };

            let opened = Self {
                handle: session,
                pixels: create_nv12_pixel_buffer(width, height)?,
                output,
                dims: (width, height),
            };
            opened.configure(config)?;
            Ok(opened)
        }

        /// Applies the settings a live remote-desktop session needs.
        ///
        /// `RealTime` and `AllowFrameReordering` are hard requirements and are
        /// reported if refused: an encoder free to reorder holds a picture
        /// back waiting for a future one, which is tens of milliseconds of lag
        /// that nothing downstream can win back (ADR 0059). The profile and
        /// the keyframe interval are best-effort — an encoder that keeps its
        /// default for either still produces a usable stream.
        fn configure(&self, config: EncoderConfig) -> Result<()> {
            // SAFETY (all of the reads below): taking a framework's
            // `CFString` constant, which is `unsafe` only because it is an
            // `extern "C"` static.
            let real_time = unsafe { kVTCompressionPropertyKey_RealTime };
            set_property(&self.handle, real_time, CFBoolean::new(true))?;

            let reordering = unsafe { kVTCompressionPropertyKey_AllowFrameReordering };
            set_property(&self.handle, reordering, CFBoolean::new(false))?;

            let bitrate = unsafe { kVTCompressionPropertyKey_AverageBitRate };
            let bits = CFNumber::new_i32(
                i32::try_from(config.bitrate_kbps.saturating_mul(BITS_PER_KBIT))
                    .unwrap_or(i32::MAX),
            );
            set_property(&self.handle, bitrate, &bits)?;

            // Main rather than Baseline where the encoder offers it: same
            // decoder cost on anything built this decade, and it buys CABAC
            // and the 8x8 transform, which is real sharpness back on exactly
            // what a desktop is made of — text edges and flat fills. Neither
            // needs frame reordering, which stays off above. An encoder that
            // refuses Main falls back to Baseline rather than failing.
            let profile_level = unsafe { kVTCompressionPropertyKey_ProfileLevel };
            let main = unsafe { kVTProfileLevel_H264_Main_AutoLevel };
            if set_property(&self.handle, profile_level, main).is_err() {
                let baseline = unsafe { kVTProfileLevel_H264_Baseline_AutoLevel };
                let _ = set_property(&self.handle, profile_level, baseline);
            }

            let fps = i32::from(config.fps.max(1));
            let expected = unsafe { kVTCompressionPropertyKey_ExpectedFrameRate };
            let _ = set_property(&self.handle, expected, &CFNumber::new_i32(fps));

            let interval = unsafe { kVTCompressionPropertyKey_MaxKeyFrameInterval };
            let gop = fps.saturating_mul(i32::try_from(GOP_SECONDS).unwrap_or(i32::MAX));
            let _ = set_property(&self.handle, interval, &CFNumber::new_i32(gop));

            Ok(())
        }

        /// Copies one NV12 picture into this session's pixel buffer.
        fn upload(&self, nv12: &[u8], width: u32, height: u32) -> Result<()> {
            let width = width as usize;
            let height = height as usize;
            let luma_len = width
                .checked_mul(height)
                .ok_or_else(|| MediaError::Encode("frame size overflows".to_owned()))?;
            let chroma_len = luma_len / 2;
            if nv12.len() < luma_len + chroma_len {
                return Err(MediaError::Encode(
                    "NV12 frame is shorter than the picture it describes".to_owned(),
                ));
            }
            if CVPixelBufferGetPlaneCount(&self.pixels) < NV12_PLANE_COUNT {
                return Err(MediaError::Encode(
                    "the pixel buffer CoreVideo returned is not bi-planar".to_owned(),
                ));
            }

            // SAFETY: locking the base address is CoreVideo's documented
            // precondition for writing through it. The matching unlock below
            // runs on every path out of this function, with identical flags.
            let status = unsafe { CVPixelBufferLockBaseAddress(&self.pixels, LOCK_FOR_WRITING) };
            if status != kCVReturnSuccess {
                return Err(MediaError::Encode(format!(
                    "locking the encoder's pixel buffer failed: CVReturn {status}"
                )));
            }

            let luma = self.copy_plane(PLANE_LUMA, &nv12[..luma_len], width, height);
            let chroma = luma.and_then(|()| {
                self.copy_plane(
                    PLANE_CHROMA,
                    &nv12[luma_len..luma_len + chroma_len],
                    width,
                    height / 2,
                )
            });

            // SAFETY: balances the successful lock above, with the same flags.
            let _ = unsafe { CVPixelBufferUnlockBaseAddress(&self.pixels, LOCK_FOR_WRITING) };
            chroma
        }

        /// Copies one tightly packed plane into the locked pixel buffer, row
        /// by row, because `CoreVideo` is free to pad each row out to its own
        /// stride.
        fn copy_plane(&self, plane: usize, source: &[u8], width: usize, rows: usize) -> Result<()> {
            let stride = CVPixelBufferGetBytesPerRowOfPlane(&self.pixels, plane);
            if stride < width {
                return Err(MediaError::Encode(format!(
                    "pixel buffer plane {plane} has stride {stride}, shorter than one {width}-byte row"
                )));
            }
            let base = CVPixelBufferGetBaseAddressOfPlane(&self.pixels, plane).cast::<u8>();
            if base.is_null() {
                return Err(MediaError::Encode(format!(
                    "pixel buffer plane {plane} has no base address"
                )));
            }
            for row in 0..rows {
                let Some(from) = source.get(row * width..row * width + width) else {
                    return Err(MediaError::Encode(format!(
                        "NV12 plane {plane} ends before row {row}"
                    )));
                };
                // SAFETY: `base` is the locked base address of a plane
                // CoreVideo reports as at least `rows` rows of `stride`
                // bytes; `row < rows` and `width <= stride`, so the
                // destination stays inside the plane. Source and destination
                // are distinct allocations, so they cannot overlap.
                unsafe {
                    std::ptr::copy_nonoverlapping(from.as_ptr(), base.add(row * stride), width);
                }
            }
            Ok(())
        }

        /// Submits the uploaded picture and waits for the frame it owes.
        ///
        /// `VTCompressionSessionCompleteFrames` up to this frame's own
        /// timestamp is what makes the session one-picture-in, one-picture-out
        /// without a guess about how much the encoder buffers. Unlike the
        /// Media Foundation drain this mirrors, it is not a pipeline flush:
        /// it forces emission of frames already submitted and leaves the
        /// session streaming with its reference frames intact, so there is no
        /// stream restart and no forced IDR behind it.
        fn encode_one(&self, pts: i64, fps: u8, force_keyframe: bool) -> Result<Compressed> {
            self.take_output();

            // SAFETY: `CMTimeMake` is arithmetic on plain integers.
            let presentation = unsafe { CMTime::new(pts, PTS_TIMESCALE) };
            // SAFETY: as above.
            let duration = unsafe {
                CMTime::new(
                    i64::from(PTS_TIMESCALE / i32::from(fps.max(1))),
                    PTS_TIMESCALE,
                )
            };

            let properties = if force_keyframe {
                Some(force_keyframe_properties()?)
            } else {
                None
            };

            // SAFETY: `self.pixels` is a live pixel buffer this session owns
            // and has finished writing (the lock in `upload` is released);
            // `properties`, when present, holds only framework constants; the
            // two null pointers are the documented "no reference value" and
            // "no info flags wanted".
            let status = unsafe {
                self.handle.encode_frame(
                    &self.pixels,
                    presentation,
                    duration,
                    properties.as_deref(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if status != NO_ERR {
                return Err(MediaError::Encode(format!(
                    "VTCompressionSessionEncodeFrame failed: OSStatus {status}"
                )));
            }

            // SAFETY: forcing out the frame just submitted; `presentation` is
            // a plain value.
            let status = unsafe { self.handle.complete_frames(presentation) };
            if status != NO_ERR {
                return Err(MediaError::Encode(format!(
                    "VTCompressionSessionCompleteFrames failed: OSStatus {status}"
                )));
            }

            match self.take_output() {
                (Some(frame), _) => Ok(frame),
                (None, Some(error)) => Err(MediaError::Encode(error)),
                (None, None) => Err(MediaError::Encode(
                    "the hardware encoder produced no output for this frame".to_owned(),
                )),
            }
        }

        /// Empties the mailbox, returning whatever was in it.
        fn take_output(&self) -> (Option<Compressed>, Option<String>) {
            let Ok(mut output) = self.output.lock() else {
                // Poisoned means the callback panicked, which it is written
                // not to do; there is nothing left in the mailbox to trust.
                return (
                    None,
                    Some("the encoder's output mailbox was poisoned".to_owned()),
                );
            };
            (output.frame.take(), output.error.take())
        }
    }

    /// Sets one session property, reporting a refusal rather than assuming it
    /// took.
    fn set_property(session: &VTCompressionSession, key: &CFString, value: &CFType) -> Result<()> {
        // SAFETY: `VTSessionSetProperty` takes the session as the `VTSession`
        // (a `CFType`) it derefs to, and reads both `key` and `value`, which
        // outlive the call.
        let status = unsafe { VTSessionSetProperty(session, key, Some(value)) };
        if status == NO_ERR {
            Ok(())
        } else {
            Err(MediaError::EncoderUnavailable(format!(
                "VideoToolbox refused a session property: OSStatus {status}"
            )))
        }
    }

    /// Builds the one-entry frame-properties dictionary that asks for an IDR.
    fn force_keyframe_properties() -> Result<CFRetained<CFDictionary>> {
        // SAFETY: reading a framework's `CFString` constant.
        let key: &CFType = unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame };
        let value: &CFType = CFBoolean::new(true);
        let mut keys: [*const c_void; 1] = [std::ptr::from_ref(key).cast()];
        let mut values: [*const c_void; 1] = [std::ptr::from_ref(value).cast()];

        // SAFETY: `keys` and `values` are one-element arrays of live CFType
        // pointers, matched by the `1` count, and the callbacks are
        // CoreFoundation's own retain/release pair for CFType keys and
        // values — which is what makes the dictionary keep both alive.
        unsafe {
            CFDictionary::new(
                None,
                keys.as_mut_ptr(),
                values.as_mut_ptr(),
                1,
                &raw const kCFTypeDictionaryKeyCallBacks,
                &raw const kCFTypeDictionaryValueCallBacks,
            )
        }
        .ok_or_else(|| MediaError::Encode("could not build the force-keyframe options".to_owned()))
    }

    /// Allocates the NV12 pixel buffer one session uploads every frame into.
    fn create_nv12_pixel_buffer(width: u32, height: u32) -> Result<CFRetained<CVPixelBuffer>> {
        let mut raw: *mut CVPixelBuffer = std::ptr::null_mut();
        // SAFETY: `raw` is a live local for the duration of the call and
        // receives an owned (+1) buffer on success; no attributes are
        // requested, so no dictionary generics can be wrong.
        let status = unsafe {
            CVPixelBufferCreate(
                None,
                width as usize,
                height as usize,
                PIXEL_FORMAT_NV12,
                None,
                NonNull::from(&mut raw),
            )
        };
        if status != kCVReturnSuccess {
            return Err(MediaError::EncoderUnavailable(format!(
                "CVPixelBufferCreate failed: CVReturn {status}"
            )));
        }
        let raw = NonNull::new(raw).ok_or_else(|| {
            MediaError::EncoderUnavailable(
                "CVPixelBufferCreate reported success with no buffer".to_owned(),
            )
        })?;
        // SAFETY: `CVPixelBufferCreate` returns a +1 reference, which
        // `CFRetained::from_raw` takes ownership of.
        Ok(unsafe { CFRetained::from_raw(raw) })
    }

    /// `VideoToolbox`'s compression callback.
    ///
    /// Runs on a thread of the framework's choosing, so everything it touches
    /// goes through the mailbox's `Mutex`. It must not unwind into
    /// Objective-C, which is why there is no `unwrap`, no indexing and no
    /// `panic!` anywhere below.
    unsafe extern "C-unwind" fn on_compressed(
        output_ref_con: *mut c_void,
        _source_frame_ref_con: *mut c_void,
        status: i32,
        info_flags: VTEncodeInfoFlags,
        sample_buffer: *mut CMSampleBuffer,
    ) {
        // SAFETY: `output_ref_con` is the address of the `Mutex<Output>`
        // inside the `Arc` that `Session::open` created and that the `Session`
        // keeps alive until after `VTCompressionSessionInvalidate`, so it is
        // either null or a live, correctly typed reference.
        let Some(output) = (unsafe { output_ref_con.cast::<Mutex<Output>>().as_ref() }) else {
            return;
        };
        let Ok(mut output) = output.lock() else {
            return;
        };

        if status != NO_ERR {
            output.error = Some(format!("the hardware encoder failed: OSStatus {status}"));
            return;
        }
        if info_flags.contains(VTEncodeInfoFlags::FrameDropped) {
            output.error = Some("the hardware encoder dropped this frame".to_owned());
            return;
        }
        // SAFETY: on success `VideoToolbox` passes a live sample buffer it
        // owns for the duration of this call; it is only read here.
        let Some(sample) = (unsafe { sample_buffer.as_ref() }) else {
            output.error = Some("the hardware encoder produced an empty sample".to_owned());
            return;
        };

        match collect(sample) {
            Ok(frame) => output.frame = Some(frame),
            Err(error) => output.error = Some(error.to_string()),
        }
    }

    /// Turns one compressed `CMSampleBuffer` into the Annex-B access unit the
    /// guest expects.
    fn collect(sample: &CMSampleBuffer) -> Result<Compressed> {
        // SAFETY: reading the sample's own data buffer, which it owns.
        let block = unsafe { sample.data_buffer() }.ok_or_else(|| {
            MediaError::Encode("the compressed sample carries no data".to_owned())
        })?;
        // SAFETY: reading the block buffer's own length.
        let length = unsafe { block.data_length() };
        if length == 0 {
            return Err(MediaError::Encode(
                "the compressed sample is empty".to_owned(),
            ));
        }

        let mut avcc = vec![0u8; length];
        let destination = NonNull::new(avcc.as_mut_ptr().cast::<c_void>()).ok_or_else(|| {
            MediaError::Encode("could not allocate for the compressed sample".to_owned())
        })?;
        // SAFETY: `destination` points at exactly `length` writable bytes,
        // which is the same length the block buffer just reported.
        let status = unsafe { block.copy_data_bytes(0, length, destination) };
        if status != NO_ERR {
            return Err(MediaError::Encode(format!(
                "CMBlockBufferCopyDataBytes failed: OSStatus {status}"
            )));
        }

        // SAFETY: reading the sample's own format description, which it owns.
        let format = unsafe { sample.format_description() }.ok_or_else(|| {
            MediaError::Encode("the compressed sample carries no format description".to_owned())
        })?;
        let (parameter_sets, length_size) = parameter_sets_annex_b(&format)?;

        let (payload, keyframe) = avcc_to_annex_b(&avcc, length_size)?;
        let data = if keyframe {
            // The SPS and PPS live in the format description, not in the
            // bitstream, so without this a guest that joined mid-stream has
            // nothing describing what follows and never draws a frame.
            let mut with_headers = Vec::with_capacity(parameter_sets.len() + payload.len());
            with_headers.extend_from_slice(&parameter_sets);
            with_headers.extend_from_slice(&payload);
            with_headers
        } else {
            payload
        };

        Ok(Compressed { data, keyframe })
    }

    /// Reads the H.264 parameter sets out of a format description as Annex-B,
    /// along with the NAL length prefix width the samples themselves use.
    fn parameter_sets_annex_b(format: &CMFormatDescription) -> Result<(Vec<u8>, usize)> {
        let mut count: usize = 0;
        let mut length_size: std::ffi::c_int = 0;
        // SAFETY: the two out-parameters are live locals; the pointer and
        // size out-parameters are null, which the function documents as
        // "only tell me the count".
        let status = unsafe {
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                format,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &raw mut count,
                &raw mut length_size,
            )
        };
        if status != NO_ERR {
            return Err(MediaError::Encode(format!(
                "the format description has no H.264 parameter sets: OSStatus {status}"
            )));
        }
        let length_size = usize::try_from(length_size).map_err(|_| {
            MediaError::Encode("the format description reports a negative NAL length".to_owned())
        })?;

        let mut out = Vec::new();
        for index in 0..count {
            let mut pointer: *const u8 = std::ptr::null();
            let mut size: usize = 0;
            // SAFETY: all four out-parameters are live locals; on success
            // `pointer` addresses `size` bytes inside `format`, which this
            // function holds a reference to for the whole loop.
            let status = unsafe {
                CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    format,
                    index,
                    &raw mut pointer,
                    &raw mut size,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if status != NO_ERR || pointer.is_null() || size == 0 {
                return Err(MediaError::Encode(format!(
                    "H.264 parameter set {index} could not be read: OSStatus {status}"
                )));
            }
            // SAFETY: `pointer` and `size` are what the call above reported
            // for a buffer owned by `format`, which outlives this borrow.
            let parameter_set = unsafe { std::slice::from_raw_parts(pointer, size) };
            out.extend_from_slice(&super::ANNEX_B_START_CODE);
            out.extend_from_slice(parameter_set);
        }
        Ok((out, length_size))
    }

    #[cfg(test)]
    mod tests {
        #![allow(clippy::unwrap_used, reason = "a failed assumption must fail the test")]

        use super::*;

        fn frame(width: u32, height: u32, fill: u8) -> Frame {
            Frame {
                width,
                height,
                format: PixelFormat::Bgra8,
                timestamp_us: 0,
                data: vec![fill; (width as usize) * (height as usize) * 4],
            }
        }

        /// Builds an encoder only if hardware is genuinely available, so the
        /// tests below skip gracefully rather than failing on a machine
        /// without one.
        fn try_new_encoder() -> Option<VideoToolboxEncoder> {
            if !hardware_available(EncoderConfig::default()) {
                return None;
            }
            VideoToolboxEncoder::new(EncoderConfig::default()).ok()
        }

        /// The central "never claim hardware is available when it isn't"
        /// rule, checked mechanically rather than by inspection — the twin of
        /// the test of the same name in `encode::windows` and
        /// `encode::linux_vaapi`.
        #[test]
        fn probe_hardware_agrees_with_whether_construction_actually_works() {
            let probed = hardware_available(EncoderConfig::default());
            let constructed = VideoToolboxEncoder::new(EncoderConfig::default())
                .and_then(|mut encoder| encoder.encode(&frame(64, 64, 0x20)))
                .is_ok();
            assert_eq!(
                probed,
                constructed,
                "hardware_available reported {probed} but encoding {}",
                if constructed { "worked" } else { "failed" }
            );
        }

        /// AV1 is a different codec type with its own parameter sets.
        /// Answering an AV1 question with an H.264 rehearsal is the mismatch
        /// §11's mutual-hardware-support rule exists to prevent.
        #[test]
        fn av1_is_refused_regardless_of_what_the_hardware_can_do() {
            let config = EncoderConfig {
                codec: VideoCodec::Av1,
                ..EncoderConfig::default()
            };
            assert!(!hardware_available(config));
            assert!(VideoToolboxEncoder::new(config).is_err());
        }

        #[test]
        fn encodes_a_frame_and_starts_with_a_keyframe_when_hardware_is_available() {
            let Some(mut encoder) = try_new_encoder() else {
                eprintln!("skipping: no usable VideoToolbox H.264 encoder on this machine");
                return;
            };
            let first = encoder.encode(&frame(64, 64, 0x20)).unwrap();
            assert!(first.keyframe, "the first frame must be decodable alone");
            assert!(!first.data.is_empty());
            assert_eq!(encoder.kind(), EncoderKind::Hardware);
        }

        /// The first frame succeeding proves nothing on its own: the encode
        /// loop calls `encode()` once per captured frame on one long-lived
        /// encoder, and a session left in a completed state after frame one
        /// reads to the guest as "waiting for the remote screen" forever.
        #[test]
        fn a_session_keeps_encoding_past_the_first_frame_when_hardware_is_available() {
            let Some(mut encoder) = try_new_encoder() else {
                eprintln!("skipping: no usable VideoToolbox H.264 encoder on this machine");
                return;
            };
            for index in 0..4u64 {
                let mut source = frame(64, 64, 0x20);
                source.timestamp_us = index * 33_333;
                let output = encoder
                    .encode(&source)
                    .unwrap_or_else(|error| panic!("frame {index} failed to encode: {error}"));
                assert!(!output.data.is_empty(), "frame {index} encoded to nothing");
            }
        }

        /// A guest that lost more than it could conceal asks for an intra
        /// frame; a backend that ignores the request leaves it looking at a
        /// broken picture until the `GOP_SECONDS` backstop comes round.
        #[test]
        fn a_requested_keyframe_arrives_when_hardware_is_available() {
            let Some(mut encoder) = try_new_encoder() else {
                eprintln!("skipping: no usable VideoToolbox H.264 encoder on this machine");
                return;
            };
            // The first frame of a stream is a keyframe on its own, so the
            // claim is only testable from the second one onwards.
            let mut source = frame(64, 64, 0x10);
            assert!(encoder.encode(&source).unwrap().keyframe);
            source.timestamp_us = 33_333;
            assert!(!encoder.encode(&source).unwrap().keyframe);
            encoder.request_keyframe().unwrap();
            source.timestamp_us = 66_666;
            assert!(
                encoder.encode(&source).unwrap().keyframe,
                "the encoder ignored a keyframe request"
            );
        }

        #[test]
        fn a_bitrate_change_is_accepted_when_hardware_is_available() {
            let Some(mut encoder) = try_new_encoder() else {
                eprintln!("skipping: no usable VideoToolbox H.264 encoder on this machine");
                return;
            };
            encoder.encode(&frame(64, 64, 0x10)).unwrap();
            encoder.set_bitrate(1_500).unwrap();
            assert_eq!(encoder.config.bitrate_kbps, 1_500);
            let mut source = frame(64, 64, 0x80);
            source.timestamp_us = 33_333;
            assert!(!encoder.encode(&source).unwrap().data.is_empty());
        }

        #[test]
        fn odd_dimensions_are_cropped_rather_than_panicking_when_hardware_is_available() {
            let Some(mut encoder) = try_new_encoder() else {
                eprintln!("skipping: no usable VideoToolbox H.264 encoder on this machine");
                return;
            };
            assert!(encoder.encode(&frame(65, 33, 0x40)).is_ok());
        }

        /// Every frame this backend emits must be Annex-B, because that is
        /// what `apps/desktop/src/view-decoder.ts` walks to derive its own
        /// `avc1.PPCCLL` codec string. An AVCC frame reaching the guest is
        /// a black view, not a decode error.
        #[test]
        fn the_stream_is_annex_b_when_hardware_is_available() {
            let Some(mut encoder) = try_new_encoder() else {
                eprintln!("skipping: no usable VideoToolbox H.264 encoder on this machine");
                return;
            };
            let first = encoder.encode(&frame(64, 64, 0x20)).unwrap();
            assert_eq!(
                first.data.get(..4),
                Some(&super::super::ANNEX_B_START_CODE[..]),
                "the first frame does not begin with an Annex-B start code"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failed assumption must fail the test")]

    use super::*;

    /// One AVCC NAL: a four-byte big-endian length, then the unit.
    fn avcc_nal(header: u8, body: &[u8]) -> Vec<u8> {
        let length = u32::try_from(body.len() + 1).unwrap();
        let mut out = length.to_be_bytes().to_vec();
        out.push(header);
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn an_avcc_access_unit_becomes_annex_b() {
        let mut avcc = avcc_nal(0x41, &[0xAA, 0xBB]); // non-IDR slice
        avcc.extend_from_slice(&avcc_nal(0x41, &[0xCC]));

        let (annex_b, keyframe) = avcc_to_annex_b(&avcc, 4).unwrap();

        assert!(!keyframe, "no IDR was present");
        assert_eq!(
            annex_b,
            vec![
                0x00, 0x00, 0x00, 0x01, 0x41, 0xAA, 0xBB, //
                0x00, 0x00, 0x00, 0x01, 0x41, 0xCC,
            ]
        );
    }

    #[test]
    fn an_idr_slice_is_recognized_as_a_keyframe() {
        let avcc = avcc_nal(0x65, &[0xAA]); // NAL type 5 = IDR
        let (annex_b, keyframe) = avcc_to_annex_b(&avcc, 4).unwrap();
        assert!(keyframe);
        assert_eq!(annex_b, vec![0x00, 0x00, 0x00, 0x01, 0x65, 0xAA]);
    }

    /// The prefix width comes from the format description, not from an
    /// assumption: `CMVideoFormatDescriptionGetH264ParameterSetAtIndex`
    /// reports it, and reading a two-byte stream as four-byte is how a whole
    /// access unit turns into one enormous bogus NAL.
    #[test]
    fn a_narrower_length_prefix_is_honoured() {
        let avcc = [0x00, 0x03, 0x65, 0xAA, 0xBB];
        let (annex_b, keyframe) = avcc_to_annex_b(&avcc, 2).unwrap();
        assert!(keyframe);
        assert_eq!(annex_b, vec![0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB]);
    }

    #[test]
    fn an_empty_sample_converts_to_nothing_rather_than_failing() {
        let (annex_b, keyframe) = avcc_to_annex_b(&[], 4).unwrap();
        assert!(annex_b.is_empty());
        assert!(!keyframe);
    }

    #[test]
    fn a_nal_longer_than_the_sample_is_refused_rather_than_panicking() {
        // Declares 16 bytes of NAL and provides two.
        let avcc = [0x00, 0x00, 0x00, 0x10, 0x65, 0xAA];
        assert!(matches!(
            avcc_to_annex_b(&avcc, 4),
            Err(MediaError::Encode(_))
        ));
    }

    #[test]
    fn a_truncated_length_prefix_is_refused_rather_than_panicking() {
        let avcc = [0x00, 0x00];
        assert!(matches!(
            avcc_to_annex_b(&avcc, 4),
            Err(MediaError::Encode(_))
        ));
    }

    #[test]
    fn a_zero_length_nal_is_refused_rather_than_looping_forever() {
        let avcc = [0x00, 0x00, 0x00, 0x00, 0x65];
        assert!(matches!(
            avcc_to_annex_b(&avcc, 4),
            Err(MediaError::Encode(_))
        ));
    }

    #[test]
    fn an_impossible_length_prefix_width_is_refused() {
        let avcc = [0x00, 0x00, 0x00, 0x01, 0x65];
        assert!(matches!(
            avcc_to_annex_b(&avcc, 0),
            Err(MediaError::Encode(_))
        ));
        assert!(matches!(
            avcc_to_annex_b(&avcc, 8),
            Err(MediaError::Encode(_))
        ));
    }
}
