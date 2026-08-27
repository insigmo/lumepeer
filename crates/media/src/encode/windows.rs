//! Windows Media Foundation hardware H.264 encoder (design doc §5.1, §11,
//! §18/§19 phase 4; ADR 0011).
//!
//! Hardware H.264 encoder MFTs (Intel Quick Sync, NVENC, AMD AMF, all exposed
//! through Media Foundation) are documented by Microsoft as always
//! asynchronous, unlike the synchronous software `CLSID_CMSH264EncoderMFT`
//! that `MFT_ENUM_FLAG_HARDWARE` filters out. This module drives the async
//! protocol: unlock `MF_TRANSFORM_ASYNC`, then feed `ProcessInput` and drain
//! `ProcessOutput` only in response to `METransformNeedInput`/
//! `METransformHaveOutput` events from the transform's
//! `IMFMediaEventGenerator`, rather than calling them blindly. It also
//! tolerates a synchronous transform (no `MF_TRANSFORM_ASYNC` attribute) by
//! skipping the event wait, in case some driver ever registers one as
//! hardware without the async requirement.
//!
//! Real dimensions are not known until the first captured [`Frame`], the same
//! way `EncoderConfig` carries no width/height for the `openh264` fallback.
//! Construction (and [`probe_hardware`](super::probe_hardware)) negotiate
//! Media Foundation's input/output types at [`PROBE_WIDTH`]x[`PROBE_HEIGHT`]
//! to prove the transform is genuinely usable, not just enumerable; `encode`
//! renegotiates at the real size on the first frame and again on any later
//! resolution change.
//!
//! Bitrate changes rebuild the negotiated types rather than going through
//! `ICodecAPI`, the same trade-off ADR 0005 accepted for the `openh264`
//! fallback: `MF_MT_AVG_BITRATE` is read once at `SetOutputType` time by most
//! encoder MFTs, so a genuinely live change needs `ICodecAPI::SetValue` with a
//! `VARIANT`, which this module does not build. Rebuilding costs one
//! keyframe, which is acceptable at `ABR_ADJUST_MAX_RATE_PER_SEC` = 1/sec.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use lumepeer_core::constants::ENCODE_HW_EVENT_TIMEOUT_MS;
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncVideoForceKeyFrame, ICodecAPI, IMFActivate, IMFMediaEventGenerator, IMFMediaType,
    IMFSample, IMFTransform, METransformDrainComplete, METransformHaveOutput, METransformNeedInput,
    MF_E_NO_EVENTS_AVAILABLE, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
    MF_EVENT_FLAG_NO_WAIT, MF_EVENT_TYPE, MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK,
    MFCreateAlignedMemoryBuffer, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFMediaType_Video, MFSTARTUP_NOSOCKET, MFSampleExtension_CleanPoint, MFShutdown, MFStartup,
    MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Variant::VARIANT;
use windows::core::Interface as _;

use super::{EncodedFrame, EncoderConfig, EncoderKind, VideoCodec, VideoEncoder};
use crate::capture::{Frame, PixelFormat};
use crate::error::{MediaError, Result};

/// Bits per kilobit, for the kbps of §14 against the bps of `MF_MT_AVG_BITRATE`.
const BITS_PER_KBIT: u32 = 1_000;
/// Probe/initial negotiation size. Real dimensions arrive with the first
/// frame (`EncoderConfig` carries none, matching the `openh264` fallback);
/// this is small enough that any genuine hardware H.264 encoder MFT accepts
/// it, so a successful negotiation here is real evidence of usability rather
/// than a guess.
const PROBE_WIDTH: u32 = 64;
/// See [`PROBE_WIDTH`].
const PROBE_HEIGHT: u32 = 64;
/// Busy-poll granularity while waiting for an async MFT event. Small enough
/// to keep p50 latency negligible; the overall wait is still bounded by
/// `ENCODE_HW_EVENT_TIMEOUT_MS`.
const EVENT_POLL_INTERVAL_MS: u64 = 2;

/// Whether a genuinely usable hardware H.264 encoder MFT is available right
/// now (§18, ADR 0011). Runs the exact same activation and type negotiation
/// [`MediaFoundationEncoder::new`] would use, so this cannot claim
/// availability that construction then fails to back up.
pub(super) fn hardware_h264_available(config: EncoderConfig) -> bool {
    activate_hardware_transform(PROBE_WIDTH, PROBE_HEIGHT, config).is_ok()
}

/// Hardware H.264 encoder backed by a Media Foundation MFT.
pub struct MediaFoundationEncoder {
    transform: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    config: EncoderConfig,
    dims: (u32, u32),
    // Keeps `MFStartup`/`MFShutdown` balanced for as long as `transform` (and
    // any COM object it produced) is alive. Order matters: this must drop
    // after `transform`, which Rust guarantees by declaration order.
    mf: MfRuntime,
}

// SAFETY: `MediaFoundationEncoder` wraps Media Foundation COM interfaces,
// which `windows-rs` does not mark `Send` by default because arbitrary COM
// objects may be apartment-affine. Hardware H.264 encoder MFTs are
// documented by Microsoft as free-threaded ("agile") specifically so the
// Media Session's work-queue threads can drive them from whichever thread is
// convenient; this module always initializes COM as the multithreaded
// apartment (`COINIT_MULTITHREADED`, see `ensure_com_initialized`) rather
// than a single-threaded one, and every entry point below (`encode`,
// `set_bitrate`) re-asserts MTA membership on whichever thread calls it
// before touching the transform, so a `Send` hand-off to a different thread
// never leaves that thread outside the MTA when it makes its first COM call.
unsafe impl Send for MediaFoundationEncoder {}

// Mirrors `OpenH264Encoder`'s `Debug` impl: the COM state is not printable
// and must never be logged, only the settings that already matter for logs.
impl std::fmt::Debug for MediaFoundationEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaFoundationEncoder")
            .field("config", &self.config)
            .field("dims", &self.dims)
            .finish_non_exhaustive()
    }
}

