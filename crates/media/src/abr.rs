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
    ENCODE_DEFAULT_BITRATE_KBPS, ENCODE_DEFAULT_FPS, ENCODE_MAX_FPS,
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

/// The fixed target a guest's chosen quality preset means, or `None` when it
/// chose nothing (§11; ADR 0064, amending ADR 0037; D7,
/// docs/bugs/13-stream-resolution.md task 2).
///
/// A preset used to be a *ceiling* the adaptive ladder was free to sit below,
/// which sounded like the careful reading and was the wrong one: the two ends
/// then had a hand each on the same three knobs, and the picture drifted
/// between sharp and soft for the whole session no matter what the selector
/// said. Recovery climbs 5% of the bitrate per adjustment and degradation
/// takes 10% back, so on any link that is neither perfect nor plainly bad
/// the controller oscillates — and an oscillation is exactly what a person
/// sees as "the quality keeps changing by itself".
///
/// So a preset is a target, not a ceiling, and it pins all three knobs rather
/// than the one it names: a bitrate that walks while the scale is held still
/// is just as visible as a scale that walks. The cost is real and deliberate
/// — a link that genuinely cannot carry the chosen picture now stutters
/// instead of quietly softening, which is the trade a person makes when they
/// pick a preset by name. Nothing adapts on its own again until the guest
/// stops naming one.
///
/// `fps` is the frame rate the same preset names, already held under the
/// session's [`ceiling_fps`] by the caller (ADR 0136): the preset picks the
/// tradeoff between fewer, sharper frames and more, softer ones, because at a
/// pinned bitrate those are the only two ways to spend it.
#[must_use]
pub fn pinned_target(manual_cap: Option<u32>, fps: u8) -> Option<QualityTarget> {
    manual_cap.map(|scale_percent| QualityTarget {
        fps,
        scale_percent,
        ..QualityTarget::default()
    })
}

