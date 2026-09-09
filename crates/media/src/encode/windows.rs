//! Windows Media Foundation hardware video encoder (design doc §5.1, §11,
//! §18/§19 phase 4; ADR 0011, ADR 0069).
//!
//! H.264 is the mandatory baseline; AV1 rides the same MFT machinery through
//! a different output subtype ([`mf_subtype`]) on hardware that has an AV1
//! encoder — Intel Arc and 12th-generation graphics onwards, NVIDIA 40
//! series, AMD RDNA 3 — and is simply absent everywhere else, which
//! [`hardware_available`] reports honestly rather than falling back to the
//! H.264 answer (ADR 0069).
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
//! Bitrate changes go through `ICodecAPI::SetValue` on
//! `CODECAPI_AVEncCommonMeanBitRate` and only fall back to rebuilding the
//! negotiated types if the driver refuses (ADR 0059). Rebuilding is not free
//! the way ADR 0005 assumed for the `openh264` fallback: `start_streaming`
//! flushes the transform, the reference frames go, and the next picture is
//! forced to IDR — once a second at `ABR_ADJUST_MAX_RATE_PER_SEC`, which is a
//! visible hitch coming from the mechanism meant to smooth one over.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use lumepeer_core::constants::ENCODE_HW_EVENT_TIMEOUT_MS;
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonLowLatency, CODECAPI_AVEncCommonMaxBitRate,
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonQualityVsSpeed,
    CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncCommonRealTime,
    CODECAPI_AVEncH264CABACEnable, CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVEncVideoMaxNumRefFrame, CODECAPI_AVLowLatencyMode,
    ICodecAPI, IMFActivate, IMFMediaEventGenerator, IMFMediaType, IMFSample, IMFTransform,
    METransformDrainComplete, METransformHaveOutput, METransformNeedInput,
    MF_E_NO_EVENTS_AVAILABLE, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
    MF_EVENT_FLAG_NO_WAIT, MF_EVENT_TYPE, MF_LOW_LATENCY, MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC,
    MF_TRANSFORM_ASYNC_UNLOCK, MFCreateAlignedMemoryBuffer, MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFSTARTUP_NOSOCKET,
    MFSampleExtension_CleanPoint, MFShutdown, MFStartup, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_MESSAGE_COMMAND_DRAIN,
    MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_AV1, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, eAVEncCommonRateControlMode, eAVEncCommonRateControlMode_CBR,
    eAVEncCommonRateControlMode_LowDelayVBR, eAVEncH264VProfile_High,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{GUID, Interface as _};

use super::nv12::bgra_to_nv12;
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

/// Probe/initial negotiation size for AV1.
///
/// Larger than [`PROBE_WIDTH`], and not for tidiness: an AV1 encoder MFT
/// refuses pictures the H.264 MFT sitting next to it on the same GPU
/// accepts. Intel's AV1 encoder has a floor of its own, so a 64x64 rehearsal
/// answers "there is no encoder" on a machine whose answer is "not at that
/// size" — measured here on a machine whose AV1 MFT declines 64x64 and
/// accepts this (ADR 0069).
///
/// Still small enough to cost nothing: it is negotiated once and the first
/// real frame reconfigures to the screen's own size.
const OPTIONAL_PROBE_WIDTH: u32 = 256;
/// See [`OPTIONAL_PROBE_WIDTH`].
const OPTIONAL_PROBE_HEIGHT: u32 = 256;

/// The geometry a probe and a fresh encoder negotiate at for `codec`.
const fn probe_dims(codec: VideoCodec) -> (u32, u32) {
    match codec {
        VideoCodec::H264 => (PROBE_WIDTH, PROBE_HEIGHT),
        VideoCodec::Av1 => (OPTIONAL_PROBE_WIDTH, OPTIONAL_PROBE_HEIGHT),
    }
}
/// Busy-poll granularity while waiting for an async MFT event. Small enough
/// to keep p50 latency negligible; the overall wait is still bounded by
/// `ENCODE_HW_EVENT_TIMEOUT_MS`.
const EVENT_POLL_INTERVAL_MS: u64 = 1;

/// How long the pipelined (no per-frame drain) path waits for the output of
/// the frame it just submitted before giving up on it and falling back to the
/// drain path for the rest of the session.
///
/// Deliberately far shorter than [`ENCODE_HW_EVENT_TIMEOUT_MS`]: this is not a
/// "the driver is wedged" timeout, it is "this encoder wants more than one
/// frame in flight before it emits anything". Answering that question must
/// cost one frame interval, not two seconds — the drain path below still
/// produces the picture either way.
const LOW_LATENCY_PROBE_TIMEOUT_MS: u64 = 200;

/// Peak-to-average ratio the rate controller may spend on a moving frame,
/// in percent of the mean bitrate.
///
/// A remote desktop is mostly still, and the interesting frames are the ones
/// where a window just moved. Constant bitrate spends the same bits on both,
/// which is exactly backwards: it wastes them on a screen nobody touched and
/// starves the one frame the user is waiting to see. A peak ceiling above the
/// mean is what lets the encoder put the bits where the change is.
const PEAK_BITRATE_PERCENT: u32 = 150;

/// Quality-versus-speed knob, 0..=100, where lower is faster.
///
/// Below the midpoint on purpose: every millisecond the encoder spends
/// looking for a better motion vector is a millisecond of hand-to-eye lag,
/// and on a desktop — large flat regions, exact repeats, hard edges — the
/// extra search buys very little.
const QUALITY_VS_SPEED: u32 = 33;

