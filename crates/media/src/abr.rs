//! Adaptive bitrate controller (design doc §11).
//!
//! The guest sends receiver feedback every `ABR_FEEDBACK_INTERVAL_MS`; the host
//! applies at most `ABR_ADJUST_MAX_RATE_PER_SEC` changes per second and stays
//! inside `ABR_MIN_BITRATE_KBPS..=ABR_MAX_BITRATE_KBPS`.

use std::time::{Duration, Instant};

use lumepeer_core::constants::{
    ABR_ADJUST_MAX_RATE_PER_SEC, ABR_MAX_BITRATE_KBPS, ABR_MIN_BITRATE_KBPS,
    ENCODE_DEFAULT_BITRATE_KBPS,
};

/// Receiver feedback reported by the guest.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReceiverFeedback {
    /// Fraction of packets lost since the previous report, 0.0..=1.0.
    pub loss: f32,
    /// Smoothed round trip time in milliseconds.
    pub rtt_ms: u32,
    /// Throughput the receiver actually observed.
    pub goodput_kbps: u32,
}

/// Bitrate controller holding the current target.
#[derive(Debug)]
pub struct AbrController {
    current_kbps: u32,
    last_adjust: Option<Instant>,
}

impl Default for AbrController {
    fn default() -> Self {
        Self::new()
    }
}

impl AbrController {
    /// Starts at the default encoder bitrate of §14.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            current_kbps: ENCODE_DEFAULT_BITRATE_KBPS,
            last_adjust: None,
        }
    }

    /// Current target bitrate.
    #[must_use]
    pub const fn current_kbps(&self) -> u32 {
        self.current_kbps
    }

    /// Consumes feedback and returns a new target, or `None` when the change
    /// is rate-limited or the target did not move.
    pub fn on_feedback(&mut self, feedback: ReceiverFeedback) -> Option<u32> {
        let min_interval = Duration::from_secs(1) / ABR_ADJUST_MAX_RATE_PER_SEC;
        if self
            .last_adjust
            .is_some_and(|at| at.elapsed() < min_interval)
        {
            return None;
        }

        // Multiplicative decrease on loss, gentle additive increase otherwise.
        let proposed = if feedback.loss > 0.05 {
            self.current_kbps / 2
        } else if feedback.loss > 0.01 {
            self.current_kbps.saturating_sub(self.current_kbps / 10)
        } else {
            self.current_kbps.saturating_add(self.current_kbps / 20)
        };
        let clamped = proposed.clamp(ABR_MIN_BITRATE_KBPS, ABR_MAX_BITRATE_KBPS);

        self.last_adjust = Some(Instant::now());
        if clamped == self.current_kbps {
            return None;
        }
        self.current_kbps = clamped;
        Some(clamped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feedback(loss: f32) -> ReceiverFeedback {
        ReceiverFeedback {
            loss,
            rtt_ms: 30,
            goodput_kbps: 3_000,
        }
    }

    #[test]
    fn heavy_loss_halves_and_stays_in_range() {
        let mut abr = AbrController::new();
        let new_rate = abr.on_feedback(feedback(0.2));
        assert_eq!(new_rate, Some(ENCODE_DEFAULT_BITRATE_KBPS / 2));
        assert!(abr.current_kbps() >= ABR_MIN_BITRATE_KBPS);
        assert!(abr.current_kbps() <= ABR_MAX_BITRATE_KBPS);
    }

    #[test]
    fn adjustments_are_rate_limited() {
        let mut abr = AbrController::new();
        assert!(abr.on_feedback(feedback(0.2)).is_some());
        assert!(abr.on_feedback(feedback(0.2)).is_none());
    }
}
