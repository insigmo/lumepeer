//! VA-API hardware video encoder for Linux — H.264 only (§5.1, §11,
//! §18/§19 phase 4; ADR 0040, ADR 0088).
//!
//! The Linux counterpart of [`super::windows`] (ADR 0011), built to the same
//! rule: [`hardware_available`] is not a capability query, it is a
//! rehearsal. It opens the same session [`VaapiEncoder::new`] opens **and
//! encodes a frame through it**, and reports `true` only when an IDR picture
//! comes back out. ADR 0040 stopped at a created context, and that turned out
//! to prove nothing: the encoder it guarded had never run on hardware, and on
//! the first machine that ran it (ADR 0088) every step up to the context
//! succeeded while the stream it produced could not be decoded at all.
//!
//! What that machine taught, each written into the code below:
//!
//! - **Low-power is the only H.264 entrypoint on current Intel.** From Ice
//!   Lake on, the iHD driver offers `VAEntrypointEncSliceLP` and no
//!   `VAEntrypointEncSlice`, so asking for the latter alone refused every
//!   recent Intel iGPU. Both are tried, full-featured first.
//! - **A driver may emit no parameter sets.** iHD writes slices and leaves the
//!   SPS and PPS to the application, which libva hands over as packed headers
//!   — and `cros-libva` has no buffer type for those (ADR 0069). So this
//!   module writes them itself, from the very values it gives the driver, and
//!   puts them in front of an IDR that arrived without its own.
//! - **Some drivers only do constant QP.** Without its `HuC` firmware loaded, an
//!   Intel iGPU offers `VA_RC_CQP` and nothing else; a bitrate sent to it is
//!   ignored. [`QpController`] steers the quantizer from the sizes of the
//!   frames that come out, so ABR (§11) still has a knob.
//! - **`last_picture` ends the sequence.** Set on every picture, it made the
//!   driver append an end-of-sequence NAL after every frame, after which no
//!   P-frame is valid.
//! - **The reconstruction is not the reference.** A P-frame predicts from the
//!   previous frame's reconstruction while writing its own into a different
//!   surface; two surfaces alternate between those roles.
//!
//! Intel and AMD only. NVIDIA's Linux encode path is NVENC, a different SDK
//! under different licence terms, and is deliberately not reached from here:
//! `nvidia-vaapi-driver` bridges VA-API to NVDEC (decode) and offers no
//! encode entrypoint, so an NVIDIA host correctly probes `false` here and
//! falls back to `openh264` rather than half-working.
//!
//! `cros-libva` is a safe wrapper over libva, so this module — unlike the
//! Media Foundation one — needs no `unsafe` of its own beyond the `Send`
//! promise below.

use std::rc::Rc;

use cros_libva::{
    BufferType, Config, Context, Display, DrmDeviceIterator, EncCodedBuffer, EncMiscParameter,
    EncMiscParameterRateControl, EncPictureParameter, EncPictureParameterBufferH264,
    EncSequenceParameter, EncSequenceParameterBufferH264, EncSliceParameter,
    EncSliceParameterBufferH264, H264EncPicFields, H264EncSeqFields, MappedCodedBuffer,
    PictureH264, RcFlags, Surface, UsageHint, VA_ATTRIB_NOT_SUPPORTED, VA_FOURCC_NV12,
    VA_INVALID_ID, VA_PICTURE_H264_INVALID, VA_RC_CBR, VA_RC_CQP, VA_RC_VBR, VA_RT_FORMAT_YUV420,
    VAConfigAttrib, VAConfigAttribType, VAEntrypoint, VAProfile,
};
#[cfg(feature = "encode-vaapi-zero-copy")]
use cros_libva::{
    ExternalBufferDescriptor, MemoryType, VA_FOURCC_BGRA, VA_FOURCC_BGRX, VA_RT_FORMAT_RGB32,
    VADRMPRIMESurfaceDescriptor,
};
use lumepeer_core::constants::{
    VAAPI_CQP_QP_MAX, VAAPI_CQP_QP_MIN, VAAPI_CQP_RATE_TOLERANCE_PERCENT,
    VAAPI_CQP_SIZE_SMOOTHING_SHIFT,
};

use super::nv12::bgra_to_nv12;
use super::{EncodedFrame, EncoderConfig, EncoderKind, VideoCodec, VideoEncoder};
#[cfg(feature = "encode-vaapi-zero-copy")]
use crate::capture::DmaBuf;
use crate::capture::{Frame, PixelFormat};
use crate::error::{MediaError, Result};

/// Probe geometry, matching the Media Foundation module's: big enough to be a
/// real macroblock grid (4x4 macroblocks), small enough that a probe costs
/// nothing. The low-power Intel encoder takes pictures down to 32x32 and
/// refuses 16x16, measured.
const PROBE_WIDTH: u32 = 64;
/// See [`PROBE_WIDTH`].
const PROBE_HEIGHT: u32 = 64;

/// The grey the rehearsal frame is painted, as BGRA bytes. Any flat colour
/// would do; the probe asks whether a picture comes out, not what it shows.
const PROBE_GREY: u8 = 128;

/// Macroblock edge in pixels. H.264 codes in 16x16 macroblocks, so every
/// dimension handed to the driver is counted in these.
const MACROBLOCK: u32 = 16;

/// Quantizer every picture starts from, and the `pic_init_qp` the PPS names.
/// Under a driver's own rate control it is only where the controller starts;
/// under constant QP it is where [`QpController`] starts.
const INITIAL_QP: u8 = 26;

/// Bits per kilobit, for the kbps of §14 against the bps of the VA-API
/// rate-control buffer.
const BITS_PER_KBIT: u32 = 1000;

/// Bits per byte, for a bitrate against the sizes of encoded frames.
const BITS_PER_BYTE: u64 = 8;

/// Reference frames this encoder keeps. One: every non-IDR frame predicts
/// from the frame before it. A remote desktop is watched live, so a deeper
/// DPB buys compression the viewer pays for in latency.
const MAX_REF_FRAMES: u32 = 1;

/// `slice_type` values of the H.264 slice header. Only these two are used:
/// there are no B-slices in a latency product.
const SLICE_TYPE_P: u8 = 0;
/// See [`SLICE_TYPE_P`].
const SLICE_TYPE_I: u8 = 2;

/// How often an IDR is emitted when nobody asked. Every frame is otherwise a
/// P-frame, and a guest that joins mid-stream would wait forever; §11's
/// `KeyframeRequest` is the responsive path and this is the backstop.
const IDR_PERIOD: u32 = 120;

/// `log2_max_frame_num_minus4` of the SPS: `frame_num` runs up to 255, and it
/// restarts at every IDR, which comes at least every [`IDR_PERIOD`] frames.
const LOG2_MAX_FRAME_NUM_MINUS4: u32 = 4;

/// `log2_max_pic_order_cnt_lsb_minus4` of the SPS: the picture order count
/// runs up to 1023 and advances by two per frame, so it too fits a whole
/// [`IDR_PERIOD`] without wrapping.
const LOG2_MAX_POC_LSB_MINUS4: u32 = 6;

const _: () = assert!(
    IDR_PERIOD < 1 << (LOG2_MAX_FRAME_NUM_MINUS4 + 4),
    "frame_num must not wrap between two IDRs"
);
const _: () = assert!(
    2 * IDR_PERIOD < 1 << (LOG2_MAX_POC_LSB_MINUS4 + 4),
    "the picture order count must not wrap between two IDRs"
);

/// Whether a genuinely usable VA-API encoder for `config.codec` exists right
/// now (§18).
///
/// For H.264 this opens the same session [`VaapiEncoder::new`] opens and
/// encodes one frame through it, and answers `true` only when that frame comes
/// back as an IDR picture. A context that opens is not a stream that decodes
/// (ADR 0088), and a probe that stops before the first picture cannot tell the
/// two apart.
///
/// AV1 is `false`, and not for want of hardware: VA-API's AV1 encode
/// entrypoint cannot be driven through `cros-libva` at all.
/// `VAEncPictureParameterBufferAV1` carries bit offsets
/// (`bit_offset_qindex`, `byte_offset_frame_hdr_obu_size`,
/// `size_in_bits_frame_hdr_obu`, …) into a frame-header OBU the *application*
/// must write and hand over as a packed header, and `cros-libva`'s
/// `BufferType` has no packed-header variant to hand it over with. An
/// AV1 session opened without one produces a stream with no frame header,
/// which is not a picture and would fail §11's mutual-hardware-support rule
/// in the worst way: with every individual call returning success. ADR 0069
/// records this so the next person does not rediscover it from a black
/// window.
pub(super) fn hardware_available(config: EncoderConfig) -> bool {
    match config.codec {
        VideoCodec::H264 => match rehearse(config) {
            Ok(()) => true,
            Err(error) => {
                tracing::debug!(%error, "the VA-API H.264 encoder did not survive its rehearsal");
                false
            }
        },
        VideoCodec::Av1 => false,
    }
}

/// Opens an encoder at the probe geometry and pushes one frame through it.
fn rehearse(config: EncoderConfig) -> Result<()> {
    let mut encoder = VaapiEncoder::open(PROBE_WIDTH, PROBE_HEIGHT, config)?;
    let pixels = vec![PROBE_GREY; (PROBE_WIDTH as usize) * (PROBE_HEIGHT as usize) * 4];
    let frame = Frame::cpu(PROBE_WIDTH, PROBE_HEIGHT, PixelFormat::Bgra8, 0, pixels);
    let output = encoder.encode(&frame)?;
    if output.keyframe && headers::contains_nal(&output.data, headers::NAL_IDR_SLICE) {
        Ok(())
    } else {
        Err(MediaError::EncoderUnavailable(
            "the VA-API encoder accepted a frame and returned no IDR picture".to_owned(),
        ))
    }
}

/// The VA-API profile this backend drives `codec` through.
///
/// H.264 asks for Constrained Baseline: it is what every VA-API encoder that
/// exists implements, and §11's baseline is exactly that — no B-frames, no
/// CABAC, no interlace. Asking for Main or High would fail on hardware that
/// would otherwise have worked.
///
/// # Errors
/// [`MediaError::EncoderUnavailable`] for a codec this backend has no profile
/// for, which is the honest answer rather than a rehearsal of the wrong one
/// (§11's mutual-hardware-support rule; ADR 0069, ADR 0071).
fn va_profile(codec: VideoCodec) -> Result<VAProfile::Type> {
    match codec {
        VideoCodec::H264 => Ok(VAProfile::VAProfileH264ConstrainedBaseline),
        VideoCodec::Av1 => Err(MediaError::EncoderUnavailable(
            "the VA-API backend cannot drive AV1 encoding (ADR 0069)".to_owned(),
        )),
    }
}

/// The edge, in pixels, of the block `codec` counts its picture geometry in.
///
/// # Errors
/// [`MediaError::EncoderUnavailable`] for a codec this backend does not
/// implement — the same closed set [`va_profile`] answers for, kept beside it
/// so the two can never disagree about which codecs exist here.
fn coding_block(codec: VideoCodec) -> Result<u32> {
    match codec {
        VideoCodec::H264 => Ok(MACROBLOCK),
        VideoCodec::Av1 => Err(MediaError::EncoderUnavailable(
            "the VA-API backend cannot drive AV1 encoding (ADR 0069)".to_owned(),
        )),
    }
}

/// The encode entrypoint to open, of the ones a driver lists (ADR 0088).
///
/// The full-featured one first, because where both exist it is the one the
/// driver tunes for quality; the low-power one second, because on Intel from
/// Ice Lake on it is the only one there is.
fn encode_entrypoint(offered: &[VAEntrypoint::Type]) -> Option<VAEntrypoint::Type> {
    [
        VAEntrypoint::VAEntrypointEncSlice,
        VAEntrypoint::VAEntrypointEncSliceLP,
    ]
    .into_iter()
    .find(|entrypoint| offered.contains(entrypoint))
}