impl MediaFoundationEncoder {
    /// Activates a hardware H.264 encoder MFT and negotiates types at
    /// [`PROBE_WIDTH`]x[`PROBE_HEIGHT`]; `encode` renegotiates at the real
    /// frame size on first use.
    ///
    /// # Errors
    /// [`MediaError::EncoderUnavailable`] if no hardware H.264 encoder MFT is
    /// available and usable, or if `config.codec` is not H.264 (AV1 hardware
    /// is not implemented by this backend).
    pub fn new(config: EncoderConfig) -> Result<Self> {
        if config.codec != VideoCodec::H264 {
            return Err(MediaError::EncoderUnavailable(
                "the Media Foundation hardware backend only implements H.264".to_owned(),
            ));
        }
        let (mf, transform, events) =
            activate_hardware_transform(PROBE_WIDTH, PROBE_HEIGHT, config)?;
        Ok(Self {
            transform,
            events,
            config,
            dims: (PROBE_WIDTH, PROBE_HEIGHT),
            mf,
        })
    }

    /// Activates a fresh transform at the new frame size, the same
    /// "next frame is a keyframe" trade-off a bitrate change makes.
    ///
    /// Not every hardware encoder MFT supports changing the negotiated frame
    /// size on a live transform - Media Foundation's dynamic format change
    /// (`MF_E_TRANSFORM_STREAM_CHANGE`) is driver-initiated, not something an
    /// app can force onto an established session by calling `SetOutputType`
    /// again (confirmed empirically: doing exactly that fails with a bare
    /// `E_FAIL` on real hardware here). A resolution change is a rare event
    /// (a screen resolution change, not a per-frame one), so re-activating is
    /// simpler and more portable than depending on each driver's support, or
    /// lack of it, for dynamic reconfiguration.
    fn reconfigure(&mut self, width: u32, height: u32) -> Result<()> {
        let (mf, transform, events) = activate_hardware_transform(width, height, self.config)?;
        self.transform = transform;
        self.events = events;
        self.mf = mf;
        self.dims = (width, height);
        Ok(())
    }

    /// Puts the transform back into its streaming state after the per-frame
    /// `MFT_MESSAGE_COMMAND_DRAIN` above.
    ///
    /// A drain is not a pause: MSDN's "Basic MFT Processing Model" ends the
    /// current stream with it, and an asynchronous MFT stops raising
    /// `METransformNeedInput` until the client starts a new one. Without this
    /// the *second* `encode()` of a session waits for an input request that
    /// never comes and fails on `ENCODE_HW_EVENT_TIMEOUT_MS` - one picture
    /// reaches the guest and the view then sits on "waiting for the remote
    /// screen" for the rest of the session (§18).
    fn restart_after_drain(&self) -> Result<()> {
        // The drain finishes with METransformDrainComplete; starting the next
        // stream before it lands is undefined for the driver, and the wait is
        // effectively free because the output this frame owns has already
        // been read by the time it runs.
        if let Some(events) = &self.events {
            wait_for_event(events, METransformDrainComplete)?;
        }
        // SAFETY: ProcessMessage with a message type that takes no pointer
        // parameter.
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
        }
        .map_err(|e| MediaError::Encode(format!("START_OF_STREAM refused after a drain: {e}")))
    }
}

impl VideoEncoder for MediaFoundationEncoder {
    fn encode(&mut self, frame: &Frame) -> Result<EncodedFrame> {
        // Re-asserts MTA membership on whatever thread calls this; see the
        // `unsafe impl Send` note above. Cheap and idempotent once the
        // calling thread has already joined.
        ensure_com_initialized()?;

        let (nv12, width, height) = bgra_to_nv12(frame)?;
        if self.dims != (width, height) {
            self.reconfigure(width, height)?;
        }

        let sample = build_input_sample(&nv12, self.config.fps, frame.timestamp_us)?;

        if let Some(events) = &self.events {
            wait_for_event(events, METransformNeedInput)?;
        }
        // SAFETY: `sample` wraps a single contiguous NV12 buffer sized to
        // match the input type just negotiated by `reconfigure`/`new`;
        // `ProcessInput` only reads it.
        unsafe { self.transform.ProcessInput(0, &sample, 0) }
            .map_err(|e| MediaError::Encode(format!("ProcessInput failed: {e}")))?;

        // Hardware encoder MFTs commonly hold more than one frame of internal
        // pipeline depth (rate-control lookahead, reordering) before they
        // will emit output on their own, which the trait's one-in/one-out
        // contract has no way to feed. DRAIN tells the transform to flush
        // whatever it can produce from the input queued so far rather than
        // waiting for enough further input to fill that pipeline (MSDN
        // "Basic MFT Processing Model": DRAIN is exactly this operation, not
        // a shutdown signal).
        // SAFETY: ProcessMessage with a message type that takes no pointer
        // parameter.
        unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) }
            .map_err(|e| MediaError::Encode(format!("DRAIN refused: {e}")))?;

        loop {
            if let Some(events) = &self.events {
                wait_for_event(events, METransformHaveOutput)?;
            }
            match drain_output(&self.transform)? {
                DrainResult::Frame(mut encoded) => {
                    encoded.timestamp_us = frame.timestamp_us;
                    self.restart_after_drain()?;
                    return Ok(encoded);
                }
                DrainResult::NeedMoreInput => {
                    if self.events.is_none() {
                        // Synchronous transform: no output for this input is
                        // an honest failure of the 1-in/1-out contract this
                        // trait promises, not something to retry forever.
                        return Err(MediaError::Encode(
                            "hardware encoder produced no output for this frame".to_owned(),
                        ));
                    }
                    // Async: a stray NeedInput/HaveOutput interleaving is
                    // normal; keep waiting for the HaveOutput this frame owns.
                }
                DrainResult::StreamChanged => {
                    negotiate_types(
                        &self.transform,
                        width,
                        height,
                        self.config.fps,
                        self.config.bitrate_kbps,
                    )?;
                }
            }
        }
    }

    fn request_keyframe(&mut self) -> Result<()> {
        // See `encode`: re-asserts MTA membership on whatever thread calls
        // this before touching the transform.
        ensure_com_initialized()?;
        // `ICodecAPI` is the documented way to ask an encoder MFT for an IDR
        // (MSDN, `CODECAPI_AVEncVideoForceKeyFrame`): it takes effect on the
        // next frame submitted and clears itself afterwards, which is exactly
        // the "at the next opportunity" the request means.
        let codec: ICodecAPI = self.transform.cast().map_err(|e| {
            MediaError::Encode(format!("this encoder MFT exposes no ICodecAPI: {e}"))
        })?;
        let force = VARIANT::from(true);
        // SAFETY: both arguments are locals that outlive the call, and
        // `SetValue` only reads them.
        unsafe { codec.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &raw const force) }
            .map_err(|e| MediaError::Encode(format!("forcing a keyframe was refused: {e}")))
    }

    fn set_bitrate(&mut self, bitrate_kbps: u32) -> Result<()> {
        if bitrate_kbps == self.config.bitrate_kbps {
            return Ok(());
        }
        // See `encode`: re-asserts MTA membership on whatever thread calls
        // this before touching the transform.
        ensure_com_initialized()?;
        let new_config = EncoderConfig {
            bitrate_kbps,
            ..self.config
        };
        negotiate_types(
            &self.transform,
            self.dims.0,
            self.dims.1,
            new_config.fps,
            bitrate_kbps,
        )?;
        start_streaming(&self.transform)?;
        self.config = new_config;
        Ok(())
    }

    fn kind(&self) -> EncoderKind {
        EncoderKind::Hardware
    }
}