/// The frame rate a session runs at when nothing holds it back: the host
/// display's own refresh rate, up to [`ENCODE_MAX_FPS`] (§11; ADR 0136).
///
/// The encoder is told this figure, and that is why it cannot simply be the
/// ceiling for everyone: a hardware encoder divides its bitrate by the frame
/// rate it was *told*, not by the one frames arrive at. Measured on the
/// reference machine's Media Foundation encoder, 60 frames a second declared
/// as 144 spent 3.5 of 8 Mbit/s — every frame 2.3 times softer than declared
/// as 60, on the 60 Hz panel most hosts have.
///
/// `None`, or a figure below [`ABR_MIN_FPS`] that no real panel reports, is a
/// display that could not say: [`ENCODE_DEFAULT_FPS`] then.
#[must_use]
pub fn ceiling_fps(refresh_hz: Option<u32>) -> u8 {
    refresh_hz
        .filter(|&hz| hz >= u32::from(ABR_MIN_FPS))
        .map_or(ENCODE_DEFAULT_FPS, |hz| {
            u8::try_from(hz.min(u32::from(ENCODE_MAX_FPS))).unwrap_or(ENCODE_MAX_FPS)
        })
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
    /// Where frame-rate recovery stops: this session's [`ceiling_fps`].
    max_fps: u8,
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
        Self::with_max_fps(ENCODE_DEFAULT_FPS)
    }

    /// As [`Self::new`], for a session whose frame rate tops out at
    /// `max_fps` — its [`ceiling_fps`] — rather than at the default. The
    /// controller starts there and recovers back to there (ADR 0136).
    #[must_use]
    pub fn with_max_fps(max_fps: u8) -> Self {
        Self {
            target: QualityTarget {
                fps: max_fps,
                ..QualityTarget::default()
            },
            last_adjust: None,
            max_fps,
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
        if self.target.fps < self.max_fps {
            self.target.fps = self
                .target
                .fps
                .saturating_add(ABR_FPS_STEP)
                .min(self.max_fps);
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

/// How long [`EncodeSpeed`] and [`LinkPressure`] each measure before they
/// decide anything (ADR 0144).
const KEEP_UP_WINDOW: Duration = Duration::from_secs(1);

/// What one step of [`EncodeSpeed`] keeps of each side of the picture, in
/// percent: 85% of each axis is 72% of the pixels, and an encoder's time
/// follows the pixels.
const SPEED_STEP_PERCENT: u32 = 85;
/// Windows in a row the encoder misses the interval before the picture is
/// reduced: one slow second is a keyframe or a scene change, two are the
/// picture.
const SPEED_SLOW_WINDOWS: u32 = 2;
/// Windows in a row with room to spare before the picture is enlarged again.
/// Five times [`SPEED_SLOW_WINDOWS`], because each change of size is a
/// keyframe, and a film that alternates calm and busy scenes must not be
/// resized at every cut.
const SPEED_ROOM_WINDOWS: u32 = 10;
/// "Room to spare": the time the next step up is predicted to take, as a
/// percentage of the interval it has to fit.
const SPEED_ROOM_PERCENT: u32 = 80;
/// The preset frame rate at and below which [`defended_fps`] gives up a third
/// of the frames before any sharpness...
const SPEED_SHARP_FPS: u8 = 30;
/// ...and at and above which it gives up sharpness before any frame.
const SPEED_SMOOTH_FPS: u8 = 120;

/// The frame rate the encoder has to make before [`EncodeSpeed`] reduces the
/// picture, for a preset that asked for `asked` frames a second on a session
/// running at `rate` (ADR 0144).
///
/// The more frames a preset asks for, the more of them it keeps: `quality`'s
/// 30 gives up a third of them before it gives up any sharpness,
/// `performance`'s 144 gives up sharpness before any frame, and `balance`'s
/// 60 sits between. Never above `rate` — a host whose display makes 60 is not
/// held to 144 — and never under [`ABR_MIN_FPS`].
#[must_use]
pub fn defended_fps(asked: u8, rate: u8) -> u8 {
    let over = u32::from(asked.clamp(SPEED_SHARP_FPS, SPEED_SMOOTH_FPS) - SPEED_SHARP_FPS);
    let span = u32::from(SPEED_SMOOTH_FPS - SPEED_SHARP_FPS);
    // Two thirds at the sharp end, all of it at the smooth one, in thousandths.
    let share = 667 + 333 * over / span;
    let kept = u32::from(rate) * share / 1_000;
    u8::try_from(kept)
        .unwrap_or(rate)
        .clamp(ABR_MIN_FPS.min(rate), rate)
}

/// Reduces the picture while the encoder cannot make the frame rate, and
/// gives the size back once it can (ADR 0144).
///
/// [`AbrController`] answers the link, and a preset switches even that off;
/// nothing answered the host's own processor. A software encoder took
/// 40–60 ms for each 1080p frame of a film on a host with no hardware
/// encoder, and the `quality` preset kept asking it for 30 of them a second.
/// This measures what reducing and encoding actually take, a second at a
/// time, and lowers the share of the captured picture the encoder is handed
/// once two seconds in a row miss the interval.
#[derive(Debug)]
pub struct EncodeSpeed {
    /// When the current window started; `None` until its first frame.
    started: Option<Instant>,
    /// Reducing and encoding, summed over the window's frames.
    busy: Duration,
    /// Frames in the window.
    frames: u32,
    /// Windows in a row that missed the interval.
    slow: u32,
    /// Windows in a row that had room for the next step up.
    room: u32,
    /// The most of the captured picture's side the encoder is handed, in
    /// percent.
    cap_percent: u32,
}

impl Default for EncodeSpeed {
    fn default() -> Self {
        Self {
            started: None,
            busy: Duration::ZERO,
            frames: 0,
            slow: 0,
            room: 0,
            cap_percent: FULL_SCALE_PERCENT,
        }
    }
}

impl EncodeSpeed {
    /// The most of the captured picture's side, in percent, the encoder is
    /// handed now: [`FULL_SCALE_PERCENT`] while it keeps up.
    #[must_use]
    pub const fn cap_percent(&self) -> u32 {
        self.cap_percent
    }

    /// Forgets everything, the reduction included: a new preset is a new
    /// tradeoff, and is measured afresh.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// One frame took `took` to reduce and encode, at `picture_percent` of
    /// the captured picture's side, on a session that has to make a frame
    /// every `interval`. The new cap when this frame closed a window that
    /// moved it.
    pub fn frame(
        &mut self,
        now: Instant,
        took: Duration,
        picture_percent: u32,
        interval: Duration,
    ) -> Option<u32> {
        let started = *self.started.get_or_insert(now);
        self.busy = self.busy.saturating_add(took);
        self.frames = self.frames.saturating_add(1);
        if now.saturating_duration_since(started) < KEEP_UP_WINDOW {
            return None;
        }
        let mean = self.busy / self.frames.max(1);
        self.started = None;
        self.busy = Duration::ZERO;
        self.frames = 0;
        // What was actually encoded, which a guest's smaller window can
        // already have made smaller than the cap: reducing from the cap would
        // spend steps that change nothing.
        let picture = picture_percent.clamp(1, self.cap_percent);

        if mean > interval {
            self.room = 0;
            self.slow = self.slow.saturating_add(1);
            if self.slow < SPEED_SLOW_WINDOWS {
                return None;
            }
            self.slow = 0;
            let reduced = (picture * SPEED_STEP_PERCENT / PERCENT).max(ABR_MIN_SCALE_PERCENT);
            if reduced >= picture {
                // On the floor already; nothing smaller is readable.
                return None;
            }
            self.cap_percent = reduced;
            return Some(reduced);
        }
        self.slow = 0;
        if self.cap_percent >= FULL_SCALE_PERCENT {
            self.room = 0;
            return None;
        }
        let next = (self.cap_percent * PERCENT / SPEED_STEP_PERCENT).min(FULL_SCALE_PERCENT);
        // The encoder's time follows the pixels: the next step up costs
        // (next / now)² of this one.
        let predicted = mean.mul_f64((f64::from(next) / f64::from(picture)).powi(2));
        if predicted * PERCENT > interval * SPEED_ROOM_PERCENT {
            self.room = 0;
            return None;
        }
        self.room = self.room.saturating_add(1);
        if self.room < SPEED_ROOM_WINDOWS {
            return None;
        }
        self.room = 0;
        self.cap_percent = next;
        Some(next)
    }
}

/// Share of a window spent waiting on the link, in percent, at and above
/// which [`LinkPressure`] calls the window pressed.
const PRESSED_HELD_PERCENT: u32 = 50;
/// Share at and below which it calls the window clear.
const CLEAR_HELD_PERCENT: u32 = 10;
/// Pressed windows in a row before the bitrate comes down: one is a Wi-Fi
/// hiccup that is over before anything could act on it, and lowering the
/// picture for it would cost seconds of softness to save nothing.
const PRESSED_WINDOWS: u32 = 2;
/// Clear windows in a row before the bitrate starts going back up, a step
/// each clear window after that.
const CLEAR_WINDOWS: u32 = 2;
/// Each step back up, in percent of the cap: from the floor to a preset's
/// 8 Mbit/s in nine steps, so about ten seconds of a clear link.
const RECOVERY_STEP_PERCENT: u32 = 150;

/// Lowers the bitrate a preset pinned while the link cannot carry the
/// picture, and gives it back as the link recovers (ADR 0144).
///
/// A preset pins the whole quality target (ADR 0064), so nothing answered a
/// link that fell below it. ADR 0139 made the host wait for the guest instead
/// of queueing frames, which keeps every frame current — and on a link that
/// loses packets steadily a 25 KB frame took seconds to arrive, so the guest
/// saw the same picture for as long as the loss lasted. The waiting is the
/// signal: windows mostly spent waiting halve the bitrate, from what was
/// actually sent rather than from the target, which a software encoder
/// undershoots by two thirds. Smaller frames get through where large ones
/// stall.
#[derive(Debug, Default)]
pub struct LinkPressure {
    /// When the current window started; `None` until it is first asked.
    started: Option<Instant>,
    /// Time spent waiting on the link in the window.
    held: Duration,
    /// Bytes handed to the link in the window.
    bytes: u64,
    /// Pressed windows in a row.
    pressed: u32,
    /// Clear windows in a row.
    clear: u32,
    /// The bitrate the picture is held under, if any.
    cap_kbps: Option<u32>,
}

impl LinkPressure {
    /// The bitrate the picture is held under now, if any.
    #[must_use]
    pub const fn cap_kbps(&self) -> Option<u32> {
        self.cap_kbps
    }

    /// The loop waited `took` for the link.
    pub fn held(&mut self, took: Duration) {
        self.held = self.held.saturating_add(took);
    }

    /// `bytes` of picture went to the link.
    pub fn sent(&mut self, bytes: usize) {
        self.bytes = self.bytes.saturating_add(bytes as u64);
    }

    /// Drops the cap and everything measured, for a session no preset pins
    /// any more: the adaptive controller has the bitrate then. Whether there
    /// was a cap to drop.
    pub fn release(&mut self) -> bool {
        let had = self.cap_kbps.is_some();
        *self = Self::default();
        had
    }

    /// Closes the window once it has run, against the `ceiling_kbps` the
    /// preset pinned. The bitrate to encode at from now on, when the cap
    /// moved: the cap, or the ceiling once the cap is gone.
    pub fn due(&mut self, now: Instant, ceiling_kbps: u32) -> Option<u32> {
        let started = *self.started.get_or_insert(now);
        let elapsed = now.saturating_duration_since(started);
        if elapsed < KEEP_UP_WINDOW {
            return None;
        }
        let held = std::mem::take(&mut self.held);
        let bytes = std::mem::take(&mut self.bytes);
        self.started = Some(now);

        if held * PERCENT >= elapsed * PRESSED_HELD_PERCENT {
            self.clear = 0;
            self.pressed = self.pressed.saturating_add(1);
            if self.pressed < PRESSED_WINDOWS {
                return None;
            }
            let millis = u64::try_from(elapsed.as_millis())
                .unwrap_or(u64::MAX)
                .max(1);
            let sent_kbps = u32::try_from(bytes.saturating_mul(8) / millis).unwrap_or(u32::MAX);
            let current = self
                .cap_kbps
                .map_or(ceiling_kbps, |cap| cap.min(ceiling_kbps));
            let lowered = (current.min(sent_kbps) / 2).max(ABR_MIN_BITRATE_KBPS);
            if lowered >= current {
                return None;
            }
            self.cap_kbps = Some(lowered);
            return Some(lowered);
        }
        self.pressed = 0;
        let Some(cap) = self.cap_kbps else {
            self.clear = 0;
            return None;
        };
        if held * PERCENT > elapsed * CLEAR_HELD_PERCENT {
            self.clear = 0;
            return None;
        }
        self.clear = self.clear.saturating_add(1);
        if self.clear < CLEAR_WINDOWS {
            return None;
        }
        let raised = cap.saturating_mul(RECOVERY_STEP_PERCENT) / PERCENT;
        self.cap_kbps = (raised < ceiling_kbps).then_some(raised);
        Some(raised.min(ceiling_kbps))
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

    /// D7, docs/bugs/13-stream-resolution.md task 2: the guest's chosen
    /// preset is the picture, exactly, on a link with room to spare and on
    /// one without.
    #[test]
    fn a_named_preset_is_the_whole_target() {
        for scale in [ABR_MIN_SCALE_PERCENT, 67, FULL_SCALE_PERCENT] {
            let pinned =
                pinned_target(Some(scale), ENCODE_DEFAULT_FPS).expect("a named preset pins");
            assert_eq!(pinned.scale_percent, scale);
        }
    }

    /// And it pins the other two knobs as well: a bitrate walking under a
    /// held scale is the same flicker by another name
    /// (docs/bugs/07-video-quality.md). The frame rate it pins is the one
    /// the preset names, not a constant (ADR 0136).
    #[test]
    fn a_named_preset_pins_the_bitrate_and_frame_rate_too() {
        for fps in [30, ENCODE_DEFAULT_FPS, ENCODE_MAX_FPS] {
            let pinned = pinned_target(Some(50), fps).expect("a named preset pins");
            assert_eq!(pinned.bitrate_kbps, ENCODE_DEFAULT_BITRATE_KBPS);
            assert_eq!(pinned.fps, fps);
        }
    }

    /// No preset at all: nothing is pinned and the adaptive controller is
    /// the whole answer, as it always was.
    #[test]
    fn no_preset_pins_nothing() {
        assert_eq!(pinned_target(None, ENCODE_DEFAULT_FPS), None);
    }

    /// ADR 0136: the display's refresh rate is the ceiling, the owner's cap
    /// is the ceiling above that, and a display that cannot say gets the
    /// default rather than a guess.
    #[test]
    fn the_ceiling_is_the_display_refresh_up_to_the_owners_cap() {
        for (refresh_hz, expected) in [
            (Some(30), 30),
            (Some(60), 60),
            (Some(75), 75),
            (Some(144), ENCODE_MAX_FPS),
            (Some(165), ENCODE_MAX_FPS),
            (Some(360), ENCODE_MAX_FPS),
            (None, ENCODE_DEFAULT_FPS),
            // Windows' "hardware default" answers; no panel refreshes this slowly.
            (Some(0), ENCODE_DEFAULT_FPS),
            (Some(1), ENCODE_DEFAULT_FPS),
        ] {
            assert_eq!(ceiling_fps(refresh_hz), expected, "refresh {refresh_hz:?}");
        }
    }

    /// A 144 Hz host starts at 144 and, once a bad link has passed, climbs
    /// back to 144 — not to the default the ladder used to stop at.
    #[test]
    fn a_session_starts_at_and_recovers_to_its_own_ceiling() {
        let mut abr = AbrController::with_max_fps(ENCODE_MAX_FPS);
        assert_eq!(abr.target().fps, ENCODE_MAX_FPS);
        for _ in 0..256 {
            allow_another_decision(&mut abr);
            abr.on_feedback(feedback(1.0));
        }
        assert_eq!(abr.target().fps, ABR_MIN_FPS);
        for _ in 0..256 {
            allow_another_decision(&mut abr);
            abr.on_feedback(feedback(0.0));
        }
        assert_eq!(abr.target().fps, ENCODE_MAX_FPS);
    }

    #[test]
    fn the_rate_limit_is_wall_clock_not_a_counter() {
        let mut abr = AbrController::new();
        assert!(abr.on_feedback(feedback(0.5)).is_some());
        assert!(abr.on_feedback(feedback(0.5)).is_none());
        std::thread::sleep(Duration::from_secs(1) / ABR_ADJUST_MAX_RATE_PER_SEC);
        assert!(abr.on_feedback(feedback(0.5)).is_some());
    }

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    /// ADR 0144: `quality` gives up a third of its frames before any
    /// sharpness, `performance` none, `balance` some; never more than the
    /// session runs at, never under the floor.
    #[test]
    fn a_preset_defends_more_of_its_frames_the_more_it_asks_for() {
        assert_eq!(defended_fps(30, 30), 20);
        assert_eq!(defended_fps(60, 60), 46);
        assert_eq!(defended_fps(144, 144), 144);
        assert_eq!(defended_fps(144, 60), 60, "a 60 Hz host is not held to 144");
        assert_eq!(defended_fps(30, 12), ABR_MIN_FPS);
        assert_eq!(
            defended_fps(30, 5),
            5,
            "never above what the session runs at"
        );
    }

    /// One second of frames that each took `took` at `picture` percent, and
    /// what the frame that closed the window said.
    fn speed_window(
        speed: &mut EncodeSpeed,
        at: &mut Instant,
        took: Duration,
        picture: u32,
        interval: Duration,
    ) -> Option<u32> {
        let mut said = None;
        for _ in 0..=10 {
            said = speed.frame(*at, took, picture, interval);
            *at += ms(100);
        }
        said
    }

    /// ADR 0144: two slow seconds in a row reduce the picture a step, from
    /// what was actually encoded, down to the floor and no further.
    #[test]
    fn an_encoder_that_misses_the_interval_twice_gets_a_smaller_picture() {
        let interval = ms(50);
        let mut speed = EncodeSpeed::default();
        let mut at = Instant::now();
        assert_eq!(
            speed_window(&mut speed, &mut at, ms(60), 100, interval),
            None,
            "one slow second is a scene change, not the picture"
        );
        assert_eq!(
            speed_window(&mut speed, &mut at, ms(60), 100, interval),
            Some(85)
        );
        assert_eq!(speed.cap_percent(), 85);

        // The guest's window already made the picture 80%: the next step
        // comes from that.
        assert_eq!(
            speed_window(&mut speed, &mut at, ms(60), 80, interval),
            None
        );
        assert_eq!(
            speed_window(&mut speed, &mut at, ms(60), 80, interval),
            Some(68)
        );

        let mut floor = speed.cap_percent();
        for _ in 0..10 {
            speed_window(&mut speed, &mut at, ms(60), floor, interval);
            floor = speed.cap_percent();
        }
        assert_eq!(floor, ABR_MIN_SCALE_PERCENT);
    }

    /// ADR 0144: the size comes back after ten seconds with room for the next
    /// step, and a second without room starts the count again.
    #[test]
    fn the_picture_grows_back_after_ten_seconds_with_room_for_it() {
        let interval = ms(50);
        let mut speed = EncodeSpeed::default();
        let mut at = Instant::now();
        speed_window(&mut speed, &mut at, ms(60), 100, interval);
        speed_window(&mut speed, &mut at, ms(60), 100, interval);
        assert_eq!(speed.cap_percent(), 85);

        // 25 ms at 85% is 35 ms at full size: inside 80% of 50 ms.
        for _ in 0..5 {
            assert_eq!(
                speed_window(&mut speed, &mut at, ms(25), 85, interval),
                None
            );
        }
        // 33 ms is still inside the interval, but the full size would take
        // 46: no room, and the count starts over.
        assert_eq!(
            speed_window(&mut speed, &mut at, ms(33), 85, interval),
            None
        );
        for _ in 0..9 {
            assert_eq!(
                speed_window(&mut speed, &mut at, ms(25), 85, interval),
                None
            );
        }
        assert_eq!(
            speed_window(&mut speed, &mut at, ms(25), 85, interval),
            Some(FULL_SCALE_PERCENT)
        );
    }

    /// One second of the link, waited on for `held_ms` and carrying `kbps`,
    /// and what closing it said.
    fn link_window(
        link: &mut LinkPressure,
        at: &mut Instant,
        held_ms: u64,
        kbps: u32,
        ceiling: u32,
    ) -> Option<u32> {
        // Opens the window the first time; mid-window it says nothing.
        assert_eq!(link.due(*at, ceiling), None);
        link.held(ms(held_ms));
        link.sent(usize::try_from(u64::from(kbps) * 1_000 / 8).expect("fits"));
        *at += ms(1_000);
        link.due(*at, ceiling)
    }

    /// ADR 0144: one stalled second is a hiccup; from the second one on the
    /// bitrate halves from what was actually sent, down to the floor.
    #[test]
    fn a_link_that_keeps_stalling_halves_the_bitrate_from_what_was_sent() {
        let mut link = LinkPressure::default();
        let mut at = Instant::now();
        assert_eq!(link_window(&mut link, &mut at, 900, 2_400, 8_000), None);
        assert_eq!(
            link_window(&mut link, &mut at, 900, 2_400, 8_000),
            Some(1_200)
        );
        assert_eq!(
            link_window(&mut link, &mut at, 900, 1_200, 8_000),
            Some(600)
        );
        assert_eq!(
            link_window(&mut link, &mut at, 900, 600, 8_000),
            Some(ABR_MIN_BITRATE_KBPS)
        );
        assert_eq!(
            link_window(&mut link, &mut at, 900, 300, 8_000),
            None,
            "on the floor"
        );
        assert_eq!(link.cap_kbps(), Some(ABR_MIN_BITRATE_KBPS));
    }

    /// ADR 0144: a link that carries nothing at all goes straight to the
    /// floor.
    #[test]
    fn a_frozen_link_goes_straight_to_the_floor() {
        let mut link = LinkPressure::default();
        let mut at = Instant::now();
        link_window(&mut link, &mut at, 1_000, 0, 8_000);
        assert_eq!(
            link_window(&mut link, &mut at, 1_000, 0, 8_000),
            Some(ABR_MIN_BITRATE_KBPS)
        );
    }

    /// ADR 0144: two clear seconds, then a step up every clear second, until
    /// the preset's own bitrate is back and the cap is gone; a middling
    /// second holds where it is.
    #[test]
    fn a_clear_link_gets_its_bitrate_back_a_step_a_second() {
        let mut link = LinkPressure::default();
        let mut at = Instant::now();
        link_window(&mut link, &mut at, 1_000, 0, 8_000);
        link_window(&mut link, &mut at, 1_000, 0, 8_000);
        assert_eq!(link.cap_kbps(), Some(300));

        assert_eq!(link_window(&mut link, &mut at, 0, 300, 8_000), None);
        assert_eq!(link_window(&mut link, &mut at, 0, 300, 8_000), Some(450));
        assert_eq!(
            link_window(&mut link, &mut at, 300, 450, 8_000),
            None,
            "neither pressed nor clear"
        );
        assert_eq!(link_window(&mut link, &mut at, 0, 450, 8_000), None);
        assert_eq!(link_window(&mut link, &mut at, 0, 450, 8_000), Some(675));
        let mut steps = 0;
        while link.cap_kbps().is_some() {
            link_window(&mut link, &mut at, 0, 1_000, 8_000);
            steps += 1;
            assert!(steps < 20, "the cap never went away");
        }
        assert!(!link.release(), "nothing left to release");
    }

    /// ADR 0144: releasing says whether there was a cap, and forgets it.
    #[test]
    fn releasing_the_link_cap_forgets_it() {
        let mut link = LinkPressure::default();
        let mut at = Instant::now();
        link_window(&mut link, &mut at, 1_000, 0, 8_000);
        link_window(&mut link, &mut at, 1_000, 0, 8_000);
        assert!(link.release());
        assert_eq!(link.cap_kbps(), None);
        assert!(!link.release());
    }
}