/// Who decides how many bits a picture gets (§11; ADR 0088).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RateControl {
    /// The driver holds a constant bitrate; `set_bitrate` reaches it through
    /// the rate-control buffer.
    Cbr,
    /// The driver holds an average bitrate, the same way.
    Vbr,
    /// The driver encodes every picture at the QP it is told, and
    /// [`QpController`] is what makes that follow a bitrate.
    ConstantQp,
}

impl RateControl {
    /// The mode to ask for, of the `VA_RC_*` bits a driver reports, or `None`
    /// when it reports none of the three this backend can drive.
    ///
    /// A bitrate the driver itself holds is preferred, because it sees the
    /// picture before encoding it and the controller only sees it afterwards.
    const fn choose(supported: u32) -> Option<Self> {
        if supported == VA_ATTRIB_NOT_SUPPORTED {
            None
        } else if supported & VA_RC_CBR != 0 {
            Some(Self::Cbr)
        } else if supported & VA_RC_VBR != 0 {
            Some(Self::Vbr)
        } else if supported & VA_RC_CQP != 0 {
            Some(Self::ConstantQp)
        } else {
            None
        }
    }

    /// The mode to ask `display` for when it encodes `profile` through
    /// `entrypoint`.
    ///
    /// # Errors
    /// [`MediaError::EncoderUnavailable`] when the driver cannot say, or
    /// offers no mode this encoder drives.
    fn offered_by(
        display: &Display,
        profile: VAProfile::Type,
        entrypoint: VAEntrypoint::Type,
    ) -> Result<Self> {
        let mut attribute = [VAConfigAttrib {
            type_: VAConfigAttribType::VAConfigAttribRateControl,
            value: 0,
        }];
        display
            .get_config_attributes(profile, entrypoint, &mut attribute)
            .map_err(|e| MediaError::EncoderUnavailable(format!("vaGetConfigAttributes: {e}")))?;
        Self::choose(attribute[0].value).ok_or_else(|| {
            MediaError::EncoderUnavailable(
                "this VA-API driver offers no rate control this encoder can drive".to_owned(),
            )
        })
    }

    /// The `VAConfigAttribRateControl` value naming this mode.
    const fn va_value(self) -> u32 {
        match self {
            Self::Cbr => VA_RC_CBR,
            Self::Vbr => VA_RC_VBR,
            Self::ConstantQp => VA_RC_CQP,
        }
    }
}

/// Steers the quantizer of a constant-QP driver towards a bitrate (§11;
/// ADR 0088).
///
/// After every P-frame the smoothed frame size is compared with the frame's
/// share of the target; a size outside the tolerance band moves the QP one
/// step, within [`VAAPI_CQP_QP_MIN`]..=[`VAAPI_CQP_QP_MAX`]. IDR frames are
/// left out of the average: they are meant to be several times a P-frame, and
/// counting them would push the quantizer up after every keyframe request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QpController {
    qp: u8,
    /// Running size of a P-frame in bytes, `None` until the first one.
    smoothed_bytes: Option<u64>,
}

impl QpController {
    const fn new() -> Self {
        Self {
            qp: INITIAL_QP,
            smoothed_bytes: None,
        }
    }

    /// The quantizer for the next picture.
    const fn qp(self) -> u8 {
        self.qp
    }

    /// Takes the size of one encoded picture into account.
    fn observe(&mut self, frame_bytes: usize, keyframe: bool, config: EncoderConfig) {
        if keyframe {
            return;
        }
        let size = u64::try_from(frame_bytes).unwrap_or(u64::MAX);
        let smoothed = match self.smoothed_bytes {
            None => size,
            Some(previous) => {
                previous - (previous >> VAAPI_CQP_SIZE_SMOOTHING_SHIFT)
                    + (size >> VAAPI_CQP_SIZE_SMOOTHING_SHIFT)
            }
        };
        self.smoothed_bytes = Some(smoothed);

        let target = u64::from(config.bitrate_kbps) * u64::from(BITS_PER_KBIT)
            / BITS_PER_BYTE
            / u64::from(config.fps.max(1));
        let percent = 100;
        if smoothed.saturating_mul(percent)
            > target.saturating_mul(percent + VAAPI_CQP_RATE_TOLERANCE_PERCENT)
        {
            self.qp = self.qp.saturating_add(1).min(VAAPI_CQP_QP_MAX);
        } else if smoothed.saturating_mul(percent)
            < target.saturating_mul(percent - VAAPI_CQP_RATE_TOLERANCE_PERCENT)
        {
            self.qp = self.qp.saturating_sub(1).max(VAAPI_CQP_QP_MIN);
        }
    }
}

/// Everything libva hands back for one encode session.
///
/// Kept in one struct because the destruction order matters and Rust's
/// declaration order is what enforces it: the coded buffer and surfaces
/// belong to the context, the context to the config, and all of them to the
/// display.
struct Session {
    coded: EncCodedBuffer,
    /// The driver's own `NV12` image format, looked up once at open time:
    /// `vaCreateImage` needs the exact `VAImageFormat` the driver published,
    /// not one assembled by hand.
    nv12_format: cros_libva::VAImageFormat,
    /// The surface the current frame is uploaded into.
    input: Surface<()>,
    /// The two reconstructions, alternating between "the picture being
    /// written" and "the picture it predicts from".
    reconstructions: [Surface<()>; 2],
    context: Rc<Context>,
    rate_control: RateControl,
    /// The kernel driver of the render node this session encodes on
    /// (`i915`, `amdgpu`, …), when sysfs says.
    driver: Option<String>,
    // Held, never read: libva objects are only valid while the config and
    // display that produced them are alive, and declaration order is what
    // makes them outlive the context above.
    _config: Config,
    #[cfg_attr(
        not(feature = "encode-vaapi-zero-copy"),
        allow(dead_code, reason = "only a DMA-BUF import needs the display again")
    )]
    display: Rc<Display>,
}

/// Hardware H.264 encoder backed by VA-API.
pub struct VaapiEncoder {
    session: Session,
    config: EncoderConfig,
    /// Coded dimensions, always a whole number of macroblocks.
    dims: (u32, u32),
    /// Visible dimensions: what the SPS crops the coded picture back to.
    visible: (u32, u32),
    /// `frame_num` of the next picture.
    frame_num: u16,
    /// Display order count of the next picture, in H.264's 2x units.
    pic_order_cnt: u16,
    /// Which of the two reconstructions the next picture is written into.
    target: usize,
    /// Set by [`VideoEncoder::request_keyframe`] and by the `IDR_PERIOD`
    /// backstop; cleared once the IDR is actually emitted.
    force_idr: bool,
    /// Frames emitted since the last IDR.
    since_idr: u32,
    /// The quantizer, when the driver leaves it to this side.
    qp: QpController,
    /// What the current sequence's pictures come in as, which is what its
    /// SPS names the colour matrix of. `None` before the first picture.
    input: Option<InputKind>,
    /// Whether a DMA-BUF frame has already failed to encode without a
    /// readback on this machine; the rest of the session reads frames back
    /// instead of failing the same way every frame (ADR 0088).
    #[cfg(feature = "encode-vaapi-zero-copy")]
    gpu_refused: bool,
}

/// What a picture was handed to the driver as (ADR 0088).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputKind {
    /// NV12 this side converted and uploaded, with BT.601.
    Nv12Upload,
    /// The compositor's `BGRx` buffer itself, which the driver converts —
    /// with BT.709 on the Intel driver it was measured on.
    #[cfg(feature = "encode-vaapi-zero-copy")]
    DmaBufImport,
}

impl InputKind {
    /// The matrix the SPS has to name for pictures that came in this way.
    const fn matrix(self) -> headers::Matrix {
        match self {
            Self::Nv12Upload => headers::Matrix::Bt601,
            #[cfg(feature = "encode-vaapi-zero-copy")]
            Self::DmaBufImport => headers::Matrix::Bt709,
        }
    }
}

/// A render node that can encode, and how (ADR 0088).
struct Device {
    display: Rc<Display>,
    driver: Option<String>,
    entrypoint: VAEntrypoint::Type,
    rate_control: RateControl,
}

/// The kernel driver behind a DRM node, from sysfs: `/dev/dri/renderD128`
/// is `/sys/class/drm/renderD128/device/driver`, a link whose last component
/// is the driver's name.
fn node_driver(node: &std::path::Path) -> Option<String> {
    let name = node.file_name()?;
    let link = std::fs::read_link(
        std::path::Path::new("/sys/class/drm")
            .join(name)
            .join("device/driver"),
    )
    .ok()?;
    Some(link.file_name()?.to_string_lossy().into_owned())
}

/// Opens the first render node that can encode `profile`, trying the ones
/// whose kernel driver is `preferred` before the rest.
///
/// Not libva's own "first node that initializes": on a machine with two GPUs
/// that node can be one with a decode-only driver, and the one that encodes is
/// the second; and a DMA-BUF is only imported without a copy by the GPU that
/// made it (ADR 0088).
fn open_device(
    profile: VAProfile::Type,
    codec: VideoCodec,
    preferred: Option<&str>,
) -> Result<Device> {
    let mut nodes: Vec<(std::path::PathBuf, Option<String>)> = DrmDeviceIterator::default()
        .map(|node| {
            let driver = node_driver(&node);
            (node, driver)
        })
        .collect();
    nodes.sort_by_key(|(_, driver)| preferred.is_none() || driver.as_deref() != preferred);

    let mut last = MediaError::EncoderUnavailable(
        "no VA-API display could be opened on any DRM device".to_owned(),
    );
    for (node, driver) in nodes {
        let Ok(display) = Display::open_drm_display(&node) else {
            continue;
        };
        let entrypoint = match display.query_config_entrypoints(profile) {
            Ok(offered) => encode_entrypoint(&offered),
            Err(error) => {
                last = MediaError::EncoderUnavailable(format!("vaQueryConfigEntrypoints: {error}"));
                continue;
            }
        };
        let Some(entrypoint) = entrypoint else {
            last = MediaError::EncoderUnavailable(format!(
                "this VA-API driver has no {codec:?} encode entrypoint"
            ));
            continue;
        };
        match RateControl::offered_by(&display, profile, entrypoint) {
            Ok(rate_control) => {
                return Ok(Device {
                    display,
                    driver,
                    entrypoint,
                    rate_control,
                });
            }
            Err(error) => last = error,
        }
    }
    Err(last)
}

// Mirrors the Media Foundation encoder's `Debug`: driver state is neither
// printable nor safe to log, only the settings that matter for a log line.
impl std::fmt::Debug for VaapiEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaapiEncoder")
            .field("config", &self.config)
            .field("dims", &self.dims)
            .field("rate_control", &self.session.rate_control)
            .finish_non_exhaustive()
    }
}

// SAFETY: `cros_libva` builds its handles on `Rc` because libva objects are
// not internally synchronized, which makes them `!Send` by inference. The
// encoder owns its entire session — display, config, context, surfaces and
// coded buffer are all reachable only through this struct and none of them is
// cloned out of it — so moving the whole thing to another thread moves the
// only reference to each. What libva forbids is *concurrent* use of one
// display from two threads, and `VideoEncoder: Send` (not `Sync`) is exactly
// the promise that only one thread touches it at a time. The same reasoning
// the Media Foundation module records for its COM pointers.
#[allow(
    unsafe_code,
    reason = "cros-libva's Rc-based handles are !Send by inference; the encoder owns the whole session exclusively. See the SAFETY note above."
)]
unsafe impl Send for VaapiEncoder {}

impl VaapiEncoder {
    /// Builds an encoder for `config` at the probe geometry; the first
    /// `encode` reopens the session at the real frame size.
    ///
    /// # Errors
    /// [`MediaError::EncoderUnavailable`] when no VA-API device offers a
    /// usable H.264 encode entrypoint.
    pub fn new(config: EncoderConfig) -> Result<Self> {
        Self::open(PROBE_WIDTH, PROBE_HEIGHT, config)
    }

    /// The whole open sequence, shared by the constructor and the probe so
    /// the two can never disagree.
    fn open(width: u32, height: u32, config: EncoderConfig) -> Result<Self> {
        Self::open_on(width, height, config, None)
    }