/// Process-wide `MFStartup`/`MFShutdown` pairing. Unlike a COM apartment,
/// Media Foundation's platform state is not thread-affine (MSDN: `MFStartup`/
/// `MFShutdown` may run on any thread), so a simple refcounted RAII guard is
/// sound even though the encoder that owns it may be dropped on a different
/// thread than the one that created it.
static MF_REFCOUNT: Mutex<u32> = Mutex::new(0);

struct MfRuntime;

impl MfRuntime {
    fn acquire() -> Result<Self> {
        let mut count = MF_REFCOUNT.lock().map_err(|_| {
            MediaError::EncoderUnavailable("Media Foundation refcount lock was poisoned".to_owned())
        })?;
        if *count == 0 {
            // SAFETY: MFStartup has no preconditions of its own beyond COM
            // being initialized on the calling thread, which every caller of
            // `activate_hardware_transform` guarantees via
            // `ensure_com_initialized` before this runs.
            unsafe { MFStartup(mf_version(), MFSTARTUP_NOSOCKET) }
                .map_err(|e| MediaError::EncoderUnavailable(format!("MFStartup failed: {e}")))?;
        }
        *count += 1;
        Ok(Self)
    }
}

impl Drop for MfRuntime {
    fn drop(&mut self) {
        let Ok(mut count) = MF_REFCOUNT.lock() else {
            // Poisoned: another thread panicked while holding this lock.
            // There is nothing safe left to do but leak the MFStartup
            // refcount rather than risk a double MFShutdown.
            return;
        };
        *count = count.saturating_sub(1);
        if *count == 0 {
            // SAFETY: balances the MFStartup call in `acquire`. Every live
            // `MediaFoundationEncoder` and probe call holds one `MfRuntime`,
            // so this only runs once the last one has already released every
            // Media Foundation COM object it created (Rust drop order runs
            // `transform`/`events` before this field, see the struct's field
            // order comment).
            let _ = unsafe { MFShutdown() };
        }
    }
}

/// `MF_VERSION` is a linked constant in the `windows` crate's Media
/// Foundation module; wrapped so the single call site above stays readable.
fn mf_version() -> u32 {
    windows::Win32::Media::MediaFoundation::MF_VERSION
}

/// Joins the calling thread to the multithreaded COM apartment (MTA) for
/// Media Foundation's COM calls. Idempotent and cheap to call on every entry
/// point (see the `unsafe impl Send for MediaFoundationEncoder` note): the
/// underlying `CoInitializeEx` call is a simple per-thread refcount bump when
/// the thread has already joined.
///
/// Deliberately never paired with `CoUninitialize`: COM apartment membership
/// is per-OS-thread, but the [`VideoEncoder`] trait only requires `Send`, not
/// that the same thread that constructs an encoder also drops it, so there is
/// no single point at which it would be correct to leave the apartment.
/// Leaving a thread's MTA membership in place until the thread exits is
/// documented as safe and is harmless for the long-lived capture/encode
/// worker threads this runs on.
fn ensure_com_initialized() -> Result<()> {
    // SAFETY: CoInitializeEx has no preconditions beyond `pvReserved` being
    // null, which `None` satisfies.
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr.is_ok() || hr == RPC_E_CHANGED_MODE {
        // S_OK: this call joined the MTA. S_FALSE (also `is_ok`): already an
        // MTA member. RPC_E_CHANGED_MODE: this thread already joined a
        // single-threaded apartment (e.g. a UI thread) before this ran; that
        // thread must not be the one driving this encoder, but detecting
        // that is a caller-discipline problem this module cannot see from
        // here, so it is surfaced structurally instead: an STA thread that
        // tries to touch the transform directly will get COM errors from the
        // calls themselves rather than silently corrupting state.
        Ok(())
    } else {
        Err(MediaError::EncoderUnavailable(format!(
            "CoInitializeEx failed: {hr}"
        )))
    }
}

/// Enumerates hardware H.264 encoder MFTs, activates the first one that
/// accepts NV12 input / H.264 output at `width`x`height`, and starts
/// streaming. Returns the [`MfRuntime`] guard alongside so the caller can
/// keep `MFStartup` balanced for as long as the transform lives.
fn activate_hardware_transform(
    width: u32,
    height: u32,
    config: EncoderConfig,
) -> Result<(MfRuntime, IMFTransform, Option<IMFMediaEventGenerator>)> {
    let mf = MfRuntime::acquire()?;
    ensure_com_initialized()?;

    let input_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };

    for activate in enum_hardware_encoders(&input_info, &output_info)? {
        match try_activate_one(&activate, width, height, config) {
            Ok((transform, events)) => return Ok((mf, transform, events)),
            Err(_) => {
                // SAFETY: ShutdownObject releases the MFT this Activate
                // stands for so the next candidate is not starved of the
                // same hardware context; best-effort, `activate` is dropped
                // (and Released) either way once this loop iteration ends.
                let _ = unsafe { activate.ShutdownObject() };
            }
        }
    }
    Err(MediaError::EncoderUnavailable(
        "no usable hardware H.264 encoder MFT is registered on this system".to_owned(),
    ))
}

