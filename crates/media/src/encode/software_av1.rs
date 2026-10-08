//! Whether this host may encode AV1 in software, and the measurement that
//! decides it (§11; ADR 0141).
//!
//! Software AV1 is the one exception to §11's mutual-hardware-support rule,
//! and only for a host that can carry it. Four things stand between a build
//! that has the encoder and a session that uses it, cheapest first:
//!
//! 1. **The build and the CPU.** `encode-aom` exists for x86-64 Windows and
//!    Linux only, and libaom's realtime kernels that were measured are its
//!    AVX2 ones. ARM and pre-AVX2 x86 were never measured, so they are not
//!    offered it at all.
//! 2. **No hardware encoder.** A host whose probe finds a hardware H.264
//!    encoder never gets software AV1; the probe runs once, on the
//!    measurement's own thread, rather than on every session's.
//! 3. **A measurement on this machine**, once per process, before the first
//!    session that could use it: the product's own path — BGRA to I420, then
//!    libaom at the session's settings — against the product's own `openh264`
//!    fallback, both on the same synthetic 1080p desktop — a line typed, a
//!    page scrolled, a window dragged — paced at 30 frames a second. Software AV1 is ready only when its p95
//!    frame time is no worse than `openh264`'s *and* within
//!    [`SOFTWARE_AV1_FRAME_BUDGET_MS`] — the owner's threshold, applied to
//!    the machine in front of it rather than to the two the research ran on.
//! 4. **The live session**, which demotes it for the rest of the process if
//!    it cannot keep the frame rate (`lumepeer_runtime`'s encode loop).
//!
//! This module holds the answers. The rule that combines them with what the
//! guest decodes and what the session asks for is `choose_media_codec`'s.

use std::sync::Mutex;

use lumepeer_core::constants::SOFTWARE_AV1_FRAME_BUDGET_MS;

/// Where this host stands on software AV1 right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    /// Not in this build: `encode-aom` is off, or the target is not x86-64
    /// Windows or Linux.
    NotBuilt,
    /// The processor lacks AVX2, which every measured run relied on.
    UnsupportedCpu,
    /// Not measured on this machine yet, or the measurement is running.
    Unmeasured,
    /// This host has a hardware H.264 encoder. Software AV1 is for hosts
    /// with none: everywhere else the hardware encoder is cheaper, and on
    /// the machines measured the `openh264` fallback this replaces is
    /// exactly what such a host never runs.
    HardwareEncoder,
    /// Measured, and slower than allowed.
    TooSlow(Measurement),
    /// A live session found this host could not keep up, or the encoder
    /// failed; for the rest of this process.
    Demoted,
    /// Measured, and within the threshold.
    Ready(Measurement),
}

impl Readiness {
    /// Whether a session may be given software AV1, as far as this host is
    /// concerned.
    #[must_use]
    pub const fn is_ready(self) -> bool {
        matches!(self, Self::Ready(_))
    }
}

/// What the measurement found, p95 frame times in microseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Measurement {
    /// BGRA to I420 plus libaom, per frame.
    pub av1_p95_us: u64,
    /// The `openh264` fallback on the same frames, conversion included.
    pub h264_p95_us: u64,
}

impl std::fmt::Display for Measurement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ms = |us: u64| format!("{}.{} ms", us / 1_000, us % 1_000 / 100);
        write!(
            f,
            "software AV1 p95 {}, openh264 p95 {}",
            ms(self.av1_p95_us),
            ms(self.h264_p95_us)
        )
    }
}

impl Measurement {
    /// The owner's threshold (ADR 0141): no slower than `openh264` on the
    /// same machine, and within [`SOFTWARE_AV1_FRAME_BUDGET_MS`].
    #[must_use]
    pub const fn passes(self) -> bool {
        self.av1_p95_us <= self.h264_p95_us
            && self.av1_p95_us <= SOFTWARE_AV1_FRAME_BUDGET_MS * 1_000
    }
}

