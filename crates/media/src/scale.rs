//! Host-side picture downscaling (design doc §11, §15; ADR 0018).
//!
//! A captured screen larger than [`MAX_PICTURE_PIXELS`] is reduced before it
//! reaches the encoder, so that everything downstream — the encoded frame on
//! `rd/media/1`, the decoded RGBA picture in the sandboxed worker's
//! shared-memory slot, and the copy the guest's canvas paints — stays inside
//! the active-session memory budget of §15. Without it a 4K host produces a
//! 33 MiB picture per frame, which does not fit
//! [`crate::decode::SLOT_PAYLOAD_BYTES`] at all and leaves the guest waiting
//! for a screen that can never arrive (§18).
//!
//! The filter is a box average over the exact source rectangle each
//! destination pixel covers. It needs no image crate, and for the 2:1 and
//! 1.5:1 ratios a desktop actually hits it is also the right filter: every
//! source pixel contributes to exactly one destination pixel.

use lumepeer_core::constants::MAX_PICTURE_PIXELS;

use crate::capture::{Frame, PixelFormat};

/// Bytes per pixel of [`PixelFormat::Bgra8`].
const BGRA_BYTES: usize = 4;

/// Target dimensions for a picture of `width`x`height`, or `None` when it
/// already fits [`MAX_PICTURE_PIXELS`].
///
/// The aspect ratio is preserved and both axes are rounded down to an even
/// number: 4:2:0 chroma subsampling has no odd rows or columns, and the
/// encoders crop odd inputs anyway.
#[must_use]
pub fn target_size(width: u32, height: u32) -> Option<(u32, u32)> {
    let pixels = (width as usize).checked_mul(height as usize)?;
    if pixels <= MAX_PICTURE_PIXELS || width < 2 || height < 2 {
        return None;
    }
    // Both axes shrink by the same factor, sqrt(MAX_PICTURE_PIXELS / pixels).
    #[expect(
        clippy::cast_precision_loss,
        reason = "a pixel count is far inside f64's exactly representable integer range"
    )]
    let factor = (MAX_PICTURE_PIXELS as f64 / pixels as f64).sqrt();
    let even = |value: u32| -> u32 {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "factor is in (0, 1), so the product is under `value` and never negative"
        )]
        let scaled = (f64::from(value) * factor) as u32;
        (scaled & !1).max(2)
    };
    let (target_width, target_height) = (even(width), even(height));
    if target_width >= width && target_height >= height {
        return None;
    }
    Some((target_width, target_height))
}

/// Downscales `frame` to fit [`MAX_PICTURE_PIXELS`], or returns it unchanged.
///
/// Only [`PixelFormat::Bgra8`] is resized — the format every capture backend
/// in this crate produces. An [`PixelFormat::Nv12`] frame is passed through
/// untouched rather than resized incorrectly; no backend emits one today, and
/// one that starts to has to bring its own path here.
#[must_use]
pub fn fit_within_budget(frame: Frame) -> Frame {
    if frame.format != PixelFormat::Bgra8 {
        return frame;
    }
    let Some((width, height)) = target_size(frame.width, frame.height) else {
        return frame;
    };
    let expected = (frame.width as usize)
        .saturating_mul(frame.height as usize)
        .saturating_mul(BGRA_BYTES);
    if frame.data.len() < expected {
        // A short buffer is not this module's to interpret; the encoder
        // rejects it with a message that names the real problem.
        return frame;
    }
    Frame {
        data: box_downscale(&frame.data, frame.width, frame.height, width, height),
        width,
        height,
        format: frame.format,
        timestamp_us: frame.timestamp_us,
    }
}