    /// [`Self::open`], on a render node of the `preferred` kernel driver when
    /// there is one that encodes.
    fn open_on(
        width: u32,
        height: u32,
        config: EncoderConfig,
        preferred: Option<&str>,
    ) -> Result<Self> {
        // Before anything touches libva: a codec this backend has no profile
        // for must be refused without opening a display (ADR 0069, ADR 0071).
        let profile = va_profile(config.codec)?;
        let visible = (width, height);
        let (width, height) = aligned_dims(width, height, coding_block(config.codec)?);

        let Device {
            display,
            driver,
            entrypoint,
            rate_control,
        } = open_device(profile, config.codec, preferred)?;

        let config_handle = display
            .create_config(
                vec![
                    VAConfigAttrib {
                        type_: VAConfigAttribType::VAConfigAttribRTFormat,
                        value: VA_RT_FORMAT_YUV420,
                    },
                    VAConfigAttrib {
                        type_: VAConfigAttribType::VAConfigAttribRateControl,
                        value: rate_control.va_value(),
                    },
                ],
                profile,
                entrypoint,
            )
            .map_err(|e| MediaError::EncoderUnavailable(format!("vaCreateConfig: {e}")))?;

        // Three surfaces: the picture being encoded and the two
        // reconstructions that take turns being the reference.
        let mut surfaces = display
            .create_surfaces(
                VA_RT_FORMAT_YUV420,
                Some(VA_FOURCC_NV12),
                width,
                height,
                Some(UsageHint::USAGE_HINT_ENCODER),
                vec![(), (), ()],
            )
            .map_err(|e| MediaError::EncoderUnavailable(format!("vaCreateSurfaces: {e}")))?;
        if surfaces.len() < 3 {
            return Err(MediaError::EncoderUnavailable(
                "the driver returned fewer encode surfaces than requested".to_owned(),
            ));
        }

        let context = display
            .create_context(&config_handle, width, height, Some(&surfaces), true)
            .map_err(|e| MediaError::EncoderUnavailable(format!("vaCreateContext: {e}")))?;

        // A generous ceiling on one coded frame: a keyframe of a full screen
        // at a high bitrate is the worst case, and the driver only writes as
        // much as it produces.
        let coded = context
            .create_enc_coded(coded_buffer_size(width, height))
            .map_err(|e| MediaError::EncoderUnavailable(format!("vaCreateBuffer(coded): {e}")))?;

        let nv12_format = display
            .query_image_formats()
            .map_err(|e| MediaError::EncoderUnavailable(format!("vaQueryImageFormats: {e}")))?
            .into_iter()
            .find(|format| format.fourcc == VA_FOURCC_NV12)
            .ok_or_else(|| {
                MediaError::EncoderUnavailable(
                    "this VA-API driver publishes no NV12 image format to upload through"
                        .to_owned(),
                )
            })?;

        let missing =
            || MediaError::EncoderUnavailable("an encode surface went missing".to_owned());
        let second = surfaces.pop().ok_or_else(missing)?;
        let first = surfaces.pop().ok_or_else(missing)?;
        let input = surfaces.pop().ok_or_else(missing)?;

        tracing::debug!(
            ?rate_control,
            ?driver,
            low_power = entrypoint == VAEntrypoint::VAEntrypointEncSliceLP,
            "VA-API H.264 session opened"
        );

        Ok(Self {
            session: Session {
                coded,
                nv12_format,
                input,
                reconstructions: [first, second],
                context,
                rate_control,
                driver,
                _config: config_handle,
                display,
            },
            config,
            dims: (width, height),
            visible,
            frame_num: 0,
            pic_order_cnt: 0,
            target: 0,
            force_idr: true,
            since_idr: 0,
            qp: QpController::new(),
            input: None,
            #[cfg(feature = "encode-vaapi-zero-copy")]
            gpu_refused: false,
        })
    }

    /// Reopens the session at `width` x `height`, which the first real frame
    /// triggers because the probe geometry is not the screen's.
    ///
    /// The quantizer carries over: the controller has learned what this
    /// content costs, and a new session is still the same desktop.
    fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        let driver = self.session.driver.clone();
        self.reopen(width, height, driver.as_deref())
    }

    /// Replaces the session with one at `width` x `height` on a node of
    /// `driver`, keeping what this encoder has learned about the machine.
    fn reopen(&mut self, width: u32, height: u32, driver: Option<&str>) -> Result<()> {
        let qp = self.qp;
        #[cfg(feature = "encode-vaapi-zero-copy")]
        let gpu_refused = self.gpu_refused;
        let reopened = Self::open_on(width, height, self.config, driver)?;
        *self = reopened;
        self.qp = qp;
        #[cfg(feature = "encode-vaapi-zero-copy")]
        {
            self.gpu_refused = gpu_refused;
        }
        Ok(())
    }

    /// Readies the next picture of `width` x `height` arriving as `kind`, and
    /// says whether it is an IDR.
    ///
    /// A change of input kind is an IDR too: the SPS names the colour matrix,
    /// and the driver's own conversion and this side's are not the same one.
    fn start_picture(&mut self, width: u32, height: u32, kind: InputKind) -> Result<bool> {
        let aligned = aligned_dims(width, height, coding_block(self.config.codec)?);
        if aligned != self.dims || (width, height) != self.visible {
            self.resize(width, height)?;
        }
        let idr = self.force_idr || self.since_idr >= IDR_PERIOD || self.input != Some(kind);
        if idr {
            self.frame_num = 0;
            self.pic_order_cnt = 0;
        }
        self.input = Some(kind);
        Ok(idr)
    }

    /// Completes a picture `data` came back for.
    fn finish_picture(&mut self, timestamp_us: u64, idr: bool, data: Vec<u8>) -> EncodedFrame {
        let data = if idr && !headers::contains_nal(&data, headers::NAL_SPS) {
            with_parameter_sets(self, data)
        } else {
            data
        };
        if idr {
            self.force_idr = false;
            self.since_idr = 0;
        } else {
            self.since_idr = self.since_idr.saturating_add(1);
        }
        self.frame_num = self.frame_num.wrapping_add(1);
        self.pic_order_cnt = self.pic_order_cnt.wrapping_add(2);
        // The picture just written is the next one's reference.
        self.target = 1 - self.target;
        if self.session.rate_control == RateControl::ConstantQp {
            self.qp.observe(data.len(), idr, self.config);
        }
        EncodedFrame {
            keyframe: idr,
            timestamp_us,
            data,
        }
    }

    /// Encodes a frame that is still the compositor's DMA-BUF, by importing it
    /// as the picture the driver reads (ADR 0088).
    ///
    /// # Errors
    /// [`MediaError::Encode`] when the buffer is not this encoder's GPU's and
    /// no node of the GPU that made it encodes, or when the driver refuses to
    /// import or encode it — which the caller answers by reading this frame
    /// back and the rest of the session's too.
    #[cfg(feature = "encode-vaapi-zero-copy")]
    fn encode_dmabuf(&mut self, frame: &Frame, dmabuf: &DmaBuf) -> Result<EncodedFrame> {
        let exporter = dmabuf.exporter();
        if exporter.is_none() || exporter != self.session.driver {
            // A buffer from another GPU imports, if at all, as a copy through
            // main memory — the cost this path exists to remove. Move to a
            // node of the GPU that made it, if one of those encodes.
            self.reopen(self.visible.0, self.visible.1, exporter.as_deref())?;
            if exporter.is_none() || exporter != self.session.driver {
                return Err(MediaError::Encode(format!(
                    "the frame's buffer comes from {exporter:?}, and no encoder on that GPU is available"
                )));
            }
        }
        let va_fourcc = va_fourcc_of(dmabuf.drm_format()).ok_or_else(|| {
            MediaError::Encode("the frame's buffer is in a format VA-API is not given".to_owned())
        })?;
        let visible = (dmabuf.width() & !1, dmabuf.height() & !1);
        let idr = self.start_picture(visible.0, visible.1, InputKind::DmaBufImport)?;

        let descriptor = PrimeImport::of(dmabuf, va_fourcc)?;
        let mut surface = self
            .session
            .display
            .create_surfaces(
                VA_RT_FORMAT_RGB32,
                Some(va_fourcc),
                dmabuf.width(),
                dmabuf.height(),
                Some(UsageHint::USAGE_HINT_ENCODER),
                vec![descriptor],
            )
            .map_err(|e| MediaError::Encode(format!("importing the DMA-BUF: {e}")))?
            .pop()
            .ok_or_else(|| {
                MediaError::Encode("the DMA-BUF import returned no surface".to_owned())
            })?;

        let buffers = picture_buffers(self, idr)?;
        let data = submit(
            Rc::clone(&self.session.context),
            &self.session.coded,
            u64::from(self.pic_order_cnt),
            buffers,
            &mut surface,
        )?;
        Ok(self.finish_picture(frame.timestamp_us, idr, data))
    }
}

/// The VA fourcc of a surface imported from a DMA-BUF of `drm_format`.
#[cfg(feature = "encode-vaapi-zero-copy")]
fn va_fourcc_of(drm_format: u32) -> Option<u32> {
    use crate::capture::pipewire_stream::{DRM_FORMAT_ARGB8888, DRM_FORMAT_XRGB8888};
    match drm_format {
        DRM_FORMAT_XRGB8888 => Some(VA_FOURCC_BGRX),
        DRM_FORMAT_ARGB8888 => Some(VA_FOURCC_BGRA),
        _ => None,
    }
}

/// A DMA-BUF handed to `vaCreateSurfaces` as the memory of a surface
/// (`VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2`).
///
/// Owns its own duplicate of the descriptor: `cros-libva` gives a descriptor to
/// the surface for the surface's whole life, and the frame the buffer came in
/// with can be dropped first.
#[cfg(feature = "encode-vaapi-zero-copy")]
struct PrimeImport {
    fd: std::os::fd::OwnedFd,
    fourcc: u32,
    drm_format: u32,
    modifier: u64,
    size: u32,
    offset: u32,
    stride: u32,
    width: u32,
    height: u32,
}

#[cfg(feature = "encode-vaapi-zero-copy")]
impl PrimeImport {
    fn of(dmabuf: &DmaBuf, fourcc: u32) -> Result<Self> {
        let fd = dmabuf
            .fd()
            .try_clone_to_owned()
            .map_err(|e| MediaError::Encode(format!("duplicating the DMA-BUF descriptor: {e}")))?;
        Ok(Self {
            fd,
            fourcc,
            drm_format: dmabuf.drm_format(),
            modifier: dmabuf.modifier(),
            size: dmabuf.size(),
            offset: dmabuf.offset(),
            stride: dmabuf.stride(),
            width: dmabuf.width(),
            height: dmabuf.height(),
        })
    }
}

#[cfg(feature = "encode-vaapi-zero-copy")]
impl ExternalBufferDescriptor for PrimeImport {
    const MEMORY_TYPE: MemoryType = MemoryType::DrmPrime2;
    type DescriptorAttribute = VADRMPRIMESurfaceDescriptor;

    fn va_surface_attribute(&mut self) -> VADRMPRIMESurfaceDescriptor {
        use std::os::fd::AsRawFd as _;

        // One object holding one layer of one plane: a BGRx picture.
        let mut descriptor = VADRMPRIMESurfaceDescriptor {
            fourcc: self.fourcc,
            width: self.width,
            height: self.height,
            num_objects: 1,
            num_layers: 1,
            ..VADRMPRIMESurfaceDescriptor::default()
        };
        descriptor.objects[0].fd = self.fd.as_raw_fd();
        descriptor.objects[0].size = self.size;
        descriptor.objects[0].drm_format_modifier = self.modifier;
        descriptor.layers[0].drm_format = self.drm_format;
        descriptor.layers[0].num_planes = 1;
        descriptor.layers[0].object_index[0] = 0;
        descriptor.layers[0].offset[0] = self.offset;
        descriptor.layers[0].pitch[0] = self.stride;
        descriptor
    }
}

