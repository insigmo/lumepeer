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
//!
//! It runs on the encoder's own thread for every reduced frame of a host
//! with no GPU to do it, so it is written for speed (ADR 0157): the spans are
//! worked out once a frame instead of once a pixel, the division is a
//! multiplication, and a large picture is split across the cores. A Mac VM
//! spent 20–30 ms of every reduced frame in the naive version, more than the
//! reduction saved its encoder.

use std::sync::OnceLock;

use lumepeer_core::constants::MAX_PICTURE_PIXELS;
use lumepeer_core::protocol::CursorShapeData;

use crate::abr::FULL_SCALE_PERCENT;
use crate::capture::{Frame, PixelFormat};

/// Bytes per pixel of [`PixelFormat::Bgra8`].
const BGRA_BYTES: usize = 4;
/// Bytes per sample of NV12's interleaved chroma plane: one U, one V.
const NV12_CHROMA_BYTES: usize = 2;
/// Destination pixels below which a reduction stays on one thread: starting
/// the others would cost more than they save.
const PARALLEL_MIN_PIXELS: usize = 64 * 1024;
/// Most threads a reduction is split across.
const MAX_SCALE_THREADS: usize = 4;
/// Largest box, in source samples, whose division is a multiplication: its
/// sums stay under 2^16, where the reciprocal is exact (ADR 0157).
const MAGIC_MAX_COUNT: u32 = 257;

/// Bytes of a tightly packed NV12 picture of `width`x`height`, both even.
const fn nv12_bytes(width: usize, height: usize) -> usize {
    width * height + width * (height / 2)
}

/// `frame`'s pixels when there are at least as many of them as the picture
/// claims, or `None` when there are not.
///
/// The pixels come through [`Frame::as_cpu`], so a frame the Windows
/// zero-copy path left on the GPU (ADR 0073) is read back here — which is
/// why [`reduced`] asks the GPU first (ADR 0139).
fn usable_pixels(frame: &Frame) -> Option<&[u8]> {
    let (width, height) = (frame.width as usize, frame.height as usize);
    let expected = match frame.format {
        PixelFormat::Bgra8 => width.saturating_mul(height).saturating_mul(BGRA_BYTES),
        PixelFormat::Nv12 => nv12_bytes(width, height),
    };
    let pixels = frame.as_cpu().ok()?;
    (pixels.len() >= expected).then_some(pixels)
}

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

/// Target dimensions for a picture reduced to `percent` of each axis, or
/// `None` when nothing would change.
///
/// The third rung of the adaptive ladder (§11; ADR 0037), and the only one
/// that touches pixels. Both axes are rounded down to an even number for the
/// same 4:2:0 reason [`target_size`] does, and neither is allowed below 2:
/// an encoder handed a 1-pixel axis fails the frame rather than sending a
/// smaller one.
#[must_use]
pub fn scaled_size(width: u32, height: u32, percent: u32) -> Option<(u32, u32)> {
    if percent >= FULL_SCALE_PERCENT || width < 2 || height < 2 {
        return None;
    }
    let even = |value: u32| -> u32 {
        let scaled = (u64::from(value) * u64::from(percent) / u64::from(FULL_SCALE_PERCENT))
            .try_into()
            .unwrap_or(u32::MAX);
        (scaled & !1).max(2)
    };
    let (target_width, target_height) = (even(width), even(height));
    if target_width >= width && target_height >= height {
        return None;
    }
    Some((target_width, target_height))
}

/// Reduces `frame` to `percent` of each axis, or returns it unchanged.
///
/// Applied before [`fit_within_budget`], not instead of it: this one is the
/// adaptive rung the host chose, that one is the hard ceiling of §15 that no
/// choice may exceed.
#[must_use]
pub fn scale_to_percent(frame: Frame, percent: u32) -> Frame {
    let Some((width, height)) = scaled_size(frame.width, frame.height, percent) else {
        return frame;
    };
    reduced(frame, width, height)
}

