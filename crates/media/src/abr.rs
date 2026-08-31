//! Adaptive quality controller (design doc §11; ADR 0015, ADR 0037).
//!
//! The guest reports what it received every `ABR_FEEDBACK_INTERVAL_MS`; the
//! host applies at most `ABR_ADJUST_MAX_RATE_PER_SEC` changes per second and
//! moves three knobs in a fixed order — bitrate first, then frame rate, then
//! picture scale — each inside its own floor from §14.
//!
//! The order is the decision, not an implementation detail: bits are the
//! cheapest thing to give up (the picture stays whole and current, it just
//! gets softer), frames are next (it stays whole and sharp, it just updates
//! less often), and pixels are last because a downscaled desktop is the one
//! degradation that can make text unreadable. Recovery walks the same ladder
//! back up in reverse, so a link that improves gets its resolution back before
//! it gets its bitrate back.

use std::time::{Duration, Instant};

use lumepeer_core::constants::{
    ABR_ADJUST_MAX_RATE_PER_SEC, ABR_FPS_STEP, ABR_GOODPUT_SHORTFALL_PERCENT, ABR_MAX_BITRATE_KBPS,
    ABR_MIN_BITRATE_KBPS, ABR_MIN_FPS, ABR_MIN_SCALE_PERCENT, ABR_SCALE_STEP_PERCENT,
    ENCODE_DEFAULT_BITRATE_KBPS, ENCODE_DEFAULT_FPS,
};

/// Loss above which the controller halves the bitrate outright.
const HEAVY_LOSS: f32 = 0.05;
/// Loss above which the controller shaves the bitrate back gently.
const LIGHT_LOSS: f32 = 0.01;
/// Denominator of a percentage.
const PERCENT: u32 = 100;
/// Full scale: the captured picture at its own size.
pub const FULL_SCALE_PERCENT: u32 = 100;

/// Receiver feedback reported by the guest.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReceiverFeedback {
    /// Fraction of frames lost since the previous report, 0.0..=1.0.
    pub loss: f32,
    /// Smoothed round trip time in milliseconds.
    pub rtt_ms: u32,
    /// Throughput the receiver actually observed, or 0 when it did not measure
    /// one — a report with nothing in it must not read as a link carrying
    /// nothing.
    pub goodput_kbps: u32,
    /// What the host actually put on the wire over the same window, or 0 when
    /// it did not measure that either.
    ///
    /// Filled in by the host from its own encoder output, never by the peer:
    /// it is the only thing that makes [`Self::goodput_kbps`] mean anything.
    /// Throughput below the target says nothing on its own — a still desktop
    /// legitimately encodes to a fraction of it — and only throughput below
    /// what was *offered* is the link saying it cannot carry the load.
    pub sent_kbps: u32,
}

/// The three knobs, as one target the encode loop applies together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QualityTarget {
    /// Encoder bitrate.
    pub bitrate_kbps: u32,
    /// Frames per second the capture loop paces itself at.
    pub fps: u8,
    /// Percentage of the captured picture's own size to encode;
    /// [`FULL_SCALE_PERCENT`] leaves it untouched.
    pub scale_percent: u32,
}

impl Default for QualityTarget {
    fn default() -> Self {
        Self {
            bitrate_kbps: ENCODE_DEFAULT_BITRATE_KBPS,
            fps: ENCODE_DEFAULT_FPS,
            scale_percent: FULL_SCALE_PERCENT,
        }
    }
}

/// Combines a guest's manual scale ceiling with what the adaptive controller
/// would otherwise pick, into the one percentage the encode loop actually
/// applies (§11; D7, docs/bugs/13-stream-resolution.md task 2).
///
/// `min` is the whole function, on purpose: the design constraint it encodes
/// is that a manual choice and the adaptive ladder must never fight over the
/// same variable. `manual_cap` is a ceiling, never a target of its own — ABR
/// stays free to sit below it when the link cannot carry it, and stays free
/// to recover only up to it, never past it. `None` means the guest asked for
/// nothing, so ABR's own target is the whole answer.
#[must_use]
pub fn effective_scale(manual_cap: Option<u32>, abr_target: u32) -> u32 {
    manual_cap.map_or(abr_target, |cap| abr_target.min(cap))
}