/// Rounds up to whole [`MACROBLOCK`]s, which is the only geometry H.264
/// codes in.
fn aligned_dims(width: u32, height: u32, block: u32) -> (u32, u32) {
    let block = block.max(1);
    (
        width.div_ceil(block).max(1) * block,
        height.div_ceil(block).max(1) * block,
    )
}

/// Bytes to reserve for one coded frame.
///
/// The pathological case is an IDR of noise, which no rate controller can
/// make small; half the uncompressed luma size is the conventional headroom
/// and costs one allocation per session, not per frame.
fn coded_buffer_size(width: u32, height: u32) -> usize {
    let pixels = (width as usize).saturating_mul(height as usize);
    pixels.saturating_add(pixels / 2).max(1 << 16)
}

/// The H.264 level whose limits a picture of `mbs` macroblocks at `fps`
/// frames per second fits, as `level_idc` (level x 10).
///
/// From Table A-1 of the specification: the smallest level whose maximum frame
/// size (in macroblocks) and maximum macroblock rate both hold. Level 3 is the
/// floor, because nothing a desktop shares is smaller than that.
fn level_idc(mbs: u32, fps: u8) -> u8 {
    /// `(level_idc, MaxFS, MaxMBPS)`, in increasing order.
    const LEVELS: [(u8, u32, u32); 11] = [
        (30, 1_620, 40_500),
        (31, 3_600, 108_000),
        (32, 5_120, 216_000),
        (40, 8_192, 245_000),
        (42, 8_704, 522_240),
        (50, 22_080, 589_824),
        (51, 36_864, 983_040),
        (52, 36_864, 2_073_600),
        (60, 139_264, 4_177_920),
        (61, 139_264, 8_355_840),
        (62, 139_264, 16_711_680),
    ];
    let rate = mbs.saturating_mul(u32::from(fps));
    LEVELS
        .iter()
        .find(|(_, max_fs, max_mbps)| mbs <= *max_fs && rate <= *max_mbps)
        .map_or(62, |(level, _, _)| *level)
}

impl VideoEncoder for VaapiEncoder {
    fn encode(&mut self, frame: &Frame) -> Result<EncodedFrame> {
        // The frame never left the GPU, and that is the whole point of
        // ADR 0088: hand the compositor's buffer to the driver rather than
        // reading it back first. A refusal — a buffer from another GPU, a
        // driver that will not import it — is not a failed frame: it falls
        // through to the readback below, once, for the rest of the session.
        #[cfg(feature = "encode-vaapi-zero-copy")]
        if let Some(dmabuf) = frame.dmabuf.as_ref()
            && !self.gpu_refused
        {
            match self.encode_dmabuf(frame, dmabuf) {
                Ok(encoded) => return Ok(encoded),
                Err(error) => {
                    tracing::info!(
                        %error,
                        "this encoder will not take the compositor's buffers; \
                         the rest of this session goes through main memory"
                    );
                    self.gpu_refused = true;
                }
            }
        }

        let (nv12, src_width, src_height) = bgra_to_nv12(frame)?;
        let idr = self.start_picture(src_width, src_height, InputKind::Nv12Upload)?;
        upload_nv12(
            &self.session.input,
            self.session.nv12_format,
            &nv12,
            (src_width, src_height),
            self.dims,
        )?;
        let buffers = picture_buffers(self, idr)?;
        let data = submit(
            Rc::clone(&self.session.context),
            &self.session.coded,
            u64::from(self.pic_order_cnt),
            buffers,
            &mut self.session.input,
        )?;
        Ok(self.finish_picture(frame.timestamp_us, idr, data))
    }

    fn set_bitrate(&mut self, bitrate_kbps: u32) -> Result<()> {
        // Nothing is rebuilt: the rate-control buffer travels with the next
        // picture, and under constant QP the controller reads the config on
        // the next frame it observes — which is what makes this usable at
        // `ABR_ADJUST_MAX_RATE_PER_SEC` (§11, §14).
        self.config.bitrate_kbps = bitrate_kbps;
        Ok(())
    }

    fn request_keyframe(&mut self) -> Result<()> {
        self.force_idr = true;
        Ok(())
    }

    fn kind(&self) -> EncoderKind {
        EncoderKind::Hardware
    }
}

/// `data` with the SPS and PPS this encoder describes in front of it, for a
/// driver that wrote an IDR without them (ADR 0088).
///
/// Written from the same values the parameter buffers carried, because they
/// are the ones the driver used to write the slice headers that follow; a
/// parameter set that disagreed with them would be a picture no decoder
/// reads correctly.
fn with_parameter_sets(encoder: &VaapiEncoder, data: Vec<u8>) -> Vec<u8> {
    let (width, height) = encoder.dims;
    let (visible_width, visible_height) = encoder.visible;
    let mbs_wide = width / MACROBLOCK;
    let mbs_high = height / MACROBLOCK;
    let sequence = headers::SequenceHeader {
        level_idc: level_idc(mbs_wide * mbs_high, encoder.config.fps),
        width_in_mbs: mbs_wide,
        height_in_mbs: mbs_high,
        crop_right: width.saturating_sub(visible_width) / 2,
        crop_bottom: height.saturating_sub(visible_height) / 2,
        log2_max_frame_num_minus4: LOG2_MAX_FRAME_NUM_MINUS4,
        log2_max_poc_lsb_minus4: LOG2_MAX_POC_LSB_MINUS4,
        max_num_ref_frames: MAX_REF_FRAMES,
        matrix: encoder
            .input
            .map_or(headers::Matrix::Bt601, InputKind::matrix),
    };
    let mut out = headers::sequence_parameter_set(&sequence);
    out.extend(headers::picture_parameter_set(INITIAL_QP));
    out.extend(data);
    out
}

/// The parameter buffers of the next picture.
///
/// Assembled by the helpers below rather than inline: VA-API takes an H.264
/// sequence, picture and slice header as three separate structs and every
/// field of each has to be named, so one function holding all of them is
/// neither readable nor reviewable.
fn picture_buffers(encoder: &VaapiEncoder, idr: bool) -> Result<Vec<cros_libva::Buffer>> {
    let (width, height) = encoder.dims;
    let block = coding_block(encoder.config.codec)?;
    let units_wide = u16::try_from(width / block).unwrap_or(u16::MAX);
    let units_high = u16::try_from(height / block).unwrap_or(u16::MAX);

    let target_id = encoder.session.reconstructions[encoder.target].id();
    let reference_id = encoder.session.reconstructions[1 - encoder.target].id();
    let coded_id = encoder.session.coded.id();

    let mut buffers = Vec::with_capacity(4);
    // Still a match rather than a straight call: every buffer below is
    // H.264's own, and a codec this backend cannot drive has to be refused
    // here rather than handed the H.264 builders — a stream the driver
    // accepts and no decoder can read (§11's mutual-hardware-support rule).
    match encoder.config.codec {
        VideoCodec::H264 => {
            // Sequence header, resent with every IDR: a guest that joined
            // mid-stream needs the parameter sets describing what follows.
            if idr {
                buffers.push(sequence_buffer(encoder, units_wide, units_high)?);
            }
            // Sent with every picture, which is what lets `set_bitrate` take
            // effect without rebuilding anything. A constant-QP driver is
            // told its quantizer through the slice instead.
            if encoder.session.rate_control != RateControl::ConstantQp {
                buffers.push(rate_control_buffer(encoder)?);
            }
            buffers.push(picture_buffer(
                encoder,
                coded_id,
                target_id,
                reference_id,
                idr,
            )?);
            buffers.push(slice_buffer(
                encoder,
                units_wide,
                units_high,
                reference_id,
                idr,
            )?);
        }
        // Unreachable in practice — `VaapiEncoder::open` refuses anything
        // `va_profile` has no answer for — but written as a refusal rather
        // than a `matches!` or an `unreachable!`, so a codec added later
        // fails loudly instead of encoding as H.264 (§21).
        VideoCodec::Av1 => {
            return Err(MediaError::Encode(
                "the VA-API backend cannot drive AV1 encoding (ADR 0069)".to_owned(),
            ));
        }
    }
    Ok(buffers)
}

/// Submits one picture read from `input` with `buffers`, and returns its
/// bitstream.
///
/// Generic over where `input`'s memory comes from, which is the whole of the
/// difference between a picture this side uploaded and one that is still the
/// compositor's buffer (ADR 0088).
fn submit<D: cros_libva::SurfaceMemoryDescriptor>(
    context: Rc<Context>,
    coded: &EncCodedBuffer,
    timestamp: u64,
    buffers: Vec<cros_libva::Buffer>,
    input: &mut Surface<D>,
) -> Result<Vec<u8>> {
    use cros_libva::Picture;

    let mut picture = Picture::new(timestamp, context, input);
    for buffer in buffers {
        picture.add_buffer(buffer);
    }

    // Submit, then wait. `sync` is where a wedged encoder would block, so the
    // driver's own completion is what is waited on rather than a poll loop of
    // our own.
    let picture = picture
        .begin()
        .map_err(|e| MediaError::Encode(format!("vaBeginPicture: {e}")))?
        .render()
        .map_err(|e| MediaError::Encode(format!("vaRenderPicture: {e}")))?
        .end()
        .map_err(|e| MediaError::Encode(format!("vaEndPicture: {e}")))?;
    let _synced = picture
        .sync()
        .map_err(|(e, _)| MediaError::Encode(format!("vaSyncSurface: {e}")))?;

    // A coded buffer can come back as several segments; concatenating them is
    // the whole of the reassembly.
    let mut data = Vec::new();
    {
        let mapped = MappedCodedBuffer::new(coded)
            .map_err(|e| MediaError::Encode(format!("mapping the coded buffer: {e}")))?;
        for segment in mapped.segments() {
            data.extend_from_slice(segment.buf);
        }
    }
    if data.is_empty() {
        return Err(MediaError::Encode(
            "the driver produced an empty bitstream".to_owned(),
        ));
    }
    Ok(data)
}

/// A `VAPictureH264` slot meaning "nothing here".
fn invalid_picture() -> PictureH264 {
    PictureH264::new(VA_INVALID_ID, 0, VA_PICTURE_H264_INVALID, 0, 0)
}

/// The single reference of [`MAX_REF_FRAMES`]: the frame before this one.
fn previous_reference(encoder: &VaapiEncoder, reference_id: u32) -> PictureH264 {
    PictureH264::new(
        reference_id,
        u32::from(encoder.frame_num.saturating_sub(1)),
        cros_libva::VA_PICTURE_H264_SHORT_TERM_REFERENCE,
        i32::from(encoder.pic_order_cnt.saturating_sub(2)),
        i32::from(encoder.pic_order_cnt.saturating_sub(2)),
    )
}

/// The H.264 sequence header (SPS) in the shape VA-API wants it.
fn sequence_buffer(
    encoder: &VaapiEncoder,
    mbs_wide: u16,
    mbs_high: u16,
) -> Result<cros_libva::Buffer> {
    let (width, height) = encoder.dims;
    let (visible_width, visible_height) = encoder.visible;
    let seq_fields = H264EncSeqFields::new(
        1, // chroma_format_idc: 4:2:0
        1, // frame_mbs_only_flag: progressive only
        0, // mb_adaptive_frame_field_flag
        0, // seq_scaling_matrix_present_flag
        1, // direct_8x8_inference_flag
        LOG2_MAX_FRAME_NUM_MINUS4,
        0, // pic_order_cnt_type
        LOG2_MAX_POC_LSB_MINUS4,
        0, // delta_pic_order_always_zero_flag
    );
    // The coded picture is macroblock-aligned; a screen usually is not. Crop
    // offsets are in chroma units for 4:2:0, hence the halving — a 1080-line
    // screen codes as 1088 and crops the eight lines back off.
    let crop = if width != visible_width || height != visible_height {
        Some(cros_libva::H264EncFrameCropOffsets {
            left: 0,
            right: width.saturating_sub(visible_width) / 2,
            top: 0,
            bottom: height.saturating_sub(visible_height) / 2,
        })
    } else {
        None
    };
    let seq = EncSequenceParameterBufferH264::new(
        0,
        level_idc(
            u32::from(mbs_wide) * u32::from(mbs_high),
            encoder.config.fps,
        ),
        IDR_PERIOD,
        IDR_PERIOD,
        1, // ip_period: no B-frames
        encoder.config.bitrate_kbps.saturating_mul(BITS_PER_KBIT),
        MAX_REF_FRAMES,
        mbs_wide,
        mbs_high,
        &seq_fields,
        0,
        0,
        0,
        0,
        0,
        [0; 256],
        crop,
        None,
        0,
        0,
        0,
        // H.264 counts time in field ticks, so the tick rate is twice the
        // frame rate.
        1,
        u32::from(encoder.config.fps).saturating_mul(2),
    );
    encoder
        .session
        .context
        .create_buffer(BufferType::EncSequenceParameter(
            EncSequenceParameter::H264(seq),
        ))
        .map_err(|e| MediaError::Encode(format!("sequence parameter buffer: {e}")))
}

