//! VA-API hardware video encoder for Linux — H.264, and H.265 in a build made
//! with `encode-h265` (§5.1, §11, §18/§19 phase 4; ADR 0040, ADR 0071).
//!
//! The Linux counterpart of [`super::windows`] (ADR 0011), built to the same
//! rule: [`hardware_available`] is not a capability query, it is a
//! rehearsal. It runs the identical open/configure/context sequence
//! [`VaapiEncoder::new`] runs and reports `true` only when that sequence
//! actually succeeds, because a driver that *lists* `VAEntrypointEncSlice`
//! and a driver that can *give* you an encode context are different
//! populations — a machine with libva installed and no encode-capable GPU
//! behind it is the common case, not the exotic one.
//!
//! Intel and AMD only. NVIDIA's Linux encode path is NVENC, a different SDK
//! under different licence terms, and is deliberately not reached from here:
//! `nvidia-vaapi-driver` bridges VA-API to NVDEC (decode) and offers no
//! encode entrypoint, so an NVIDIA host correctly probes `false` here and
//! falls back to `openh264` rather than half-working.
//!
//! `cros-libva` is a safe wrapper over libva, so this module — unlike the
//! Media Foundation one — needs no `unsafe` of its own.

use std::rc::Rc;

use cros_libva::{
    BufferType, Config, Context, Display, EncCodedBuffer, EncMiscParameter,
    EncMiscParameterRateControl, EncPictureParameter, EncPictureParameterBufferH264,
    EncSequenceParameter, EncSequenceParameterBufferH264, EncSliceParameter,
    EncSliceParameterBufferH264, H264EncPicFields, H264EncSeqFields, MappedCodedBuffer,
    PictureH264, RcFlags, Surface, UsageHint, VA_FOURCC_NV12, VA_INVALID_ID,
    VA_PICTURE_H264_INVALID, VA_RT_FORMAT_YUV420, VAEntrypoint, VAProfile,
};
#[cfg(feature = "encode-h265")]
use cros_libva::{
    EncPictureParameterBufferHEVC, EncSequenceParameterBufferHEVC, EncSliceParameterBufferHEVC,
    HEVCEncPicFields, HEVCEncSeqFields, HevcEncPicSccFields, HevcEncSeqSccFields,
    HevcEncSliceFields, PictureHEVC, VA_PICTURE_HEVC_INVALID,
};

use super::nv12::bgra_to_nv12;
use super::{EncodedFrame, EncoderConfig, EncoderKind, VideoCodec, VideoEncoder};
use crate::capture::Frame;
use crate::error::{MediaError, Result};

/// Probe geometry, matching the Media Foundation module's: big enough to be a
/// real macroblock grid (4x4 macroblocks), small enough that a probe costs
/// nothing.
const PROBE_WIDTH: u32 = 64;
/// See [`PROBE_WIDTH`].
const PROBE_HEIGHT: u32 = 64;

/// Probe geometry for the optional codecs, larger than [`PROBE_WIDTH`] for
/// the reason `super::windows`'s `OPTIONAL_PROBE_WIDTH` records: an HEVC
/// encoder refuses picture sizes the H.264 encoder on the same GPU accepts,
/// and a probe that asked at 64x64 would report "no encoder" where the true
/// answer is "not at that size" (ADR 0071).
#[cfg(feature = "encode-h265")]
const OPTIONAL_PROBE_WIDTH: u32 = 256;
/// See [`OPTIONAL_PROBE_WIDTH`].
#[cfg(feature = "encode-h265")]
const OPTIONAL_PROBE_HEIGHT: u32 = 256;

/// Macroblock edge in pixels. H.264 codes in 16x16 macroblocks, so every
/// dimension handed to the driver is counted in these.
const MACROBLOCK: u32 = 16;

/// Coding tree unit edge in pixels for H.265, the unit its picture geometry
/// is counted in the way H.264's is counted in [`MACROBLOCK`]s.
///
/// 32 rather than 64: it follows from the two log2 sizes below — minimum
/// coding block 8, three doublings up to the maximum — and a smaller CTU
/// costs a little compression on flat areas while giving the rate controller
/// finer granularity on a desktop, which is mostly flat areas with a small
/// moving region in it.
#[cfg(feature = "encode-h265")]
const HEVC_CTU: u32 = 32;

/// Level 4.0: 1080p at 30 fps, which is the ceiling §14's defaults imply.
/// Sent as `level_idc`, in the units H.264 uses (level x 10).
const LEVEL_IDC: u8 = 40;

/// Initial quantizer. Rate control moves it; this is only where it starts.
const INITIAL_QP: u32 = 26;