/// Whether software AV1 is compiled into this build at all.
#[must_use]
pub const fn built() -> bool {
    cfg!(all(
        feature = "encode-aom",
        target_arch = "x86_64",
        any(target_os = "windows", target_os = "linux")
    ))
}

/// Whether this processor has what the measured libaom ran on.
#[must_use]
pub fn cpu_supported() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("sse4.1")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

#[derive(Debug, Default)]
struct State {
    /// A measurement thread is running.
    measuring: bool,
    /// The measurement's probe found a hardware H.264 encoder.
    hardware: bool,
    measured: Option<Measurement>,
    /// Why a live session gave up on it, once one has.
    demoted: Option<String>,
}

static STATE: Mutex<State> = Mutex::new(State {
    measuring: false,
    hardware: false,
    measured: None,
    demoted: None,
});

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Where this host stands on software AV1 now. Cheap: no probing, no
/// measuring.
#[must_use]
pub fn readiness() -> Readiness {
    if !built() {
        return Readiness::NotBuilt;
    }
    if !cpu_supported() {
        return Readiness::UnsupportedCpu;
    }
    let state = state();
    if state.demoted.is_some() {
        return Readiness::Demoted;
    }
    if state.hardware {
        return Readiness::HardwareEncoder;
    }
    match state.measured {
        None => Readiness::Unmeasured,
        Some(found) if found.passes() => Readiness::Ready(found),
        Some(found) => Readiness::TooSlow(found),
    }
}

/// Starts the one measurement of this process on a thread of its own, unless
/// it has started already or could not lead anywhere. Returns whether it
/// started now. The thread asks for a hardware H.264 encoder first, and
/// measures nothing on a host that has one.
///
/// About six seconds of wall time, three per encoder, most of it waiting
/// between frames: the frames are paced at 30 a second, as a session paces
/// them, because a processor allowed to idle between frames clocks down and
/// is slower per frame than one encoding back to back.
pub fn start_measurement() -> bool {
    if !built() || !cpu_supported() {
        return false;
    }
    {
        let mut state = state();
        if state.measuring || state.hardware || state.measured.is_some() || state.demoted.is_some()
        {
            return false;
        }
        state.measuring = true;
    }
    let spawned = std::thread::Builder::new()
        .name("software-av1-measure".to_owned())
        .spawn(|| {
            // The hardware question first: on a host that has an encoder
            // there is nothing to measure, and six seconds of encoding to save.
            let hardware = super::probe_hardware(super::EncoderConfig::default())
                == Some(super::EncoderKind::Hardware);
            if hardware {
                tracing::info!("hardware H.264 encoder found: software AV1 is not for this host (ADR 0141)");
                let mut state = state();
                state.measuring = false;
                state.hardware = true;
                return;
            }
            let outcome = measure();
            let mut state = state();
            state.measuring = false;
            match outcome {
                Ok(found) => {
                    tracing::info!(
                        %found,
                        budget_ms = SOFTWARE_AV1_FRAME_BUDGET_MS,
                        ready = found.passes(),
                        "software AV1 measured on this host (ADR 0141)"
                    );
                    state.measured = Some(found);
                }
                Err(error) => {
                    tracing::warn!(%error, "software AV1 could not be measured; this host stays on H.264");
                    state.demoted = Some(format!("measurement failed: {error}"));
                }
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "cannot start the software AV1 measurement");
        state().measuring = false;
        return false;
    }
    true
}

/// Gives up on software AV1 for the rest of this process: a live session
/// found it could not keep up, or the encoder failed. Logged once.
pub fn demote(reason: &str) {
    let mut state = state();
    if state.demoted.is_none() {
        tracing::warn!(%reason, "software AV1 is off on this host until lumepeer restarts (ADR 0141)");
        state.demoted = Some(reason.to_owned());
    }
}

/// Runs the measurement here and now, on the calling thread.
///
/// # Errors
/// [`crate::MediaError::EncoderUnavailable`] when software AV1 is not built
/// in, and whatever either encoder reports if it refuses a frame.
pub fn measure() -> crate::Result<Measurement> {
    #[cfg(all(
        feature = "encode-aom",
        target_arch = "x86_64",
        any(target_os = "windows", target_os = "linux")
    ))]
    {
        use lumepeer_core::constants::{ENCODE_DEFAULT_BITRATE_KBPS, SOFTWARE_AV1_MAX_FPS};

        use super::{EncoderConfig, VideoCodec};
        let config = EncoderConfig {
            fps: SOFTWARE_AV1_MAX_FPS,
            // The `quality` preset's figure, which the AV1 encoder halves
            // exactly as it would in the session.
            bitrate_kbps: ENCODE_DEFAULT_BITRATE_KBPS,
            codec: VideoCodec::Av1,
        };
        let screen = timing::Screen::new();
        let mut av1 = super::aom::AomEncoder::new(config)?;
        let av1_p95 = timing::time(&mut av1, &screen)?;
        let mut h264 = super::software::OpenH264Encoder::new(EncoderConfig {
            codec: VideoCodec::H264,
            ..config
        })?;
        let h264_p95 = timing::time(&mut h264, &screen)?;
        let micros =
            |took: std::time::Duration| u64::try_from(took.as_micros()).unwrap_or(u64::MAX);
        Ok(Measurement {
            av1_p95_us: micros(av1_p95),
            h264_p95_us: micros(h264_p95),
        })
    }
    #[cfg(not(all(
        feature = "encode-aom",
        target_arch = "x86_64",
        any(target_os = "windows", target_os = "linux")
    )))]
    {
        Err(crate::MediaError::EncoderUnavailable(
            "software AV1 is not built into this binary".to_owned(),
        ))
    }
}