/// Frames between two unrequested intra frames.
///
/// Long, not infinite. A keyframe is the largest frame in the stream and
/// every one of them is a visible hitch at low bitrates, so periodic ones are
/// worth almost nothing here: the guest asks for an intra frame when it
/// actually needs one (§11's `KeyframeRequest`), which is the only moment one
/// helps. What this bounds is the drift a stream accumulates when no request
/// ever comes.
const GOP_SECONDS: u32 = 10;

/// The Media Foundation output subtype for one codec, or `None` when this
/// build cannot ask for that codec at all.
///
/// The one place a codec turns into a GUID, so enumeration
/// ([`enum_hardware_encoders`]) and the negotiated output type
/// ([`build_output_type`]) can never ask for different things — a transform
/// enumerated for one subtype and configured for another is the mismatch that
/// makes a driver hand back a bitstream the guest cannot decode (ADR 0069).
///
/// `Option` rather than a plain `GUID` because the type is the place a codec
/// this backend cannot serve is refused: a `None` here ends the question
/// before a single Media Foundation call is made.
#[allow(
    clippy::unnecessary_wraps,
    reason = "the `Option` is the refusal path for any codec this backend does not serve; flattening it would move that decision into the callers"
)]
const fn mf_subtype(codec: VideoCodec) -> Option<GUID> {
    match codec {
        VideoCodec::H264 => Some(MFVideoFormat_H264),
        VideoCodec::Av1 => Some(MFVideoFormat_AV1),
    }
}

/// The `MF_MT_MPEG2_PROFILE` value for `codec`, or `None` for a codec whose
/// profile this module leaves to the driver.
///
/// The attribute is shared but its meaning is not: it carries an
/// `eAVEncH264VProfile` for an H.264 MFT and a codec-specific enum for any
/// other, which is exactly why this is a per-codec answer rather than one
/// constant. AV1 gets `None` — Main is the only profile any hardware AV1
/// encoder produces for the 8-bit 4:2:0 input this feeds it, so the driver's
/// default is already right (ADR 0069).
fn mf_profile(codec: VideoCodec) -> Option<u32> {
    match codec {
        // High profile. It is the same decoder cost on anything built this
        // decade and it buys CABAC and 8x8 transforms, which is real
        // sharpness back on exactly the content a desktop is made of: text
        // edges and flat fills.
        VideoCodec::H264 => Some(platform_profile(eAVEncH264VProfile_High.0)),
        VideoCodec::Av1 => None,
    }
}

/// One `eAVEnc*VProfile` constant as the `u32` `MF_MT_MPEG2_PROFILE` wants;
/// they are fixed, non-negative platform enum values.
#[allow(
    clippy::cast_sign_loss,
    reason = "the video profiles are fixed, non-negative platform enum constants"
)]
const fn platform_profile(profile: i32) -> u32 {
    profile as u32
}

/// Whether a genuinely usable hardware encoder MFT for `config.codec` is
/// available right now (§18; ADR 0011, ADR 0069, ADR 0071).
///
/// H.264 runs the exact same activation and type negotiation
/// [`MediaFoundationEncoder::new`] would use, so this cannot claim
/// availability that construction then fails to back up.
///
/// The optional codecs go one step further and encode a frame, the way the
/// `VideoToolbox` probe does (ADR 0066). The reason is the same one that
/// applies there: on H.264 the interesting question is whether an encoder
/// exists at all, and plenty of machines have none. On a codec a session only
/// reaches because a guest asked for it and this build was made to offer it,
/// the question is whether the transform that just enumerated actually
/// produces a picture — §11 allows either optional codec only with real
/// hardware behind it on both sides, and "it enumerated" is not that
/// evidence. It costs one 64x64 frame, once, at encoder selection.
pub(super) fn hardware_available(config: EncoderConfig) -> bool {
    match config.codec {
        VideoCodec::H264 => activate_hardware_transform(PROBE_WIDTH, PROBE_HEIGHT, config).is_ok(),
        VideoCodec::Av1 => encodes_one_frame(config),
    }
}

/// Builds an encoder for `config` and pushes one picture through it,
/// answering `true` only when a non-empty bitstream comes back.
fn encodes_one_frame(config: EncoderConfig) -> bool {
    let Ok(mut encoder) = MediaFoundationEncoder::new(config) else {
        return false;
    };
    match encoder.encode(&probe_frame(config.codec)) {
        Ok(frame) => !frame.data.is_empty(),
        Err(error) => {
            tracing::info!(%error, codec = ?config.codec, "a hardware encoder MFT activated but produced no picture");
            false
        }
    }
}

/// A flat grey [`probe_dims`] picture for [`encodes_one_frame`] to push
/// through a transform it just activated.
fn probe_frame(codec: VideoCodec) -> Frame {
    /// Mid grey, so the picture is neither degenerate black nor saturated
    /// white; nothing depends on the value beyond it being a real image.
    const FILL: u8 = 0x80;
    let (width, height) = probe_dims(codec);
    Frame {
        width,
        height,
        format: PixelFormat::Bgra8,
        timestamp_us: 0,
        data: vec![FILL; (width as usize) * (height as usize) * 4],
    }
}