/// Bits per kilobit, for the kbps of §14 against the bps of the VA-API
/// rate-control buffer.
const BITS_PER_KBIT: u32 = 1000;

/// Reference frames this encoder keeps. One: every non-IDR frame predicts
/// from the frame before it. A remote desktop is watched live, so a deeper
/// DPB buys compression the viewer pays for in latency.
const MAX_REF_FRAMES: u32 = 1;

/// `slice_type` values of the H.264 slice header. Only these two are used:
/// there are no B-slices in a latency product.
const SLICE_TYPE_P: u8 = 0;
/// See [`SLICE_TYPE_P`].
const SLICE_TYPE_I: u8 = 2;

/// H.265 `general_profile_idc` for Main profile — the only profile this
/// pipeline's 8-bit 4:2:0 NV12 input can feed, and the one every hardware
/// HEVC encoder implements (ADR 0071).
#[cfg(feature = "encode-h265")]
const HEVC_PROFILE_IDC: u8 = 1;
/// H.265 `general_level_idc` for level 4.0. H.265 counts levels in thirtieths
/// rather than H.264's tenths, so the same 1080p30 ceiling [`LEVEL_IDC`]
/// names is 120 here.
#[cfg(feature = "encode-h265")]
const HEVC_LEVEL_IDC: u8 = 120;
/// `log2_min_luma_coding_block_size_minus3`: minimum coding block 8x8.
#[cfg(feature = "encode-h265")]
const HEVC_LOG2_MIN_CB_MINUS3: u8 = 0;
/// `log2_diff_max_min_luma_coding_block_size`: two doublings from 8 to the
/// [`HEVC_CTU`] edge of 32.
#[cfg(feature = "encode-h265")]
const HEVC_LOG2_DIFF_MAX_MIN_CB: u8 = 2;
/// `log2_min_transform_block_size_minus2`: minimum transform block 4x4.
#[cfg(feature = "encode-h265")]
const HEVC_LOG2_MIN_TB_MINUS2: u8 = 0;
/// `log2_diff_max_min_transform_block_size`: three doublings from 4 to 32.
#[cfg(feature = "encode-h265")]
const HEVC_LOG2_DIFF_MAX_MIN_TB: u8 = 3;
/// `max_transform_hierarchy_depth_inter`/`_intra`: one level of splitting.
/// Deeper search costs encode time a latency product does not have.
#[cfg(feature = "encode-h265")]
const HEVC_MAX_TRANSFORM_DEPTH: u8 = 1;
/// `max_num_merge_cand`: the full five candidates the specification allows.
#[cfg(feature = "encode-h265")]
const HEVC_MAX_MERGE_CAND: u8 = 5;
/// `slice_type` values of the H.265 slice header. H.265 numbers them the
/// other way round from H.264, which is exactly the kind of detail that makes
/// reusing the H.264 constants here a silent corruption rather than an error.
#[cfg(feature = "encode-h265")]
const HEVC_SLICE_TYPE_P: u8 = 1;
/// See [`HEVC_SLICE_TYPE_P`].
#[cfg(feature = "encode-h265")]
const HEVC_SLICE_TYPE_I: u8 = 2;
/// `nal_unit_type` 19, `IDR_W_RADL`: the intra picture a decoder joins on.
#[cfg(feature = "encode-h265")]
const HEVC_NAL_IDR_W_RADL: u8 = 19;
/// `nal_unit_type` 1, `TRAIL_R`: a reference picture that is not a random
/// access point.
#[cfg(feature = "encode-h265")]
const HEVC_NAL_TRAIL_R: u8 = 1;
/// `coding_type` in `VAEncPictureParameterBufferHEVC`: 1 for an I picture,
/// 2 for a P picture. Not the same numbering as `slice_type` above, and not
/// the same as H.264's.
#[cfg(feature = "encode-h265")]
const HEVC_CODING_TYPE_I: u32 = 1;
/// See [`HEVC_CODING_TYPE_I`].
#[cfg(feature = "encode-h265")]
const HEVC_CODING_TYPE_P: u32 = 2;
/// `collocated_ref_pic_index` meaning "no collocated picture", which is what
/// every picture here has: temporal motion vector prediction is off.
#[cfg(feature = "encode-h265")]
const HEVC_NO_COLLOCATED_REF: u8 = 0xFF;

/// How often an IDR is emitted when nobody asked. Every frame is otherwise a
/// P-frame, and a guest that joins mid-stream would wait forever; §11's
/// `KeyframeRequest` is the responsive path and this is the backstop.
const IDR_PERIOD: u32 = 120;

