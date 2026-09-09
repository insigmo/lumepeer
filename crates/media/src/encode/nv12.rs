//! BGRA/NV12 conversion shared by every hardware encoder (§11).
//!
//! NV12 is what all three hardware backends take on their input side —
//! Media Foundation (ADR 0011), VA-API and VideoToolbox alike — while the
//! Linux and Windows capturers hand out BGRA8. The conversion is pure
//! arithmetic with no platform API in it, so it lives here rather than
//! inside any one backend: a second copy per platform is three chances to
//! get BT.601 subtly different on one of them, and the tests below then only
//! ever run on whichever machine built that backend.
//!
//! Cropping to even dimensions matches what the `openh264` fallback's
//! `even_bgra` does, and for the same reason: 4:2:0 subsampling has no odd
//! rows or columns.

use crate::capture::{Frame, PixelFormat};
use crate::error::{MediaError, Result};

/// Converts a captured frame to NV12, cropping to even dimensions the same
/// way the `openh264` fallback's `even_bgra` does: 4:2:0 subsampling has no
/// odd rows or columns.
pub(super) fn bgra_to_nv12(frame: &Frame) -> Result<(Vec<u8>, u32, u32)> {
    match frame.format {
        PixelFormat::Nv12 => nv12_passthrough(frame),
        PixelFormat::Bgra8 => bgra8_to_nv12(frame),
    }
}

pub(super) fn nv12_size(width: u32, height: u32) -> usize {
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
    let pixels = frame.as_cpu()?;
    if pixels.len() < nv12_size(width, height) {
        return Err(MediaError::Encode("NV12 frame buffer is short".to_owned()));
    }
    Ok((pixels.to_vec(), width, height))
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
    // Through `as_cpu`, not `data`: a frame the Windows zero-copy path left
    // on the GPU (ADR 0073) has nothing in `data` until it is read back, and
    // reaching this function is exactly the case where it has to be.
    let pixels = frame.as_cpu()?;
    if pixels.len() < src_stride * height {
        return Err(MediaError::Encode("frame buffer is short".to_owned()));
    }

    // One allocation, one pass, one read of every source pixel (ADR 0059).
    //
    // The shape is not incidental. This runs on every captured frame on the
    // host's own machine, and at 1080p the naive version - a pass for luma,
    // then a second pass over the same 8 MiB for chroma, then a copy of both
    // planes into a third buffer - measured 4.5 ms per frame, which was half
    // of everything `encode()` cost including the hardware encoder itself.
    // Walking the picture in 2x2 blocks reads each pixel once, while it is
    // still in L1 from the luma calculation, and writes straight into the
    // buffer the encoder is handed.
    let y_len = width * height;
    let mut out = vec![0u8; y_len + width * (height / 2)];
    let (y_plane, uv_plane) = out.split_at_mut(y_len);

    for (block_row, uv_row) in uv_plane.chunks_exact_mut(width).enumerate() {
        let top = block_row * 2;
        let (y_top, y_bottom) = y_plane[top * width..][..width * 2].split_at_mut(width);

        let src_top = &pixels[top * src_stride..][..width * 4];
        let src_bottom = &pixels[(top + 1) * src_stride..][..width * 4];

        // Four zipped iterators over exactly `width / 2` blocks: the two
        // source row halves in 8-byte (two-pixel) steps, the two luma row
        // halves and the interleaved chroma row in 2-byte steps. Written this
        // way so every access is a slice of statically known length and the
        // bounds checks fall out.
        let blocks = src_top
            .chunks_exact(8)
            .zip(src_bottom.chunks_exact(8))
            .zip(y_top.chunks_exact_mut(2))
            .zip(y_bottom.chunks_exact_mut(2))
            .zip(uv_row.chunks_exact_mut(2));

        for ((((top_px, bottom_px), y_top_px), y_bottom_px), uv) in blocks {
            let pixel = |px: &[u8], at: usize| -> (i32, i32, i32) {
                (
                    i32::from(px[at + 2]),
                    i32::from(px[at + 1]),
                    i32::from(px[at]),
                )
            };
            let (r0, g0, b0) = pixel(top_px, 0);
            let (r1, g1, b1) = pixel(top_px, 4);
            let (r2, g2, b2) = pixel(bottom_px, 0);
            let (r3, g3, b3) = pixel(bottom_px, 4);

            y_top_px[0] = bt601_y(r0, g0, b0);
            y_top_px[1] = bt601_y(r1, g1, b1);
            y_bottom_px[0] = bt601_y(r2, g2, b2);
            y_bottom_px[1] = bt601_y(r3, g3, b3);

            // Averaged before conversion and truncated per channel, exactly
            // as the two-pass version did: the chroma samples must not move.
            let r = (r0 + r1 + r2 + r3) / 4;
            let g = (g0 + g1 + g2 + g3) / 4;
            let b = (b0 + b1 + b2 + b3) / 4;
            uv[0] = bt601_u(r, g, b);
            uv[1] = bt601_v(r, g, b);
        }
    }

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
    #![allow(clippy::unwrap_used, reason = "a failed assumption must fail the test")]

    use super::*;

    fn frame(width: u32, height: u32, fill: u8) -> Frame {
        Frame::cpu(
            width,
            height,
            PixelFormat::Bgra8,
            0,
            vec![fill; (width as usize) * (height as usize) * 4],
        )
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

    /// NV12 straight from a capturer that already produces it must survive
    /// untouched: re-deriving chroma from a plane that is already chroma is
    /// how a passthrough path quietly halves the colour resolution twice.
    #[test]
    fn nv12_input_passes_through_unchanged() {
        let mut nv12 = frame(4, 4, 0);
        nv12.format = PixelFormat::Nv12;
        nv12.data = (0..nv12_size(4, 4))
            .map(|i| u8::try_from(i % 256).unwrap())
            .collect();
        let expected = nv12.data.clone();
        let (out, width, height) = bgra_to_nv12(&nv12).unwrap();
        assert_eq!((width, height), (4, 4));
        assert_eq!(out, expected);
    }
}