/// Tries to activate and fully configure one candidate MFT. Any failure at
/// any step means this candidate is not usable; the caller moves on to the
/// next one rather than reporting hardware as available on a hope.
fn try_activate_one(
    activate: &IMFActivate,
    width: u32,
    height: u32,
    config: EncoderConfig,
) -> Result<(IMFTransform, Option<IMFMediaEventGenerator>)> {
    // SAFETY: ActivateObject creates the MFT this IMFActivate describes and
    // hands back an owned interface pointer on success.
    let transform: IMFTransform = unsafe { activate.ActivateObject() }
        .map_err(|e| MediaError::EncoderUnavailable(format!("ActivateObject failed: {e}")))?;

    let is_async = transform_is_async(&transform);
    if is_async {
        unlock_async(&transform)?;
    }

    negotiate_types(&transform, width, height, config.fps, config.bitrate_kbps)?;
    start_streaming(&transform)?;

    let events = if is_async {
        Some(
            transform
                .cast::<IMFMediaEventGenerator>()
                .map_err(|e| MediaError::EncoderUnavailable(format!("no event generator: {e}")))?,
        )
    } else {
        None
    };

    Ok((transform, events))
}

/// Enumerates hardware-accelerated video encoder MFTs matching `input`/
/// `output`. Returns an empty list rather than an error when Media
/// Foundation simply has none registered; a hard error is reserved for
/// `MFTEnumEx` itself failing.
fn enum_hardware_encoders(
    input: &MFT_REGISTER_TYPE_INFO,
    output: &MFT_REGISTER_TYPE_INFO,
) -> Result<Vec<IMFActivate>> {
    let mut activates_ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count: u32 = 0;
    // SAFETY: MFTEnumEx, on success, writes a CoTaskMemAlloc'd array of
    // `count` `Option<IMFActivate>` slots into `activates_ptr`; ownership of
    // that allocation (and of every non-`None` slot's COM reference) passes
    // to this function.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(std::ptr::from_ref(input)),
            Some(std::ptr::from_ref(output)),
            &raw mut activates_ptr,
            &raw mut count,
        )
    }
    .map_err(|e| MediaError::EncoderUnavailable(format!("MFTEnumEx failed: {e}")))?;

    if activates_ptr.is_null() || count == 0 {
        return Ok(Vec::new());
    }

    // SAFETY: `activates_ptr` points to exactly `count` valid
    // `Option<IMFActivate>` slots written by MFTEnumEx above. `Option::take`
    // moves each present value out without touching its refcount, leaving
    // `None` behind; `CoTaskMemFree` then frees only the array's own backing
    // memory (not the objects, which are now owned by `out`).
    let activates = unsafe {
        let slots = std::slice::from_raw_parts_mut(activates_ptr, count as usize);
        let out: Vec<IMFActivate> = slots.iter_mut().filter_map(Option::take).collect();
        CoTaskMemFree(Some(activates_ptr.cast()));
        out
    };
    Ok(activates)
}

/// Whether `transform` is an asynchronous MFT (MSDN: all hardware MFTs are).
fn transform_is_async(transform: &IMFTransform) -> bool {
    // SAFETY: GetAttributes/GetUINT32 only read the transform's own
    // attribute store.
    unsafe {
        transform
            .GetAttributes()
            .and_then(|attrs| attrs.GetUINT32(&MF_TRANSFORM_ASYNC))
            .is_ok_and(|v| v != 0)
    }
}

/// Opts into driving an async MFT directly (MSDN: required before any other
/// call on one) rather than through the full Media Session pipeline.
fn unlock_async(transform: &IMFTransform) -> Result<()> {
    // SAFETY: GetAttributes/SetUINT32 only touch the transform's own
    // attribute store.
    unsafe {
        let attrs = transform
            .GetAttributes()
            .map_err(|e| MediaError::EncoderUnavailable(format!("GetAttributes failed: {e}")))?;
        attrs
            .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
            .map_err(|e| MediaError::EncoderUnavailable(format!("async unlock refused: {e}")))
    }
}

/// Sets the output type (H.264/bitrate) first, then the input type
/// (NV12/size), the order Microsoft's own hardware encoder samples use:
/// until the output type is set, `GetInputAvailableType` on an encoder MFT
/// can fail with `MF_E_TRANSFORM_TYPE_NOT_SET`.
fn negotiate_types(
    transform: &IMFTransform,
    width: u32,
    height: u32,
    fps: u8,
    bitrate_kbps: u32,
) -> Result<()> {
    let output_type = build_output_type(width, height, fps, bitrate_kbps)?;
    // SAFETY: SetOutputType takes a reference to a media type this function
    // owns; `dwflags = 0` commits it rather than merely testing it.
    unsafe { transform.SetOutputType(0, &output_type, 0) }
        .map_err(|e| MediaError::EncoderUnavailable(format!("SetOutputType refused: {e}")))?;

    let input_type = build_input_type(width, height, fps)?;
    // SAFETY: same as above, for the input side.
    unsafe { transform.SetInputType(0, &input_type, 0) }
        .map_err(|e| MediaError::EncoderUnavailable(format!("SetInputType refused: {e}")))?;
    Ok(())
}

fn build_output_type(width: u32, height: u32, fps: u8, bitrate_kbps: u32) -> Result<IMFMediaType> {
    // SAFETY: MFCreateMediaType and the attribute setters below are COM
    // calls on a media type this function owns exclusively until it returns
    // it to the caller.
    let media_type = unsafe { MFCreateMediaType() }
        .map_err(|e| MediaError::EncoderUnavailable(format!("MFCreateMediaType failed: {e}")))?;
    unsafe {
        media_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| MediaError::EncoderUnavailable(e.to_string()))?;
        media_type
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
            .map_err(|e| MediaError::EncoderUnavailable(e.to_string()))?;
        media_type
            .SetUINT32(
                &MF_MT_AVG_BITRATE,
                bitrate_kbps.saturating_mul(BITS_PER_KBIT),
            )
            .map_err(|e| MediaError::EncoderUnavailable(e.to_string()))?;
        media_type
            .SetUINT32(&MF_MT_INTERLACE_MODE, interlace_progressive())
            .map_err(|e| MediaError::EncoderUnavailable(e.to_string()))?;
    }
    set_frame_size(&media_type, width, height)?;
    set_frame_rate(&media_type, fps)?;
    set_pixel_aspect_ratio(&media_type)?;
    Ok(media_type)
}