/// Hardware H.264 encoder backed by a Media Foundation MFT.
pub struct MediaFoundationEncoder {
    transform: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    config: EncoderConfig,
    dims: (u32, u32),
    /// Async-MFT events that arrived while this side was waiting for a
    /// different one. See [`EventPump`].
    pump: EventPump,
    /// Set once the transform has proved it will not hand back the frame it
    /// was just given without a drain. From then on every frame pays for the
    /// drain, which is what this module did unconditionally before.
    needs_drain: bool,
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
    /// Activates a hardware encoder MFT for `config.codec` and negotiates
    /// types at [`PROBE_WIDTH`]x[`PROBE_HEIGHT`]; `encode` renegotiates at the
    /// real frame size on first use.
    ///
    /// Every [`VideoCodec`] this crate knows has an [`mf_subtype`], so there
    /// is no codec check here: a machine without an encoder for the one asked
    /// for simply enumerates nothing, and this returns the same error it
    /// returns for a machine with no hardware encoder at all (ADR 0069).
    ///
    /// # Errors
    /// [`MediaError::EncoderUnavailable`] if no hardware encoder MFT for that
    /// codec is available and usable.
    pub fn new(config: EncoderConfig) -> Result<Self> {
        let (width, height) = probe_dims(config.codec);
        let (mf, transform, events) = activate_hardware_transform(width, height, config)?;
        Ok(Self {
            transform,
            events,
            config,
            dims: (width, height),
            pump: EventPump::default(),
            needs_drain: false,
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
        // A different transform raises its own events; anything counted for
        // the old one is not a promise this one made.
        self.pump.reset();
        self.needs_drain = false;
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
    fn restart_after_drain(&mut self) -> Result<()> {
        // The drain finishes with METransformDrainComplete; starting the next
        // stream before it lands is undefined for the driver, and the wait is
        // effectively free because the output this frame owns has already
        // been read by the time it runs.
        if let Some(events) = &self.events {
            self.pump.take(
                events,
                METransformDrainComplete,
                Duration::from_millis(ENCODE_HW_EVENT_TIMEOUT_MS),
            )?;
        }
        // SAFETY: ProcessMessage with a message type that takes no pointer
        // parameter.
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
        }
        .map_err(|e| MediaError::Encode(format!("START_OF_STREAM refused after a drain: {e}")))?;
        // The new stream raises its own `METransformNeedInput`; a token left
        // over from the stream that just ended would let the next frame call
        // `ProcessInput` before the transform is ready for it.
        self.pump.reset();
        Ok(())
    }

    /// Hands one sample to the transform, waiting for the input request an
    /// asynchronous MFT signals first.
    fn submit(&mut self, sample: &IMFSample) -> Result<()> {
        if let Some(events) = &self.events {
            self.pump.take(
                events,
                METransformNeedInput,
                Duration::from_millis(ENCODE_HW_EVENT_TIMEOUT_MS),
            )?;
        }
        // SAFETY: `sample` wraps a single contiguous NV12 buffer sized to
        // match the input type negotiated by `reconfigure`/`new`;
        // `ProcessInput` only reads it.
        unsafe { self.transform.ProcessInput(0, sample, 0) }
            .map_err(|e| MediaError::Encode(format!("ProcessInput failed: {e}")))
    }

    /// Reads the frame the transform owes for the sample just submitted,
    /// without ending the stream.
    ///
    /// This is what `CODECAPI_AVLowLatencyMode` buys (ADR 0059): an encoder in that mode
    /// has no reordering window and no lookahead, so it emits one picture per
    /// picture it is given and the client never has to ask. The per-frame
    /// `MFT_MESSAGE_COMMAND_DRAIN` this replaces cost a full pipeline flush, a
    /// `METransformDrainComplete` round trip with the driver and a stream
    /// restart *for every frame* - the encoder was being stopped and started
    /// thirty times a second.
    ///
    /// `Err` here is not a session failure: the caller falls back to the drain
    /// path and stays there.
    fn collect(&mut self, width: u32, height: u32) -> Result<EncodedFrame> {
        let deadline = Duration::from_millis(LOW_LATENCY_PROBE_TIMEOUT_MS);
        loop {
            if let Some(events) = &self.events {
                self.pump.take(events, METransformHaveOutput, deadline)?;
            }
            match drain_output(&self.transform, self.config.codec)? {
                DrainResult::Frame(encoded) => return Ok(encoded),
                DrainResult::NeedMoreInput => {
                    return Err(MediaError::Encode(
                        "the encoder holds this frame back until it is given more input".to_owned(),
                    ));
                }
                DrainResult::StreamChanged => {
                    negotiate_types(&self.transform, width, height, self.config)?;
                }
            }
        }
    }

    /// Forces out whatever the transform can produce from the input queued so
    /// far, then restarts the stream. The original behaviour of this module,
    /// kept for encoders that will not run one-in/one-out.
    fn collect_by_draining(&mut self, width: u32, height: u32) -> Result<EncodedFrame> {
        // MSDN "Basic MFT Processing Model": DRAIN is exactly "emit what you
        // can from what you already have", not a shutdown signal.
        // SAFETY: ProcessMessage with a message type that takes no pointer
        // parameter.
        unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) }
            .map_err(|e| MediaError::Encode(format!("DRAIN refused: {e}")))?;

        let deadline = Duration::from_millis(ENCODE_HW_EVENT_TIMEOUT_MS);
        loop {
            if let Some(events) = &self.events {
                self.pump.take(events, METransformHaveOutput, deadline)?;
            }
            match drain_output(&self.transform, self.config.codec)? {
                DrainResult::Frame(encoded) => {
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
                    negotiate_types(&self.transform, width, height, self.config)?;
                }
            }
        }
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
        self.submit(&sample)?;

        // One picture in, one picture out, with the transform left streaming.
        // Encoders that will not do that say so once - by holding this frame
        // back - and are driven with the per-frame drain from then on.
        let mut encoded = if self.needs_drain {
            self.collect_by_draining(width, height)?
        } else {
            match self.collect(width, height) {
                Ok(encoded) => encoded,
                Err(error) => {
                    tracing::info!(
                        %error,
                        "this encoder MFT will not run one-in/one-out; draining every frame"
                    );
                    self.needs_drain = true;
                    self.collect_by_draining(width, height)?
                }
            }
        };
        encoded.timestamp_us = frame.timestamp_us;
        Ok(encoded)
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

        // `ICodecAPI::SetValue` first, and renegotiation only if the driver
        // refuses it. Renegotiating the output type resets the transform:
        // `start_streaming` flushes it, the reference frames go, and the next
        // picture has to be an IDR. The adaptive controller may move the
        // target once a second (`ABR_ADJUST_MAX_RATE_PER_SEC`), so paying that
        // put a keyframe-sized spike and a visible hitch into the stream every
        // second the link was not perfectly steady - the very condition the
        // adaptation exists to smooth over.
        if set_mean_bitrate(&self.transform, bitrate_kbps).is_err() {
            negotiate_types(&self.transform, self.dims.0, self.dims.1, new_config)?;
            start_streaming(&self.transform)?;
            self.pump.reset();
        }
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

/// Enumerates hardware encoder MFTs for `config.codec`, activates the first
/// one that accepts NV12 input / that codec's output at `width`x`height`, and
/// starts streaming. Returns the [`MfRuntime`] guard alongside so the caller
/// can keep `MFStartup` balanced for as long as the transform lives.
fn activate_hardware_transform(
    width: u32,
    height: u32,
    config: EncoderConfig,
) -> Result<(MfRuntime, IMFTransform, Option<IMFMediaEventGenerator>)> {
    // Before `MFStartup`, deliberately: a build that cannot ask for this
    // codec at all must not start Media Foundation to find that out
    // (ADR 0071).
    let Some(subtype) = mf_subtype(config.codec) else {
        return Err(MediaError::EncoderUnavailable(format!(
            "this build was not made with support for {:?}",
            config.codec
        )));
    };
    let mf = MfRuntime::acquire()?;
    ensure_com_initialized()?;

    let input_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype,
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
    Err(MediaError::EncoderUnavailable(format!(
        "no usable hardware {:?} encoder MFT is registered on this system",
        config.codec
    )))
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

    // Before the types, not after: `MF_LOW_LATENCY` and the rate-control mode
    // change what the transform is willing to negotiate, and several drivers
    // latch both at `SetOutputType` time.
    request_low_latency(&transform);
    negotiate_types(&transform, width, height, config)?;
    tune_for_low_latency(&transform, config);
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
    config: EncoderConfig,
) -> Result<()> {
    let output_type = build_output_type(width, height, config)?;
    // SAFETY: SetOutputType takes a reference to a media type this function
    // owns; `dwflags = 0` commits it rather than merely testing it.
    unsafe { transform.SetOutputType(0, &output_type, 0) }
        .map_err(|e| MediaError::EncoderUnavailable(format!("SetOutputType refused: {e}")))?;

    let input_type = build_input_type(width, height, config.fps)?;
    // SAFETY: same as above, for the input side.
    unsafe { transform.SetInputType(0, &input_type, 0) }
        .map_err(|e| MediaError::EncoderUnavailable(format!("SetInputType refused: {e}")))?;
    Ok(())
}

fn build_output_type(width: u32, height: u32, config: EncoderConfig) -> Result<IMFMediaType> {
    // SAFETY: MFCreateMediaType and the attribute setters below are COM
    // calls on a media type this function owns exclusively until it returns
    // it to the caller.
    let media_type = unsafe { MFCreateMediaType() }
        .map_err(|e| MediaError::EncoderUnavailable(format!("MFCreateMediaType failed: {e}")))?;
    unsafe {
        media_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| MediaError::EncoderUnavailable(e.to_string()))?;
        let subtype = mf_subtype(config.codec).ok_or_else(|| {
            MediaError::EncoderUnavailable(format!(
                "this build was not made with support for {:?}",
                config.codec
            ))
        })?;
        media_type
            .SetGUID(&MF_MT_SUBTYPE, &raw const subtype)
            .map_err(|e| MediaError::EncoderUnavailable(e.to_string()))?;
        media_type
            .SetUINT32(
                &MF_MT_AVG_BITRATE,
                config.bitrate_kbps.saturating_mul(BITS_PER_KBIT),
            )
            .map_err(|e| MediaError::EncoderUnavailable(e.to_string()))?;
        media_type
            .SetUINT32(&MF_MT_INTERLACE_MODE, interlace_progressive())
            .map_err(|e| MediaError::EncoderUnavailable(e.to_string()))?;
        // Best-effort, and per codec — see [`mf_profile`]. A driver that
        // refuses the value keeps whatever profile it defaults to, so this is
        // set rather than negotiated.
        if let Some(profile) = mf_profile(config.codec) {
            let _ = media_type.SetUINT32(&MF_MT_MPEG2_PROFILE, profile);
        }
    }
    set_frame_size(&media_type, width, height)?;
    set_frame_rate(&media_type, config.fps)?;
    set_pixel_aspect_ratio(&media_type)?;
    Ok(media_type)
}

/// Asks the transform's own attribute store for low-latency processing.
///
/// Separate from [`tune_for_low_latency`] and run *before* type negotiation:
/// `MF_LOW_LATENCY` is an MFT attribute rather than a codec property, and
/// MSDN documents it as something the client sets on the transform before it
/// starts. Best-effort throughout - an encoder that does not know the
/// attribute simply keeps its defaults.
fn request_low_latency(transform: &IMFTransform) {
    // SAFETY: GetAttributes/SetUINT32 only touch the transform's own
    // attribute store.
    unsafe {
        if let Ok(attrs) = transform.GetAttributes() {
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
        }
    }
}

/// Puts the encoder into the mode a remote desktop actually needs, through
/// `ICodecAPI` (ADR 0059).
///
/// Every one of these is best-effort: `ICodecAPI` properties are optional per
/// MFT, drivers differ in which ones they implement, and a refusal means
/// "this encoder keeps its default", never "this session cannot run". What
/// they are for:
///
/// - `AVLowLatencyMode` / `AVEncCommonLowLatency` / `AVEncCommonRealTime`:
///   no reordering window, no lookahead, no B-frames held back waiting for a
///   future picture. Without it an encoder is free to buffer one to three
///   frames before emitting anything, which is 30-100 ms of lag that no
///   amount of work anywhere else in the pipeline can win back.
/// - `LowDelayVBR` (falling back to CBR): a desktop is still most of the
///   time and interesting exactly when it is not. Constant bitrate spends the
///   same bits on both.
/// - `MaxNumRefFrame = 1` and no B-pictures: nothing may depend on a picture
///   that has not been sent yet.
/// - `GOPSize`: see [`GOP_SECONDS`] - the guest asks for an intra frame when
///   it needs one, so periodic ones only cost.
fn tune_for_low_latency(transform: &IMFTransform, config: EncoderConfig) {
    let Ok(codec) = transform.cast::<ICodecAPI>() else {
        // A transform with no ICodecAPI keeps every default it has. Not worth
        // a warning: it is legal, and the session still runs.
        return;
    };
    let set = |key: &windows::core::GUID, value: VARIANT| {
        // SAFETY: both arguments outlive the call and `SetValue` only reads
        // them.
        unsafe { codec.SetValue(key, &raw const value) }.is_ok()
    };

    set(&CODECAPI_AVLowLatencyMode, VARIANT::from(true));
    set(&CODECAPI_AVEncCommonLowLatency, VARIANT::from(true));
    set(&CODECAPI_AVEncCommonRealTime, VARIANT::from(true));

    if !set(
        &CODECAPI_AVEncCommonRateControlMode,
        VARIANT::from(rate_control_mode(eAVEncCommonRateControlMode_LowDelayVBR)),
    ) {
        set(
            &CODECAPI_AVEncCommonRateControlMode,
            VARIANT::from(rate_control_mode(eAVEncCommonRateControlMode_CBR)),
        );
    }
    set(
        &CODECAPI_AVEncCommonMeanBitRate,
        VARIANT::from(config.bitrate_kbps.saturating_mul(BITS_PER_KBIT)),
    );
    set(
        &CODECAPI_AVEncCommonMaxBitRate,
        VARIANT::from(peak_bitrate_bps(config.bitrate_kbps)),
    );
    set(
        &CODECAPI_AVEncCommonQualityVsSpeed,
        VARIANT::from(QUALITY_VS_SPEED),
    );
    set(&CODECAPI_AVEncVideoMaxNumRefFrame, VARIANT::from(1u32));
    set(&CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32));
    // H.264 only. CABAC is an H.264 entropy coder and the property names it;
    // AV1 has one entropy coder with nothing to select, so asking would be
    // asking a question that codec does not have (ADR 0069).
    if config.codec == VideoCodec::H264 {
        set(&CODECAPI_AVEncH264CABACEnable, VARIANT::from(true));
    }
    set(
        &CODECAPI_AVEncMPVGOPSize,
        VARIANT::from(u32::from(config.fps.max(1)).saturating_mul(GOP_SECONDS)),
    );
}

