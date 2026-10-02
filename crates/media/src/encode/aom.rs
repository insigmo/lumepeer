//! Software AV1: libaom's realtime encoder for a host with no hardware
//! encoder (§11; ADR 0139).
//!
//! The configuration is the one the stage-1 measurement ran
//! (docs/research/software-av1.md): `cpu-used` 10, WebRTC's RTC settings, the
//! screen-content tools, four tile columns with row threading, the whole
//! quantizer range. libaom itself is vendored and linked statically by
//! `lumepeer-aom-sys`, with its assembly, so an installed host carries all of
//! it and needs nothing on the machine.
//!
//! Pixels go through the same BGRA-to-I420 conversion as the `openh264`
//! fallback — `openh264::formats::YUVBuffer`, the converter the measurement
//! timed for both — and are read through [`Frame::as_cpu`], never
//! [`Frame::data`], so a zero-copy capture frame (ADR 0073) encodes like any
//! other.
//!
//! The encoder is opened at the first frame, because libaom fixes the
//! picture size at initialisation, and opened again when the size changes.
//! Each new encoder starts with a keyframe on its own.

use std::ffi::{CStr, c_char, c_int};
use std::ptr::NonNull;

use lumepeer_aom_sys as sys;
use lumepeer_core::constants::{
    SOFTWARE_AV1_BITRATE_PERCENT, SOFTWARE_AV1_MAX_FPS, SOFTWARE_AV1_MAX_THREADS,
    SOFTWARE_AV1_SPEED,
};
use openh264::formats::{BgraSliceU8, YUVBuffer, YUVSource};

use super::software::even_bgra;
use super::{EncodedFrame, EncoderConfig, EncoderKind, VideoEncoder};
use crate::capture::Frame;
use crate::error::{MediaError, Result};

/// Four tile columns, as measured: the columns are what row threading
/// spreads across cores.
const TILE_COLUMNS_LOG2: c_int = 2;
/// The whole AV1 quantizer range. WebRTC's floor of 10 held a desktop under
/// about 2 Mbit/s and cut the quality curve off (docs/research/software-av1.md).
const MIN_QUANTIZER: c_int = 0;
const MAX_QUANTIZER: c_int = 63;
/// Room for libaom's error text.
const ERROR_BYTES: usize = 256;

/// The bitrate libaom is asked for when the session asks for `h264_kbps`
/// (§11; ADR 0139): the share the BD-rate measurement found buys the same
/// picture, never zero.
#[must_use]
pub fn av1_kbps(h264_kbps: u32) -> u32 {
    (u64::from(h264_kbps) * u64::from(SOFTWARE_AV1_BITRATE_PERCENT) / 100)
        .clamp(1, u64::from(u32::MAX))
        .try_into()
        .unwrap_or(u32::MAX)
}

/// Encoder threads: what the machine has, up to the measured eight.
fn threads() -> c_int {
    let available = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    c_int::try_from(available.clamp(1, SOFTWARE_AV1_MAX_THREADS)).unwrap_or(1)
}

/// libaom's version, e.g. `v3.15.1`.
#[must_use]
pub fn version() -> String {
    // SAFETY: `aom_shim_version` returns libaom's static, NUL-terminated
    // version string; it is never freed and never null.
    unsafe { CStr::from_ptr(sys::aom_shim_version()) }
        .to_string_lossy()
        .into_owned()
}

/// One open libaom encoder, owned.
struct Shim {
    raw: NonNull<sys::AomShim>,
    width: usize,
    height: usize,
}

// SAFETY: the shim and the libaom context inside it are only ever touched
// through `&mut self` (or `&self` for reads of its own buffers), so moving the
// owner to another thread moves exclusive access with it. libaom keeps no
// thread affinity; its worker threads are its own.
#[allow(
    unsafe_code,
    reason = "a raw pointer is not Send; the encoder is used by one thread at a time"
)]
unsafe impl Send for Shim {}