/// Whether a genuinely usable VA-API encoder for `config.codec` exists right
/// now (§18).
///
/// For H.264 this runs the exact same sequence [`VaapiEncoder::new`] runs —
/// open the DRM display, create a config for H.264 + `VAEntrypointEncSlice`,
/// allocate NV12 surfaces and create the encode context — so it cannot claim
/// an availability that construction then fails to back up.
///
/// H.265 is answered the same way in a build made with `encode-h265`, through
/// `VAProfileHEVCMain` and its own sequence, picture and slice parameter
/// buffers — never the H.264 ones (ADR 0071). Without that feature it is
/// `false` before any libva call is made, which is what makes enabling H.265
/// a single build-time switch.
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
        VideoCodec::H264 => VaapiEncoder::open(PROBE_WIDTH, PROBE_HEIGHT, config).is_ok(),
        #[cfg(feature = "encode-h265")]
        VideoCodec::H265 => {
            VaapiEncoder::open(OPTIONAL_PROBE_WIDTH, OPTIONAL_PROBE_HEIGHT, config).is_ok()
        }
        #[cfg(not(feature = "encode-h265"))]
        VideoCodec::H265 => false,
        VideoCodec::Av1 => false,
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
        #[cfg(feature = "encode-h265")]
        VideoCodec::H265 => Ok(VAProfile::VAProfileHEVCMain),
        #[cfg(not(feature = "encode-h265"))]
        VideoCodec::H265 => Err(MediaError::EncoderUnavailable(
            "this build was not made with H.265 support".to_owned(),
        )),
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
        #[cfg(feature = "encode-h265")]
        VideoCodec::H265 => Ok(HEVC_CTU),
        #[cfg(not(feature = "encode-h265"))]
        VideoCodec::H265 => Err(MediaError::EncoderUnavailable(
            "this build was not made with H.265 support".to_owned(),
        )),
        VideoCodec::Av1 => Err(MediaError::EncoderUnavailable(
            "the VA-API backend cannot drive AV1 encoding (ADR 0069)".to_owned(),
        )),
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
    /// The reconstructed picture the driver writes back, which the next
    /// P-frame predicts from.
    reference: Surface<()>,
    context: Rc<Context>,
    // Held, never read: libva objects are only valid while the config and
    // display that produced them are alive, and declaration order is what
    // makes them outlive the context above.
    _config: Config,
    _display: Rc<Display>,
}

/// Hardware H.264 encoder backed by VA-API.
pub struct VaapiEncoder {
    session: Session,
    config: EncoderConfig,
    /// Coded dimensions, always a whole number of macroblocks.
    dims: (u32, u32),
    /// `frame_num` of the next picture, wrapped at the sequence's maximum.
    frame_num: u16,
    /// Display order count of the next picture, in H.264's 2x units.
    pic_order_cnt: u16,
    /// Set by [`VideoEncoder::request_keyframe`] and by the `IDR_PERIOD`
    /// backstop; cleared once the IDR is actually emitted.
    force_idr: bool,
    /// Frames emitted since the last IDR.
    since_idr: u32,
}

// Mirrors the Media Foundation encoder's `Debug`: driver state is neither
// printable nor safe to log, only the settings that matter for a log line.
impl std::fmt::Debug for VaapiEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaapiEncoder")
            .field("config", &self.config)
            .field("dims", &self.dims)
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
        // Before anything touches libva: a codec this backend has no profile
        // for must be refused without opening a display (ADR 0069, ADR 0071).
        let profile = va_profile(config.codec)?;
        let (width, height) = aligned_dims(width, height, coding_block(config.codec)?);

        let display = Display::open().ok_or_else(|| {
            MediaError::EncoderUnavailable(
                "no VA-API display could be opened on any DRM device".to_owned(),
            )
        })?;

        let entrypoints = display.query_config_entrypoints(profile).map_err(|e| {
            MediaError::EncoderUnavailable(format!("vaQueryConfigEntrypoints: {e}"))
        })?;
        if !entrypoints.contains(&VAEntrypoint::VAEntrypointEncSlice) {
            return Err(MediaError::EncoderUnavailable(format!(
                "this VA-API driver has no {:?} encode-slice entrypoint",
                config.codec
            )));
        }

        let config_handle = display
            .create_config(Vec::new(), profile, VAEntrypoint::VAEntrypointEncSlice)
            .map_err(|e| MediaError::EncoderUnavailable(format!("vaCreateConfig: {e}")))?;

        // Two surfaces: the picture being encoded and the reconstruction the
        // next P-frame predicts from.
        let mut surfaces = display
            .create_surfaces(
                VA_RT_FORMAT_YUV420,
                Some(VA_FOURCC_NV12),
                width,
                height,
                Some(UsageHint::USAGE_HINT_ENCODER),
                vec![(), ()],
            )
            .map_err(|e| MediaError::EncoderUnavailable(format!("vaCreateSurfaces: {e}")))?;
        if surfaces.len() < 2 {
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

        let reference = surfaces.pop().ok_or_else(|| {
            MediaError::EncoderUnavailable("no reference surface available".to_owned())
        })?;
        let input = surfaces.pop().ok_or_else(|| {
            MediaError::EncoderUnavailable("no input surface available".to_owned())
        })?;

        Ok(Self {
            session: Session {
                coded,
                nv12_format,
                input,
                reference,
                context,
                _config: config_handle,
                _display: display,
            },
            config,
            dims: (width, height),
            frame_num: 0,
            pic_order_cnt: 0,
            force_idr: true,
            since_idr: 0,
        })
    }

    /// Reopens the session at `width` x `height`, which the first real frame
    /// triggers because the probe geometry is not the screen's.
    fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        let reopened = Self::open(width, height, self.config)?;
        *self = reopened;
        Ok(())
    }
}