/// Which way the last feedback pushed the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pressure {
    /// The link cannot carry what is being sent.
    Down,
    /// The link is carrying it comfortably.
    Up,
}

/// Quality controller holding the current target.
#[derive(Debug)]
pub struct AbrController {
    target: QualityTarget,
    last_adjust: Option<Instant>,
}

impl Default for AbrController {
    fn default() -> Self {
        Self::new()
    }
}

impl AbrController {
    /// Starts at the encoder defaults of §14 and the captured picture's own
    /// size.
    #[must_use]
    pub fn new() -> Self {
        Self {
            target: QualityTarget::default(),
            last_adjust: None,
        }
    }

    /// Current bitrate target.
    #[must_use]
    pub const fn current_kbps(&self) -> u32 {
        self.target.bitrate_kbps
    }

    /// Current target across all three knobs.
    #[must_use]
    pub const fn target(&self) -> QualityTarget {
        self.target
    }

    /// Consumes feedback and returns the new target, or `None` when the change
    /// is rate-limited or nothing moved.
    ///
    /// A feedback frame whose loss is outside `0.0..=1.0` is not a
    /// measurement — it comes from an untrusted peer (§9.1) — and is dropped
    /// without touching the target or the rate-limit clock.
    pub fn on_feedback(&mut self, feedback: ReceiverFeedback) -> Option<QualityTarget> {
        if !(0.0..=1.0).contains(&feedback.loss) {
            return None;
        }
        let min_interval = Duration::from_secs(1) / ABR_ADJUST_MAX_RATE_PER_SEC;
        if self
            .last_adjust
            .is_some_and(|at| at.elapsed() < min_interval)
        {
            return None;
        }
        self.last_adjust = Some(Instant::now());

        let before = self.target;
        match self.pressure(feedback) {
            Pressure::Down => self.degrade(feedback),
            Pressure::Up => self.recover(),
        }
        (self.target != before).then_some(self.target)
    }

    /// Whether this feedback asks for less or allows more.
    ///
    /// Two independent signals, because `rd/media/1` is a reliable ordered
    /// stream and only one of them is ever available at a time. Loss is real
    /// content the guest's decoder could not reconstruct. Goodput below
    /// [`ABR_GOODPUT_SHORTFALL_PERCENT`] of what the host *offered* is the
    /// link saying it cannot carry the load.
    ///
    /// Offered, not targeted. The bitrate target is a ceiling, and a desktop
    /// that is not moving encodes to a small fraction of it: comparing arrival
    /// against the ceiling turned "there was nothing to send" into "the link
    /// is congested" on a link with no loss at all, and the ladder then walked
    /// all the way to its floor — 300 kbps, 10 fps and half of each axis — on
    /// an idle LAN session. Measured; the numbers are in
    /// docs/bugs/07-video-quality.md.
    fn pressure(&self, feedback: ReceiverFeedback) -> Pressure {
        if feedback.loss > LIGHT_LOSS {
            return Pressure::Down;
        }
        // The host cannot have offered more than the target, and a host that
        // did not measure its own output leaves the target as the only basis
        // there is.
        let offered = if feedback.sent_kbps > 0 {
            self.target.bitrate_kbps.min(feedback.sent_kbps)
        } else {
            self.target.bitrate_kbps
        };
        let floor = offered.saturating_mul(ABR_GOODPUT_SHORTFALL_PERCENT) / PERCENT;
        if feedback.goodput_kbps > 0 && feedback.goodput_kbps < floor {
            return Pressure::Down;
        }
        Pressure::Up
    }

