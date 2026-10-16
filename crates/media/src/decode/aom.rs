//! libaom's AV1 decoder, for the sandboxed decoder worker of §11.3 (ADR 0162).
//!
//! What a guest whose webview cannot decode AV1 shows a host's picture with:
//! since ADR 0162 removed `openh264`, a host with no hardware encoder sends
//! AV1 from libaom whatever the guest's webview decodes, and the worker that
//! used to turn H.264 into pixels with `openh264` turns AV1 into them with the
//! same vendored libaom the host encodes with.
//!
//! The bitstream is untrusted input from the network, and this runs only
//! inside the worker's sandbox. A picture that is not 8-bit 4:2:0 — all a
//! host's encoders make — is refused rather than guessed at.

use std::ffi::{CStr, c_char, c_int};
use std::ptr::NonNull;

use lumepeer_aom_sys as sys;

use crate::error::{MediaError, Result};

/// Room for libaom's error text.
const ERROR_BYTES: usize = 256;

/// Decoder threads. libaom spreads a picture's tiles across them, and the
/// host's encoder cuts four tile columns (ADR 0141).
const DECODE_THREADS: c_int = 4;

/// One decoded picture in RGBA8, tightly packed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picture {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height * 4` bytes, row by row.
    pub rgba: Vec<u8>,
}

/// An open libaom AV1 decoder, owned.
pub struct AomDecoder {
    raw: NonNull<sys::AomShimDec>,
}

// SAFETY: the decoder is only ever touched through `&mut self` (or `&self`
// for reads of its own error text), so moving the owner to another thread
// moves exclusive access with it. libaom keeps no thread affinity; its worker
// threads are its own.
#[allow(
    unsafe_code,
    reason = "a raw pointer is not Send; the decoder is used by one thread at a time"
)]
unsafe impl Send for AomDecoder {}

// libaom's context is not `Debug`, and there is nothing in it a log needs.
impl std::fmt::Debug for AomDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AomDecoder").finish_non_exhaustive()
    }
}

impl AomDecoder {
    /// Opens a decoder.
    ///
    /// # Errors
    /// [`MediaError::DecoderWorker`] when libaom will not initialise one.
    pub fn new() -> Result<Self> {
        let mut error = [0 as c_char; ERROR_BYTES];
        // SAFETY: a plain value in, and an error buffer of exactly the length
        // passed (`ERROR_BYTES` fits a c_int). The shim writes at most
        // `errlen` bytes into it, NUL-terminated (`snprintf`).
        let raw = unsafe {
            sys::aom_shim_dec_open(
                DECODE_THREADS,
                error.as_mut_ptr(),
                c_int::try_from(ERROR_BYTES).unwrap_or(c_int::MAX),
            )
        };
        let Some(raw) = NonNull::new(raw) else {
            // SAFETY: on failure the shim has written a NUL-terminated reason
            // into `error` (or left it zeroed, which reads as empty).
            let reason = unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy();
            return Err(MediaError::DecoderWorker(format!("libaom: {reason}")));
        };
        Ok(Self { raw })
    }

    /// Decodes one temporal unit into a picture, or `None` when it made none.
    ///
    /// # Errors
    /// [`MediaError::DecoderWorker`] when libaom refuses the bitstream, or it
    /// decodes to a picture that is not 8-bit 4:2:0.
    pub fn decode(&mut self, bitstream: &[u8]) -> Result<Option<Picture>> {
        let (mut got, mut y, mut u, mut v) = (
            0 as c_int,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
        );
        let (mut y_stride, mut uv_stride, mut width, mut height) =
            (0 as c_int, 0 as c_int, 0 as c_int, 0 as c_int);
        // SAFETY: `raw` is live and exclusively ours (`&mut self`). The
        // bitstream is a live slice of exactly the length passed, read during
        // the call only. The shim writes the out-parameters and nothing else.
        let status = unsafe {
            sys::aom_shim_dec_decode(
                self.raw.as_ptr(),
                bitstream.as_ptr(),
                bitstream.len(),
                &raw mut got,
                &raw mut y,
                &raw mut u,
                &raw mut v,
                &raw mut y_stride,
                &raw mut uv_stride,
                &raw mut width,
                &raw mut height,
            )
        };
        if status != 0 {
            // SAFETY: `raw` is live, and the shim's error text is its own
            // NUL-terminated buffer, valid until the next call on it.
            let reason = unsafe { CStr::from_ptr(sys::aom_shim_dec_error(self.raw.as_ptr())) }
                .to_string_lossy()
                .into_owned();
            return Err(MediaError::DecoderWorker(reason));
        }
        if got == 0 {
            return Ok(None);
        }
        let size = |value: c_int| usize::try_from(value).ok().filter(|&value| value > 0);
        let (Some(width), Some(height), Some(y_stride), Some(uv_stride)) =
            (size(width), size(height), size(y_stride), size(uv_stride))
        else {
            return Err(MediaError::DecoderWorker(
                "libaom handed back a picture with no size".to_owned(),
            ));
        };
        if y_stride < width
            || uv_stride < width.div_ceil(2)
            || y.is_null()
            || u.is_null()
            || v.is_null()
        {
            return Err(MediaError::DecoderWorker(
                "libaom handed back planes narrower than the picture".to_owned(),
            ));
        }
        let chroma_rows = height.div_ceil(2);
        // SAFETY: on success with `got` set, the shim points at the three
        // planes of the decoder's own 8-bit 4:2:0 picture (checked by the
        // shim), each `stride` bytes a row, `height` rows of luma and
        // `height / 2` rounded up of chroma, valid until the next call on
        // this decoder, which `&mut self` rules out. They are only read, and
        // copied out before this returns.
        let (y, u, v) = unsafe {
            (
                std::slice::from_raw_parts(y, y_stride * (height - 1) + width),
                std::slice::from_raw_parts(u, uv_stride * (chroma_rows - 1) + width.div_ceil(2)),
                std::slice::from_raw_parts(v, uv_stride * (chroma_rows - 1) + width.div_ceil(2)),
            )
        };
        let rgba = i420_to_rgba(
            &Planes {
                y,
                u,
                v,
                y_stride,
                uv_stride,
            },
            width,
            height,
        );
        Ok(Some(Picture {
            width: u32::try_from(width)
                .map_err(|_| MediaError::DecoderWorker("picture too wide".to_owned()))?,
            height: u32::try_from(height)
                .map_err(|_| MediaError::DecoderWorker("picture too tall".to_owned()))?,
            rgba,
        }))
    }
}