/// Downscales `frame` to fit [`MAX_PICTURE_PIXELS`], or returns it unchanged.
///
/// [`PixelFormat::Bgra8`] and [`PixelFormat::Nv12`] are both resized, the
/// latter plane by plane (ADR 0157): the Mac's capture hands out NV12, so its
/// encoder converts nothing.
#[must_use]
pub fn fit_within_budget(frame: Frame) -> Frame {
    let Some((width, height)) = target_size(frame.width, frame.height) else {
        return frame;
    };
    reduced(frame, width, height)
}

/// Target dimensions for a picture fitted inside `max_width`x`max_height`,
/// or `None` when it already fits (§11; ADR 0060).
///
/// The aspect ratio is preserved and the picture is **never enlarged**: a
/// guest asking for a box bigger than the host's screen gets the host's own
/// size, which is the sharpest picture that exists. Upscaling here would
/// invent detail on the host and then spend bitrate sending it.
///
/// Both axes are rounded down to an even number for the same 4:2:0 reason
/// [`target_size`] does.
#[must_use]
pub fn box_size(width: u32, height: u32, max_width: u32, max_height: u32) -> Option<(u32, u32)> {
    if width < 2 || height < 2 || max_width == 0 || max_height == 0 {
        return None;
    }
    if width <= max_width && height <= max_height {
        return None;
    }
    let factor = (f64::from(max_width) / f64::from(width))
        .min(f64::from(max_height) / f64::from(height))
        .min(1.0);
    let even = |value: u32| -> u32 {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "factor is in (0, 1], so the product is at most `value` and never negative"
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

/// Downscales `frame` to fit inside `max_width`x`max_height`, or returns it
/// unchanged (§11; ADR 0060).
///
/// The guest's own request, applied *instead of* [`fit_within_budget`] rather
/// than before it: [`MAX_PICTURE_PIXELS`] is the ceiling a guest gets when it
/// has not said what it can take, and a guest that has said so has already
/// been bounded by `MAX_STREAM_PIXELS` on decode.
#[must_use]
pub fn fit_within(frame: Frame, max_width: u32, max_height: u32) -> Frame {
    let Some((width, height)) = box_size(frame.width, frame.height, max_width, max_height) else {
        return frame;
    };
    reduced(frame, width, height)
}

/// `frame` reduced to `width`x`height`: on the GPU it is already on when it is
/// on one and that GPU will do it (ADR 0139), with [`box_downscale`] on the CPU
/// otherwise.
fn reduced(frame: Frame, width: u32, height: u32) -> Frame {
    #[cfg(all(target_os = "windows", feature = "encode-mf-zero-copy"))]
    if let Some(scaled) = frame.scaled_on_gpu(width, height) {
        return scaled;
    }
    // NV12 has no odd rows or columns to reduce from or to; every size this
    // module asks for is even, and a capture backend hands out none either.
    let odd = |value: u32| value % 2 != 0;
    if frame.format == PixelFormat::Nv12
        && [frame.width, frame.height, width, height]
            .into_iter()
            .any(odd)
    {
        return frame;
    }
    let Some(pixels) = usable_pixels(&frame) else {
        // A short buffer is not this module's to interpret; the encoder
        // rejects it with a message that names the real problem.
        return frame;
    };
    let data = match frame.format {
        PixelFormat::Bgra8 => box_downscale(pixels, frame.width, frame.height, width, height),
        PixelFormat::Nv12 => nv12_downscale(pixels, frame.width, frame.height, width, height),
    };
    Frame::cpu(width, height, frame.format, frame.timestamp_us, data)
}

/// The host's cursor at the scale of the picture the guest is sent, or a copy
/// of it when the picture is the captured surface's own size (ADR 0138).
///
/// A capture backend reports the cursor in the captured surface's pixels, and
/// the guest draws it in the picture's: the picture is the only size it has.
/// Every reduction in this module shrinks the picture and not the cursor, so a
/// 4K host sending a 1080p picture had its cursor drawn twice as large as the
/// screen under it — and its hotspot twice as far from the pointer.
///
/// Premultiplied BGRA averages correctly under the same box filter the picture
/// gets. The picture is never enlarged, and neither is the cursor.
#[must_use]
pub fn cursor_for_picture(
    shape: &CursorShapeData,
    captured: (u32, u32),
    picture: (u32, u32),
) -> CursorShapeData {
    let (width, height) = (u32::from(shape.width), u32::from(shape.height));
    let reduced = picture.0 < captured.0 || picture.1 < captured.1;
    let whole = (width as usize) * (height as usize) * BGRA_BYTES;
    if !reduced || shape.rgba.len() < whole {
        return shape.clone();
    }
    // `picture <= captured` on both axes, so every result fits the `u16` the
    // value came from.
    let across = |value: u32, to: u32, from: u32| -> u16 {
        u16::try_from(u64::from(value) * u64::from(to.min(from)) / u64::from(from.max(1)))
            .unwrap_or(u16::MAX)
    };
    let target_width = across(width, picture.0, captured.0).max(1);
    let target_height = across(height, picture.1, captured.1).max(1);
    CursorShapeData {
        width: target_width,
        height: target_height,
        hotspot_x: across(u32::from(shape.hotspot_x), picture.0, captured.0).min(target_width - 1),
        hotspot_y: across(u32::from(shape.hotspot_y), picture.1, captured.1).min(target_height - 1),
        rgba: box_downscale(
            &shape.rgba,
            width,
            height,
            u32::from(target_width),
            u32::from(target_height),
        ),
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
    let (width, height) = (dst_width as usize, dst_height as usize);
    let mut out = vec![0u8; width * height * BGRA_BYTES];
    box_plane::<BGRA_BYTES>(
        src,
        (src_width as usize, src_height as usize),
        &mut out,
        (width, height),
    );
    out
}

/// Box-averages a tightly packed NV12 picture (`src_width`x`src_height`,
/// both even) down to `dst_width`x`dst_height` (both even): the luma plane at
/// full size, the interleaved chroma plane at half of each (ADR 0157).
fn nv12_downscale(
    src: &[u8],
    src_width: u32,
    src_height: u32,
    dst_width: u32,
    dst_height: u32,
) -> Vec<u8> {
    let (source_width, source_height) = (src_width as usize, src_height as usize);
    let (width, height) = (dst_width as usize, dst_height as usize);
    let mut out = vec![0u8; nv12_bytes(width, height)];
    let (luma, chroma) = out.split_at_mut(width * height);
    let source_luma = source_width * source_height;
    box_plane::<1>(
        &src[..source_luma],
        (source_width, source_height),
        luma,
        (width, height),
    );
    box_plane::<NV12_CHROMA_BYTES>(
        &src[source_luma..nv12_bytes(source_width, source_height)],
        (source_width / 2, source_height / 2),
        chroma,
        (width / 2, height / 2),
    );
    out
}

/// The source interval each destination index along one axis averages: the
/// half-open `[i*source/target, (i+1)*source/target)`, never empty.
fn spans(source: usize, target: usize) -> Vec<(usize, usize)> {
    (0..target)
        .map(|i| {
            let first = i * source / target;
            (first, ((i + 1) * source / target).max(first + 1))
        })
        .collect()
}

/// Threads a reduction to `pixels` destination pixels is split across.
fn workers(pixels: usize) -> usize {
    static CORES: OnceLock<usize> = OnceLock::new();
    if pixels < PARALLEL_MIN_PIXELS {
        return 1;
    }
    let cores = *CORES.get_or_init(|| {
        std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
    });
    cores.clamp(1, MAX_SCALE_THREADS)
}

/// Box-averages a plane of `C`-byte samples, `source` samples wide and high,
/// into `dst`, `target` samples wide and high; rows are tightly packed.
///
/// Every output byte is the truncated mean of exactly the samples the naive
/// per-pixel loop averaged — the GPU reduction is held to the same bytes
/// (ADR 0139) — only found faster.
fn box_plane<const C: usize>(
    src: &[u8],
    source: (usize, usize),
    dst: &mut [u8],
    target: (usize, usize),
) {
    let columns = spans(source.0, target.0);
    let rows = spans(source.1, target.1);
    let row_bytes = target.0 * C;
    if row_bytes == 0 || rows.is_empty() {
        return;
    }
    let threads = workers(target.0 * target.1);
    if threads <= 1 {
        box_rows::<C>(src, source.0, &columns, &rows, dst);
        return;
    }
    let per_thread = rows.len().div_ceil(threads);
    std::thread::scope(|scope| {
        let columns = &columns;
        let mut parts = rows
            .chunks(per_thread)
            .zip(dst.chunks_mut(per_thread * row_bytes));
        // This thread takes a share itself rather than waiting idle.
        let own = parts.next();
        for (rows, dst) in parts {
            scope.spawn(move || box_rows::<C>(src, source.0, columns, rows, dst));
        }
        if let Some((rows, dst)) = own {
            box_rows::<C>(src, source.0, columns, rows, dst);
        }
    });
}

/// [`box_plane`] for the destination rows `rows` names, into `dst`.
fn box_rows<const C: usize>(
    src: &[u8],
    source_width: usize,
    columns: &[(usize, usize)],
    rows: &[(usize, usize)],
    dst: &mut [u8],
) {
    let line_bytes = source_width * C;
    let mut sums = vec![0u32; columns.len() * C];
    let mut divisors = Divisors::new(columns);
    for (&(first_row, last_row), out) in rows.iter().zip(dst.chunks_exact_mut(columns.len() * C)) {
        sums.fill(0);
        for row in first_row..last_row {
            let line = &src[row * line_bytes..][..line_bytes];
            for (&(first, last), sum) in columns.iter().zip(sums.chunks_exact_mut(C)) {
                for sample in line[first * C..last * C].chunks_exact(C) {
                    for (total, &byte) in sum.iter_mut().zip(sample) {
                        *total += u32::from(byte);
                    }
                }
            }
        }
        let divisors = divisors.for_rows(last_row - first_row);
        for ((sum, out), divisor) in sums
            .chunks_exact(C)
            .zip(out.chunks_exact_mut(C))
            .zip(divisors)
        {
            for (byte, &total) in out.iter_mut().zip(sum) {
                *byte = divisor.divide(total);
            }
        }
    }
}

/// How one box's sum becomes its mean.
#[derive(Debug, Clone, Copy)]
enum Divisor {
    /// `ceil(2^32 / count)`: exact for sums and counts under 2^16, which
    /// every box of at most [`MAGIC_MAX_COUNT`] samples of a byte keeps to.
    Magic(u64),
    /// An integer division, for a box too large for the reciprocal.
    Plain(u32),
}

impl Divisor {
    fn new(count: u32) -> Self {
        let count = count.max(1);
        if count <= MAGIC_MAX_COUNT {
            Self::Magic((1_u64 << 32).div_ceil(u64::from(count)))
        } else {
            Self::Plain(count)
        }
    }

    fn divide(self, sum: u32) -> u8 {
        let mean = match self {
            Self::Magic(reciprocal) => (u64::from(sum) * reciprocal) >> 32,
            Self::Plain(count) => u64::from(sum / count),
        };
        u8::try_from(mean).unwrap_or(u8::MAX)
    }
}

/// Each destination column's [`Divisor`] for a row of boxes so many source
/// rows high, worked out once per height rather than once per pixel: a
/// reduction's rows come in at most two heights.
struct Divisors<'a> {
    columns: &'a [(usize, usize)],
    by_height: Vec<(usize, Vec<Divisor>)>,
}

impl<'a> Divisors<'a> {
    fn new(columns: &'a [(usize, usize)]) -> Self {
        Self {
            columns,
            by_height: Vec::new(),
        }
    }

    fn for_rows(&mut self, height: usize) -> &[Divisor] {
        let index = if let Some(index) = self.by_height.iter().position(|(h, _)| *h == height) {
            index
        } else {
            let divisors = self
                .columns
                .iter()
                .map(|&(first, last)| {
                    Divisor::new(u32::try_from((last - first) * height).unwrap_or(u32::MAX))
                })
                .collect();
            self.by_height.push((height, divisors));
            self.by_height.len() - 1
        };
        &self.by_height[index].1
    }
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
        Frame::cpu(
            width,
            height,
            PixelFormat::Bgra8,
            7,
            vec![fill; (width as usize) * (height as usize) * BGRA_BYTES],
        )
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

    /// The per-pixel loop this module used before ADR 0157, kept as the
    /// definition the fast filter is held to byte for byte: `channels`
    /// interleaved bytes per sample, rows tightly packed.
    fn reference_downscale(
        src: &[u8],
        channels: usize,
        (source_width, source_height): (usize, usize),
        (width, height): (usize, usize),
    ) -> Vec<u8> {
        let mut out = vec![0u8; width * height * channels];
        for y in 0..height {
            let first_row = y * source_height / height;
            let last_row = ((y + 1) * source_height / height).max(first_row + 1);
            for x in 0..width {
                let first_column = x * source_width / width;
                let last_column = ((x + 1) * source_width / width).max(first_column + 1);
                let mut sums = vec![0u32; channels];
                for row in first_row..last_row {
                    for column in first_column..last_column {
                        let base = (row * source_width + column) * channels;
                        for (sum, byte) in sums.iter_mut().zip(&src[base..base + channels]) {
                            *sum += u32::from(*byte);
                        }
                    }
                }
                let count =
                    u32::try_from((last_row - first_row) * (last_column - first_column)).unwrap();
                for (channel, sum) in sums.iter().enumerate() {
                    out[(y * width + x) * channels + channel] = u8::try_from(sum / count).unwrap();
                }
            }
        }
        out
    }

    fn noise(len: usize, mut seed: u32) -> Vec<u8> {
        (0..len)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed.to_le_bytes()[0]
            })
            .collect()
    }

    /// The ratios hosts hit — 2:1, 1.5:1, the speed controller's 85% steps,
    /// a guest's odd box — a box large enough for the plain division, and
    /// a growth by one pixel short of none: the same bytes as the loop the
    /// GPU reduction is also held to (ADR 0139, ADR 0157).
    #[test]
    fn the_fast_filter_makes_the_reference_filters_very_bytes() {
        for ((sw, sh), (dw, dh)) in [
            ((64, 32), (32, 16)),
            ((1280, 800), (640, 400)),
            ((1280, 800), (1088, 680)),
            ((1718, 920), (1460, 782)),
            ((1366, 768), (1024, 576)),
            ((1920, 1080), (1280, 720)),
            ((300, 200), (7, 5)),
            ((640, 640), (2, 2)),
            ((50, 50), (49, 49)),
            ((8, 8), (1, 1)),
        ] {
            let src = noise(sw * sh * BGRA_BYTES, 0x2545_f491);
            let fast = box_downscale(
                &src,
                u32::try_from(sw).unwrap(),
                u32::try_from(sh).unwrap(),
                u32::try_from(dw).unwrap(),
                u32::try_from(dh).unwrap(),
            );
            let slow = reference_downscale(&src, BGRA_BYTES, (sw, sh), (dw, dh));
            assert!(
                fast == slow,
                "{sw}x{sh} -> {dw}x{dh} differs from the reference"
            );
        }
    }

    /// NV12 is reduced plane by plane: luma at the picture's size, the
    /// interleaved chroma at half of each, each the reference's bytes.
    #[test]
    fn an_nv12_picture_is_reduced_plane_by_plane() {
        let (sw, sh, dw, dh) = (1718, 920, 1460, 782);
        let src = noise(nv12_bytes(sw, sh), 0x1234_5678);
        let frame = Frame::cpu(
            u32::try_from(sw).unwrap(),
            u32::try_from(sh).unwrap(),
            PixelFormat::Nv12,
            9,
            src.clone(),
        );
        let reduced = scale_to_percent(frame, 85);
        assert_eq!((reduced.width, reduced.height), (1460, 782));
        assert_eq!(reduced.format, PixelFormat::Nv12);
        assert_eq!(reduced.timestamp_us, 9);
        assert_eq!(reduced.data.len(), nv12_bytes(dw, dh));
        let luma = reference_downscale(&src[..sw * sh], 1, (sw, sh), (dw, dh));
        let chroma = reference_downscale(
            &src[sw * sh..],
            NV12_CHROMA_BYTES,
            (sw / 2, sh / 2),
            (dw / 2, dh / 2),
        );
        assert!(reduced.data[..dw * dh] == luma[..], "luma differs");
        assert!(reduced.data[dw * dh..] == chroma[..], "chroma differs");
    }

    /// An NV12 picture with an odd side has no chroma layout to reduce, and
    /// goes through as it came rather than being cut wrongly.
    #[test]
    fn an_odd_nv12_picture_is_left_alone() {
        let frame = Frame::cpu(65, 32, PixelFormat::Nv12, 0, vec![0x40; 65 * 32 * 2]);
        let kept = scale_to_percent(frame, 50);
        assert_eq!((kept.width, kept.height), (65, 32));
    }

    /// What the filter costs on the sizes the logs showed, for a release
    /// build: `cargo test -p lumepeer-media --release --lib scale -- --ignored
    /// --nocapture`.
    #[test]
    #[ignore = "a timing, not a check"]
    fn reduction_timings() {
        for ((sw, sh), (dw, dh)) in [
            ((1280, 800), (640, 400)),
            ((1718, 920), (1460, 782)),
            ((1718, 920), (858, 460)),
            ((1920, 1080), (1280, 720)),
            ((3840, 2160), (1920, 1080)),
        ] {
            let src = noise(sw * sh * BGRA_BYTES, 7);
            let size = |v: usize| u32::try_from(v).unwrap();
            let started = std::time::Instant::now();
            for _ in 0..20 {
                drop(box_downscale(&src, size(sw), size(sh), size(dw), size(dh)));
            }
            let fast = started.elapsed() / 20;
            let started = std::time::Instant::now();
            drop(reference_downscale(&src, BGRA_BYTES, (sw, sh), (dw, dh)));
            let slow = started.elapsed();
            eprintln!("{sw}x{sh} -> {dw}x{dh}: {fast:?} against {slow:?} for the reference");
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
    fn the_adaptive_rung_reduces_both_axes_and_keeps_them_even() {
        assert_eq!(scaled_size(1920, 1080, 50), Some((960, 540)));
        assert_eq!(scaled_size(1920, 1080, 75), Some((1440, 810)));
        assert_eq!(
            scaled_size(1920, 1080, 100),
            None,
            "full scale changes nothing"
        );
        assert_eq!(
            scaled_size(1, 1, 50),
            None,
            "a picture this small cannot shrink"
        );
        for percent in [50u32, 75] {
            let (width, height) = scaled_size(1366, 768, percent).expect("this shrinks");
            assert_eq!(width % 2, 0);
            assert_eq!(height % 2, 0);
            assert!(width >= 2 && height >= 2);
        }
    }

    #[test]
    fn scaling_to_a_percent_rewrites_the_frame_and_keeps_its_timestamp() {
        let reduced = scale_to_percent(frame(64, 32, 0x30), 50);
        assert_eq!((reduced.width, reduced.height), (32, 16));
        assert_eq!(reduced.timestamp_us, 7);
        assert_eq!(reduced.data.len(), 32 * 16 * BGRA_BYTES);
        assert!(reduced.data.iter().all(|byte| *byte == 0x30));
    }

    #[test]
    fn a_short_buffer_is_passed_through_rather_than_indexed_out_of_bounds() {
        let mut broken = frame(3840, 2160, 0x11);
        broken.data.truncate(16);
        let kept = fit_within_budget(broken);
        assert_eq!(kept.width, 3840);
        assert_eq!(kept.data.len(), 16);
    }

    /// ADR 0060: a guest asking for a box at least as big as the host's
    /// screen gets the host's own picture untouched. This is the case that
    /// matters most - it is what makes a 1440p guest watching a 1440p host
    /// see one frame pixel per device pixel, where the ADR 0018 ceiling used
    /// to hand it a resampled 1080p one.
    #[test]
    fn a_box_at_or_above_the_picture_never_enlarges_it() {
        assert_eq!(box_size(2560, 1440, 2560, 1440), None);
        assert_eq!(box_size(1920, 1080, 3840, 2160), None);
        let kept = fit_within(frame(64, 64, 0x20), 1920, 1080);
        assert_eq!((kept.width, kept.height), (64, 64));
    }

    /// The aspect ratio decides which axis binds, not the order of the
    /// arguments: a wide picture in a tall box is limited by its width.
    #[test]
    fn a_box_fits_the_picture_by_whichever_axis_binds_first() {
        assert_eq!(box_size(2560, 1440, 1280, 1440), Some((1280, 720)));
        assert_eq!(box_size(2560, 1440, 2560, 720), Some((1280, 720)));
        assert_eq!(box_size(3840, 2160, 1920, 1080), Some((1920, 1080)));
    }

    #[test]
    fn every_boxed_size_is_even_and_inside_the_box() {
        for (width, height, max_width, max_height) in [
            (2560u32, 1440u32, 1366u32, 768u32),
            (3840, 2160, 1000, 1000),
            (1366, 768, 300, 900),
            (1920, 1200, 1919, 1199),
        ] {
            let (out_width, out_height) =
                box_size(width, height, max_width, max_height).expect("this shrinks");
            assert_eq!(out_width % 2, 0, "{width}x{height} produced an odd width");
            assert_eq!(out_height % 2, 0, "{width}x{height} produced an odd height");
            assert!(
                out_width <= max_width && out_height <= max_height,
                "{width}x{height} escaped the {max_width}x{max_height} box as {out_width}x{out_height}"
            );
        }
    }

    #[test]
    fn fitting_within_a_box_rewrites_the_frame_and_keeps_its_timestamp() {
        let reduced = fit_within(frame(64, 32, 0x30), 32, 32);
        assert_eq!((reduced.width, reduced.height), (32, 16));
        assert_eq!(reduced.timestamp_us, 7);
        assert_eq!(reduced.data.len(), 32 * 16 * BGRA_BYTES);
        assert!(reduced.data.iter().all(|byte| *byte == 0x30));
    }

    #[test]
    fn a_short_buffer_is_passed_through_by_the_box_fit_too() {
        let mut broken = frame(2560, 1440, 0x11);
        broken.data.truncate(16);
        let kept = fit_within(broken, 1280, 720);
        assert_eq!(kept.width, 2560);
        assert_eq!(kept.data.len(), 16);
    }

    fn cursor(width: u16, height: u16, hotspot: (u16, u16), rgba: Vec<u8>) -> CursorShapeData {
        CursorShapeData {
            width,
            height,
            hotspot_x: hotspot.0,
            hotspot_y: hotspot.1,
            rgba,
        }
    }

    /// The reported bug: a 4K host on the `performance` preset sends a 1080p
    /// picture, and the guest drew the 4K cursor over it at twice its size
    /// with the hotspot twice as far from the pointer (ADR 0138).
    #[test]
    fn a_cursor_shrinks_with_the_picture_hotspot_included() {
        let shape = cursor(64, 64, (20, 30), vec![0xFF; 64 * 64 * BGRA_BYTES]);
        let scaled = cursor_for_picture(&shape, (3840, 2160), (1920, 1080));
        assert_eq!((scaled.width, scaled.height), (32, 32));
        assert_eq!((scaled.hotspot_x, scaled.hotspot_y), (10, 15));
        assert_eq!(scaled.rgba.len(), 32 * 32 * BGRA_BYTES);
        assert!(scaled.rgba.iter().all(|byte| *byte == 0xFF));
    }

    #[test]
    fn a_cursor_over_a_picture_at_full_size_is_sent_as_it_is() {
        let shape = cursor(2, 1, (1, 0), vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(
            cursor_for_picture(&shape, (1920, 1080), (1920, 1080)),
            shape
        );
    }

    /// Premultiplied pixels average under the box filter: an opaque white
    /// pixel next to a transparent one is half-covered white, not grey.
    #[test]
    fn a_reduced_cursor_is_box_averaged() {
        let shape = cursor(2, 1, (0, 0), vec![0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0]);
        let scaled = cursor_for_picture(&shape, (200, 100), (100, 100));
        assert_eq!((scaled.width, scaled.height), (1, 1));
        assert_eq!(scaled.rgba, vec![0x7F, 0x7F, 0x7F, 0x7F]);
    }

    /// However far the picture shrinks, the cursor keeps a pixel and its
    /// hotspot stays inside it — §14 refuses a shape that breaks either.
    #[test]
    fn a_tiny_cursor_keeps_a_pixel_and_its_hotspot_inside_it() {
        let shape = cursor(3, 3, (2, 2), vec![0xFF; 3 * 3 * BGRA_BYTES]);
        let scaled = cursor_for_picture(&shape, (3840, 2160), (2, 2));
        assert_eq!((scaled.width, scaled.height), (1, 1));
        assert_eq!((scaled.hotspot_x, scaled.hotspot_y), (0, 0));
    }

    /// A picture of the kind a desktop is, which is the kind a cheap filter
    /// gets wrong: one-pixel rules, thin dark strokes on white like text, a
    /// smooth gradient, and a patch of noise standing in for a photograph.
    #[cfg(all(target_os = "windows", feature = "encode-mf-zero-copy"))]
    fn desktop_like(width: u32, height: u32) -> Vec<u8> {
        let mut seed = 0x2545_f491_u32;
        let mut noise = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        let mut out = vec![0u8; (width as usize) * (height as usize) * BGRA_BYTES];
        for y in 0..height {
            for x in 0..width {
                let base = ((y * width + x) as usize) * BGRA_BYTES;
                let pixel: [u8; 3] = if y < height / 2 {
                    // Rules every 9 pixels and "glyphs": short strokes on a
                    // 6x11 character cell.
                    let rule = x % 9 == 0 || y % 9 == 0;
                    let (cx, cy) = (x % 6, y % 11);
                    let stroke = (cx == 1 && cy > 1 && cy < 9) || (cy == 5 && cx < 5);
                    if rule || stroke {
                        [20, 20, 20]
                    } else {
                        [250, 250, 250]
                    }
                } else if x < width / 2 {
                    let level = u8::try_from(x * 255 / (width / 2)).unwrap_or(u8::MAX);
                    [level, 128, 255 - level]
                } else {
                    let value = noise().to_le_bytes();
                    [value[0], value[1], value[2]]
                };
                out[base..base + 3].copy_from_slice(&pixel);
                out[base + 3] = 0xFF;
            }
        }
        out
    }

    /// ADR 0139: a frame still on the GPU is reduced there, and the picture
    /// that comes out is byte for byte the one the CPU box filter makes — on
    /// the ratios guests actually ask for, on a picture of the kind that
    /// shows a filter's faults. Skipped on a machine with no Direct3D 11
    /// device to run the shader on.
    #[cfg(all(target_os = "windows", feature = "encode-mf-zero-copy"))]
    #[test]
    fn a_gpu_frame_is_reduced_on_the_gpu_into_the_cpu_filters_very_picture() {
        for ((sw, sh), (dw, dh)) in [
            ((3840, 2160), (1920, 1080)),
            ((2560, 1440), (1920, 1080)),
            ((3840, 2160), (1600, 900)),
            ((1920, 1080), (1280, 720)),
        ] {
            let pixels = desktop_like(sw, sh);
            let Some((frame, _scaler)) =
                crate::capture::windows::gpu_test_frame_of(sw, sh, &pixels)
            else {
                eprintln!("no Direct3D 11 device to reduce on here: skipped");
                return;
            };

            // Twice: the first pass also builds the size pair's constant
            // buffer and its output texture, the second is the steady state.
            drop(fit_within(frame.clone(), dw, dh));
            let started = std::time::Instant::now();
            let on_gpu = fit_within(frame.clone(), dw, dh);
            let submitted = started.elapsed();
            assert!(on_gpu.gpu.is_some(), "the reduction left the GPU");
            assert_eq!((on_gpu.width, on_gpu.height), (dw, dh));
            let gpu_pixels = on_gpu.as_cpu().unwrap().to_vec();
            let gpu_took = started.elapsed();

            let started = std::time::Instant::now();
            let cpu_pixels = box_downscale(frame.as_cpu().unwrap(), sw, sh, dw, dh);
            let cpu_took = started.elapsed();
            eprintln!(
                "{sw}x{sh} -> {dw}x{dh}: GPU {submitted:?} to submit, {gpu_took:?} with the                  small readback; CPU {cpu_took:?} with the large readback"
            );
            assert!(
                gpu_pixels == cpu_pixels,
                "{sw}x{sh} -> {dw}x{dh}: {} of {} bytes differ from the CPU filter's",
                gpu_pixels
                    .iter()
                    .zip(&cpu_pixels)
                    .filter(|(a, b)| a != b)
                    .count(),
                cpu_pixels.len()
            );
        }
    }
}