/// Moves the mean and peak bitrate on a live transform, without touching the
/// negotiated types.
///
/// # Errors
/// [`MediaError::Encode`] if the transform exposes no `ICodecAPI` or refuses
/// the property - which is the caller's signal to renegotiate instead.
fn set_mean_bitrate(transform: &IMFTransform, bitrate_kbps: u32) -> Result<()> {
    let codec: ICodecAPI = transform
        .cast()
        .map_err(|e| MediaError::Encode(format!("this encoder MFT exposes no ICodecAPI: {e}")))?;
    let mean = VARIANT::from(bitrate_kbps.saturating_mul(BITS_PER_KBIT));
    // SAFETY: both arguments are locals that outlive the call, and `SetValue`
    // only reads them.
    unsafe { codec.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &raw const mean) }
        .map_err(|e| MediaError::Encode(format!("a live bitrate change was refused: {e}")))?;
    let peak = VARIANT::from(peak_bitrate_bps(bitrate_kbps));
    // SAFETY: as above. The peak is best-effort: an encoder that took the
    // mean but not the peak is still following the target.
    let _ = unsafe { codec.SetValue(&CODECAPI_AVEncCommonMaxBitRate, &raw const peak) };
    Ok(())
}

/// The peak the rate controller may reach for a moving frame, in bits per
/// second. See [`PEAK_BITRATE_PERCENT`].
fn peak_bitrate_bps(bitrate_kbps: u32) -> u32 {
    bitrate_kbps
        .saturating_mul(BITS_PER_KBIT)
        .saturating_div(100)
        .saturating_mul(PEAK_BITRATE_PERCENT)
}