/// The rate-control misc buffer: how a bitrate change reaches a driver that
/// controls the rate itself, without rebuilding the session.
fn rate_control_buffer(encoder: &VaapiEncoder) -> Result<cros_libva::Buffer> {
    let rc = EncMiscParameterRateControl::new(
        encoder.config.bitrate_kbps.saturating_mul(BITS_PER_KBIT),
        100, // target_percentage: spend the whole budget
        // Rate-control window in milliseconds: one second, matching the
        // period `AbrController` itself adjusts on.
        BITS_PER_KBIT,
        u32::from(INITIAL_QP),
        0, // min_qp: let the driver decide
        0, // basic_unit_size
        RcFlags::new(0, 0, 0, 0, 0, 0, 0, 0, 0),
        0,
        0,
        0,
        0,
    );
    encoder
        .session
        .context
        .create_buffer(BufferType::EncMiscParameter(EncMiscParameter::RateControl(
            rc,
        )))
        .map_err(|e| MediaError::Encode(format!("rate control buffer: {e}")))
}

/// The H.264 picture header (PPS plus this picture's own fields).
fn picture_buffer(
    encoder: &VaapiEncoder,
    coded_id: u32,
    target_id: u32,
    reference_id: u32,
    idr: bool,
) -> Result<cros_libva::Buffer> {
    let curr_pic = PictureH264::new(
        target_id,
        u32::from(encoder.frame_num),
        cros_libva::VA_PICTURE_H264_SHORT_TERM_REFERENCE,
        i32::from(encoder.pic_order_cnt),
        i32::from(encoder.pic_order_cnt),
    );
    let mut refs = std::array::from_fn::<_, 16, _>(|_| invalid_picture());
    if !idr {
        refs[0] = previous_reference(encoder, reference_id);
    }
    let pic_fields = H264EncPicFields::new(
        u32::from(idr), // idr_pic_flag
        1,              // reference_pic_flag: every frame is a reference here
        0,              // entropy_coding_mode_flag: CAVLC, per Constrained Baseline
        0,              // weighted_pred_flag
        0,              // weighted_bipred_idc
        0,              // constrained_intra_pred_flag
        0,              // transform_8x8_mode_flag: not in Baseline
        1,              // deblocking_filter_control_present_flag
        0,              // redundant_pic_cnt_present_flag
        0,              // pic_order_present_flag
        0,              // pic_scaling_matrix_present_flag
    );
    let pic = EncPictureParameterBufferH264::new(
        curr_pic,
        refs,
        coded_id,
        0,
        0,
        // last_picture: never. It tells the driver the stream ends here, and
        // it answers with an end-of-sequence NAL after which no P-frame is
        // valid (ADR 0088).
        0,
        encoder.frame_num,
        INITIAL_QP,
        0,
        0,
        0,
        0,
        &pic_fields,
    );
    encoder
        .session
        .context
        .create_buffer(BufferType::EncPictureParameter(EncPictureParameter::H264(
            pic,
        )))
        .map_err(|e| MediaError::Encode(format!("picture parameter buffer: {e}")))
}

/// The H.264 slice header. One slice per picture: slicing buys error
/// resilience that a reliable QUIC stream already provides (§11).
fn slice_buffer(
    encoder: &VaapiEncoder,
    mbs_wide: u16,
    mbs_high: u16,
    reference_id: u32,
    idr: bool,
) -> Result<cros_libva::Buffer> {
    let total_mbs = u32::from(mbs_wide) * u32::from(mbs_high);
    let mut list0 = std::array::from_fn::<_, 32, _>(|_| invalid_picture());
    if !idr {
        list0[0] = previous_reference(encoder, reference_id);
    }
    let list1 = std::array::from_fn::<_, 32, _>(|_| invalid_picture());
    // Relative to the PPS's `pic_init_qp`. Zero under a driver's own rate
    // control, which ignores it; the controller's quantizer under constant QP.
    let slice_qp_delta = if encoder.session.rate_control == RateControl::ConstantQp {
        i8::try_from(i16::from(encoder.qp.qp()) - i16::from(INITIAL_QP)).unwrap_or(0)
    } else {
        0
    };
    let slice = EncSliceParameterBufferH264::new(
        0,
        total_mbs,
        VA_INVALID_ID,
        if idr { SLICE_TYPE_I } else { SLICE_TYPE_P },
        0,
        0,
        encoder.pic_order_cnt,
        0,
        [0; 2],
        0,
        0,
        0,
        0,
        list0,
        list1,
        0,
        0,
        0,
        [0; 32],
        [0; 32],
        0,
        [[0; 2]; 32],
        [[0; 2]; 32],
        0,
        [0; 32],
        [0; 32],
        0,
        [[0; 2]; 32],
        [[0; 2]; 32],
        0,
        slice_qp_delta,
        0,
        0,
        0,
    );
    encoder
        .session
        .context
        .create_buffer(BufferType::EncSliceParameter(EncSliceParameter::H264(
            slice,
        )))
        .map_err(|e| MediaError::Encode(format!("slice parameter buffer: {e}")))
}

/// Copies a `visible`-sized NV12 picture into a `coded`-sized VA surface,
/// honouring the driver's own stride, which is rarely the picture width.
///
/// The coded surface is a whole number of macroblocks and the picture usually
/// is not. The margin is filled by repeating the last row and column rather
/// than left as whatever the surface held: the SPS crops it off, but the
/// encoder still codes it, and a margin of stale bytes costs bits and bleeds
/// into the prediction of the pixels next to it.
fn upload_nv12(
    surface: &Surface<()>,
    format: cros_libva::VAImageFormat,
    nv12: &[u8],
    visible: (u32, u32),
    coded: (u32, u32),
) -> Result<()> {
    use cros_libva::Image;

    let (visible_width, visible_height) = (visible.0 as usize, visible.1 as usize);
    let (coded_width, coded_height) = (coded.0 as usize, coded.1 as usize);
    let luma_len = visible_width * visible_height;
    if visible_width == 0
        || visible_height == 0
        || visible_width > coded_width
        || visible_height > coded_height
        || nv12.len() < luma_len + luma_len / 2
    {
        return Err(MediaError::Encode(
            "the NV12 picture does not fit the surface it is being uploaded into".to_owned(),
        ));
    }

    let mut image = Image::create_from(surface, format, coded, coded)
        .map_err(|e| MediaError::Encode(format!("vaCreateImage: {e}")))?;
    let image_inner = *image.image();
    let offsets = image_inner.offsets;
    let pitches = image_inner.pitches;
    let dst = image.as_mut();

    // Luma, then interleaved chroma at half the rows: each plane is its
    // visible rows, each widened to the coded width by repeating its last
    // sample, and then the last such row repeated down to the coded height.
    let planes = [
        (
            0usize,
            0usize,
            visible_width,
            visible_height,
            coded_height,
            1usize,
        ),
        (
            1,
            luma_len,
            visible_width,
            visible_height / 2,
            coded_height / 2,
            2,
        ),
    ];
    for (plane, source_offset, row_bytes, rows, coded_rows, sample_bytes) in planes {
        let pitch = pitches[plane] as usize;
        let offset = offsets[plane] as usize;
        for row in 0..coded_rows {
            let source_row = row.min(rows - 1);
            let source = source_offset + source_row * row_bytes;
            let start = offset + row * pitch;
            let Some(target) = dst.get_mut(start..start + coded_width) else {
                return Err(MediaError::Encode(
                    "the mapped surface is smaller than its own plane".to_owned(),
                ));
            };
            target[..row_bytes].copy_from_slice(&nv12[source..source + row_bytes]);
            let last = &nv12[source + row_bytes - sample_bytes..source + row_bytes];
            for margin in target[row_bytes..].chunks_mut(sample_bytes) {
                margin.copy_from_slice(&last[..margin.len()]);
            }
        }
    }

    Ok(())
}

/// The H.264 parameter sets this encoder writes itself (ADR 0088).
///
/// Only what a Constrained Baseline stream of this encoder can contain, which
/// is what keeps it short enough to check field by field against the
/// specification (ITU-T H.264, 7.3.2.1.1 and 7.3.2.2, VUI in E.1.1).
mod headers {
    /// `nal_unit_type` of an IDR slice.
    pub(super) const NAL_IDR_SLICE: u8 = 5;
    /// `nal_unit_type` of a sequence parameter set.
    pub(super) const NAL_SPS: u8 = 7;
    /// `nal_unit_type` of a picture parameter set.
    const NAL_PPS: u8 = 8;
    /// `nal_ref_idc` of both parameter sets: they are always "reference".
    const NAL_REF_IDC_HIGHEST: u8 = 3;
    /// Constrained Baseline: `profile_idc` 66 with `constraint_set0_flag`
    /// and `constraint_set1_flag` set.
    const PROFILE_IDC_BASELINE: u32 = 66;
    /// `constraint_set0_flag` and `constraint_set1_flag`, then six zero bits.
    const CONSTRAINT_FLAGS_CONSTRAINED_BASELINE: u32 = 0b1100_0000;
    /// `video_format` of the VUI: unspecified.
    const VIDEO_FORMAT_UNSPECIFIED: u32 = 5;
    /// `colour_primaries` 1: the primaries of BT.709, which are sRGB's, and a
    /// desktop is an sRGB picture.
    const COLOUR_PRIMARIES_BT709: u32 = 1;
    /// `transfer_characteristics` 13: IEC 61966-2-1, the sRGB curve the
    /// desktop was drawn with.
    const TRANSFER_SRGB: u32 = 13;
    /// `log2_max_mv_length_*` at its maximum: no promise about motion vectors.
    const LOG2_MAX_MV_LENGTH_UNRESTRICTED: u32 = 16;