    /// One rung down the ladder: bitrate, then frame rate, then scale.
    fn degrade(&mut self, feedback: ReceiverFeedback) {
        if self.target.bitrate_kbps > ABR_MIN_BITRATE_KBPS {
            let proposed = if feedback.loss > HEAVY_LOSS {
                self.target.bitrate_kbps / 2
            } else {
                self.target
                    .bitrate_kbps
                    .saturating_sub(self.target.bitrate_kbps / 10)
            };
            self.target.bitrate_kbps = proposed.clamp(ABR_MIN_BITRATE_KBPS, ABR_MAX_BITRATE_KBPS);
            return;
        }
        if self.target.fps > ABR_MIN_FPS {
            self.target.fps = self
                .target
                .fps
                .saturating_sub(ABR_FPS_STEP)
                .max(ABR_MIN_FPS);
            return;
        }
        if self.target.scale_percent > ABR_MIN_SCALE_PERCENT {
            self.target.scale_percent = self
                .target
                .scale_percent
                .saturating_sub(ABR_SCALE_STEP_PERCENT)
                .max(ABR_MIN_SCALE_PERCENT);
        }
        // Every knob is on its floor. §11 has no fourth one, and a picture
        // below these is indistinguishable from a session that is not working
        // at all — which is exactly what the floors exist to prevent.
    }

    /// One rung back up, in the reverse order: scale, then frame rate, then
    /// bitrate.
    fn recover(&mut self) {
        if self.target.scale_percent < FULL_SCALE_PERCENT {
            self.target.scale_percent = self
                .target
                .scale_percent
                .saturating_add(ABR_SCALE_STEP_PERCENT)
                .min(FULL_SCALE_PERCENT);
            return;
        }
        if self.target.fps < ENCODE_DEFAULT_FPS {
            self.target.fps = self
                .target
                .fps
                .saturating_add(ABR_FPS_STEP)
                .min(ENCODE_DEFAULT_FPS);
            return;
        }
        if self.target.bitrate_kbps < ABR_MAX_BITRATE_KBPS {
            self.target.bitrate_kbps = self
                .target
                .bitrate_kbps
                .saturating_add(self.target.bitrate_kbps / 20)
                .clamp(ABR_MIN_BITRATE_KBPS, ABR_MAX_BITRATE_KBPS);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "a failed assumption must fail the test")]

    use super::*;

    fn feedback(loss: f32) -> ReceiverFeedback {
        ReceiverFeedback {
            loss,
            rtt_ms: 30,
            goodput_kbps: 0,
            sent_kbps: 0,
        }
    }

    /// The rate limit is wall clock, so a test that wants a second decision
    /// without waiting takes the clock out of the way instead.
    fn allow_another_decision(abr: &mut AbrController) {
        abr.last_adjust = None;
    }

    #[test]
    fn heavy_loss_halves_and_stays_in_range() {
        let mut abr = AbrController::new();
        let target = abr.on_feedback(feedback(0.2)).expect("loss must adjust");
        assert_eq!(target.bitrate_kbps, ENCODE_DEFAULT_BITRATE_KBPS / 2);
        assert!(abr.current_kbps() >= ABR_MIN_BITRATE_KBPS);
        assert!(abr.current_kbps() <= ABR_MAX_BITRATE_KBPS);
    }

    /// The measured regression of docs/bugs/07-video-quality.md, task 2: a
    /// desktop nobody is touching, on a link with no loss and room to spare.
    ///
    /// The guest reports what actually arrived, which for a still screen is a
    /// fraction of the bitrate ceiling. Read against the ceiling that was
    /// congestion, and the ladder walked to its floor in about half a minute:
    /// 300 kbps, 10 fps, and half of each axis — a quarter of the pixels, on a
    /// LAN. Read against what the host actually sent, it is what it is: a
    /// quiet screen.
    #[test]
    fn a_still_screen_on_a_fast_link_is_not_congestion() {
        let mut abr = AbrController::new();
        for _ in 0..60 {
            allow_another_decision(&mut abr);
            abr.on_feedback(ReceiverFeedback {
                loss: 0.0,
                rtt_ms: 2,
                // Everything the host encoded arrived, and it was not much:
                // this is a desktop with a clock on it.
                goodput_kbps: 200,
                sent_kbps: 200,
            });
        }
        let target = abr.target();
        assert_eq!(
            target.scale_percent, FULL_SCALE_PERCENT,
            "the picture was downscaled on an idle LAN"
        );
        assert_eq!(
            target.fps, ENCODE_DEFAULT_FPS,
            "the frame rate was cut on an idle LAN"
        );
        assert!(
            target.bitrate_kbps >= ENCODE_DEFAULT_BITRATE_KBPS,
            "the ceiling fell below the default on an idle LAN: {}",
            target.bitrate_kbps
        );
    }