/// One `eAVEncCommonRateControlMode` value as the `u32` `ICodecAPI` wants.
#[allow(
    clippy::cast_sign_loss,
    reason = "the rate-control modes are fixed, non-negative platform enum constants"
)]
fn rate_control_mode(mode: eAVEncCommonRateControlMode) -> u32 {
    mode.0 as u32
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
fn drain_output(transform: &IMFTransform, codec: VideoCodec) -> Result<DrainResult> {
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
            Ok(DrainResult::Frame(sample_to_encoded_frame(&sample, codec)?))
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

fn sample_to_encoded_frame(sample: &IMFSample, codec: VideoCodec) -> Result<EncodedFrame> {
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
        // but not every driver sets it faithfully; cross-check the bitstream
        // itself so a missing attribute cannot turn a real keyframe into a
        // false negative.
        let clean_point = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0);
        let keyframe = clean_point != 0 || bitstream_is_random_access(codec, &data);

        Ok(EncodedFrame {
            keyframe,
            timestamp_us: 0, // overwritten by the caller with the input frame's timestamp
            data,
        })
    }
}

/// Whether `data` can be decoded without anything before it, read from the
/// bitstream itself rather than from the driver's own claim.
///
/// The cross-check behind `MFSampleExtension_CleanPoint`, so it answers per
/// codec: the two bitstreams have nothing structurally in common, and asking
/// an AV1 temporal unit whether it contains an H.264 IDR NAL would find
/// whatever the byte pattern happened to hit (ADR 0069).
fn bitstream_is_random_access(codec: VideoCodec, data: &[u8]) -> bool {
    match codec {
        VideoCodec::H264 => bitstream_has_idr(data),
        VideoCodec::Av1 => av1_has_sequence_header(data),
    }
}

