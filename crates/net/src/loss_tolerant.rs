//! A congestion controller that does not take every lost packet for
//! congestion (ADR 0144).
//!
//! QUIC's Cubic halves — well, takes 30% off — its window for every loss
//! event, which is right on a wire, where a lost packet is a full queue. On
//! the Wi-Fi links this app actually runs over it is often not: a packet is
//! lost to radio noise while the bottleneck has room to spare. Measured on
//! 2026-10-07 between two homes, 260 ms apart and losing about 0.4% of
//! packets: one TCP transfer reached 0.59 Mbit/s, four at once 3.18 Mbit/s
//! between them, and the picture's own QUIC connection sat at the first
//! figure — a film at 5–8 frames a second on a link with room for five times
//! the bits.
//!
//! This wraps Cubic and hands it only the losses that look like a queue: a
//! share of recent bytes above [`TOLERATED_LOSS_PERMILLE`], a single event
//! that lost [`BURST_BYTES`] or more, an ECN mark, or persistent congestion.
//! Sporadic losses are retransmitted like any other — reliability is QUIC's
//! and untouched — but do not shrink the window. The picture's own pacing
//! (ADR 0139) is the guard against the queue this could otherwise build: the
//! host stops producing frames as soon as acknowledgements fall behind the
//! link's best case, whatever the window allows.

use std::sync::Arc;
use std::time::{Duration, Instant};

use noq::congestion::{Controller, ControllerFactory, ControllerMetrics, CubicConfig};
use noq_proto::RttEstimator;

/// Share of the bytes recently sent, in permille, that may be lost before a
/// loss counts as congestion: 2%. Radio noise loses a fraction of a percent;
/// a queue that overflows under a sender probing past it loses more, and in
/// bursts.
const TOLERATED_LOSS_PERMILLE: u64 = 20;

/// Bytes lost in one loss event that count as congestion whatever the share:
/// eight full-size packets at once is a queue overflowing or the path going
/// away, not noise.
const BURST_BYTES: u64 = 8 * 1_200;

/// How long the loss share is measured over before its counts are halved, so
/// the share follows the last few seconds rather than the whole connection.
const SHARE_HALF_LIFE: Duration = Duration::from_secs(1);

/// The least the window is held to, however little has been delivered
/// lately: a keyframe after a still screen has to fit in it.
const WINDOW_FLOOR_BYTES: u64 = 128 * 1_024;

/// Builds a [`LossTolerant`] around Cubic for every connection.
#[derive(Debug, Default)]
pub struct LossTolerantConfig {
    cubic: Arc<CubicConfig>,
}

impl ControllerFactory for LossTolerantConfig {
    fn build(self: Arc<Self>, now: Instant, current_mtu: u16) -> Box<dyn Controller> {
        Box::new(LossTolerant::around(
            Arc::clone(&self.cubic).build(now, current_mtu),
            now,
        ))
    }
}

/// Cubic, told only about the losses that look like congestion.
#[derive(Debug)]
pub struct LossTolerant {
    inner: Box<dyn Controller>,
    /// Bytes acknowledged recently, halved every [`SHARE_HALF_LIFE`].
    acked: u64,
    /// Bytes lost recently, halved with `acked`.
    lost: u64,
    /// When the counts were last halved.
    since: Instant,
    /// The connection's quickest round trip, once one is known.
    min_rtt: Option<Duration>,
}

impl LossTolerant {
    fn around(inner: Box<dyn Controller>, now: Instant) -> Self {
        Self {
            inner,
            acked: 0,
            lost: 0,
            since: now,
            min_rtt: None,
        }
    }

    /// The most the window may be: a few times what the link has delivered
    /// lately over its quickest round trip — the bandwidth-delay product,
    /// with room to grow — and never under [`WINDOW_FLOOR_BYTES`].
    ///
    /// Cubic, no longer shrunk by sporadic loss, grew its window to 10 MB in
    /// a measured session on a link carrying 3–6 Mbit/s, and paced its
    /// bursts by that window: a burst a router could not queue lost hundreds
    /// of packets at once and stalled the picture for five seconds. `acked`
    /// is a sum halved every second, between one and two seconds' worth of
    /// delivery, so twice it over the round trip is two to four times the
    /// product.
    fn ceiling(&self) -> u64 {
        let Some(min_rtt) = self.min_rtt else {
            return u64::MAX;
        };
        let product = u128::from(self.acked) * min_rtt.as_micros() * 2 / 1_000_000;
        u64::try_from(product)
            .unwrap_or(u64::MAX)
            .max(WINDOW_FLOOR_BYTES)
    }