fn build_input_type(width: u32, height: u32, fps: u8) -> Result<IMFMediaType> {
    // SAFETY: see `build_output_type`; same ownership shape.
    let media_type = unsafe { MFCreateMediaType() }
        .map_err(|e| MediaError::EncoderUnavailable(format!("MFCreateMediaType failed: {e}")))?;
    unsafe {
        media_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| MediaError::EncoderUnavailable(e.to_string()))?;
        media_type
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
            .map_err(|e| MediaError::EncoderUnavailable(e.to_string()))?;
        media_type
            .SetUINT32(&MF_MT_INTERLACE_MODE, interlace_progressive())
            .map_err(|e| MediaError::EncoderUnavailable(e.to_string()))?;
        media_type
            .SetUINT32(&MF_MT_DEFAULT_STRIDE, width)
            .map_err(|e| MediaError::EncoderUnavailable(e.to_string()))?;
    }
    set_frame_size(&media_type, width, height)?;
    set_frame_rate(&media_type, fps)?;
    set_pixel_aspect_ratio(&media_type)?;
    Ok(media_type)
}

/// `MFVideoInterlace_Progressive` as the `u32` `MF_MT_INTERLACE_MODE` wants;
/// the constant is a known-small, always-non-negative enum value from the
/// `windows` crate, not attacker-controlled data.
#[allow(
    clippy::cast_sign_loss,
    reason = "MFVideoInterlace_Progressive is a fixed, non-negative platform enum constant"
)]
fn interlace_progressive() -> u32 {
    MFVideoInterlace_Progressive.0 as u32
}

fn set_frame_size(media_type: &IMFMediaType, width: u32, height: u32) -> Result<()> {
    // SAFETY: SetUINT64 only writes an attribute on a media type this
    // function's caller owns.
    unsafe { media_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(width, height)) }
        .map_err(|e| MediaError::EncoderUnavailable(format!("MF_MT_FRAME_SIZE refused: {e}")))
}

fn set_frame_rate(media_type: &IMFMediaType, fps: u8) -> Result<()> {
    // SAFETY: see `set_frame_size`.
    unsafe { media_type.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(u32::from(fps), 1)) }
        .map_err(|e| MediaError::EncoderUnavailable(format!("MF_MT_FRAME_RATE refused: {e}")))
}

fn set_pixel_aspect_ratio(media_type: &IMFMediaType) -> Result<()> {
    // SAFETY: see `set_frame_size`.
    unsafe { media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u64(1, 1)) }.map_err(|e| {
        MediaError::EncoderUnavailable(format!("MF_MT_PIXEL_ASPECT_RATIO refused: {e}"))
    })
}

/// Packs the high/low halves of a Media Foundation "packed 64-bit" attribute
/// (`MF_MT_FRAME_SIZE`, `MF_MT_FRAME_RATE`, `MF_MT_PIXEL_ASPECT_RATIO`).
const fn pack_u64(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | (low as u64)
}

/// Sends the documented MFT startup sequence (MSDN "Basic MFT Processing
/// Model"): flush any stale state, then announce streaming is about to
/// begin.
fn start_streaming(transform: &IMFTransform) -> Result<()> {
    // SAFETY: ProcessMessage with these message types takes no pointer
    // arguments beyond the message/param themselves.
    unsafe {
        transform
            .ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)
            .map_err(|e| MediaError::EncoderUnavailable(format!("FLUSH refused: {e}")))?;
        transform
            .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
            .map_err(|e| MediaError::EncoderUnavailable(format!("BEGIN_STREAMING refused: {e}")))?;
        transform
            .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
            .map_err(|e| MediaError::EncoderUnavailable(format!("START_OF_STREAM refused: {e}")))
    }
}

/// Builds one input sample wrapping `nv12` in a single contiguous buffer.
fn build_input_sample(nv12: &[u8], fps: u8, timestamp_us: u64) -> Result<IMFSample> {
    let len = u32::try_from(nv12.len()).map_err(|_| {
        MediaError::Encode("frame is larger than Media Foundation can address".to_owned())
    })?;

    // SAFETY: MFCreateSample/MFCreateMemoryBuffer/Lock/Unlock/AddBuffer are
    // COM calls into mfplat.dll. `Lock` guarantees `ptr` is valid for at
    // least `len` bytes (the buffer was allocated with exactly that
    // capacity) until the matching `Unlock`, and `ptr` is checked non-null
    // before it is written through.
    unsafe {
        let sample = MFCreateSample()
            .map_err(|e| MediaError::Encode(format!("MFCreateSample failed: {e}")))?;
        let buffer = MFCreateMemoryBuffer(len)
            .map_err(|e| MediaError::Encode(format!("MFCreateMemoryBuffer failed: {e}")))?;

        let mut ptr: *mut u8 = std::ptr::null_mut();
        buffer
            .Lock(&raw mut ptr, None, None)
            .map_err(|e| MediaError::Encode(format!("input buffer Lock failed: {e}")))?;
        if ptr.is_null() {
            let _ = buffer.Unlock();
            return Err(MediaError::Encode(
                "hardware encoder returned a null input buffer".to_owned(),
            ));
        }
        std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr, nv12.len());
        buffer
            .Unlock()
            .map_err(|e| MediaError::Encode(format!("input buffer Unlock failed: {e}")))?;
        buffer
            .SetCurrentLength(len)
            .map_err(|e| MediaError::Encode(format!("SetCurrentLength failed: {e}")))?;

        sample
            .AddBuffer(&buffer)
            .map_err(|e| MediaError::Encode(format!("AddBuffer failed: {e}")))?;
        sample
            .SetSampleTime(hns_from_us(timestamp_us))
            .map_err(|e| MediaError::Encode(format!("SetSampleTime failed: {e}")))?;
        sample
            .SetSampleDuration(hns_per_frame(fps))
            .map_err(|e| MediaError::Encode(format!("SetSampleDuration failed: {e}")))?;
        Ok(sample)
    }
}

