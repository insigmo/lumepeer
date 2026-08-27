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
    /// [`ABR_GOODPUT_SHORTFALL_PERCENT`] of the target is the link saying it
    /// cannot carry the rate — but only when the guest actually measured one,
    /// since a still desktop legitimately produces almost no bytes and must
    /// not read as congestion.
    fn pressure(&self, feedback: ReceiverFeedback) -> Pressure {
        if feedback.loss > LIGHT_LOSS {
            return Pressure::Down;
        }
        let floor = self
            .target
            .bitrate_kbps
            .saturating_mul(ABR_GOODPUT_SHORTFALL_PERCENT)
            / PERCENT;
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

    /// Goodput well under the target is the only congestion signal a reliable
    /// ordered stream can give, so it has to count on its own.
    #[test]
    fn goodput_far_under_the_target_degrades_without_any_reported_loss() {
        let mut abr = AbrController::new();
        let starved = ReceiverFeedback {
            loss: 0.0,
            rtt_ms: 30,
            goodput_kbps: ENCODE_DEFAULT_BITRATE_KBPS / 10,
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

    #[test]
    fn the_rate_limit_is_wall_clock_not_a_counter() {
        let mut abr = AbrController::new();
        assert!(abr.on_feedback(feedback(0.5)).is_some());
        assert!(abr.on_feedback(feedback(0.5)).is_none());
        std::thread::sleep(Duration::from_secs(1) / ABR_ADJUST_MAX_RATE_PER_SEC);
        assert!(abr.on_feedback(feedback(0.5)).is_some());
    }
}