    /// The connection's quickest round trip is `min_rtt`.
    fn note_rtt(&mut self, min_rtt: Duration) {
        if !min_rtt.is_zero() {
            self.min_rtt = Some(min_rtt);
        }
    }

    /// `bytes` were acknowledged at `now`.
    fn note_acked(&mut self, now: Instant, bytes: u64) {
        self.decay(now);
        self.acked = self.acked.saturating_add(bytes);
    }

    fn decay(&mut self, now: Instant) {
        if now.saturating_duration_since(self.since) >= SHARE_HALF_LIFE {
            self.acked /= 2;
            self.lost /= 2;
            self.since = now;
        }
    }

    /// Whether a loss of `lost_bytes` just counted in is congestion.
    fn congested(&self, lost_bytes: u64) -> bool {
        lost_bytes >= BURST_BYTES
            || self.lost.saturating_mul(1_000)
                > self
                    .acked
                    .saturating_add(self.lost)
                    .saturating_mul(TOLERATED_LOSS_PERMILLE)
    }
}

impl Controller for LossTolerant {
    fn on_sent(&mut self, now: Instant, bytes: u64, largest_pn: u64) {
        self.inner.on_sent(now, bytes, largest_pn);
    }

    fn on_packet_sent(&mut self, now: Instant, bytes: u16, pn: u64) {
        self.inner.on_packet_sent(now, bytes, pn);
    }

    fn on_cwnd_limited(&mut self) {
        self.inner.on_cwnd_limited();
    }

    fn on_ack(
        &mut self,
        now: Instant,
        sent: Instant,
        bytes: u64,
        pn: u64,
        app_limited: bool,
        rtt: &RttEstimator,
    ) {
        self.note_acked(now, bytes);
        self.note_rtt(rtt.min());
        self.inner.on_ack(now, sent, bytes, pn, app_limited, rtt);
    }

    fn on_end_acks(
        &mut self,
        now: Instant,
        in_flight: u64,
        app_limited: bool,
        largest_packet_num_acked: Option<u64>,
    ) {
        self.inner
            .on_end_acks(now, in_flight, app_limited, largest_packet_num_acked);
    }

    fn on_congestion_event(
        &mut self,
        now: Instant,
        sent: Instant,
        is_persistent_congestion: bool,
        is_ecn: bool,
        lost_bytes: u64,
        largest_lost_pn: u64,
    ) {
        self.decay(now);
        self.lost = self.lost.saturating_add(lost_bytes);
        if is_persistent_congestion || is_ecn || self.congested(lost_bytes) {
            self.inner.on_congestion_event(
                now,
                sent,
                is_persistent_congestion,
                is_ecn,
                lost_bytes,
                largest_lost_pn,
            );
        }
    }

    fn on_packet_lost(&mut self, lost_bytes: u16, pn: u64, now: Instant) {
        self.inner.on_packet_lost(lost_bytes, pn, now);
    }

    fn on_spurious_congestion_event(&mut self) {
        self.inner.on_spurious_congestion_event();
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.inner.on_mtu_update(new_mtu);
    }

    fn on_ack_frequency_update(
        &mut self,
        ack_eliciting_threshold: u64,
        requested_max_ack_delay: Duration,
    ) {
        self.inner
            .on_ack_frequency_update(ack_eliciting_threshold, requested_max_ack_delay);
    }

    fn window(&self) -> u64 {
        self.inner.window().min(self.ceiling())
    }

    fn metrics(&self) -> ControllerMetrics {
        // The pacer spreads a window over a round trip: it has to be the
        // window this controller actually allows.
        let mut metrics = self.inner.metrics();
        metrics.congestion_window = self.window();
        metrics
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(Self {
            inner: self.inner.clone_box(),
            acked: self.acked,
            lost: self.lost,
            since: self.since,
            min_rtt: self.min_rtt,
        })
    }