/// Rounds up to whole coding blocks — [`MACROBLOCK`]s for H.264, [`HEVC_CTU`]s
/// for H.265 — which is the only geometry either codec codes in.
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

impl VideoEncoder for VaapiEncoder {
    fn encode(&mut self, frame: &Frame) -> Result<EncodedFrame> {
        let (nv12, src_width, src_height) = bgra_to_nv12(frame)?;
        let (width, height) = aligned_dims(src_width, src_height, coding_block(self.config.codec)?);
        if (width, height) != self.dims {
            self.resize(width, height)?;
        }

        let idr = self.force_idr || self.since_idr >= IDR_PERIOD;

        let data = encode_one(self, &nv12, src_width, src_height, idr)?;

        if idr {
            self.force_idr = false;
            self.since_idr = 0;
            self.frame_num = 0;
            self.pic_order_cnt = 0;
        } else {
            self.since_idr = self.since_idr.saturating_add(1);
            self.frame_num = self.frame_num.wrapping_add(1);
        }
        self.pic_order_cnt = self.pic_order_cnt.wrapping_add(2);

        Ok(EncodedFrame {
            keyframe: idr,
            timestamp_us: frame.timestamp_us,
            data,
        })
    }

    fn set_bitrate(&mut self, bitrate_kbps: u32) -> Result<()> {
        // Nothing is rebuilt: the rate-control buffer travels with the next
        // picture, which is what makes this usable at
        // `ABR_ADJUST_MAX_RATE_PER_SEC` (§11, §14). An encoder that needed a
        // new session per adjustment would be useless here, and that is why
        // the bitrate lives in the config rather than in the driver.
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

/// Submits one picture and returns its bitstream.
///
/// The parameter buffers are assembled by the helpers below rather than
/// inline: VA-API takes an H.264 sequence, picture and slice header as three
/// separate structs and every field of each has to be named, so one function
/// holding all of them is neither readable nor reviewable.
fn encode_one(
    encoder: &mut VaapiEncoder,
    nv12: &[u8],
    visible_width: u32,
    visible_height: u32,
    idr: bool,
) -> Result<Vec<u8>> {
    use cros_libva::Picture;

    let (width, height) = encoder.dims;
    let block = coding_block(encoder.config.codec)?;
    let units_wide = u16::try_from(width / block).unwrap_or(u16::MAX);
    let units_high = u16::try_from(height / block).unwrap_or(u16::MAX);

    // Uploaded before the `Picture` is built: `Image` borrows the surface and
    // `Picture::new` takes it mutably.
    upload_nv12(
        &encoder.session.input,
        encoder.session.nv12_format,
        nv12,
        width,
        height,
    )?;

    let reference_id = encoder.session.reference.id();
    let coded_id = encoder.session.coded.id();
    let context = Rc::clone(&encoder.session.context);
    let pic_order_cnt = encoder.pic_order_cnt;

    let mut buffers = Vec::with_capacity(4);
    // Sequence header, resent with every IDR: a guest that joined mid-stream
    // needs the parameter sets describing what follows, and there is no
    // out-of-band channel to send them on.
    //
    // Every buffer below is per codec, and that is the whole point of this
    // match: H.265's sequence, picture and slice parameters are different
    // structures with different field meanings — its `slice_type` even
    // numbers I and P the other way round from H.264's — so reusing the
    // H.264 builders would be a stream the driver accepts and no decoder can
    // read (§11's mutual-hardware-support rule; ADR 0071).
    match encoder.config.codec {
        VideoCodec::H264 => {
            if idr {
                buffers.push(sequence_buffer(
                    encoder,
                    units_wide,
                    units_high,
                    visible_width,
                    visible_height,
                )?);
            }
            // Sent with every picture, which is what lets `set_bitrate` take
            // effect without rebuilding anything.
            buffers.push(rate_control_buffer(encoder)?);
            buffers.push(picture_buffer(encoder, coded_id, reference_id, idr)?);
            buffers.push(slice_buffer(
                encoder,
                units_wide,
                units_high,
                reference_id,
                idr,
            )?);
        }
        #[cfg(feature = "encode-h265")]
        VideoCodec::H265 => {
            if idr {
                buffers.push(hevc_sequence_buffer(encoder)?);
            }
            buffers.push(rate_control_buffer(encoder)?);
            buffers.push(hevc_picture_buffer(encoder, coded_id, reference_id, idr)?);
            buffers.push(hevc_slice_buffer(
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
        #[cfg(not(feature = "encode-h265"))]
        VideoCodec::H265 => {
            return Err(MediaError::Encode(
                "this build was not made with H.265 support".to_owned(),
            ));
        }
        VideoCodec::Av1 => {
            return Err(MediaError::Encode(
                "the VA-API backend cannot drive AV1 encoding (ADR 0069)".to_owned(),
            ));
        }
    }

    let mut picture = Picture::new(
        u64::from(pic_order_cnt),
        context,
        &mut encoder.session.input,
    );
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
        let mapped = MappedCodedBuffer::new(&encoder.session.coded)
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

    // The reconstruction becomes the next frame's reference.
    std::mem::swap(&mut encoder.session.input, &mut encoder.session.reference);

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
    visible_width: u32,
    visible_height: u32,
) -> Result<cros_libva::Buffer> {
    let (width, height) = encoder.dims;
    let seq_fields = H264EncSeqFields::new(
        1, // chroma_format_idc: 4:2:0
        1, // frame_mbs_only_flag: progressive only
        0, // mb_adaptive_frame_field_flag
        0, // seq_scaling_matrix_present_flag
        1, // direct_8x8_inference_flag
        0, // log2_max_frame_num_minus4
        0, // pic_order_cnt_type
        2, // log2_max_pic_order_cnt_lsb_minus4
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
        LEVEL_IDC,
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

/// The rate-control misc buffer: how a bitrate change reaches the driver
/// without rebuilding the session.
fn rate_control_buffer(encoder: &VaapiEncoder) -> Result<cros_libva::Buffer> {
    let rc = EncMiscParameterRateControl::new(
        encoder.config.bitrate_kbps.saturating_mul(BITS_PER_KBIT),
        100, // target_percentage: spend the whole budget
        // Rate-control window in milliseconds: one second, matching the
        // period `AbrController` itself adjusts on.
        BITS_PER_KBIT,
        INITIAL_QP,
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
    reference_id: u32,
    idr: bool,
) -> Result<cros_libva::Buffer> {
    let curr_pic = PictureH264::new(
        reference_id,
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
        1, // last_picture: one picture per submission
        encoder.frame_num,
        u8::try_from(INITIAL_QP).unwrap_or(26),
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
        0,
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

/// A `VAPictureHEVC` slot meaning "nothing here".
#[cfg(feature = "encode-h265")]
fn hevc_invalid_picture() -> PictureHEVC {
    PictureHEVC::new(VA_INVALID_ID, 0, VA_PICTURE_HEVC_INVALID)
}

/// The single reference of [`MAX_REF_FRAMES`] for H.265: the frame before
/// this one.
#[cfg(feature = "encode-h265")]
fn hevc_previous_reference(encoder: &VaapiEncoder, reference_id: u32) -> PictureHEVC {
    PictureHEVC::new(
        reference_id,
        i32::from(encoder.pic_order_cnt.saturating_sub(2)),
        0,
    )
}

/// The H.265 sequence header (VPS/SPS) in the shape VA-API wants it.
///
/// The picture size goes in luma samples here rather than in coded units the
/// way H.264 counts macroblocks, and `encoder.dims` is already rounded up to
/// whole [`HEVC_CTU`]s — so there is no separate crop to apply: a screen that
/// is not a multiple of 32 is coded at the rounded size, the same size the
/// guest is told about, rather than cropped back down inside the stream.
#[cfg(feature = "encode-h265")]
fn hevc_sequence_buffer(encoder: &VaapiEncoder) -> Result<cros_libva::Buffer> {
    let (width, height) = encoder.dims;
    let seq_fields = HEVCEncSeqFields::new(
        1, // chroma_format_idc: 4:2:0
        0, // separate_colour_plane_flag
        0, // bit_depth_luma_minus8: 8-bit
        0, // bit_depth_chroma_minus8: 8-bit
        0, // scaling_list_enabled_flag
        0, // strong_intra_smoothing_enabled_flag
        1, // amp_enabled_flag: asymmetric partitions, free quality on edges
        1, // sample_adaptive_offset_enabled_flag
        0, // pcm_enabled_flag
        0, // pcm_loop_filter_disabled_flag
        // Temporal motion vector prediction off: it predicts from a picture's
        // collocated block, which is one more dependency between frames in a
        // stream that already keeps exactly one reference.
        0, // sps_temporal_mvp_enabled_flag
        1, // low_delay_seq: no B-frames, nothing reordered
        0, // hierachical_flag
    );
    let seq = EncSequenceParameterBufferHEVC::new(
        HEVC_PROFILE_IDC,
        HEVC_LEVEL_IDC,
        0, // general_tier_flag: main tier
        IDR_PERIOD,
        IDR_PERIOD,
        1, // ip_period: no B-frames
        encoder.config.bitrate_kbps.saturating_mul(BITS_PER_KBIT),
        u16::try_from(width).unwrap_or(u16::MAX),
        u16::try_from(height).unwrap_or(u16::MAX),
        &seq_fields,
        HEVC_LOG2_MIN_CB_MINUS3,
        HEVC_LOG2_DIFF_MAX_MIN_CB,
        HEVC_LOG2_MIN_TB_MINUS2,
        HEVC_LOG2_DIFF_MAX_MIN_TB,
        HEVC_MAX_TRANSFORM_DEPTH,
        HEVC_MAX_TRANSFORM_DEPTH,
        0, // pcm_sample_bit_depth_luma_minus1
        0, // pcm_sample_bit_depth_chroma_minus1
        0, // log2_min_pcm_luma_coding_block_size_minus3
        0, // log2_max_pcm_luma_coding_block_size_minus3
        // No VUI: nothing downstream reads timing or aspect information out
        // of the stream — the guest is told the picture size by the chunk it
        // arrives in — and an absent VUI is one less thing to get wrong.
        None,
        0, // aspect_ratio_idc
        0, // sar_width
        0, // sar_height
        0, // vui_num_units_in_tick
        0, // vui_time_scale
        0, // min_spatial_segmentation_idc
        0, // max_bytes_per_pic_denom
        0, // max_bits_per_min_cu_denom
        &HevcEncSeqSccFields::new(0),
    );
    encoder
        .session
        .context
        .create_buffer(BufferType::EncSequenceParameter(
            EncSequenceParameter::HEVC(seq),
        ))
        .map_err(|e| MediaError::Encode(format!("HEVC sequence parameter buffer: {e}")))
}

/// The H.265 picture header (PPS plus this picture's own fields).
#[cfg(feature = "encode-h265")]
fn hevc_picture_buffer(
    encoder: &VaapiEncoder,
    coded_id: u32,
    reference_id: u32,
    idr: bool,
) -> Result<cros_libva::Buffer> {
    let curr_pic = PictureHEVC::new(reference_id, i32::from(encoder.pic_order_cnt), 0);
    let mut refs = std::array::from_fn::<_, 15, _>(|_| hevc_invalid_picture());
    if !idr {
        refs[0] = hevc_previous_reference(encoder, reference_id);
    }
    let pic_fields = HEVCEncPicFields::new(
        u32::from(idr), // idr_pic_flag
        if idr {
            HEVC_CODING_TYPE_I
        } else {
            HEVC_CODING_TYPE_P
        },
        1, // reference_pic_flag: every picture here is the next one's reference
        0, // dependent_slice_segments_enabled_flag
        0, // sign_data_hiding_enabled_flag
        0, // constrained_intra_pred_flag
        0, // transform_skip_enabled_flag
        0, // cu_qp_delta_enabled_flag
        0, // weighted_pred_flag
        0, // weighted_bipred_flag
        0, // transquant_bypass_enabled_flag
        0, // tiles_enabled_flag: one tile, as there is one slice
        0, // entropy_coding_sync_enabled_flag
        0, // loop_filter_across_tiles_enabled_flag
        1, // pps_loop_filter_across_slices_enabled_flag
        0, // scaling_list_data_present_flag
        // Screen content coding tools are a separate profile this does not
        // ask for, whatever the pictures happen to contain.
        0, // screen_content_flag
        0, // enable_gpu_weighted_prediction
        0, // no_output_of_prior_pics_flag
    );
    let pic = EncPictureParameterBufferHEVC::new(
        curr_pic,
        refs,
        coded_id,
        HEVC_NO_COLLOCATED_REF,
        1, // last_picture: one picture per submission
        u8::try_from(INITIAL_QP).unwrap_or(u8::MAX),
        0, // diff_cu_qp_delta_depth
        0, // pps_cb_qp_offset
        0, // pps_cr_qp_offset
        0, // num_tile_columns_minus1
        0, // num_tile_rows_minus1
        [0; 19],
        [0; 21],
        0, // log2_parallel_merge_level_minus2
        0, // ctu_max_bitsize_allowed: no per-CTU cap
        0, // num_ref_idx_l0_default_active_minus1: one reference
        0, // num_ref_idx_l1_default_active_minus1
        0, // slice_pic_parameter_set_id
        if idr {
            HEVC_NAL_IDR_W_RADL
        } else {
            HEVC_NAL_TRAIL_R
        },
        &pic_fields,
        0, // hierarchical_level_plus1
        0, // va_byte_reserved
        &HevcEncPicSccFields::new(0),
    );
    encoder
        .session
        .context
        .create_buffer(BufferType::EncPictureParameter(EncPictureParameter::HEVC(
            pic,
        )))
        .map_err(|e| MediaError::Encode(format!("HEVC picture parameter buffer: {e}")))
}

/// The H.265 slice header. One slice per picture, for the reason
/// [`slice_buffer`] gives for H.264.
#[cfg(feature = "encode-h265")]
fn hevc_slice_buffer(
    encoder: &VaapiEncoder,
    ctus_wide: u16,
    ctus_high: u16,
    reference_id: u32,
    idr: bool,
) -> Result<cros_libva::Buffer> {
    let total_ctus = u32::from(ctus_wide) * u32::from(ctus_high);
    let mut list0 = std::array::from_fn::<_, 15, _>(|_| hevc_invalid_picture());
    if !idr {
        list0[0] = hevc_previous_reference(encoder, reference_id);
    }
    let list1 = std::array::from_fn::<_, 15, _>(|_| hevc_invalid_picture());
    let slice_fields = HevcEncSliceFields::new(
        1, // last_slice_of_pic_flag: the only slice
        0, // dependent_slice_segment_flag
        0, // colour_plane_id
        0, // slice_temporal_mvp_enabled_flag: matches sps_temporal_mvp above
        1, // slice_sao_luma_flag: matches sample_adaptive_offset above
        1, // slice_sao_chroma_flag
        1, // num_ref_idx_active_override_flag
        0, // mvd_l1_zero_flag
        0, // cabac_init_flag
        0, // slice_deblocking_filter_disabled_flag
        1, // slice_loop_filter_across_slices_enabled_flag
        1, // collocated_from_l0_flag
    );
    let slice = EncSliceParameterBufferHEVC::new(
        0, // slice_segment_address: the picture's first CTU
        total_ctus,
        if idr {
            HEVC_SLICE_TYPE_I
        } else {
            HEVC_SLICE_TYPE_P
        },
        0, // slice_pic_parameter_set_id
        0, // num_ref_idx_l0_active_minus1: one reference
        0, // num_ref_idx_l1_active_minus1
        list0,
        list1,
        0, // luma_log2_weight_denom
        0, // delta_chroma_log2_weight_denom
        [0; 15],
        [0; 15],
        [[0; 2]; 15],
        [[0; 2]; 15],
        [0; 15],
        [0; 15],
        [[0; 2]; 15],
        [[0; 2]; 15],
        HEVC_MAX_MERGE_CAND,
        0, // slice_qp_delta: the rate controller owns the quantizer
        0, // slice_cb_qp_offset
        0, // slice_cr_qp_offset
        0, // slice_beta_offset_div2
        0, // slice_tc_offset_div2
        &slice_fields,
        0, // pred_weight_table_bit_offset
        0, // pred_weight_table_bit_length
    );
    encoder
        .session
        .context
        .create_buffer(BufferType::EncSliceParameter(EncSliceParameter::HEVC(
            slice,
        )))
        .map_err(|e| MediaError::Encode(format!("HEVC slice parameter buffer: {e}")))
}

/// Copies an NV12 buffer into a VA surface, honouring the driver's own
/// stride, which is rarely the picture width.
fn upload_nv12(
    surface: &Surface<()>,
    format: cros_libva::VAImageFormat,
    nv12: &[u8],
    width: u32,
    height: u32,
) -> Result<()> {
    use cros_libva::Image;

    let mut image = Image::create_from(surface, format, (width, height), (width, height))
        .map_err(|e| MediaError::Encode(format!("vaCreateImage: {e}")))?;

    let (w, h) = (width as usize, height as usize);
    let luma_len = w * h;
    if nv12.len() < luma_len + luma_len / 2 {
        return Err(MediaError::Encode(
            "NV12 buffer is shorter than the surface it is being uploaded into".to_owned(),
        ));
    }

    let image_inner = *image.image();
    let offsets = image_inner.offsets;
    let pitches = image_inner.pitches;
    let dst = image.as_mut();

    // Luma plane, row by row: `pitches[0]` is the driver's stride.
    let y_pitch = pitches[0] as usize;
    let y_off = offsets[0] as usize;
    for row in 0..h {
        let src = row * w;
        let dst_start = y_off + row * y_pitch;
        let Some(target) = dst.get_mut(dst_start..dst_start + w) else {
            return Err(MediaError::Encode(
                "the mapped surface is smaller than its own luma plane".to_owned(),
            ));
        };
        target.copy_from_slice(&nv12[src..src + w]);
    }

    // Interleaved chroma plane: half the rows, same width in bytes.
    let uv_pitch = pitches[1] as usize;
    let uv_off = offsets[1] as usize;
    for row in 0..h / 2 {
        let src = luma_len + row * w;
        let dst_start = uv_off + row * uv_pitch;
        let Some(target) = dst.get_mut(dst_start..dst_start + w) else {
            return Err(MediaError::Encode(
                "the mapped surface is smaller than its own chroma plane".to_owned(),
            ));
        };
        target.copy_from_slice(&nv12[src..src + w]);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failed assumption must fail the test")]

    use super::*;

    /// The central "never claim hardware is available when it isn't" rule of
    /// this task, checked mechanically rather than by inspection — the twin
    /// of `encode::windows`'s test of the same name.
    ///
    /// Runs identically on a machine with a VA-API encoder and one without,
    /// because it only asserts that the two answers agree. On CI and on
    /// every virtual machine tried so far, both are `false`, which is the
    /// case that matters most: a host with libva installed and no
    /// encode-capable GPU must fall back to `openh264` rather than fail the
    /// session (§18).
    #[test]
    fn probe_hardware_agrees_with_whether_construction_actually_works() {
        let probed = hardware_available(EncoderConfig::default());
        let constructed = VaapiEncoder::new(EncoderConfig::default()).is_ok();
        assert_eq!(
            probed,
            constructed,
            "hardware_available reported {probed} but construction {}",
            if constructed { "succeeded" } else { "failed" }
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

    /// H.265 counts in 32-pixel coding tree units, not 16-pixel macroblocks,
    /// so a screen whose height is not a multiple of 32 codes at a different
    /// size under each: 1440x900 rounds to 1440x912 for H.264 and 1440x928
    /// for H.265. Handing the H.264 alignment to an HEVC session is a picture
    /// geometry the driver takes and no decoder agrees with (ADR 0071).
    #[cfg(feature = "encode-h265")]
    #[test]
    fn h265_rounds_up_to_whole_coding_tree_units_instead() {
        assert_eq!(aligned_dims(1920, 1080, HEVC_CTU), (1920, 1088));
        assert_eq!(aligned_dims(1440, 900, HEVC_CTU), (1440, 928));
        assert_eq!(aligned_dims(1440, 900, MACROBLOCK), (1440, 912));
        assert_eq!(aligned_dims(0, 0, HEVC_CTU), (32, 32));
    }

    /// Each codec is opened under its own VA-API profile, and a codec this
    /// backend cannot drive is refused before libva is touched at all — which
    /// is what makes `encode-h265` a build-time switch rather than a runtime
    /// hope (ADR 0071).
    #[test]
    fn each_codec_is_opened_under_its_own_profile() {
        assert_eq!(
            va_profile(VideoCodec::H264).ok(),
            Some(VAProfile::VAProfileH264ConstrainedBaseline)
        );
        assert!(va_profile(VideoCodec::Av1).is_err());
        assert!(coding_block(VideoCodec::Av1).is_err());
        #[cfg(feature = "encode-h265")]
        {
            assert_eq!(
                va_profile(VideoCodec::H265).ok(),
                Some(VAProfile::VAProfileHEVCMain)
            );
            assert_ne!(
                va_profile(VideoCodec::H265).ok(),
                va_profile(VideoCodec::H264).ok()
            );
            assert_eq!(coding_block(VideoCodec::H265).ok(), Some(HEVC_CTU));
        }
        #[cfg(not(feature = "encode-h265"))]
        {
            assert!(va_profile(VideoCodec::H265).is_err());
            assert!(coding_block(VideoCodec::H265).is_err());
            assert!(!hardware_available(EncoderConfig {
                codec: VideoCodec::H265,
                ..EncoderConfig::default()
            }));
        }
    }

    #[test]
    fn the_coded_buffer_is_never_smaller_than_a_worst_case_keyframe() {
        assert!(coded_buffer_size(1920, 1088) >= 1920 * 1088);
        // Tiny pictures still get a floor: a 16x16 IDR plus its SPS/PPS is
        // bigger than 16x16x1.5 bytes.
        assert!(coded_buffer_size(16, 16) >= 1 << 16);
    }
}