    /// Which matrix turned the desktop's RGB into this stream's YCbCr, so a
    /// decoder turns it back with the same one.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum Matrix {
        /// BT.601, which `encode::nv12` converts with.
        Bt601,
        /// BT.709, which the Intel driver converts a `BGRx` surface with before
        /// encoding it (measured; ADR 0088).
        #[cfg_attr(
            not(feature = "encode-vaapi-zero-copy"),
            allow(dead_code, reason = "only a DMA-BUF import is converted with it")
        )]
        Bt709,
    }

    impl Matrix {
        /// `matrix_coefficients` of the VUI.
        const fn code(self) -> u32 {
            match self {
                // SMPTE 170M, BT.601's 525-line form; the coefficients are
                // BT.601's.
                Self::Bt601 => 6,
                Self::Bt709 => 1,
            }
        }
    }

    /// The values an SPS has to agree on with the slices it describes.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) struct SequenceHeader {
        pub(super) level_idc: u8,
        pub(super) width_in_mbs: u32,
        pub(super) height_in_mbs: u32,
        /// Right crop, in the chroma sample units 4:2:0 counts it in.
        pub(super) crop_right: u32,
        /// Bottom crop, likewise.
        pub(super) crop_bottom: u32,
        pub(super) log2_max_frame_num_minus4: u32,
        pub(super) log2_max_poc_lsb_minus4: u32,
        pub(super) max_num_ref_frames: u32,
        pub(super) matrix: Matrix,
    }

    /// An Annex-B SPS NAL unit, start code included.
    pub(super) fn sequence_parameter_set(header: &SequenceHeader) -> Vec<u8> {
        let mut bits = BitWriter::default();
        bits.bits(PROFILE_IDC_BASELINE, 8);
        bits.bits(CONSTRAINT_FLAGS_CONSTRAINED_BASELINE, 8);
        bits.bits(u32::from(header.level_idc), 8);
        bits.ue(0); // seq_parameter_set_id
        bits.ue(header.log2_max_frame_num_minus4);
        bits.ue(0); // pic_order_cnt_type
        bits.ue(header.log2_max_poc_lsb_minus4);
        bits.ue(header.max_num_ref_frames);
        bits.flag(false); // gaps_in_frame_num_value_allowed_flag
        bits.ue(header.width_in_mbs.saturating_sub(1));
        bits.ue(header.height_in_mbs.saturating_sub(1));
        bits.flag(true); // frame_mbs_only_flag
        bits.flag(true); // direct_8x8_inference_flag
        let cropped = header.crop_right > 0 || header.crop_bottom > 0;
        bits.flag(cropped);
        if cropped {
            bits.ue(0); // frame_crop_left_offset
            bits.ue(header.crop_right);
            bits.ue(0); // frame_crop_top_offset
            bits.ue(header.crop_bottom);
        }
        bits.flag(true); // vui_parameters_present_flag
        write_vui(&mut bits, header);
        nal_unit(NAL_SPS, bits.finish())
    }

    /// The VUI: which colours the samples mean, and that no picture is ever
    /// held back for reordering.
    fn write_vui(bits: &mut BitWriter, header: &SequenceHeader) {
        bits.flag(false); // aspect_ratio_info_present_flag
        bits.flag(false); // overscan_info_present_flag
        bits.flag(true); // video_signal_type_present_flag
        bits.bits(VIDEO_FORMAT_UNSPECIFIED, 3);
        bits.flag(false); // video_full_range_flag: 16..=235, as nv12 writes
        bits.flag(true); // colour_description_present_flag
        bits.bits(COLOUR_PRIMARIES_BT709, 8);
        bits.bits(TRANSFER_SRGB, 8);
        bits.bits(header.matrix.code(), 8);
        bits.flag(false); // chroma_loc_info_present_flag
        bits.flag(false); // timing_info_present_flag
        bits.flag(false); // nal_hrd_parameters_present_flag
        bits.flag(false); // vcl_hrd_parameters_present_flag
        bits.flag(false); // pic_struct_present_flag
        // A decoder that knows nothing is reordered can show each picture the
        // moment it is decoded, which is the latency this stream exists for.
        bits.flag(true); // bitstream_restriction_flag
        bits.flag(true); // motion_vectors_over_pic_boundaries_flag
        bits.ue(0); // max_bytes_per_pic_denom
        bits.ue(0); // max_bits_per_mb_denom
        bits.ue(LOG2_MAX_MV_LENGTH_UNRESTRICTED);
        bits.ue(LOG2_MAX_MV_LENGTH_UNRESTRICTED);
        bits.ue(0); // max_num_reorder_frames
        bits.ue(header.max_num_ref_frames); // max_dec_frame_buffering
    }

    /// An Annex-B PPS NAL unit, start code included, for a stream whose
    /// pictures start from `pic_init_qp`.
    pub(super) fn picture_parameter_set(pic_init_qp: u8) -> Vec<u8> {
        let mut bits = BitWriter::default();
        bits.ue(0); // pic_parameter_set_id
        bits.ue(0); // seq_parameter_set_id
        bits.flag(false); // entropy_coding_mode_flag: CAVLC
        bits.flag(false); // bottom_field_pic_order_in_frame_present_flag
        bits.ue(0); // num_slice_groups_minus1
        bits.ue(0); // num_ref_idx_l0_default_active_minus1
        bits.ue(0); // num_ref_idx_l1_default_active_minus1
        bits.flag(false); // weighted_pred_flag
        bits.bits(0, 2); // weighted_bipred_idc
        bits.se(i32::from(pic_init_qp) - 26); // pic_init_qp_minus26
        bits.se(0); // pic_init_qs_minus26
        bits.se(0); // chroma_qp_index_offset
        bits.flag(true); // deblocking_filter_control_present_flag
        bits.flag(false); // constrained_intra_pred_flag
        bits.flag(false); // redundant_pic_cnt_present_flag
        nal_unit(NAL_PPS, bits.finish())
    }

    /// Whether an Annex-B stream contains a NAL unit of `nal_type`.
    pub(super) fn contains_nal(stream: &[u8], nal_type: u8) -> bool {
        nal_types(stream).any(|found| found == nal_type)
    }

    /// The `nal_unit_type` of every NAL unit in an Annex-B stream, in order.
    pub(super) fn nal_types(stream: &[u8]) -> impl Iterator<Item = u8> + '_ {
        stream
            .windows(3)
            .enumerate()
            .filter(|(_, window)| *window == [0, 0, 1])
            .filter_map(|(start, _)| stream.get(start + 3).map(|header| header & 0x1f))
    }

    /// Wraps an RBSP in a NAL unit: start code, header, and the emulation
    /// prevention bytes that stop a payload from containing a start code.
    fn nal_unit(nal_type: u8, rbsp: Vec<u8>) -> Vec<u8> {
        let mut out = Vec::with_capacity(rbsp.len() + rbsp.len() / 64 + 5);
        out.extend_from_slice(&[0, 0, 0, 1, (NAL_REF_IDC_HIGHEST << 5) | nal_type]);
        let mut zeros = 0usize;
        for byte in rbsp {
            if zeros >= 2 && byte <= 3 {
                out.push(3);
                zeros = 0;
            }
            out.push(byte);
            zeros = if byte == 0 { zeros + 1 } else { 0 };
        }
        out
    }

    /// Most-significant-bit-first writer with the two Exp-Golomb codes H.264
    /// headers use.
    #[derive(Debug, Default)]
    struct BitWriter {
        bytes: Vec<u8>,
        current: u8,
        used: u8,
    }

    impl BitWriter {
        fn flag(&mut self, set: bool) {
            self.current = (self.current << 1) | u8::from(set);
            self.used += 1;
            if self.used == 8 {
                self.bytes.push(self.current);
                self.current = 0;
                self.used = 0;
            }
        }

        fn bits(&mut self, value: u32, count: u32) {
            for shift in (0..count).rev() {
                self.flag((value >> shift) & 1 == 1);
            }
        }

        /// `ue(v)`: unsigned Exp-Golomb.
        fn ue(&mut self, value: u32) {
            let coded = u64::from(value) + 1;
            let length = u64::BITS - coded.leading_zeros();
            for _ in 1..length {
                self.flag(false);
            }
            for shift in (0..length).rev() {
                self.flag((coded >> shift) & 1 == 1);
            }
        }

        /// `se(v)`: signed Exp-Golomb, positive values on the odd codes.
        fn se(&mut self, value: i32) {
            let mapped = if value > 0 {
                value.unsigned_abs() * 2 - 1
            } else {
                value.unsigned_abs() * 2
            };
            self.ue(mapped);
        }

        /// The RBSP: these bits, a stop bit, and zeros to the byte boundary.
        fn finish(mut self) -> Vec<u8> {
            self.flag(true);
            while self.used != 0 {
                self.flag(false);
            }
            self.bytes
        }
    }

    #[cfg(test)]
    pub(super) mod reader {
        //! Just enough of a bit reader to read a header back field by field,
        //! so a test states what the bytes mean instead of what they are.

        /// Reads the RBSP of `nal`, a start-code-prefixed NAL unit, after
        /// removing its emulation prevention bytes.
        pub(in super::super) fn rbsp(nal: &[u8]) -> Vec<u8> {
            let payload = &nal[5..];
            let mut out = Vec::with_capacity(payload.len());
            let mut zeros = 0usize;
            for &byte in payload {
                if zeros >= 2 && byte == 3 {
                    zeros = 0;
                    continue;
                }
                out.push(byte);
                zeros = if byte == 0 { zeros + 1 } else { 0 };
            }
            out
        }

        #[derive(Debug)]
        pub(in super::super) struct BitReader<'a> {
            bytes: &'a [u8],
            position: usize,
        }

        impl<'a> BitReader<'a> {
            pub(in super::super) const fn new(bytes: &'a [u8]) -> Self {
                Self { bytes, position: 0 }
            }

            pub(in super::super) fn flag(&mut self) -> bool {
                let byte = self.bytes[self.position / 8];
                let set = (byte >> (7 - self.position % 8)) & 1 == 1;
                self.position += 1;
                set
            }

            pub(in super::super) fn bits(&mut self, count: u32) -> u32 {
                (0..count).fold(0, |value, _| (value << 1) | u32::from(self.flag()))
            }

            pub(in super::super) fn ue(&mut self) -> u32 {
                let mut zeros = 0;
                while !self.flag() {
                    zeros += 1;
                }
                ((1u32 << zeros) - 1) + self.bits(zeros)
            }

            pub(in super::super) fn se(&mut self) -> i32 {
                let coded = self.ue();
                let magnitude = i32::try_from(coded.div_ceil(2)).unwrap_or(i32::MAX);
                if coded % 2 == 1 {
                    magnitude
                } else {
                    -magnitude
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failed assumption must fail the test")]

    use super::headers::reader::{BitReader, rbsp};
    use super::*;

    /// The central "never claim hardware is available when it isn't" rule of
    /// this task, checked mechanically rather than by inspection — the twin
    /// of `encode::windows`'s test of the same name.
    ///
    /// The probe now encodes a frame, so it can say no where construction
    /// alone would have said yes (ADR 0088). What it may never do is say yes
    /// where construction fails. On CI and on every virtual machine tried so
    /// far both are `false`, which is the case that matters most: a host with
    /// libva installed and no encode-capable GPU must fall back to `openh264`
    /// rather than fail the session (§18).
    #[test]
    fn probe_hardware_never_claims_more_than_construction_delivers() {
        let probed = hardware_available(EncoderConfig::default());
        let constructed = VaapiEncoder::new(EncoderConfig::default()).is_ok();
        assert!(
            !probed || constructed,
            "hardware_available reported true but construction failed"
        );
    }

    /// AV1 is a different profile with different parameter buffers. Reusing
    /// the H.264 rehearsal to answer an AV1 question is the mismatch §11's
    /// mutual-hardware-support rule exists to prevent.
    #[test]
    fn av1_is_refused_regardless_of_what_the_driver_can_do() {
        let config = EncoderConfig {
            codec: VideoCodec::Av1,
            ..EncoderConfig::default()
        };
        assert!(!hardware_available(config));
        assert!(VaapiEncoder::open(PROBE_WIDTH, PROBE_HEIGHT, config).is_err());
    }

    #[test]
    fn dimensions_round_up_to_whole_macroblocks() {
        assert_eq!(aligned_dims(1920, 1080, MACROBLOCK), (1920, 1088));
        assert_eq!(aligned_dims(1366, 768, MACROBLOCK), (1376, 768));
        assert_eq!(aligned_dims(64, 64, MACROBLOCK), (64, 64));
        // Never zero: a degenerate frame must still name a legal geometry
        // rather than ask the driver for a zero-macroblock picture.
        assert_eq!(aligned_dims(0, 0, MACROBLOCK), (16, 16));
    }

    /// Each codec is opened under its own VA-API profile, and a codec this
    /// backend cannot drive is refused before libva is touched at all
    /// (ADR 0040, ADR 0069).
    #[test]
    fn each_codec_is_opened_under_its_own_profile() {
        assert_eq!(
            va_profile(VideoCodec::H264).ok(),
            Some(VAProfile::VAProfileH264ConstrainedBaseline)
        );
        assert!(va_profile(VideoCodec::Av1).is_err());
        assert!(coding_block(VideoCodec::Av1).is_err());
    }

    #[test]
    fn the_coded_buffer_is_never_smaller_than_a_worst_case_keyframe() {
        assert!(coded_buffer_size(1920, 1088) >= 1920 * 1088);
        // Tiny pictures still get a floor: a 16x16 IDR plus its SPS/PPS is
        // bigger than 16x16x1.5 bytes.
        assert!(coded_buffer_size(16, 16) >= 1 << 16);
    }

    /// ADR 0088: current Intel offers only the low-power entrypoint, and an
    /// encoder that asked for the full-featured one alone refused it.
    #[test]
    fn the_low_power_entrypoint_is_used_when_it_is_the_only_one() {
        assert_eq!(
            encode_entrypoint(&[
                VAEntrypoint::VAEntrypointVLD,
                VAEntrypoint::VAEntrypointEncSliceLP
            ]),
            Some(VAEntrypoint::VAEntrypointEncSliceLP)
        );
        assert_eq!(
            encode_entrypoint(&[
                VAEntrypoint::VAEntrypointEncSliceLP,
                VAEntrypoint::VAEntrypointEncSlice
            ]),
            Some(VAEntrypoint::VAEntrypointEncSlice),
            "where both exist the full-featured one is preferred"
        );
        assert_eq!(encode_entrypoint(&[VAEntrypoint::VAEntrypointVLD]), None);
    }

    /// ADR 0088: a bitrate the driver holds is preferred, constant QP is what
    /// is left, and a driver offering neither is refused.
    #[test]
    fn rate_control_prefers_the_drivers_own_and_falls_back_to_constant_qp() {
        assert_eq!(
            RateControl::choose(VA_RC_CQP | VA_RC_CBR | VA_RC_VBR),
            Some(RateControl::Cbr)
        );
        assert_eq!(
            RateControl::choose(VA_RC_CQP | VA_RC_VBR),
            Some(RateControl::Vbr)
        );
        assert_eq!(
            RateControl::choose(VA_RC_CQP),
            Some(RateControl::ConstantQp)
        );
        assert_eq!(RateControl::choose(VA_ATTRIB_NOT_SUPPORTED), None);
        assert_eq!(RateControl::choose(0), None);
    }

    /// ADR 0088: frames bigger than their share of the bitrate push the
    /// quantizer up, smaller ones let it down, and it never leaves its bounds.
    #[test]
    fn the_constant_qp_controller_follows_the_bitrate_within_its_bounds() {
        let config = EncoderConfig {
            fps: 30,
            bitrate_kbps: 2_400,
            codec: VideoCodec::H264,
        };
        // 2400 kbit/s at 30 fps is 10 000 bytes a frame.
        let share = 10_000;

        let mut controller = QpController::new();
        for _ in 0..200 {
            controller.observe(share * 4, false, config);
        }
        assert_eq!(controller.qp(), VAAPI_CQP_QP_MAX);

        for _ in 0..400 {
            controller.observe(share / 4, false, config);
        }
        assert_eq!(controller.qp(), VAAPI_CQP_QP_MIN);

        let mut steady = QpController::new();
        for _ in 0..200 {
            steady.observe(share, false, config);
        }
        assert_eq!(steady.qp(), INITIAL_QP, "a size on target moves nothing");
    }

    /// An IDR is supposed to be several P-frames' worth; counting it would
    /// raise the quantizer after every keyframe request.
    #[test]
    fn keyframes_do_not_move_the_constant_qp_controller() {
        let config = EncoderConfig::default();
        let mut controller = QpController::new();
        for _ in 0..50 {
            controller.observe(usize::MAX / 2, true, config);
        }
        assert_eq!(controller.qp(), INITIAL_QP);
    }

    #[test]
    fn the_level_is_the_smallest_that_holds_the_picture_and_its_rate() {
        // 1920x1088 is 8160 macroblocks: level 4 at 30 fps, 4.2 at 60.
        assert_eq!(level_idc(8_160, 30), 40);
        assert_eq!(level_idc(8_160, 60), 42);
        // 2560x1440 is 14 400 macroblocks.
        assert_eq!(level_idc(14_400, 30), 50);
        // 3840x2160 is 32 400.
        assert_eq!(level_idc(32_400, 30), 51);
        // Something small is still level 3.
        assert_eq!(level_idc(99, 30), 30);
    }

    fn sample_sequence() -> headers::SequenceHeader {
        headers::SequenceHeader {
            level_idc: 40,
            width_in_mbs: 120,
            height_in_mbs: 68,
            crop_right: 0,
            crop_bottom: 4,
            log2_max_frame_num_minus4: LOG2_MAX_FRAME_NUM_MINUS4,
            log2_max_poc_lsb_minus4: LOG2_MAX_POC_LSB_MINUS4,
            max_num_ref_frames: MAX_REF_FRAMES,
            matrix: headers::Matrix::Bt601,
        }
    }

    /// ADR 0088: the SPS this module writes reads back as the values it was
    /// written from, in the order 7.3.2.1.1 of the specification lays out.
    #[test]
    fn the_sequence_parameter_set_reads_back_field_by_field() {
        let nal = headers::sequence_parameter_set(&sample_sequence());
        assert_eq!(&nal[..5], &[0, 0, 0, 1, 0x67]);
        let payload = rbsp(&nal);
        let mut r = BitReader::new(&payload);
        assert_eq!(r.bits(8), 66, "profile_idc");
        assert_eq!(r.bits(8), 0b1100_0000, "constraint flags");
        assert_eq!(r.bits(8), 40, "level_idc");
        assert_eq!(r.ue(), 0, "seq_parameter_set_id");
        assert_eq!(r.ue(), LOG2_MAX_FRAME_NUM_MINUS4);
        assert_eq!(r.ue(), 0, "pic_order_cnt_type");
        assert_eq!(r.ue(), LOG2_MAX_POC_LSB_MINUS4);
        assert_eq!(r.ue(), MAX_REF_FRAMES);
        assert!(!r.flag(), "gaps_in_frame_num_value_allowed_flag");
        assert_eq!(r.ue(), 119, "pic_width_in_mbs_minus1");
        assert_eq!(r.ue(), 67, "pic_height_in_map_units_minus1");
        assert!(r.flag(), "frame_mbs_only_flag");
        assert!(r.flag(), "direct_8x8_inference_flag");
        assert!(r.flag(), "frame_cropping_flag");
        assert_eq!((r.ue(), r.ue(), r.ue(), r.ue()), (0, 0, 0, 4));
        assert!(r.flag(), "vui_parameters_present_flag");
        assert!(!r.flag(), "aspect_ratio_info_present_flag");
        assert!(!r.flag(), "overscan_info_present_flag");
        assert!(r.flag(), "video_signal_type_present_flag");
        assert_eq!(r.bits(3), 5, "video_format");
        assert!(!r.flag(), "video_full_range_flag");
        assert!(r.flag(), "colour_description_present_flag");
        assert_eq!((r.bits(8), r.bits(8), r.bits(8)), (1, 13, 6));
        assert!(!r.flag(), "chroma_loc_info_present_flag");
        assert!(!r.flag(), "timing_info_present_flag");
        assert!(!r.flag(), "nal_hrd_parameters_present_flag");
        assert!(!r.flag(), "vcl_hrd_parameters_present_flag");
        assert!(!r.flag(), "pic_struct_present_flag");
        assert!(r.flag(), "bitstream_restriction_flag");
        assert!(r.flag(), "motion_vectors_over_pic_boundaries_flag");
        assert_eq!((r.ue(), r.ue(), r.ue(), r.ue()), (0, 0, 16, 16));
        assert_eq!(r.ue(), 0, "max_num_reorder_frames");
        assert_eq!(r.ue(), MAX_REF_FRAMES, "max_dec_frame_buffering");
        assert!(r.flag(), "rbsp_stop_one_bit");
    }

    #[test]
    fn the_picture_parameter_set_reads_back_field_by_field() {
        let nal = headers::picture_parameter_set(INITIAL_QP);
        assert_eq!(&nal[..5], &[0, 0, 0, 1, 0x68]);
        let payload = rbsp(&nal);
        let mut r = BitReader::new(&payload);
        assert_eq!((r.ue(), r.ue()), (0, 0), "pps and sps ids");
        assert!(!r.flag(), "entropy_coding_mode_flag");
        assert!(!r.flag(), "bottom_field_pic_order_in_frame_present_flag");
        assert_eq!(r.ue(), 0, "num_slice_groups_minus1");
        assert_eq!((r.ue(), r.ue()), (0, 0), "num_ref_idx defaults");
        assert!(!r.flag(), "weighted_pred_flag");
        assert_eq!(r.bits(2), 0, "weighted_bipred_idc");
        assert_eq!(r.se(), 0, "pic_init_qp_minus26");
        assert_eq!((r.se(), r.se()), (0, 0), "qs and chroma offset");
        assert!(r.flag(), "deblocking_filter_control_present_flag");
        assert!(!r.flag(), "constrained_intra_pred_flag");
        assert!(!r.flag(), "redundant_pic_cnt_present_flag");
        assert!(r.flag(), "rbsp_stop_one_bit");
    }

    #[test]
    fn nal_types_are_found_after_three_and_four_byte_start_codes() {
        let stream = [
            0, 0, 0, 1, 0x67, 0xAA, 0, 0, 1, 0x68, 0xBB, 0, 0, 1, 0x25, 0xCC,
        ];
        assert_eq!(
            headers::nal_types(&stream).collect::<Vec<_>>(),
            vec![7, 8, 5]
        );
        assert!(headers::contains_nal(&stream, headers::NAL_IDR_SLICE));
        assert!(!headers::contains_nal(&stream[..8], headers::NAL_IDR_SLICE));
    }

    /// A payload that happens to contain two zero bytes and a small value is
    /// escaped, so no start code can appear inside a header.
    #[test]
    fn emulation_prevention_keeps_start_codes_out_of_a_header() {
        // A PPS whose QP makes the RBSP contain zeros would be contrived; a
        // wide SPS with a large crop produces them naturally.
        let mut header = sample_sequence();
        header.width_in_mbs = 1;
        header.height_in_mbs = 1;
        header.crop_bottom = 0;
        let nal = headers::sequence_parameter_set(&header);
        let body = &nal[4..];
        assert!(
            !body.windows(3).any(|window| window == [0, 0, 1]),
            "a start code inside a NAL unit"
        );
    }

    /// ADR 0088, on the hardware itself: a VA-API encoder that probes
    /// available produces a stream that an independent decoder turns back into
    /// the picture that went in — IDR, P-frames and a crop included.
    ///
    /// A machine without a VA-API encoder has nothing to say here and passes;
    /// the probe tests above hold it to not pretending otherwise.
    #[cfg(feature = "encode-openh264")]
    #[test]
    fn a_hardware_stream_decodes_back_to_the_picture_that_went_in() {
        use openh264::formats::YUVSource as _;

        if !hardware_available(EncoderConfig::default()) {
            eprintln!("no VA-API H.264 encoder on this machine; nothing to check");
            return;
        }
        // Neither dimension a multiple of 16, so the crop is exercised.
        let (width, height) = (320u32, 184u32);
        let mut hardware = VaapiEncoder::new(EncoderConfig::default()).unwrap();
        let mut software = openh264::decoder::Decoder::new().unwrap();

        for index in 0..30u32 {
            let mut bgra = vec![0u8; (width * height * 4) as usize];
            for y in 0..height {
                for x in 0..width {
                    let at = ((y * width + x) * 4) as usize;
                    bgra[at] = u8::try_from((x + index * 3) % 256).unwrap();
                    bgra[at + 1] = u8::try_from((y * 2) % 256).unwrap();
                    bgra[at + 2] = u8::try_from((x + y + index) % 256).unwrap();
                    bgra[at + 3] = 255;
                }
            }
            let frame = Frame::cpu(width, height, PixelFormat::Bgra8, u64::from(index), bgra);
            let (expected, _, _) = bgra_to_nv12(&frame).unwrap();
            let bitstream = hardware.encode(&frame).unwrap();
            assert_eq!(bitstream.keyframe, index == 0);

            let picture = software
                .decode(&bitstream.data)
                .unwrap()
                .unwrap_or_else(|| panic!("frame {index} decoded to nothing"));
            assert_eq!(
                picture.dimensions(),
                (width as usize, height as usize),
                "the crop did not come back out"
            );
            let psnr = luma_psnr(&picture, &expected, width, height);
            assert!(psnr > 35.0, "frame {index} came back at {psnr:.1} dB");
        }
    }

    /// Peak signal-to-noise ratio of a decoded picture's luma against the one
    /// expected, over the visible `width` x `height`.
    #[cfg(feature = "encode-openh264")]
    fn luma_psnr(
        picture: &openh264::decoder::DecodedYUV<'_>,
        expected: &[u8],
        width: u32,
        height: u32,
    ) -> f64 {
        use openh264::formats::YUVSource as _;

        let (stride, _, _) = picture.strides();
        let luma = picture.y();
        let mut squared_error = 0f64;
        for row in 0..height as usize {
            for column in 0..width as usize {
                let difference = f64::from(luma[row * stride + column])
                    - f64::from(expected[row * width as usize + column]);
                squared_error += difference * difference;
            }
        }
        let mse = squared_error / f64::from(width * height);
        10.0 * (255.0 * 255.0 / mse.max(1e-9)).log10()
    }

    /// A `BGRx` picture the GPU itself holds, exported as a DMA-BUF the way a
    /// compositor would hand one over, and the pixels that were written.
    #[cfg(feature = "encode-vaapi-zero-copy")]
    fn exported_picture(width: u32, height: u32, index: u32) -> (std::sync::Arc<DmaBuf>, Vec<u8>) {
        use cros_libva::Image;

        let device = open_device(
            VAProfile::VAProfileH264ConstrainedBaseline,
            VideoCodec::H264,
            None,
        )
        .unwrap();
        let surface = device
            .display
            .create_surfaces(
                VA_RT_FORMAT_RGB32,
                Some(VA_FOURCC_BGRX),
                width,
                height,
                Some(UsageHint::USAGE_HINT_EXPORT),
                vec![()],
            )
            .unwrap()
            .pop()
            .unwrap();
        let mut bgra = vec![0u8; (width * height * 4) as usize];
        {
            let mut image = Image::derive_from(&surface, (width, height)).unwrap();
            let offset = image.image().offsets[0] as usize;
            let pitch = image.image().pitches[0] as usize;
            let mapped = image.as_mut();
            for y in 0..height {
                for x in 0..width {
                    let pixel = [
                        u8::try_from((x * 2 + index * 5) % 256).unwrap(),
                        u8::try_from((y + x / 2) % 256).unwrap(),
                        u8::try_from((y * 3 + index) % 256).unwrap(),
                        255,
                    ];
                    let at = ((y * width + x) * 4) as usize;
                    bgra[at..at + 4].copy_from_slice(&pixel);
                    let to = offset + y as usize * pitch + x as usize * 4;
                    mapped[to..to + 4].copy_from_slice(&pixel);
                }
            }
        }
        surface.sync().unwrap();
        let mut exported = surface.export_prime().unwrap();
        let layer = &exported.layers[0];
        let (drm_format, offset, pitch) = (layer.drm_format, layer.offset[0], layer.pitch[0]);
        let object = exported
            .objects
            .swap_remove(usize::from(layer.object_index[0]));
        let dmabuf = DmaBuf::detached(
            object.fd,
            drm_format,
            object.drm_format_modifier,
            offset,
            pitch,
            width,
            height,
            object.size,
        );
        (std::sync::Arc::new(dmabuf), bgra)
    }

    /// BT.709 luma in studio range, which the driver's own conversion of a
    /// `BGRx` surface is measured to produce (ADR 0088).
    #[cfg(all(feature = "encode-vaapi-zero-copy", feature = "encode-openh264"))]
    fn bt709_luma(bgra: &[u8]) -> Vec<u8> {
        bgra.chunks_exact(4)
            .map(|pixel| {
                let (b, g, r) = (
                    f64::from(pixel[0]),
                    f64::from(pixel[1]),
                    f64::from(pixel[2]),
                );
                let y = 16.0 + 219.0 * (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255.0;
                // In 16..=235 by construction.
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "in 16..=235"
                )]
                let y = y.round() as u8;
                y
            })
            .collect()
    }

    /// ADR 0088, on the hardware: a frame that exists only as a DMA-BUF is
    /// encoded without being read back, and decodes to the picture the GPU
    /// held, converted with the matrix the SPS names.
    #[cfg(all(feature = "encode-vaapi-zero-copy", feature = "encode-openh264"))]
    #[test]
    fn a_frame_that_never_left_the_gpu_encodes_and_decodes_back() {
        if !hardware_available(EncoderConfig::default()) {
            eprintln!("no VA-API H.264 encoder on this machine; nothing to check");
            return;
        }
        let (width, height) = (320u32, 184u32);
        let mut hardware = VaapiEncoder::new(EncoderConfig::default()).unwrap();
        let mut software = openh264::decoder::Decoder::new().unwrap();

        for index in 0..10u32 {
            let (dmabuf, bgra) = exported_picture(width, height, index);
            let mut frame = Frame::cpu(
                width,
                height,
                PixelFormat::Bgra8,
                u64::from(index),
                Vec::new(),
            );
            frame.dmabuf = Some(dmabuf);
            let bitstream = hardware.encode(&frame).unwrap();
            assert!(!hardware.gpu_refused, "the driver refused its own buffer");
            assert_eq!(hardware.input, Some(InputKind::DmaBufImport));
            assert!(frame.data.is_empty(), "the frame was read back");
            assert_eq!(bitstream.keyframe, index == 0);

            let picture = software
                .decode(&bitstream.data)
                .unwrap()
                .unwrap_or_else(|| panic!("frame {index} decoded to nothing"));
            let psnr = luma_psnr(&picture, &bt709_luma(&bgra), width, height);
            assert!(psnr > 35.0, "frame {index} came back at {psnr:.1} dB");
        }
    }

    /// ADR 0088: switching between a buffer the driver imports and a picture
    /// this side uploads starts a new sequence, because the two are converted
    /// with different matrices and one SPS can name only one.
    #[cfg(all(feature = "encode-vaapi-zero-copy", feature = "encode-openh264"))]
    #[test]
    fn changing_how_pictures_arrive_starts_a_new_sequence() {
        if !hardware_available(EncoderConfig::default()) {
            eprintln!("no VA-API H.264 encoder on this machine; nothing to check");
            return;
        }
        let (width, height) = (160u32, 96u32);
        let mut hardware = VaapiEncoder::new(EncoderConfig::default()).unwrap();

        let (dmabuf, bgra) = exported_picture(width, height, 0);
        let mut imported = Frame::cpu(width, height, PixelFormat::Bgra8, 0, Vec::new());
        imported.dmabuf = Some(dmabuf);
        assert!(hardware.encode(&imported).unwrap().keyframe);

        let uploaded = Frame::cpu(width, height, PixelFormat::Bgra8, 1, bgra);
        let switched = hardware.encode(&uploaded).unwrap();
        assert!(switched.keyframe, "a new matrix went out without a new SPS");
        assert_eq!(hardware.input, Some(InputKind::Nv12Upload));
        assert!(!hardware.encode(&uploaded).unwrap().keyframe);
    }

    /// ADR 0088: a frame whose pixels only a DMA-BUF holds still reads back,
    /// for every consumer that is not this encoder.
    #[cfg(feature = "encode-vaapi-zero-copy")]
    #[test]
    fn a_linear_dmabuf_reads_back_to_the_pixels_written_into_it() {
        if !hardware_available(EncoderConfig::default()) {
            eprintln!("no VA-API H.264 encoder on this machine; nothing to check");
            return;
        }
        let (dmabuf, bgra) = exported_picture(64, 48, 3);
        if dmabuf.modifier() != 0 {
            eprintln!("this driver exports a tiled layout; nothing to read row by row");
            assert!(dmabuf.pixels().is_err());
            return;
        }
        let pixels = dmabuf.pixels().unwrap();
        let rgb = |bytes: &[u8]| -> Vec<u8> {
            bytes
                .chunks_exact(4)
                .flat_map(|pixel| pixel[..3].to_vec())
                .collect()
        };
        assert_eq!(rgb(pixels), rgb(&bgra));
    }

    /// Processor time this process has used, in milliseconds, from
    /// `/proc/self/stat`: user and system time, in the kernel's fixed
    /// `USER_HZ` of 100 ticks a second.
    #[cfg(all(feature = "encode-vaapi-zero-copy", feature = "encode-openh264"))]
    fn process_cpu_ms() -> f64 {
        let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
        // The command name is parenthesized and may hold spaces; the fields
        // count from after it, where `utime` and `stime` are the 12th and 13th.
        let after_name = &stat[stat.rfind(')').unwrap() + 2..];
        let ticks: u32 = after_name
            .split(' ')
            .skip(11)
            .take(2)
            .map(|field| field.parse::<u32>().unwrap())
            .sum();
        f64::from(ticks) * 10.0
    }

    /// ADR 0088's numbers: processor time per 1080p frame for each way a
    /// desktop frame can be encoded on this machine. A measurement, not a
    /// check, so it runs only when asked for:
    /// `cargo test -p lumepeer-media --features encode-vaapi-zero-copy,encode-openh264
    /// --lib -- --ignored --nocapture cpu_per_frame`.
    #[cfg(all(feature = "encode-vaapi-zero-copy", feature = "encode-openh264"))]
    #[test]
    #[ignore = "a measurement for ADR 0088, not a check"]
    fn cpu_per_frame_of_each_encode_path() {
        const FRAMES: u32 = 600;
        let (width, height) = (1920u32, 1080u32);
        if !hardware_available(EncoderConfig::default()) {
            eprintln!("no VA-API H.264 encoder on this machine; nothing to measure");
            return;
        }
        let pictures: Vec<_> = (0..8)
            .map(|index| exported_picture(width, height, index))
            .collect();

        let mut zero_copy = VaapiEncoder::new(EncoderConfig::default()).unwrap();
        let started = process_cpu_ms();
        for index in 0..FRAMES {
            let (dmabuf, _) = &pictures[index as usize % pictures.len()];
            let mut frame = Frame::cpu(
                width,
                height,
                PixelFormat::Bgra8,
                u64::from(index),
                Vec::new(),
            );
            frame.dmabuf = Some(std::sync::Arc::clone(dmabuf));
            zero_copy.encode(&frame).unwrap();
        }
        assert!(!zero_copy.gpu_refused);
        let zero_copy_ms = (process_cpu_ms() - started) / f64::from(FRAMES);

        // What the readback path starts from: the pixels already in main
        // memory, as the compositor's shared-memory buffers deliver them.
        let frames: Vec<_> = pictures
            .iter()
            .map(|(_, bgra)| Frame::cpu(width, height, PixelFormat::Bgra8, 0, bgra.clone()))
            .collect();
        let mut upload = VaapiEncoder::new(EncoderConfig::default()).unwrap();
        let started = process_cpu_ms();
        for index in 0..FRAMES {
            upload
                .encode(&frames[index as usize % frames.len()])
                .unwrap();
        }
        let upload_ms = (process_cpu_ms() - started) / f64::from(FRAMES);

        let mut software =
            crate::encode::software::OpenH264Encoder::new(EncoderConfig::default()).unwrap();
        let started = process_cpu_ms();
        for index in 0..FRAMES {
            software
                .encode(&frames[index as usize % frames.len()])
                .unwrap();
        }
        let software_ms = (process_cpu_ms() - started) / f64::from(FRAMES);

        eprintln!(
            "1080p CPU ms/frame: VA-API from DMA-BUF {zero_copy_ms:.2}, \
             VA-API from main memory {upload_ms:.2}, openh264 {software_ms:.2}"
        );
    }
}