    /// And the signal still works: a link that really is dropping what the
    /// host offered gets the ladder it is there for.
    #[test]
    fn arrival_far_under_what_was_sent_is_still_congestion() {
        let mut abr = AbrController::new();
        for _ in 0..10 {
            allow_another_decision(&mut abr);
            abr.on_feedback(ReceiverFeedback {
                loss: 0.0,
                rtt_ms: 40,
                // A quarter of what went out came back reported.
                goodput_kbps: 1_000,
                sent_kbps: 4_000,
            });
        }
        assert!(
            abr.current_kbps() < ENCODE_DEFAULT_BITRATE_KBPS,
            "a link losing three quarters of the load was read as healthy"
        );
    }

    #[test]
    fn adjustments_are_rate_limited() {
        let mut abr = AbrController::new();
        assert!(abr.on_feedback(feedback(0.2)).is_some());
        assert!(abr.on_feedback(feedback(0.2)).is_none());
    }

    /// The order of the degradation is the decision ADR 0037 records: bitrate
    /// all the way to its floor, only then frame rate, only then scale — and
    /// never a knob out of turn.
    #[test]
    fn degradation_walks_bitrate_then_fps_then_scale() {
        let mut abr = AbrController::new();
        let mut seen_fps_move = false;
        let mut seen_scale_move = false;

        for _ in 0..64 {
            let before = abr.target();
            allow_another_decision(&mut abr);
            let Some(after) = abr.on_feedback(feedback(0.5)) else {
                break;
            };
            if after.fps != before.fps {
                assert_eq!(
                    before.bitrate_kbps, ABR_MIN_BITRATE_KBPS,
                    "frame rate moved while the bitrate still had room"
                );
                seen_fps_move = true;
            }
            if after.scale_percent != before.scale_percent {
                assert_eq!(
                    before.fps, ABR_MIN_FPS,
                    "scale moved while the frame rate still had room"
                );
                seen_scale_move = true;
            }
        }

        assert!(seen_fps_move, "the frame rate rung was never reached");
        assert!(seen_scale_move, "the scale rung was never reached");
        assert_eq!(abr.target().bitrate_kbps, ABR_MIN_BITRATE_KBPS);
        assert_eq!(abr.target().fps, ABR_MIN_FPS);
        assert_eq!(abr.target().scale_percent, ABR_MIN_SCALE_PERCENT);
    }

    /// The floors of §14 are where the controller stops: a picture below them
    /// is indistinguishable from no picture at all.
    #[test]
    fn the_floors_hold_however_bad_the_feedback_gets() {
        let mut abr = AbrController::new();
        for _ in 0..256 {
            allow_another_decision(&mut abr);
            abr.on_feedback(feedback(1.0));
        }
        let target = abr.target();
        assert_eq!(target.bitrate_kbps, ABR_MIN_BITRATE_KBPS);
        assert_eq!(target.fps, ABR_MIN_FPS);
        assert_eq!(target.scale_percent, ABR_MIN_SCALE_PERCENT);
        allow_another_decision(&mut abr);
        assert!(
            abr.on_feedback(feedback(1.0)).is_none(),
            "there is nothing left to give"
        );
    }

    /// Recovery is the ladder in reverse: pixels come back first, bits last.
    #[test]
    fn recovery_walks_scale_then_fps_then_bitrate() {
        let mut abr = AbrController::new();
        for _ in 0..256 {
            allow_another_decision(&mut abr);
            abr.on_feedback(feedback(1.0));
        }
        allow_another_decision(&mut abr);
        let first = abr
            .on_feedback(feedback(0.0))
            .expect("a clean link recovers");
        assert!(first.scale_percent > ABR_MIN_SCALE_PERCENT);
        assert_eq!(first.fps, ABR_MIN_FPS, "frames must wait for the pixels");
        assert_eq!(first.bitrate_kbps, ABR_MIN_BITRATE_KBPS);

        for _ in 0..256 {
            allow_another_decision(&mut abr);
            abr.on_feedback(feedback(0.0));
        }
        let target = abr.target();
        assert_eq!(target.scale_percent, FULL_SCALE_PERCENT);
        assert_eq!(target.fps, ENCODE_DEFAULT_FPS);
        assert_eq!(target.bitrate_kbps, ABR_MAX_BITRATE_KBPS);
    }