/// `nal_unit_type` occupies the low five bits of an H.264 NAL header byte.
const H264_NAL_TYPE_MASK: u8 = 0b0001_1111;
/// H.264 `nal_unit_type` 5: a slice of an IDR picture.
const H264_NAL_IDR: u8 = 5;
/// Offsets of every NAL unit header byte in an Annex-B buffer.
fn annex_b_nal_headers(data: &[u8]) -> impl Iterator<Item = usize> + '_ {
    let mut i = 0usize;
    std::iter::from_fn(move || {
        while i + 3 <= data.len() {
            let start_code_len = if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                3usize
            } else if i + 4 <= data.len()
                && data[i] == 0
                && data[i + 1] == 0
                && data[i + 2] == 0
                && data[i + 3] == 1
            {
                4usize
            } else {
                i += 1;
                continue;
            };
            let nal_start = i + start_code_len;
            // Never stands still: `nal_start` is at least `i + 3`.
            i = nal_start;
            return Some(nal_start);
        }
        None
    })
}

/// Scans an Annex-B H.264 bitstream for an IDR slice NAL unit (type 5).
fn bitstream_has_idr(data: &[u8]) -> bool {
    annex_b_nal_headers(data).any(|at| {
        data.get(at)
            .is_some_and(|header| header & H264_NAL_TYPE_MASK == H264_NAL_IDR)
    })
}

/// `obu_forbidden_bit` of an AV1 OBU header; always 0 in a well-formed
/// stream, so a 1 means this is not an OBU boundary at all.
const AV1_OBU_FORBIDDEN_BIT: u8 = 0b1000_0000;
/// `obu_type` occupies the four bits below `obu_forbidden_bit`.
const AV1_OBU_TYPE_SHIFT: u32 = 3;
/// See [`AV1_OBU_TYPE_SHIFT`].
const AV1_OBU_TYPE_MASK: u8 = 0b1111;
/// `obu_extension_flag`: one more header byte follows when it is set.
const AV1_OBU_EXTENSION_FLAG: u8 = 0b0000_0100;
/// `obu_has_size_field`: a leb128 payload length follows the header. Media
/// Foundation emits the low-overhead bitstream format, where it is always set.
const AV1_OBU_HAS_SIZE_FIELD: u8 = 0b0000_0010;
/// `OBU_SEQUENCE_HEADER` (AV1 specification, section 6.2.2).
const AV1_OBU_SEQUENCE_HEADER: u8 = 1;
/// Bytes a leb128 value may occupy in AV1 (specification, section 4.10.5).
const AV1_LEB128_MAX_BYTES: usize = 8;
/// Payload bits carried by one leb128 byte; the eighth is the continuation
/// flag.
const AV1_LEB128_PAYLOAD_BITS: u32 = 7;
/// The [`AV1_LEB128_PAYLOAD_BITS`] payload bits of one leb128 byte.
const AV1_LEB128_PAYLOAD_MASK: u8 = 0b0111_1111;
/// The continuation flag of one leb128 byte: another byte follows.
const AV1_LEB128_CONTINUATION: u8 = 0b1000_0000;