impl Drop for AomDecoder {
    fn drop(&mut self) {
        // SAFETY: opened by `aom_shim_dec_open`, closed exactly once, here.
        unsafe { sys::aom_shim_dec_close(self.raw.as_ptr()) };
    }
}

/// The three planes of an 8-bit 4:2:0 picture and their strides.
struct Planes<'a> {
    y: &'a [u8],
    u: &'a [u8],
    v: &'a [u8],
    y_stride: usize,
    uv_stride: usize,
}

/// BT.601 studio range to full-range RGBA — the inverse of the conversion
/// every host encoder is given its picture in (`encode::nv12`).
fn i420_to_rgba(planes: &Planes<'_>, width: usize, height: usize) -> Vec<u8> {
    let mut rgba = vec![0u8; width * height * 4];
    for (row, out) in rgba.chunks_exact_mut(width * 4).enumerate() {
        let luma = &planes.y[row * planes.y_stride..][..width];
        let chroma = (row / 2) * planes.uv_stride;
        for (column, pixel) in out.chunks_exact_mut(4).enumerate() {
            let c = 298 * (i32::from(luma[column]) - 16);
            let d = i32::from(planes.u[chroma + column / 2]) - 128;
            let e = i32::from(planes.v[chroma + column / 2]) - 128;
            let channel =
                |value: i32| u8::try_from(((value + 128) >> 8).clamp(0, 255)).unwrap_or(0);
            pixel[0] = channel(c + 409 * e);
            pixel[1] = channel(c - 100 * d - 208 * e);
            pixel[2] = channel(c + 516 * d);
            pixel[3] = 0xff;
        }
    }
    rgba
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failed assumption must fail the test")]

    use super::*;

    /// A picture the host's own libaom made decodes here to the same size and
    /// close to the same colour: the two ends of ADR 0162's software path.
    #[test]
    #[cfg(feature = "encode-aom")]
    fn the_hosts_av1_decodes_to_its_picture() {
        use crate::capture::{Frame, PixelFormat};
        use crate::encode::{EncoderConfig, VideoCodec, VideoEncoder as _, aom::AomEncoder};

        let mut encoder = AomEncoder::new(EncoderConfig {
            codec: VideoCodec::Av1,
            fps: 30,
            bitrate_kbps: 8_000,
        })
        .unwrap();
        // Mid grey, BGRA.
        let grey = Frame::cpu(320, 180, PixelFormat::Bgra8, 0, vec![0x80; 320 * 180 * 4]);
        let unit = encoder.encode(&grey).unwrap();
        let mut decoder = AomDecoder::new().unwrap();
        let picture = decoder.decode(&unit.data).unwrap().unwrap();
        assert_eq!((picture.width, picture.height), (320, 180));
        assert_eq!(picture.rgba.len(), 320 * 180 * 4);
        let centre = &picture.rgba[(90 * 320 + 160) * 4..][..4];
        for channel in &centre[..3] {
            assert!(
                (0x78..=0x88).contains(channel),
                "grey came back as {centre:?}"
            );
        }
        assert_eq!(centre[3], 0xff);
    }

    /// Untrusted input: rubbish is a refusal, never a crash.
    #[test]
    fn a_bitstream_that_is_not_av1_is_refused() {
        let mut decoder = AomDecoder::new().unwrap();
        assert!(
            decoder
                .decode(&[0xde, 0xad, 0xbe, 0xef, 0x00, 0x12])
                .is_err()
        );
    }

    #[test]
    fn studio_range_black_and_white_come_back_full_range() {
        for (luma, full) in [(16_u8, 0_u8), (235, 255)] {
            let y = [luma; 4];
            let planes = Planes {
                y: &y,
                u: &[128],
                v: &[128],
                y_stride: 2,
                uv_stride: 1,
            };
            assert_eq!(&i420_to_rgba(&planes, 2, 2)[..4], [full, full, full, 255]);
        }
    }
}