/// Microsoft's 100-nanosecond time unit, per second.
const HNS_PER_SEC: i64 = 10_000_000;

fn hns_per_frame(fps: u8) -> i64 {
    HNS_PER_SEC / i64::from(fps.max(1))
}

fn hns_from_us(timestamp_us: u64) -> i64 {
    // Saturate rather than wrap: a session runs far short of the ~29,000
    // years i64 hundred-nanoseconds would take to overflow from a
    // microsecond timestamp, so saturation only ever guards against a
    // corrupt input, never a real session.
    i64::try_from(timestamp_us.saturating_mul(10)).unwrap_or(i64::MAX)
}

/// Outcome of one `ProcessOutput` attempt.
enum DrainResult {
    /// A complete encoded frame was produced.
    Frame(EncodedFrame),
    /// The transform needs another `ProcessInput` before it has output.
    NeedMoreInput,
    /// The output type changed; the caller must renegotiate and retry.
    StreamChanged,
}

/// Calls `ProcessOutput` once and extracts a frame, allocating the output
/// sample ourselves unless the transform provides its own (MSDN: check
/// `MFT_OUTPUT_STREAM_PROVIDES_SAMPLES` on `GetOutputStreamInfo` first).
fn drain_output(transform: &IMFTransform) -> Result<DrainResult> {
    // SAFETY: GetOutputStreamInfo only reads transform state into a plain
    // `#[repr(C)]` struct.
    let stream_info = unsafe { transform.GetOutputStreamInfo(0) }
        .map_err(|e| MediaError::Encode(format!("GetOutputStreamInfo failed: {e}")))?;
    let provides_samples =
        stream_info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0.cast_unsigned() != 0;

    let own_sample = if provides_samples {
        None
    } else {
        Some(allocate_output_sample(
            stream_info.cbSize,
            stream_info.cbAlignment,
        )?)
    };

    let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
        dwStreamID: 0,
        pSample: std::mem::ManuallyDrop::new(own_sample),
        dwStatus: 0,
        pEvents: std::mem::ManuallyDrop::new(None),
    }];
    let mut status = 0u32;

    // SAFETY: `buffers` has exactly one entry for stream 0, the only output
    // stream this module ever configures (`GetOutputStreamInfo(0)` above).
    let outcome = unsafe { transform.ProcessOutput(0, &mut buffers, &raw mut status) };

    // Always reclaim ownership of the `ManuallyDrop` fields so the COM
    // references they hold are released exactly once, regardless of whether
    // `ProcessOutput` succeeded, failed, or replaced our sample with its own.
    let sample = std::mem::ManuallyDrop::into_inner(std::mem::replace(
        &mut buffers[0].pSample,
        std::mem::ManuallyDrop::new(None),
    ));
    drop(std::mem::ManuallyDrop::into_inner(std::mem::replace(
        &mut buffers[0].pEvents,
        std::mem::ManuallyDrop::new(None),
    )));

    match outcome {
        Ok(()) => {
            let sample = sample.ok_or_else(|| {
                MediaError::Encode(
                    "hardware encoder reported success with no output sample".to_owned(),
                )
            })?;
            Ok(DrainResult::Frame(sample_to_encoded_frame(&sample)?))
        }
        Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(DrainResult::NeedMoreInput),
        Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => Ok(DrainResult::StreamChanged),
        Err(e) => Err(MediaError::Encode(format!("ProcessOutput failed: {e}"))),
    }
}

fn allocate_output_sample(size: u32, alignment: u32) -> Result<IMFSample> {
    // SAFETY: MFCreateSample/MFCreate(Aligned)MemoryBuffer/AddBuffer are COM
    // calls into mfplat.dll; ownership of the returned interfaces transfers
    // to the caller on success. `alignment` is `MFT_OUTPUT_STREAM_INFO::
    // cbAlignment`, which MSDN documents as already being in the
    // `MFCreateAlignedMemoryBuffer` "alignment minus one" form, or 0 for "no
    // requirement".
    unsafe {
        let sample = MFCreateSample()
            .map_err(|e| MediaError::Encode(format!("MFCreateSample failed: {e}")))?;
        let buffer = if alignment > 0 {
            MFCreateAlignedMemoryBuffer(size, alignment)
        } else {
            MFCreateMemoryBuffer(size)
        }
        .map_err(|e| MediaError::Encode(format!("output buffer allocation failed: {e}")))?;
        sample
            .AddBuffer(&buffer)
            .map_err(|e| MediaError::Encode(format!("AddBuffer failed: {e}")))?;
        Ok(sample)
    }
}

fn sample_to_encoded_frame(sample: &IMFSample) -> Result<EncodedFrame> {
    // SAFETY: ConvertToContiguousBuffer/Lock/Unlock/GetUINT32 are COM calls
    // on a sample this function owns; `Lock` guarantees `ptr` is valid for
    // `current_len` bytes until the matching `Unlock`.
    unsafe {
        let buffer = sample
            .ConvertToContiguousBuffer()
            .map_err(|e| MediaError::Encode(format!("ConvertToContiguousBuffer failed: {e}")))?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut current_len: u32 = 0;
        buffer
            .Lock(&raw mut ptr, None, Some(&raw mut current_len))
            .map_err(|e| MediaError::Encode(format!("output buffer Lock failed: {e}")))?;
        let data = if ptr.is_null() || current_len == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(ptr, current_len as usize).to_vec()
        };
        buffer
            .Unlock()
            .map_err(|e| MediaError::Encode(format!("output buffer Unlock failed: {e}")))?;

        // `MFSampleExtension_CleanPoint` is the documented keyframe marker,
        // but not every driver sets it faithfully; cross-check the Annex-B
        // bitstream itself for an IDR NAL so a missing attribute cannot turn
        // a real keyframe into a false negative.
        let clean_point = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0);
        let keyframe = clean_point != 0 || bitstream_has_idr(&data);

        Ok(EncodedFrame {
            keyframe,
            timestamp_us: 0, // overwritten by the caller with the input frame's timestamp
            data,
        })
    }
}