    fn initial_window(&self) -> u64 {
        self.inner.initial_window()
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// A controller that only counts the congestion events it is handed.
    #[derive(Debug, Clone, Default)]
    struct Counted(Arc<AtomicUsize>);

    impl Controller for Counted {
        fn on_congestion_event(
            &mut self,
            _: Instant,
            _: Instant,
            _: bool,
            _: bool,
            _: u64,
            _: u64,
        ) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }

        fn on_mtu_update(&mut self, _: u16) {}

        fn window(&self) -> u64 {
            u64::MAX
        }

        fn clone_box(&self) -> Box<dyn Controller> {
            Box::new(self.clone())
        }

        fn initial_window(&self) -> u64 {
            0
        }

        fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
            self
        }
    }

    fn tolerant(now: Instant) -> (LossTolerant, Arc<AtomicUsize>) {
        let counted = Counted::default();
        let events = Arc::clone(&counted.0);
        (LossTolerant::around(Box::new(counted), now), events)
    }

    /// ADR 0144: one packet lost among a few hundred acknowledged is noise,
    /// and Cubic never hears of it.
    #[test]
    fn a_sporadic_loss_is_not_congestion() {
        let now = Instant::now();
        let (mut tolerant, events) = tolerant(now);
        tolerant.note_acked(now, 400 * 1_200);
        tolerant.on_congestion_event(now, now, false, false, 1_200, 500);
        assert_eq!(events.load(Ordering::Relaxed), 0);
    }

    /// ADR 0144: a burst, an ECN mark and persistent congestion are each
    /// passed on however little was lost overall.
    #[test]
    fn a_burst_an_ecn_mark_and_persistent_congestion_are_congestion() {
        let now = Instant::now();
        for (lost_bytes, ecn, persistent) in [
            (BURST_BYTES, false, false),
            (0, true, false),
            (1_200, false, true),
        ] {
            let (mut tolerant, events) = tolerant(now);
            tolerant.note_acked(now, 400 * 1_200);
            tolerant.on_congestion_event(now, now, persistent, ecn, lost_bytes, 500);
            assert_eq!(
                events.load(Ordering::Relaxed),
                1,
                "lost {lost_bytes} ecn {ecn} persistent {persistent} was not passed on"
            );
        }
    }

    /// ADR 0144: single packets, but more than 2% of the recent bytes, are a
    /// queue.
    #[test]
    fn a_share_of_loss_over_the_tolerance_is_congestion() {
        let now = Instant::now();
        let (mut tolerant, events) = tolerant(now);
        tolerant.note_acked(now, 100 * 1_200);
        tolerant.on_congestion_event(now, now, false, false, 1_200, 200);
        tolerant.on_congestion_event(now, now, false, false, 1_200, 201);
        assert_eq!(events.load(Ordering::Relaxed), 0, "2 in 102 is under 2%");
        tolerant.on_congestion_event(now, now, false, false, 1_200, 202);
        assert_eq!(events.load(Ordering::Relaxed), 1, "3 in 103 is over it");
    }

    /// ADR 0144: the share follows the last few seconds, so old loss stops
    /// counting and old acknowledgements stop diluting new loss.
    #[test]
    fn the_share_forgets_what_happened_seconds_ago() {
        let start = Instant::now();
        let (mut tolerant, events) = tolerant(start);
        tolerant.note_acked(start, 10_000 * 1_200);
        let later = start + SHARE_HALF_LIFE * 12;
        for second in 1..=12 {
            tolerant.note_acked(start + SHARE_HALF_LIFE * second, 0);
        }
        tolerant.note_acked(later, 50 * 1_200);
        tolerant.on_congestion_event(later, later, false, false, 1_200, 1);
        tolerant.on_congestion_event(later, later, false, false, 1_200, 2);
        assert_eq!(
            events.load(Ordering::Relaxed),
            1,
            "a minute-old flood of acknowledgements still hid 2 losses in about 50"
        );
    }

    /// ADR 0144: the window stays within a few times what the link delivers
    /// over its round trip, and never under the floor.
    #[test]
    fn the_window_is_held_near_what_the_link_delivers() {
        let now = Instant::now();
        let (mut tolerant, _) = tolerant(now);
        assert_eq!(
            tolerant.window(),
            u64::MAX,
            "nothing measured, nothing held"
        );

        tolerant.note_rtt(Duration::from_millis(250));
        assert_eq!(tolerant.window(), WINDOW_FLOOR_BYTES);

        // About 3 Mbit/s for a second: 375 KB, so a product of 94 KB.
        tolerant.note_acked(now, 375_000);
        assert_eq!(tolerant.window(), 187_500);
    }
}