impl Shim {
    fn open(width: usize, height: usize, fps: u8, kbps: u32) -> Result<Self> {
        let dimension = |value: usize| {
            c_int::try_from(value)
                .map_err(|_| MediaError::Encode(format!("a {value}-pixel side is too large")))
        };
        let mut error = [0 as c_char; ERROR_BYTES];
        // SAFETY: plain values in, and an error buffer of exactly the length
        // passed (`ERROR_BYTES` fits a c_int). The shim writes at most
        // `errlen` bytes into it, NUL-terminated (`snprintf`).
        let raw = unsafe {
            sys::aom_shim_open(
                dimension(width)?,
                dimension(height)?,
                c_int::from(fps),
                c_int::try_from(kbps).unwrap_or(c_int::MAX),
                SOFTWARE_AV1_SPEED,
                1,
                threads(),
                TILE_COLUMNS_LOG2,
                MIN_QUANTIZER,
                MAX_QUANTIZER,
                error.as_mut_ptr(),
                c_int::try_from(ERROR_BYTES).unwrap_or(c_int::MAX),
            )
        };
        let Some(raw) = NonNull::new(raw) else {
            // SAFETY: on failure the shim has written a NUL-terminated reason
            // into `error` (or left it zeroed, which reads as empty).
            let reason = unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy();
            return Err(MediaError::EncoderUnavailable(format!("libaom: {reason}")));
        };
        let shim = Self { raw, width, height };
        let refused = shim.text(sys::aom_shim_refused);
        if !refused.is_empty() {
            tracing::info!(%refused, "libaom refused controls this build has no use for");
        }
        Ok(shim)
    }

    /// One of the shim's own NUL-terminated strings.
    fn text(&self, read: unsafe extern "C" fn(*const sys::AomShim) -> *const c_char) -> String {
        // SAFETY: `raw` is a live encoder (dropped only in `Drop`), and both
        // functions passed here return a pointer into its own NUL-terminated
        // buffers, valid until the next call that writes them — which cannot
        // happen while `&self` is held.
        unsafe { CStr::from_ptr(read(self.raw.as_ptr())) }
            .to_string_lossy()
            .into_owned()
    }

    fn encode(&mut self, yuv: &YUVBuffer, pts: i64, keyframe: bool) -> Result<(Vec<u8>, bool)> {
        let (y_stride, u_stride, _) = yuv.strides();
        let stride = |value: usize| {
            c_int::try_from(value).map_err(|_| MediaError::Encode("stride too large".to_owned()))
        };
        let (mut out, mut out_len, mut is_key) = (std::ptr::null(), 0usize, 0 as c_int);
        // SAFETY: `raw` is live and exclusively ours (`&mut self`). The three
        // planes belong to `yuv`, which outlives the call, and hold an I420
        // picture of exactly `width`x`height` at the strides passed — the
        // size this encoder was opened at, which `AomEncoder::encode`
        // checked. The shim writes the three out-parameters and nothing else.
        let status = unsafe {
            sys::aom_shim_encode(
                self.raw.as_ptr(),
                yuv.y().as_ptr(),
                yuv.u().as_ptr(),
                yuv.v().as_ptr(),
                stride(y_stride)?,
                stride(u_stride)?,
                pts,
                c_int::from(keyframe),
                &raw mut out,
                &raw mut out_len,
                &raw mut is_key,
            )
        };
        if status != 0 {
            return Err(MediaError::Encode(self.text(sys::aom_shim_error)));
        }
        let data = if out_len == 0 || out.is_null() {
            Vec::new()
        } else {
            // SAFETY: on success `out` points at `out_len` bytes the shim
            // owns until the next call on this encoder; they are copied out
            // before `&mut self` is released.
            unsafe { std::slice::from_raw_parts(out, out_len) }.to_vec()
        };
        Ok((data, is_key != 0))
    }

    fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        // SAFETY: `raw` is live and exclusively ours (`&mut self`).
        let status = unsafe {
            sys::aom_shim_set_bitrate(
                self.raw.as_ptr(),
                c_int::try_from(kbps).unwrap_or(c_int::MAX),
            )
        };
        if status != 0 {
            return Err(MediaError::Encode(self.text(sys::aom_shim_error)));
        }
        Ok(())
    }

    fn bitrate(&self) -> u32 {
        // SAFETY: `raw` is live; the call only reads the encoder's own
        // configuration.
        let kbps = unsafe { sys::aom_shim_bitrate(self.raw.as_ptr()) };
        u32::try_from(kbps).unwrap_or(0)
    }
}

impl Drop for Shim {
    fn drop(&mut self) {
        // SAFETY: opened by `aom_shim_open`, closed exactly once, here.
        unsafe { sys::aom_shim_close(self.raw.as_ptr()) };
    }
}

/// Software AV1 encoder over libaom's realtime mode (§11; ADR 0139).
pub struct AomEncoder {
    /// What the session asked for: an H.264-equivalent bitrate and a frame
    /// rate already held to [`SOFTWARE_AV1_MAX_FPS`].
    config: EncoderConfig,
    /// Opened at the first frame, and again when the picture size changes.
    shim: Option<Shim>,
    /// Frames into the current encoder, as libaom's presentation timestamp
    /// in its time base of one frame interval.
    pts: i64,
    /// A keyframe has been asked for and not yet produced.
    keyframe: bool,
}

// libaom's context is not `Debug`, and the settings are what a log needs.
impl std::fmt::Debug for AomEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AomEncoder")
            .field("config", &self.config)
            .field(
                "size",
                &self.shim.as_ref().map(|shim| (shim.width, shim.height)),
            )
            .finish_non_exhaustive()
    }
}

impl AomEncoder {
    /// An encoder for `config`.
    ///
    /// `config.bitrate_kbps` is the session's H.264 figure; libaom is asked
    /// for [`av1_kbps`] of it. `config.fps` above [`SOFTWARE_AV1_MAX_FPS`] is
    /// held to it: this encoder is only chosen for the 30 fps preset, and the
    /// encode loop paces such a session at 30 whatever it asked for.
    ///
    /// Nothing is opened until the first frame says how large the picture
    /// is, so this cannot fail on its own; it returns `Result` like every
    /// other constructor of a `VideoEncoder`.
    ///
    /// # Errors
    /// None today.
    pub fn new(config: EncoderConfig) -> Result<Self> {
        let fps = config.fps.clamp(1, SOFTWARE_AV1_MAX_FPS);
        Ok(Self {
            config: EncoderConfig { fps, ..config },
            shim: None,
            pts: 0,
            keyframe: false,
        })
    }

    /// The frame rate this encoder budgets each frame by.
    #[must_use]
    pub fn fps(&self) -> u8 {
        self.config.fps
    }

    /// The bitrate libaom currently targets, kbit/s; `None` before the
    /// first frame.
    #[must_use]
    pub fn av1_bitrate_kbps(&self) -> Option<u32> {
        self.shim.as_ref().map(Shim::bitrate)
    }
}

impl VideoEncoder for AomEncoder {
    fn encode(&mut self, frame: &Frame) -> Result<EncodedFrame> {
        let (bgra, width, height) = even_bgra(frame, "the software AV1 encoder")?;
        let yuv = YUVBuffer::from_bgra8_source(BgraSliceU8::new(&bgra, (width, height)));
        let reopen = self
            .shim
            .as_ref()
            .is_none_or(|shim| (shim.width, shim.height) != (width, height));
        if reopen {
            // The old encoder goes first: two 1080p libaom contexts at once
            // is memory for nothing.
            self.shim = None;
            self.shim = Some(Shim::open(
                width,
                height,
                self.config.fps,
                av1_kbps(self.config.bitrate_kbps),
            )?);
            self.pts = 0;
        }
        let Some(shim) = self.shim.as_mut() else {
            return Err(MediaError::Encode(
                "the libaom encoder is not open".to_owned(),
            ));
        };
        let (data, keyframe) = shim.encode(&yuv, self.pts, self.keyframe)?;
        self.pts += 1;
        if keyframe {
            self.keyframe = false;
        }
        Ok(EncodedFrame {
            keyframe,
            timestamp_us: frame.timestamp_us,
            data,
        })
    }