/// Box-averages `src` (BGRA8, `src_width`x`src_height`) down to
/// `dst_width`x`dst_height`.
fn box_downscale(
    src: &[u8],
    src_width: u32,
    src_height: u32,
    dst_width: u32,
    dst_height: u32,
) -> Vec<u8> {
    let (source_width, source_height) = (src_width as usize, src_height as usize);
    let (width, height) = (dst_width as usize, dst_height as usize);
    let mut out = vec![0u8; width * height * BGRA_BYTES];

    for y in 0..height {
        // The source rows this destination row averages: the half-open
        // interval [y*source_height/height, (y+1)*source_height/height).
        let first_row = y * source_height / height;
        let last_row = ((y + 1) * source_height / height).max(first_row + 1);
        for x in 0..width {
            let first_column = x * source_width / width;
            let last_column = ((x + 1) * source_width / width).max(first_column + 1);
            let mut sums = [0u32; BGRA_BYTES];
            for row in first_row..last_row {
                let row_base = row * source_width * BGRA_BYTES;
                for column in first_column..last_column {
                    let base = row_base + column * BGRA_BYTES;
                    for (sum, byte) in sums.iter_mut().zip(&src[base..base + BGRA_BYTES]) {
                        *sum += u32::from(*byte);
                    }
                }
            }
            let count = u32::try_from((last_row - first_row) * (last_column - first_column))
                .unwrap_or(u32::MAX)
                .max(1);
            let base = (y * width + x) * BGRA_BYTES;
            for (channel, sum) in sums.iter().enumerate() {
                out[base + channel] = u8::try_from(sum / count).unwrap_or(u8::MAX);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "a failed assumption must fail the test"
    )]

    use super::*;

    fn frame(width: u32, height: u32, fill: u8) -> Frame {
        Frame {
            width,
            height,
            format: PixelFormat::Bgra8,
            timestamp_us: 7,
            data: vec![fill; (width as usize) * (height as usize) * BGRA_BYTES],
        }
    }

    #[test]
    fn a_picture_inside_the_budget_is_left_alone() {
        assert_eq!(target_size(1920, 1080), None);
        assert_eq!(target_size(1280, 800), None);
        let original = frame(64, 64, 0x20);
        let kept = fit_within_budget(original.clone());
        assert_eq!(kept.width, original.width);
        assert_eq!(kept.data, original.data);
    }

    #[test]
    fn a_4k_picture_is_reduced_to_something_that_fits_the_decoder_slot() {
        let (width, height) = target_size(3840, 2160).expect("4K is over the budget");
        assert_eq!((width, height), (1920, 1080));
        assert!((width as usize) * (height as usize) <= MAX_PICTURE_PIXELS);
    }

    #[test]
    fn every_reduced_size_fits_the_budget_and_keeps_the_aspect_ratio() {
        for (width, height) in [
            (2560u32, 1440u32),
            (3840, 2160),
            (5120, 2880),
            (3440, 1440),
            (2048, 2048),
            (7680, 4320),
        ] {
            let (target_width, target_height) =
                target_size(width, height).expect("this size is over the budget");
            assert!(
                (target_width as usize) * (target_height as usize) <= MAX_PICTURE_PIXELS,
                "{width}x{height} reduced to {target_width}x{target_height}, still over budget"
            );
            assert_eq!(target_width % 2, 0);
            assert_eq!(target_height % 2, 0);
            let source_ratio = f64::from(width) / f64::from(height);
            let target_ratio = f64::from(target_width) / f64::from(target_height);
            assert!(
                (source_ratio - target_ratio).abs() < 0.01,
                "{width}x{height} -> {target_width}x{target_height} changed the aspect ratio"
            );
        }
    }

    #[test]
    fn downscaling_averages_rather_than_dropping_pixels() {
        // Two source columns, one black and one white, collapse to one grey
        // destination column — a nearest-neighbour pick would return 0 or 255.
        let mut source = frame(2, 2, 0);
        for row in 0..2usize {
            let base = (row * 2 + 1) * BGRA_BYTES;
            source.data[base..base + BGRA_BYTES].copy_from_slice(&[255; BGRA_BYTES]);
        }
        let reduced = box_downscale(&source.data, 2, 2, 1, 1);
        assert_eq!(reduced, vec![127; BGRA_BYTES]);
    }

    #[test]
    fn a_reduced_frame_keeps_its_timestamp_and_buffer_length() {
        let reduced = fit_within_budget(frame(3840, 2160, 0x40));
        assert_eq!(reduced.timestamp_us, 7);
        assert_eq!(reduced.width, 1920);
        assert_eq!(reduced.height, 1080);
        assert_eq!(reduced.data.len(), 1920 * 1080 * BGRA_BYTES);
        assert!(reduced.data.iter().all(|byte| *byte == 0x40));
    }

    #[test]
    fn a_short_buffer_is_passed_through_rather_than_indexed_out_of_bounds() {
        let mut broken = frame(3840, 2160, 0x11);
        broken.data.truncate(16);
        let kept = fit_within_budget(broken);
        assert_eq!(kept.width, 3840);
        assert_eq!(kept.data.len(), 16);
    }
}