/// Scans an Annex-B H.264 bitstream for an IDR slice NAL unit (type 5).
fn bitstream_has_idr(data: &[u8]) -> bool {
    let mut i = 0usize;
    while i + 3 <= data.len() {
        let start_code_len = if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            Some(3usize)
        } else if i + 4 <= data.len()
            && data[i] == 0
            && data[i + 1] == 0
            && data[i + 2] == 0
            && data[i + 3] == 1
        {
            Some(4usize)
        } else {
            None
        };
        match start_code_len {
            Some(len) => {
                let nal_start = i + len;
                if let Some(&header) = data.get(nal_start)
                    && header & 0x1F == 5
                {
                    return true;
                }
                i = nal_start.max(i + 1);
            }
            None => i += 1,
        }
    }
    false
}

/// Waits for `expected` on `events`, bounded by `ENCODE_HW_EVENT_TIMEOUT_MS`
/// so a stalled or crashed hardware encoder driver fails one `encode()` call
/// instead of hanging the session forever (§24.5, ADR 0011).
fn wait_for_event(events: &IMFMediaEventGenerator, expected: MF_EVENT_TYPE) -> Result<()> {
    let deadline = Instant::now() + Duration::from_millis(ENCODE_HW_EVENT_TIMEOUT_MS);
    loop {
        // SAFETY: GetEvent with MF_EVENT_FLAG_NO_WAIT returns immediately
        // with either an owned IMFMediaEvent or MF_E_NO_EVENTS_AVAILABLE.
        match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
            Ok(event) => {
                // SAFETY: GetType only reads the event's own type field.
                let ty = unsafe { event.GetType() }
                    .map_err(|e| MediaError::Encode(format!("event GetType failed: {e}")))?;
                if ty == expected.0.cast_unsigned() {
                    return Ok(());
                }
                // A different event than the one this call is waiting for
                // (e.g. a stray NeedInput while draining output) is not an
                // error; keep waiting for the one we asked for.
            }
            Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                if Instant::now() >= deadline {
                    return Err(MediaError::Encode(
                        "timed out waiting for the hardware encoder".to_owned(),
                    ));
                }
                std::thread::sleep(Duration::from_millis(EVENT_POLL_INTERVAL_MS));
            }
            Err(e) => return Err(MediaError::Encode(format!("GetEvent failed: {e}"))),
        }
    }
}

/// Converts a captured frame to NV12, cropping to even dimensions the same
/// way the `openh264` fallback's `even_bgra` does: 4:2:0 subsampling has no
/// odd rows or columns.
fn bgra_to_nv12(frame: &Frame) -> Result<(Vec<u8>, u32, u32)> {
    match frame.format {
        PixelFormat::Nv12 => nv12_passthrough(frame),
        PixelFormat::Bgra8 => bgra8_to_nv12(frame),
    }
}

fn nv12_size(width: u32, height: u32) -> usize {
    let w = width as usize;
    let h = height as usize;
    w * h + 2 * w.div_ceil(2) * h.div_ceil(2)
}

fn nv12_passthrough(frame: &Frame) -> Result<(Vec<u8>, u32, u32)> {
    let width = frame.width & !1;
    let height = frame.height & !1;
    if width == 0 || height == 0 {
        return Err(MediaError::Encode("frame is smaller than 2x2".to_owned()));
    }
    if width != frame.width || height != frame.height {
        // Cropping NV12 in place needs a plane-aware copy (the chroma plane
        // does not shrink the same way the luma plane does); reject rather
        // than silently miscropping chroma.
        return Err(MediaError::Encode(
            "odd-dimension NV12 input is not supported".to_owned(),
        ));
    }
    if frame.data.len() < nv12_size(width, height) {
        return Err(MediaError::Encode("NV12 frame buffer is short".to_owned()));
    }
    Ok((frame.data.clone(), width, height))
}

fn bgra8_to_nv12(frame: &Frame) -> Result<(Vec<u8>, u32, u32)> {
    let width_u32 = frame.width & !1;
    let height_u32 = frame.height & !1;
    let width = width_u32 as usize;
    let height = height_u32 as usize;
    if width == 0 || height == 0 {
        return Err(MediaError::Encode("frame is smaller than 2x2".to_owned()));
    }
    let src_stride = frame.width as usize * 4;
    if frame.data.len() < src_stride * height {
        return Err(MediaError::Encode("frame buffer is short".to_owned()));
    }

    let mut y_plane = vec![0u8; width * height];
    for row in 0..height {
        let row_start = row * src_stride;
        for col in 0..width {
            let px = row_start + col * 4;
            let (b, g, r) = (
                i32::from(frame.data[px]),
                i32::from(frame.data[px + 1]),
                i32::from(frame.data[px + 2]),
            );
            y_plane[row * width + col] = bt601_y(r, g, b);
        }
    }

    let uv_stride = width; // 2 bytes/sample pair * (width/2) samples
    let mut uv_plane = vec![0u8; uv_stride * (height / 2)];
    for block_row in 0..height / 2 {
        for block_col in 0..width / 2 {
            let mut sums = (0i32, 0i32, 0i32); // (r, g, b)
            for dy in 0..2 {
                for dx in 0..2 {
                    let row = block_row * 2 + dy;
                    let col = block_col * 2 + dx;
                    let px = row * src_stride + col * 4;
                    sums.2 += i32::from(frame.data[px]);
                    sums.1 += i32::from(frame.data[px + 1]);
                    sums.0 += i32::from(frame.data[px + 2]);
                }
            }
            let (r, g, b) = (sums.0 / 4, sums.1 / 4, sums.2 / 4);
            let uv_off = block_row * uv_stride + block_col * 2;
            uv_plane[uv_off] = bt601_u(r, g, b);
            uv_plane[uv_off + 1] = bt601_v(r, g, b);
        }
    }

    let mut out = Vec::with_capacity(y_plane.len() + uv_plane.len());
    out.extend_from_slice(&y_plane);
    out.extend_from_slice(&uv_plane);
    Ok((out, width_u32, height_u32))
}

fn clamp_u8(v: i32) -> u8 {
    // `clamp` provably puts `v` in `0..=255`, so this cannot actually fail;
    // `try_from` still expresses the narrowing honestly instead of an `as`
    // cast clippy cannot tell is safe.
    u8::try_from(v.clamp(0, 255)).unwrap_or(0)
}