    /// Goodput well under what was sent is the only congestion signal a
    /// reliable ordered stream can give, so it has to count on its own.
    #[test]
    fn goodput_far_under_what_was_sent_degrades_without_any_reported_loss() {
        let mut abr = AbrController::new();
        let starved = ReceiverFeedback {
            loss: 0.0,
            rtt_ms: 30,
            goodput_kbps: ENCODE_DEFAULT_BITRATE_KBPS / 10,
            sent_kbps: ENCODE_DEFAULT_BITRATE_KBPS,
        };
        let target = abr
            .on_feedback(starved)
            .expect("a starved link must adjust");
        assert!(target.bitrate_kbps < ENCODE_DEFAULT_BITRATE_KBPS);
    }

    /// A report with no goodput in it is a report that did not measure one,
    /// not a link carrying nothing: an idle screen must not read as
    /// congestion.
    #[test]
    fn an_unmeasured_goodput_never_reads_as_congestion() {
        let mut abr = AbrController::new();
        let target = abr
            .on_feedback(feedback(0.0))
            .expect("a clean link with no measurement still recovers");
        assert!(target.bitrate_kbps > ENCODE_DEFAULT_BITRATE_KBPS);
    }

    /// The numbers come from a peer that has proven nothing about them (§9.1):
    /// out of range means "drop this frame of feedback", never "believe it"
    /// and never a panic.
    #[test]
    fn feedback_outside_the_loss_contract_is_dropped_whole() {
        for nonsense in [-0.5f32, 1.5, f32::NAN, f32::INFINITY] {
            let mut abr = AbrController::new();
            assert!(abr.on_feedback(feedback(nonsense)).is_none());
            assert_eq!(abr.target(), QualityTarget::default());
            // The dropped frame must not have spent the rate-limit budget
            // either, or a peer could mute adaptation by sending garbage.
            assert!(abr.on_feedback(feedback(0.5)).is_some());
        }
    }

    /// D7, docs/bugs/13-stream-resolution.md task 2: a ceiling below the
    /// adaptive target wins — the guest's own choice caps the picture even
    /// when the link has room to spare.
    #[test]
    fn effective_scale_prefers_the_lower_manual_ceiling() {
        assert_eq!(effective_scale(Some(50), 100), 50);
        assert_eq!(
            effective_scale(Some(ABR_MIN_SCALE_PERCENT), 75),
            ABR_MIN_SCALE_PERCENT
        );
    }

    /// A ceiling is not a floor: ABR still gets to sit below it when the
    /// link cannot carry what the guest asked for.
    #[test]
    fn effective_scale_lets_abr_sit_below_a_higher_manual_ceiling() {
        assert_eq!(effective_scale(Some(100), 50), 50);
        assert_eq!(
            effective_scale(Some(75), ABR_MIN_SCALE_PERCENT),
            ABR_MIN_SCALE_PERCENT
        );
    }

    /// No manual choice at all: ABR's own target is the whole answer, as it
    /// always was before this existed.
    #[test]
    fn effective_scale_with_no_manual_cap_is_just_the_abr_target() {
        for target in [ABR_MIN_SCALE_PERCENT, 60, FULL_SCALE_PERCENT] {
            assert_eq!(effective_scale(None, target), target);
        }
    }

    #[test]
    fn the_rate_limit_is_wall_clock_not_a_counter() {
        let mut abr = AbrController::new();
        assert!(abr.on_feedback(feedback(0.5)).is_some());
        assert!(abr.on_feedback(feedback(0.5)).is_none());
        std::thread::sleep(Duration::from_secs(1) / ABR_ADJUST_MAX_RATE_PER_SEC);
        assert!(abr.on_feedback(feedback(0.5)).is_some());
    }
}