/// Whether an AV1 temporal unit carries a sequence header OBU, which is what
/// makes it a point a decoder can start from.
///
/// A sequence header rather than a key frame header: AV1 puts the decoder
/// configuration — resolution, bit depth, the enabled coding tools — in the
/// sequence header, and a decoder that has not read one cannot decode the key
/// frame that follows it either. An encoder therefore emits one alongside
/// every random-access point, which makes "does this temporal unit contain a
/// sequence header" the same question as "can a decoder join here", and it is
/// answerable by walking OBU sizes instead of parsing a frame header's
/// variable-length bit fields (ADR 0069).
///
/// Refuses rather than guesses at anything it cannot walk: a header with no
/// size field ends the scan, because from there the next OBU boundary is not
/// discoverable and a byte picked out of the middle of a payload would be
/// noise. This runs on encoder output rather than on a peer's bytes, but it
/// is a bitstream parser either way, so it neither panics nor loops on one
/// (§21).
fn av1_has_sequence_header(data: &[u8]) -> bool {
    let mut at = 0usize;
    while let Some(&header) = data.get(at) {
        if header & AV1_OBU_FORBIDDEN_BIT != 0 {
            return false;
        }
        if (header >> AV1_OBU_TYPE_SHIFT) & AV1_OBU_TYPE_MASK == AV1_OBU_SEQUENCE_HEADER {
            return true;
        }
        if header & AV1_OBU_HAS_SIZE_FIELD == 0 {
            return false;
        }
        let after_header = at + 1 + usize::from(header & AV1_OBU_EXTENSION_FLAG != 0);
        let Some((payload, leb_bytes)) = read_leb128(data, after_header) else {
            return false;
        };
        // Always strictly greater than `at`: `after_header` is at least
        // `at + 1` and `leb_bytes` is at least 1, so the walk cannot stand
        // still even on a zero-length payload.
        let Some(next) = after_header
            .checked_add(leb_bytes)
            .and_then(|end| end.checked_add(payload))
        else {
            return false;
        };
        at = next;
    }
    false
}

/// Reads the leb128 at `at`, returning its value and how many bytes it took,
/// or `None` if it runs off the end or past [`AV1_LEB128_MAX_BYTES`].
fn read_leb128(data: &[u8], at: usize) -> Option<(usize, usize)> {
    let mut value = 0usize;
    for index in 0..AV1_LEB128_MAX_BYTES {
        let byte = *data.get(at + index)?;
        value |= usize::from(byte & AV1_LEB128_PAYLOAD_MASK)
            .checked_shl(u32::try_from(index).ok()? * AV1_LEB128_PAYLOAD_BITS)?;
        if byte & AV1_LEB128_CONTINUATION == 0 {
            return Some((value, index + 1));
        }
    }
    None
}

/// Asynchronous-MFT events that arrived out of turn.
///
/// An async MFT signals `METransformNeedInput` and `METransformHaveOutput`
/// whenever it is ready, in whatever order suits it, and every event is a
/// one-shot credit: MSDN's asynchronous processing model says a client may
/// call `ProcessInput` exactly once per `METransformNeedInput` it received.
///
/// The version of this that this replaced simply *discarded* every event it
/// was not currently waiting for. That is why the per-frame drain existed: a
/// `NeedInput` thrown away while collecting output is a credit the next frame
/// then waits two seconds for and never gets, and restarting the stream was
/// the only thing that made the driver issue a fresh one. Counting them
/// instead is what makes running the transform without a drain possible at
/// all.
#[derive(Debug, Default)]
struct EventPump {
    need_input: u32,
    have_output: u32,
}

impl EventPump {
    /// Forgets every outstanding credit. Called wherever the transform's
    /// stream is restarted or replaced, because credits belong to the stream
    /// that issued them.
    fn reset(&mut self) {
        self.need_input = 0;
        self.have_output = 0;
    }

    /// Records one event, if it is one of the two this cares about.
    fn record(&mut self, ty: u32) {
        if ty == METransformNeedInput.0.cast_unsigned() {
            self.need_input = self.need_input.saturating_add(1);
        } else if ty == METransformHaveOutput.0.cast_unsigned() {
            self.have_output = self.have_output.saturating_add(1);
        }
    }

    /// Spends one credit of `expected`, if one is already banked.
    fn spend(&mut self, expected: MF_EVENT_TYPE) -> bool {
        let slot = if expected.0 == METransformNeedInput.0 {
            &mut self.need_input
        } else if expected.0 == METransformHaveOutput.0 {
            &mut self.have_output
        } else {
            // Anything else (a drain completing, say) is a one-off that is
            // never banked, so it is always waited for.
            return false;
        };
        if *slot == 0 {
            return false;
        }
        *slot -= 1;
        true
    }