/// The measurement's pictures and clock. Only the software AV1 build runs
/// them; the tests check them everywhere.
mod timing {
    #![cfg_attr(
        not(all(
            feature = "encode-aom",
            target_arch = "x86_64",
            any(target_os = "windows", target_os = "linux")
        )),
        allow(dead_code, reason = "only the software AV1 build measures")
    )]

    use std::time::{Duration, Instant};

    use lumepeer_core::constants::SOFTWARE_AV1_MAX_FPS;

    /// Frames timed per encoder, after [`WARMUP_FRAMES`]: three seconds, a
    /// third each of typing, scrolling and dragging a window.
    const TIMED_FRAMES: usize = 90;
    /// Frames per act of the screen.
    const ACT: usize = TIMED_FRAMES / 3;
    /// Frames encoded first and not timed: the keyframe and the rate
    /// controller finding its feet.
    const WARMUP_FRAMES: usize = 5;
    /// A p95 this slow ends the measurement at once: a machine this slow
    /// should not be kept busy finding out by how much (ADR 0145).
    const HOPELESS: Duration = Duration::from_millis(250);
    /// The timed frames at or above the p95: the p95 is the `TAIL`-th
    /// slowest of them. One slow frame is not a slow p95; `TAIL` of them are.
    const TAIL: usize = TIMED_FRAMES + 1 - (TIMED_FRAMES * 95).div_ceil(100);
    /// Picture size of the measurement: the largest software AV1 is chosen for.
    pub(super) const WIDTH: usize = 1920;
    pub(super) const HEIGHT: usize = 1080;
    /// How far the page scrolls each frame of the second act.
    const SCROLL_ROWS: usize = 12;
    /// The dragged window of the third act: where on the page its content
    /// comes from, how large it is, and how far it moves each frame.
    const WINDOW_FROM: (usize, usize) = (300, 1300);
    const WINDOW_SIZE: (usize, usize) = (1100, 700);
    const WINDOW_STEP: (usize, usize) = (7, 3);
    const WINDOW_BORDER: usize = 6;
    const WINDOW_TITLE: usize = 32;
    const WINDOW_COLOUR: [u8; 4] = [0xc0, 0x60, 0x30, 0xff];
    /// The line the first act types into, and how often a glyph appears on
    /// it: one every three frames, about the 110 ms a keystroke took in the
    /// recording.
    const TYPED_LINE_TOP: usize = 998;
    const FRAMES_PER_GLYPH: usize = 3;

    /// A 1080p desktop in the three acts of the stage-1 recording
    /// (docs/research/software-av1.md): a line being typed, a page of text
    /// scrolling, a window of text dragged across it. A dark sidebar and a
    /// light page of pseudo-random glyphs in lines, rendered once.
    /// Deterministic, so every host measures the same pictures.
    ///
    /// The three acts matter, not only the text. On the 4-core VM this was
    /// written on, a 1080p clip of these acts with real rendered text timed
    /// libaom at 14, 22 and 29 ms at p95 per act — 24–28 ms over the whole
    /// clip, against 17 ms for a measurement of scrolling alone. Both
    /// encoders stood in about the same ratio (1.45) to beta's stage-1
    /// timings of the recorded desktop, which is what says that clip weighs
    /// what the recording did. This screen, in turn, measures 20–22 ms there:
    /// still a little kinder than the clip, which ADR 0141 records.
    pub(super) struct Screen {
        page: Vec<u8>,
    }

    impl Screen {
        const SIDEBAR: usize = 280;
        const LINE_PITCH: usize = 22;
        const GLYPH_WIDTH: usize = 7;
        const GLYPH_HEIGHT: usize = 12;
        const BACKGROUND: [u8; 4] = [0xf3, 0xf3, 0xf3, 0xff];

        const fn rows() -> usize {
            WINDOW_FROM.1 + WINDOW_SIZE.1
        }

        pub(super) fn new() -> Self {
            let rows = Self::rows();
            let mut page = vec![0u8; WIDTH * rows * 4];
            let mut seed: u32 = 0x2545_f491;
            let mut next = move || {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed
            };
            for (index, pixel) in page.chunks_exact_mut(4).enumerate() {
                let x = index % WIDTH;
                let colour: [u8; 4] = if x < Self::SIDEBAR {
                    [0x2b, 0x2b, 0x2b, 0xff]
                } else {
                    Self::BACKGROUND
                };
                pixel.copy_from_slice(&colour);
            }
            let mut line_top = 8;
            while line_top + Self::GLYPH_HEIGHT < rows {
                // A line of words, ragged at the end, in each of the two panes.
                for (left, right, ink) in [
                    (16, Self::SIDEBAR - 24, [0xd0, 0xd0, 0xd0, 0xff]),
                    (Self::SIDEBAR + 48, WIDTH - 64, [0x20, 0x20, 0x20, 0xff]),
                ] {
                    let end = left + (next() as usize % (right - left));
                    let mut x = left;
                    while x + Self::GLYPH_WIDTH < end {
                        let word = 2 + next() as usize % 9;
                        for _ in 0..word {
                            if x + Self::GLYPH_WIDTH >= end {
                                break;
                            }
                            let bits = next();
                            for gy in 0..Self::GLYPH_HEIGHT {
                                for gx in 0..Self::GLYPH_WIDTH - 1 {
                                    // A glyph is a 6x12 bitmap, its edges
                                    // shaded the way smoothed text is.
                                    let cell = bits
                                        .rotate_left(u32::try_from(gy * 3 + gx).unwrap_or(0))
                                        & 7;
                                    let at = ((line_top + gy) * WIDTH + x + gx) * 4;
                                    if cell < 4 {
                                        page[at..at + 4].copy_from_slice(&ink);
                                    } else if cell < 6 {
                                        for channel in 0..3 {
                                            page[at + channel] =
                                                page[at + channel] / 2 + ink[channel] / 2;
                                        }
                                    }
                                }
                            }
                            x += Self::GLYPH_WIDTH;
                        }
                        x += Self::GLYPH_WIDTH;
                    }
                }
                line_top += Self::LINE_PITCH;
            }
            Self { page }
        }

        /// The screen with the page scrolled down by `rows`.
        fn scrolled(&self, rows: usize) -> Vec<u8> {
            let start = rows * WIDTH * 4;
            self.page[start..start + WIDTH * HEIGHT * 4].to_vec()
        }

        /// Frame `n` of the measurement, warm-up included: the warm-up and
        /// the first act type, the second scrolls, the third drags.
        pub(super) fn frame(&self, n: usize) -> crate::capture::Frame {
            let timed = n.saturating_sub(WARMUP_FRAMES);
            let pixels = if n < WARMUP_FRAMES || timed < ACT {
                self.typing(n)
            } else if timed < 2 * ACT {
                self.scrolled((timed - ACT + 1) * SCROLL_ROWS)
            } else {
                self.dragging(timed - 2 * ACT)
            };
            crate::capture::Frame::cpu(
                u32::try_from(WIDTH).unwrap_or(u32::MAX),
                u32::try_from(HEIGHT).unwrap_or(u32::MAX),
                crate::capture::PixelFormat::Bgra8,
                u64::try_from(n).unwrap_or(u64::MAX) * 1_000_000 / u64::from(SOFTWARE_AV1_MAX_FPS),
                pixels,
            )
        }

        /// The top of the page, with the typed line shown up to where the
        /// typing has got by frame `n`.
        fn typing(&self, n: usize) -> Vec<u8> {
            let mut pixels = self.scrolled(0);
            let typed = Self::SIDEBAR + 48 + (n / FRAMES_PER_GLYPH + 1) * Self::GLYPH_WIDTH;
            for row in TYPED_LINE_TOP..TYPED_LINE_TOP + Self::GLYPH_HEIGHT {
                for pixel in
                    pixels[(row * WIDTH + typed) * 4..(row + 1) * WIDTH * 4].chunks_exact_mut(4)
                {
                    pixel.copy_from_slice(&Self::BACKGROUND);
                }
            }
            pixels
        }

        /// The page where the scrolling stopped, with a window of other text
        /// dragged `step` steps across it.
        fn dragging(&self, step: usize) -> Vec<u8> {
            let mut pixels = self.scrolled(ACT * SCROLL_ROWS);
            let (left, top) = (100 + step * WINDOW_STEP.0, 80 + step * WINDOW_STEP.1);
            let (width, height) = WINDOW_SIZE;
            for row in 0..height {
                let from = ((WINDOW_FROM.1 + row) * WIDTH + WINDOW_FROM.0) * 4;
                let to = ((top + row) * WIDTH + left) * 4;
                pixels[to..to + width * 4].copy_from_slice(&self.page[from..from + width * 4]);
                let frame = row < WINDOW_TITLE.max(WINDOW_BORDER) || row >= height - WINDOW_BORDER;
                for column in 0..width {
                    if frame || column < WINDOW_BORDER || column >= width - WINDOW_BORDER {
                        let at = to + column * 4;
                        pixels[at..at + 4].copy_from_slice(&WINDOW_COLOUR);
                    }
                }
            }
            pixels
        }
    }

    /// p95 of `encoder`'s frame time over the screen, frames paced at
    /// [`SOFTWARE_AV1_MAX_FPS`]. The copy that makes each frame is outside the
    /// timing; the conversion and the encode are inside it, as in a session.
    pub(super) fn time(
        encoder: &mut dyn crate::encode::VideoEncoder,
        screen: &Screen,
    ) -> crate::Result<Duration> {
        let interval = Duration::from_micros(1_000_000 / u64::from(SOFTWARE_AV1_MAX_FPS));
        let started = Instant::now();
        let mut tally = Tally::default();
        for n in 0..WARMUP_FRAMES + TIMED_FRAMES {
            let frame = screen.frame(n);
            let due = started + interval * u32::try_from(n).unwrap_or(u32::MAX);
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
            let begun = Instant::now();
            encoder.encode(&frame)?;
            if let Some(hopeless) = tally.frame(n, begun.elapsed()) {
                return Ok(hopeless);
            }
        }
        Ok(tally.p95())
    }

    /// The frame times of one encoder's run.
    #[derive(Debug, Default)]
    pub(super) struct Tally {
        took: Vec<Duration>,
    }

    impl Tally {
        /// Records frame `n`, warm-up included, and says whether the run can
        /// stop: `Some` once the p95 is [`HOPELESS`] whatever follows, with
        /// the least it can come to — the `TAIL`-th slowest frame so far.
        ///
        /// A warm-up frame never stops it, and neither does one slow frame:
        /// a start-up stall or a slow keyframe is not the machine's speed,
        /// and the p95 leaves out exactly such frames (ADR 0145).
        pub(super) fn frame(&mut self, n: usize, took: Duration) -> Option<Duration> {
            if n < WARMUP_FRAMES {
                return None;
            }
            self.took.push(took);
            let mut slow: Vec<Duration> = self
                .took
                .iter()
                .copied()
                .filter(|&t| t >= HOPELESS)
                .collect();
            if slow.len() < TAIL {
                return None;
            }
            slow.sort_unstable_by(|a, b| b.cmp(a));
            Some(slow[TAIL - 1])
        }

        /// The p95 of the timed frames.
        pub(super) fn p95(mut self) -> Duration {
            p95(&mut self.took)
        }
    }

    /// The 95th percentile, nearest rank.
    pub(super) fn p95(samples: &mut [Duration]) -> Duration {
        if samples.is_empty() {
            return Duration::ZERO;
        }
        samples.sort_unstable();
        let rank = (samples.len() * 95).div_ceil(100);
        samples[rank.saturating_sub(1)]
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::timing::{HEIGHT, Screen, Tally, WIDTH, p95};
    use super::*;

    #[test]
    fn the_threshold_is_both_halves_of_the_owners_rule() {
        let budget = SOFTWARE_AV1_FRAME_BUDGET_MS * 1_000;
        let within = Measurement {
            av1_p95_us: budget,
            h264_p95_us: budget + 5_000,
        };
        assert!(within.passes());
        let slower_than_h264 = Measurement {
            av1_p95_us: 12_000,
            h264_p95_us: 11_000,
        };
        assert!(
            !slower_than_h264.passes(),
            "faster than the budget is not enough"
        );
        let over_budget = Measurement {
            av1_p95_us: budget + 1,
            h264_p95_us: budget * 3,
        };
        assert!(!over_budget.passes(), "faster than openh264 is not enough");
    }

    /// What the reference host measured beside a running session on
    /// 2026-10-03, which the 22 ms budget refused (ADR 0142).
    #[test]
    fn the_reference_host_passes_its_own_measurement() {
        let beta = Measurement {
            av1_p95_us: 26_300,
            h264_p95_us: 27_800,
        };
        assert!(beta.passes());
    }

    #[test]
    fn p95_is_the_nearest_rank() {
        let mut samples: Vec<Duration> = (1..=40).map(Duration::from_millis).collect();
        assert_eq!(p95(&mut samples), Duration::from_millis(38));
        assert_eq!(p95(&mut []), Duration::ZERO);
    }

    /// Five warm-up frames, then ninety timed: `slow` of the timed ones take
    /// `slow_ms`, the rest 20 ms. What the run ends with, and at which frame.
    fn run(slow: &[usize], slow_ms: u64, warmup_ms: u64) -> (Duration, usize) {
        let mut tally = Tally::default();
        for n in 0..95 {
            let took = if n < 5 {
                Duration::from_millis(warmup_ms)
            } else if slow.contains(&(n - 5)) {
                Duration::from_millis(slow_ms)
            } else {
                Duration::from_millis(20)
            };
            if let Some(hopeless) = tally.frame(n, took) {
                return (hopeless, n);
            }
        }
        (tally.p95(), 95)
    }

    /// beta on 2026-10-07 logged a p95 of 262.9, 251.8 and 284.7 ms: each
    /// the time of the one frame that ended the run, kept as the answer for
    /// the rest of the process. One frame, or a slow keyframe in the
    /// warm-up, is not the machine's speed (ADR 0145).
    #[test]
    fn one_slow_frame_does_not_end_the_measurement() {
        assert_eq!(run(&[10], 1_050, 20), (Duration::from_millis(20), 95));
        assert_eq!(run(&[], 20, 1_050), (Duration::from_millis(20), 95));
        assert_eq!(
            run(&[1, 30, 60, 89], 300, 20),
            (Duration::from_millis(20), 95),
            "four slow frames of ninety are left out of the p95"
        );
    }

    #[test]
    fn a_hopeless_p95_ends_the_measurement_at_once() {
        // The fifth slow frame puts the p95 past HOPELESS, whatever follows.
        assert_eq!(
            run(&[0, 1, 2, 3, 4], 300, 20),
            (Duration::from_millis(300), 9)
        );
        let (p95, at) = run(&[0, 2, 4, 6, 8], 260, 20);
        assert_eq!((p95, at), (Duration::from_millis(260), 13));
    }

    #[test]
    fn readiness_says_why_when_the_build_has_no_encoder() {
        if !built() {
            assert_eq!(readiness(), Readiness::NotBuilt);
            assert!(!start_measurement());
        }
    }

    /// The measurement itself, on whatever machine runs it. Ignored by
    /// default: it takes six seconds and its answer is the machine's, not
    /// the code's. `cargo test --release -p lumepeer-media --features
    /// encode-aom --lib measures_this_host -- --ignored --nocapture`.
    #[test]
    #[ignore = "a timing of this machine, not a check of the code"]
    fn measures_this_host() {
        match measure() {
            Ok(found) => println!("{found}, passes: {}", found.passes()),
            Err(error) => println!("not measured: {error}"),
        }
    }

    /// Every frame of the measurement is a full 1080p picture with text in
    /// it, and every act moves: typing, then scrolling, then a window.
    #[test]
    fn the_screen_types_scrolls_and_drags_text() {
        let screen = Screen::new();
        // Five warm-up frames, then thirty frames of each act.
        let frames: Vec<_> = (0..95).map(|n| screen.frame(n)).collect();
        for frame in &frames {
            assert_eq!(frame.data.len(), WIDTH * HEIGHT * 4);
            let ink = frame
                .data
                .chunks_exact(4)
                .filter(|pixel| pixel[0] == 0x20)
                .count();
            assert!(ink > WIDTH * HEIGHT / 50, "too little text: {ink} pixels");
        }
        // Typing changes one line now and then; scrolling and dragging
        // change most of the picture every frame.
        let changed = |a: &crate::capture::Frame, b: &crate::capture::Frame| {
            a.data.iter().zip(&b.data).filter(|(x, y)| x != y).count()
        };
        assert!(changed(&frames[2], &frames[5]) > 0, "nothing was typed");
        assert!(changed(&frames[2], &frames[5]) < WIDTH * 20 * 4);
        assert!(
            changed(&frames[30], &frames[33]) < WIDTH * 20 * 4,
            "typing moved more than a line"
        );
        assert!(
            changed(&frames[40], &frames[41]) > WIDTH * HEIGHT / 4,
            "the page did not scroll"
        );
        assert!(
            changed(&frames[80], &frames[81]) > 900 * 600,
            "the window did not move"
        );
    }
}