/// ITU-R BT.601 fixed-point RGB-to-YUV (studio/limited range), the
/// conventional default for H.264 when no other range is negotiated.
fn bt601_y(r: i32, g: i32, b: i32) -> u8 {
    clamp_u8(((66 * r + 129 * g + 25 * b + 128) >> 8) + 16)
}
fn bt601_u(r: i32, g: i32, b: i32) -> u8 {
    clamp_u8(((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128)
}
fn bt601_v(r: i32, g: i32, b: i32) -> u8 {
    clamp_u8(((112 * r - 94 * g - 18 * b + 128) >> 8) + 128)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::capture::PixelFormat;

    fn frame(width: u32, height: u32, fill: u8) -> Frame {
        Frame {
            width,
            height,
            format: PixelFormat::Bgra8,
            timestamp_us: 0,
            data: vec![fill; (width as usize) * (height as usize) * 4],
        }
    }

    /// Builds an encoder only if hardware is genuinely available, so tests
    /// that need real hardware skip gracefully on a machine that has none
    /// rather than failing.
    fn try_new_encoder() -> Option<MediaFoundationEncoder> {
        if !hardware_h264_available(EncoderConfig::default()) {
            return None;
        }
        MediaFoundationEncoder::new(EncoderConfig::default()).ok()
    }

    #[test]
    fn probe_hardware_agrees_with_whether_construction_actually_works() {
        // The central "never claim hardware is available when it isn't"
        // requirement, checked mechanically rather than by inspection: this
        // runs identically whether or not the machine has a hardware H.264
        // encoder MFT, because it only asserts that the two answers match.
        let probed = hardware_h264_available(EncoderConfig::default());
        let constructed = MediaFoundationEncoder::new(EncoderConfig::default()).is_ok();
        assert_eq!(
            probed,
            constructed,
            "hardware_h264_available reported {probed} but construction {}",
            if constructed { "succeeded" } else { "failed" }
        );
    }

    #[test]
    fn av1_is_refused_regardless_of_hardware_availability() {
        let config = EncoderConfig {
            codec: VideoCodec::Av1,
            ..EncoderConfig::default()
        };
        assert!(matches!(
            MediaFoundationEncoder::new(config),
            Err(MediaError::EncoderUnavailable(_))
        ));
    }

    #[test]
    fn encodes_a_frame_and_starts_with_a_keyframe_when_hardware_is_available() {
        let Some(mut encoder) = try_new_encoder() else {
            eprintln!("skipping: no hardware H.264 encoder MFT on this machine");
            return;
        };
        let first = encoder.encode(&frame(64, 64, 0x20)).unwrap();
        assert!(first.keyframe, "the first frame must be decodable alone");
        assert!(!first.data.is_empty());
        assert_eq!(encoder.kind(), EncoderKind::Hardware);
    }

    #[test]
    fn a_session_keeps_encoding_past_the_first_frame_when_hardware_is_available() {
        // The encode loop of `apps/desktop/src/view.rs` calls `encode()` once
        // per captured frame on one long-lived encoder, so the very first
        // frame succeeding proves nothing on its own: an async MFT that is
        // left in its drained state after frame one stops asking for input
        // and every later frame fails with the event timeout, which reads to
        // the guest as "waiting for the remote screen" forever.
        let Some(mut encoder) = try_new_encoder() else {
            eprintln!("skipping: no hardware H.264 encoder MFT on this machine");
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

    #[test]
    fn odd_dimensions_are_cropped_rather_than_panicking_when_hardware_is_available() {
        let Some(mut encoder) = try_new_encoder() else {
            eprintln!("skipping: no hardware H.264 encoder MFT on this machine");
            return;
        };
        assert!(encoder.encode(&frame(65, 33, 0x40)).is_ok());
    }

    #[test]
    fn a_bitrate_change_is_accepted_when_hardware_is_available() {
        let Some(mut encoder) = try_new_encoder() else {
            eprintln!("skipping: no hardware H.264 encoder MFT on this machine");
            return;
        };
        encoder.encode(&frame(64, 64, 0x10)).unwrap();
        encoder.set_bitrate(1_500).unwrap();
        assert_eq!(encoder.config.bitrate_kbps, 1_500);
        assert!(
            !encoder
                .encode(&frame(64, 64, 0x80))
                .unwrap()
                .data
                .is_empty()
        );
    }

    #[test]
    fn bgra_to_nv12_produces_the_expected_plane_sizes() {
        let (nv12, width, height) = bgra_to_nv12(&frame(66, 34, 0x7f)).unwrap();
        assert_eq!((width, height), (66, 34));
        assert_eq!(nv12.len(), nv12_size(width, height));
    }

    #[test]
    fn bgra_to_nv12_crops_odd_dimensions_instead_of_panicking() {
        let (_, width, height) = bgra_to_nv12(&frame(65, 33, 0x10)).unwrap();
        assert_eq!((width, height), (64, 32));
    }

    #[test]
    fn white_converts_to_the_expected_nv12_neutral_chroma() {
        let (nv12, width, height) = bgra_to_nv12(&frame(2, 2, 0xff)).unwrap();
        // White: Y should land near the studio-range peak (235), and chroma
        // should be near neutral (128) since R=G=B.
        assert!(nv12[0] > 230, "luma sample was {}", nv12[0]);
        let uv_start = (width * height) as usize;
        assert!(
            (120..=136).contains(&nv12[uv_start]),
            "U sample was {}",
            nv12[uv_start]
        );
        assert!(
            (120..=136).contains(&nv12[uv_start + 1]),
            "V sample was {}",
            nv12[uv_start + 1]
        );
    }

    #[test]
    fn bitstream_has_idr_finds_a_type_5_nal_after_a_start_code() {
        let data = [0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB]; // NAL type 5 = IDR
        assert!(bitstream_has_idr(&data));
    }

    #[test]
    fn bitstream_has_idr_is_false_for_non_idr_nals() {
        let data = [0x00, 0x00, 0x01, 0x41, 0xAA, 0xBB]; // NAL type 1 = non-IDR slice
        assert!(!bitstream_has_idr(&data));
    }
}