    fn set_bitrate(&mut self, bitrate_kbps: u32) -> Result<()> {
        if bitrate_kbps == self.config.bitrate_kbps {
            return Ok(());
        }
        // Through `aom_codec_enc_config_set`: the stream carries on, with no
        // keyframe, unlike the `openh264` fallback, which is rebuilt.
        if let Some(shim) = self.shim.as_mut() {
            shim.set_bitrate(av1_kbps(bitrate_kbps))?;
        }
        self.config.bitrate_kbps = bitrate_kbps;
        Ok(())
    }

    fn request_keyframe(&mut self) -> Result<()> {
        self.keyframe = true;
        Ok(())
    }

    fn kind(&self) -> EncoderKind {
        EncoderKind::SoftwareAv1
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::capture::PixelFormat;
    use crate::encode::VideoCodec;

    fn config() -> EncoderConfig {
        EncoderConfig {
            codec: VideoCodec::Av1,
            fps: 30,
            bitrate_kbps: 8_000,
        }
    }

    /// A picture with something in it: a gradient that moves with `shift`.
    fn frame(width: u32, height: u32, shift: u32) -> Frame {
        let mut data = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                let value = u8::try_from((x + y + shift) % 256).unwrap();
                data.extend_from_slice(&[value, value / 2, 255 - value, 255]);
            }
        }
        Frame::cpu(width, height, PixelFormat::Bgra8, u64::from(shift), data)
    }

    /// The temporal unit starts with a temporal delimiter and carries a
    /// sequence header: what the guest's decoder and ADR 0069's random-access
    /// check both look for in a keyframe.
    fn has_sequence_header(unit: &[u8]) -> bool {
        // OBU type is bits 6..3 of the header byte; 1 = sequence header.
        let mut at = 0;
        while at < unit.len() {
            let header = unit[at];
            let kind = (header >> 3) & 0x0f;
            let has_size = header & 0x02 != 0;
            let extension = usize::from(header & 0x04 != 0);
            if kind == 1 {
                return true;
            }
            if !has_size {
                return false;
            }
            let mut size = 0usize;
            let mut cursor = at + 1 + extension;
            for shift in (0..56).step_by(7) {
                let byte = unit[cursor];
                cursor += 1;
                size |= usize::from(byte & 0x7f) << shift;
                if byte & 0x80 == 0 {
                    break;
                }
            }
            at = cursor + size;
        }
        false
    }

    #[test]
    fn the_first_frame_is_a_keyframe_with_a_sequence_header() {
        let mut encoder = AomEncoder::new(config()).unwrap();
        let first = encoder.encode(&frame(320, 180, 0)).unwrap();
        assert!(first.keyframe);
        assert!(has_sequence_header(&first.data));
        assert_eq!(encoder.kind(), EncoderKind::SoftwareAv1);
        let second = encoder.encode(&frame(320, 180, 4)).unwrap();
        assert!(!second.keyframe, "libaom made every frame a keyframe");
    }

    #[test]
    fn a_requested_keyframe_arrives_on_the_next_frame() {
        let mut encoder = AomEncoder::new(config()).unwrap();
        encoder.encode(&frame(320, 180, 0)).unwrap();
        assert!(!encoder.encode(&frame(320, 180, 1)).unwrap().keyframe);
        encoder.request_keyframe().unwrap();
        let forced = encoder.encode(&frame(320, 180, 2)).unwrap();
        assert!(forced.keyframe, "the keyframe request was ignored");
        assert!(has_sequence_header(&forced.data));
        assert!(!encoder.encode(&frame(320, 180, 3)).unwrap().keyframe);
    }

    /// `aom_codec_enc_config_set`, not a new encoder: the next frame is not a
    /// keyframe, and libaom holds the new target.
    #[test]
    fn a_bitrate_change_moves_the_target_without_a_keyframe() {
        let mut encoder = AomEncoder::new(config()).unwrap();
        encoder.encode(&frame(320, 180, 0)).unwrap();
        encoder.encode(&frame(320, 180, 1)).unwrap();
        assert_eq!(encoder.av1_bitrate_kbps(), Some(av1_kbps(8_000)));
        encoder.set_bitrate(2_000).unwrap();
        assert_eq!(encoder.av1_bitrate_kbps(), Some(av1_kbps(2_000)));
        let next = encoder.encode(&frame(320, 180, 2)).unwrap();
        assert!(!next.keyframe, "a bitrate change cost a keyframe");
    }

    #[test]
    fn a_new_picture_size_opens_a_new_encoder_from_a_keyframe() {
        let mut encoder = AomEncoder::new(config()).unwrap();
        encoder.encode(&frame(320, 180, 0)).unwrap();
        encoder.encode(&frame(320, 180, 1)).unwrap();
        let resized = encoder.encode(&frame(640, 360, 2)).unwrap();
        assert!(resized.keyframe);
        assert!(has_sequence_header(&resized.data));
    }

    #[test]
    fn odd_dimensions_are_cropped_rather_than_refused() {
        let mut encoder = AomEncoder::new(config()).unwrap();
        assert!(!encoder.encode(&frame(321, 181, 0)).unwrap().data.is_empty());
    }

    #[test]
    fn a_frame_rate_above_the_preset_is_held_to_thirty() {
        let encoder = AomEncoder::new(EncoderConfig {
            fps: 144,
            ..config()
        })
        .unwrap();
        assert_eq!(encoder.fps(), SOFTWARE_AV1_MAX_FPS);
    }

    #[test]
    fn the_av1_target_is_half_the_h264_one() {
        assert_eq!(av1_kbps(8_000), 4_000);
        assert_eq!(av1_kbps(1), 1, "never zero");
    }

    #[test]
    fn the_vendored_release_is_the_measured_one() {
        assert!(version().contains("3.15.1"), "libaom {}", version());
    }

    #[test]
    fn a_frame_that_is_not_bgra_is_refused_with_a_reason() {
        let mut encoder = AomEncoder::new(config()).unwrap();
        let nv12 = Frame::cpu(64, 64, PixelFormat::Nv12, 0, vec![0; 64 * 64 * 3 / 2]);
        let error = encoder.encode(&nv12).unwrap_err();
        assert!(error.to_string().contains("BGRA8"), "{error}");
    }

    /// A host with no hardware encoder still gets GPU frames from the
    /// zero-copy capture once the guest draws its own cursor (ADR 0073);
    /// reading `Frame::data` would refuse every one of them.
    #[test]
    #[cfg(all(target_os = "windows", feature = "encode-mf-zero-copy"))]
    fn a_frame_that_stayed_on_the_gpu_is_read_back_and_encoded() {
        let Some(source) = crate::capture::windows::gpu_test_frame(64, 64, 0x60) else {
            eprintln!("skipping: no Direct3D 11 hardware device on this machine");
            return;
        };
        assert!(source.data.is_empty(), "the test frame has to be GPU-only");
        let mut encoder = AomEncoder::new(config()).unwrap();
        let output = encoder
            .encode(&source)
            .unwrap_or_else(|error| panic!("a GPU frame failed to encode: {error}"));
        assert!(!output.data.is_empty());
    }
}