    /// Waits until `expected` is available, banking anything else that
    /// arrives on the way.
    ///
    /// Bounded by `timeout` so a stalled or crashed hardware encoder driver
    /// fails one `encode()` call instead of hanging the session forever
    /// (§24.5, ADR 0011).
    ///
    /// # Errors
    /// [`MediaError::Encode`] on timeout or on a failing event queue.
    fn take(
        &mut self,
        events: &IMFMediaEventGenerator,
        expected: MF_EVENT_TYPE,
        timeout: Duration,
    ) -> Result<()> {
        if self.spend(expected) {
            return Ok(());
        }
        let deadline = Instant::now() + timeout;
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
                    self.record(ty);
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
        if !hardware_available(EncoderConfig::default()) {
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
        let probed = hardware_available(EncoderConfig::default());
        let constructed = MediaFoundationEncoder::new(EncoderConfig::default()).is_ok();
        assert_eq!(
            probed,
            constructed,
            "hardware_available reported {probed} but construction {}",
            if constructed { "succeeded" } else { "failed" }
        );
    }

    /// The AV1 half of the rule above, and one notch stricter, because that is
    /// what [`hardware_available`] promises for AV1: a `true` answer must be
    /// backed by a transform that actually produced a picture, not merely by
    /// one that constructed (ADR 0069). Runs identically on a machine with an
    /// AV1 encoder and on one without.
    #[test]
    fn an_av1_probe_that_says_yes_is_backed_by_a_real_picture() {
        let config = EncoderConfig {
            codec: VideoCodec::Av1,
            ..EncoderConfig::default()
        };
        if !hardware_available(config) {
            eprintln!("skipping: no hardware AV1 encoder MFT on this machine");
            return;
        }
        // The probe above already built one of these and encoded through it.
        let mut encoder = MediaFoundationEncoder::new(config).unwrap();
        let first = encoder.encode(&probe_frame(config.codec)).unwrap();
        assert!(!first.data.is_empty());
        assert!(first.keyframe, "the first frame must be decodable alone");
        assert_eq!(encoder.kind(), EncoderKind::Hardware);
    }

    /// Each codec is enumerated and negotiated under its own Media Foundation
    /// subtype. The failure this guards is the copy-paste one: an AV1 request
    /// that enumerates `MFVideoFormat_H264` finds the H.264 encoder every
    /// machine has, activates it, and hands a guest expecting AV1 an H.264
    /// bitstream — §11's mutual-hardware-support rule violated with every
    /// individual step apparently succeeding.
    #[test]
    fn each_codec_has_its_own_media_foundation_subtype() {
        assert_eq!(mf_subtype(VideoCodec::H264), Some(MFVideoFormat_H264));
        assert_eq!(mf_subtype(VideoCodec::Av1), Some(MFVideoFormat_AV1));
        assert_ne!(mf_subtype(VideoCodec::H264), mf_subtype(VideoCodec::Av1));
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
    fn bitstream_has_idr_finds_a_type_5_nal_after_a_start_code() {
        let data = [0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB]; // NAL type 5 = IDR
        assert!(bitstream_has_idr(&data));
    }

    #[test]
    fn bitstream_has_idr_is_false_for_non_idr_nals() {
        let data = [0x00, 0x00, 0x01, 0x41, 0xAA, 0xBB]; // NAL type 1 = non-IDR slice
        assert!(!bitstream_has_idr(&data));
    }

    /// One OBU header byte: `obu_type` in bits 6..3, `obu_has_size_field` set,
    /// everything else clear — the low-overhead form Media Foundation emits.
    const fn obu_header(obu_type: u8) -> u8 {
        (obu_type << AV1_OBU_TYPE_SHIFT) | AV1_OBU_HAS_SIZE_FIELD
    }

    #[test]
    fn av1_finds_a_sequence_header_after_a_temporal_delimiter() {
        // OBU_TEMPORAL_DELIMITER (2), empty, then OBU_SEQUENCE_HEADER (1).
        let data = [obu_header(2), 0x00, obu_header(1), 0x01, 0x00];
        assert!(av1_has_sequence_header(&data));
    }

    #[test]
    fn av1_is_false_for_a_temporal_unit_that_only_carries_a_frame() {
        // OBU_TEMPORAL_DELIMITER (2), empty, then OBU_FRAME (6).
        let data = [obu_header(2), 0x00, obu_header(6), 0x03, 0xAA, 0xBB, 0xCC];
        assert!(!av1_has_sequence_header(&data));
    }

    #[test]
    fn av1_walks_past_an_obu_whose_length_took_two_leb128_bytes() {
        // OBU_FRAME (6) of 200 bytes — 0xC8 0x01 in leb128 — then a sequence
        // header. Reading the length as one byte would land mid-payload.
        let mut data = vec![obu_header(6), 0xC8, 0x01];
        data.extend(std::iter::repeat_n(0x5Au8, 200));
        data.extend_from_slice(&[obu_header(1), 0x01, 0x00]);
        assert!(av1_has_sequence_header(&data));
    }

    #[test]
    fn av1_refuses_a_truncated_temporal_unit_rather_than_running_off_the_end() {
        assert!(!av1_has_sequence_header(&[]));
        assert!(!av1_has_sequence_header(&[obu_header(2)]));
        // A length that claims more than is there.
        assert!(!av1_has_sequence_header(&[obu_header(6), 0x40, 0x00]));
    }

    /// The two scanners must never be applied to each other's bitstream: an
    /// H.264 IDR is a byte pattern an OBU walk can wander into, and an AV1
    /// keyframe is one an H.264 walk reads as a NAL type entirely its own.
    #[test]
    fn random_access_detection_does_not_cross_codecs() {
        let h264_idr = [0x00, 0x00, 0x00, 0x01, 0x65, 0xAA];
        let av1_keyframe = [obu_header(2), 0x00, obu_header(1), 0x01, 0x00];
        assert!(bitstream_is_random_access(VideoCodec::H264, &h264_idr));
        assert!(!bitstream_is_random_access(VideoCodec::Av1, &h264_idr));
        assert!(bitstream_is_random_access(VideoCodec::Av1, &av1_keyframe));
        assert!(!bitstream_is_random_access(VideoCodec::H264, &av1_keyframe));
    }
}
