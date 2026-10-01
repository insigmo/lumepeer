//! Numbers about the picture pipeline, measured where they happen, for a
//! person working out why a session is slow or soft: the guest's statistics
//! overlay and the host's periodic media log line.
//!
//! Nothing here feeds a decision. The adaptive controller has its own inputs
//! (ADR 0037, ADR 0059); these are what a person reads to check whether those
//! inputs, and the pipeline around them, are telling the truth.
//!
//! The one figure that needs explaining is the guest's *queue delay*. The two
//! machines share no clock, so the age of a frame on arrival cannot be read
//! directly. What can be read is how much later than usual it arrived: every
//! frame carries the host's capture timestamp, and `arrival - capture` is the
//! true delay plus a constant clock offset. The lowest such value seen
//! recently is the best case the link has delivered, and anything above it is
//! time the frame spent waiting somewhere — in the encoder, in a send buffer,
//! or in a router. That excess is the number that grows when a link is asked
//! to carry more than it can.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use lumepeer_net::PathSnapshot;

/// The span the per-second figures are measured over.
const RATE_WINDOW: Duration = Duration::from_secs(1);

/// One bucket of the delay floor. The floor is the lowest delay over the last
/// one to two buckets, so a floor set by a lucky frame, or by a clock that
/// drifted since, ages out rather than sticking for the whole session.
const FLOOR_BUCKET: Duration = Duration::from_secs(10);

/// How often the host's encode loop summarises itself into the log.
pub const ENCODE_STATS_PERIOD: Duration = Duration::from_secs(5);

const MICROS_PER_MILLI: u64 = 1_000;
const BITS_PER_BYTE: u64 = 8;

/// One frame as it arrived at the guest.
#[derive(Debug, Clone, Copy)]
struct Arrival {
    at: Instant,
    bytes: usize,
    keyframe: bool,
    /// `arrival - capture`, in microseconds, including the unknown offset
    /// between the two machines' clocks.
    delay_us: i64,
}

/// The lowest delay seen over the last one to two [`FLOOR_BUCKET`]s.
#[derive(Debug, Clone, Copy)]
struct DelayFloor {
    current: Option<i64>,
    previous: Option<i64>,
    bucket_started: Instant,
}

impl DelayFloor {
    const fn new(now: Instant) -> Self {
        Self {
            current: None,
            previous: None,
            bucket_started: now,
        }
    }

    fn note(&mut self, delay_us: i64, now: Instant) {
        if now.saturating_duration_since(self.bucket_started) >= FLOOR_BUCKET {
            self.previous = self.current;
            self.current = None;
            self.bucket_started = now;
        }
        self.current = Some(self.current.map_or(delay_us, |floor| floor.min(delay_us)));
    }

    fn get(&self) -> Option<i64> {
        match (self.current, self.previous) {
            (Some(current), Some(previous)) => Some(current.min(previous)),
            (current, previous) => current.or(previous),
        }
    }
}

/// Guest side: what arrived on one view's media stream.
#[derive(Debug)]
pub struct ArrivalStats {
    /// Origin of the guest-side half of every delay; any fixed instant will
    /// do, since only differences between delays are ever reported.
    epoch: Instant,
    recent: VecDeque<Arrival>,
    last_timestamp_us: Option<u64>,
    floor: DelayFloor,
    frames: u64,
    keyframes: u64,
    path: Option<PathSnapshot>,
}

impl Default for ArrivalStats {
    fn default() -> Self {
        Self::new(Instant::now())
    }
}

impl ArrivalStats {
    /// Nothing measured yet.
    #[must_use]
    pub fn new(now: Instant) -> Self {
        Self {
            epoch: now,
            recent: VecDeque::new(),
            last_timestamp_us: None,
            floor: DelayFloor::new(now),
            frames: 0,
            keyframes: 0,
            path: None,
        }
    }

    /// One frame arrived at `now`, captured on the host at `timestamp_us`.
    pub fn record(&mut self, now: Instant, timestamp_us: u64, bytes: usize, keyframe: bool) {
        // The host's capture clock restarts whenever capture reopens (a
        // secure-desktop switch, a monitor change). A floor measured against
        // the old clock says nothing about the new one.
        if self
            .last_timestamp_us
            .is_some_and(|last| timestamp_us < last)
        {
            self.floor = DelayFloor::new(now);
        }
        self.last_timestamp_us = Some(timestamp_us);

        let arrived_us = i64::try_from(now.saturating_duration_since(self.epoch).as_micros())
            .unwrap_or(i64::MAX);
        let delay_us = arrived_us.saturating_sub(i64::try_from(timestamp_us).unwrap_or(i64::MAX));
        self.floor.note(delay_us, now);

        self.frames = self.frames.saturating_add(1);
        if keyframe {
            self.keyframes = self.keyframes.saturating_add(1);
        }
        self.recent.push_back(Arrival {
            at: now,
            bytes,
            keyframe,
            delay_us,
        });
        self.prune(now);
    }

    /// The latest reading of the media connection's QUIC path.
    pub const fn set_path(&mut self, path: Option<PathSnapshot>) {
        self.path = path;
    }

    /// Everything measured, as of `now`.
    pub fn snapshot(&mut self, now: Instant) -> MediaStatsSnapshot {
        self.prune(now);
        let count = self.recent.len();
        let bytes: usize = self.recent.iter().map(|arrival| arrival.bytes).sum();
        let largest = self
            .recent
            .iter()
            .map(|arrival| arrival.bytes)
            .max()
            .unwrap_or(0);
        let floor = self.floor.get();
        let excess_ms = |arrival: &Arrival| -> Option<u32> {
            let excess = arrival.delay_us.saturating_sub(floor?).max(0);
            u32::try_from(excess.unsigned_abs() / MICROS_PER_MILLI).ok()
        };
        let window_ms = u64::try_from(RATE_WINDOW.as_millis()).unwrap_or(u64::MAX);
        MediaStatsSnapshot {
            fps: saturate(count as u64),
            kbps: saturate((bytes as u64).saturating_mul(BITS_PER_BYTE) / window_ms),
            frame_bytes_avg: saturate(bytes.checked_div(count).unwrap_or(0) as u64),
            frame_bytes_max: saturate(largest as u64),
            keyframes_recent: saturate(
                self.recent
                    .iter()
                    .filter(|arrival| arrival.keyframe)
                    .count() as u64,
            ),
            frames: self.frames,
            keyframes: self.keyframes,
            queue_ms: self.recent.back().and_then(excess_ms),
            queue_max_ms: self.recent.iter().filter_map(excess_ms).max(),
            rtt_ms: self
                .path
                .map(|path| saturate(u64::try_from(path.rtt.as_millis()).unwrap_or(u64::MAX))),
            cwnd_bytes: self.path.map(|path| path.cwnd),
            lost_packets: self.path.map(|path| path.lost_packets),
            congestion_events: self.path.map(|path| path.congestion_events),
            relay: self.path.map(|path| path.relay),
        }
    }

    fn prune(&mut self, now: Instant) {
        while self
            .recent
            .front()
            .is_some_and(|arrival| now.saturating_duration_since(arrival.at) > RATE_WINDOW)
        {
            self.recent.pop_front();
        }
    }
}

/// One reading of [`ArrivalStats`], as the view window's overlay shows it.
///
/// Every per-second figure covers the last second; a field that nothing has
/// measured yet is `None`, never a zero pretending to be a reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct MediaStatsSnapshot {
    /// Frames that arrived.
    pub fps: u32,
    /// Kilobits per second that arrived.
    pub kbps: u32,
    /// Mean encoded frame size, bytes.
    pub frame_bytes_avg: u32,
    /// Largest encoded frame, bytes.
    pub frame_bytes_max: u32,
    /// Intra frames among them.
    pub keyframes_recent: u32,
    /// Frames since this media stream started.
    pub frames: u64,
    /// Intra frames since this media stream started.
    pub keyframes: u64,
    /// How much later than the best recent case the newest frame arrived.
    pub queue_ms: Option<u32>,
    /// The same, for the worst frame of the last second.
    pub queue_max_ms: Option<u32>,
    /// QUIC's round trip on the media connection.
    pub rtt_ms: Option<u32>,
    /// Congestion window of the media connection, bytes.
    pub cwnd_bytes: Option<u64>,
    /// Packets the media connection has lost.
    pub lost_packets: Option<u64>,
    /// Times the media connection's congestion controller backed off.
    pub congestion_events: Option<u64>,
    /// Whether the media connection goes through a relay.
    pub relay: Option<bool>,
}

fn saturate(value: u64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// Host side: time spent in one stage of the encode loop.
#[derive(Debug, Clone, Copy)]
struct Stage {
    total: Duration,
    max: Duration,
    count: u32,
}

impl Stage {
    const EMPTY: Self = Self {
        total: Duration::ZERO,
        max: Duration::ZERO,
        count: 0,
    };

    fn note(&mut self, took: Duration) {
        self.total = self.total.saturating_add(took);
        self.max = self.max.max(took);
        self.count = self.count.saturating_add(1);
    }

    fn avg_ms(&self) -> f32 {
        if self.count == 0 {
            return 0.0;
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "a log figure; a period's total is far inside f32's exact range"
        )]
        let avg = self.total.as_secs_f32() * 1_000.0 / self.count as f32;
        avg
    }

    fn max_ms(&self) -> f32 {
        self.max.as_secs_f32() * 1_000.0
    }
}

/// Host side: one [`ENCODE_STATS_PERIOD`] of the encode loop.
#[derive(Debug)]
pub struct EncodeStats {
    started: Instant,
    frames: u32,
    keyframes: u32,
    skipped: u32,
    bytes: u64,
    capture: Stage,
    scale: Stage,
    encode: Stage,
    width: u32,
    height: u32,
}

impl EncodeStats {
    /// Starts a period at `now`.
    #[must_use]
    pub const fn new(now: Instant) -> Self {
        Self {
            started: now,
            frames: 0,
            keyframes: 0,
            skipped: 0,
            bytes: 0,
            capture: Stage::EMPTY,
            scale: Stage::EMPTY,
            encode: Stage::EMPTY,
            width: 0,
            height: 0,
        }
    }

    /// A tick the link would not take, so nothing was captured.
    pub const fn skipped(&mut self) {
        self.skipped = self.skipped.saturating_add(1);
    }

    /// Capture produced a picture, after `took` (which includes waiting for
    /// the screen to change).
    pub fn captured(&mut self, took: Duration) {
        self.capture.note(took);
    }

    /// One picture was reduced and encoded into `bytes`.
    pub fn encoded(
        &mut self,
        scale_took: Duration,
        encode_took: Duration,
        size: (u32, u32),
        bytes: usize,
        keyframe: bool,
    ) {
        self.scale.note(scale_took);
        self.encode.note(encode_took);
        (self.width, self.height) = size;
        self.frames = self.frames.saturating_add(1);
        if keyframe {
            self.keyframes = self.keyframes.saturating_add(1);
        }
        self.bytes = self.bytes.saturating_add(bytes as u64);
    }

    /// The summary of the period, if it has run its course; starts the next
    /// one in the same call.
    pub fn due(&mut self, now: Instant) -> Option<EncodeReport> {
        let elapsed = now.saturating_duration_since(self.started);
        if elapsed < ENCODE_STATS_PERIOD {
            return None;
        }
        let millis = u64::try_from(elapsed.as_millis())
            .unwrap_or(u64::MAX)
            .max(1);
        let report = EncodeReport {
            fps: self.frames.saturating_mul(1_000) / saturate(millis).max(1),
            kbps: saturate(self.bytes.saturating_mul(BITS_PER_BYTE) / millis),
            frames: self.frames,
            keyframes: self.keyframes,
            skipped: self.skipped,
            width: self.width,
            height: self.height,
            capture_ms_avg: self.capture.avg_ms(),
            capture_ms_max: self.capture.max_ms(),
            scale_ms_avg: self.scale.avg_ms(),
            scale_ms_max: self.scale.max_ms(),
            encode_ms_avg: self.encode.avg_ms(),
            encode_ms_max: self.encode.max_ms(),
        };
        *self = Self::new(now);
        Some(report)
    }
}

/// What the host's encode loop did over one [`ENCODE_STATS_PERIOD`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EncodeReport {
    /// Frames encoded per second.
    pub fps: u32,
    /// Kilobits per second handed to the media stream.
    pub kbps: u32,
    /// Frames encoded.
    pub frames: u32,
    /// Intra frames among them.
    pub keyframes: u32,
    /// Ticks skipped because the link was still busy with the previous frame.
    pub skipped: u32,
    /// Size of the last picture handed to the encoder.
    pub width: u32,
    /// See [`Self::width`].
    pub height: u32,
    /// Capture, including the wait for the screen to change.
    pub capture_ms_avg: f32,
    /// See [`Self::capture_ms_avg`].
    pub capture_ms_max: f32,
    /// Downscaling on the CPU (zero when nothing needed reducing).
    pub scale_ms_avg: f32,
    /// See [`Self::scale_ms_avg`].
    pub scale_ms_max: f32,
    /// The encoder call itself.
    pub encode_ms_avg: f32,
    /// See [`Self::encode_ms_avg`].
    pub encode_ms_max: f32,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failed assumption must fail the test")]

    use super::*;

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    /// Frames captured every 33 ms and arriving a constant 40 ms later: the
    /// link is keeping up, and nothing is queued.
    #[test]
    fn a_steady_stream_reports_its_rate_and_no_queue() {
        let start = Instant::now();
        let mut stats = ArrivalStats::new(start);
        for i in 0..30u64 {
            let captured = i * 33_000;
            stats.record(start + ms(40) + ms(i * 33), captured, 10_000, i == 0);
        }
        let now = start + ms(40) + ms(29 * 33);
        let snapshot = stats.snapshot(now);
        assert_eq!(snapshot.fps, 30);
        assert_eq!(snapshot.frame_bytes_avg, 10_000);
        assert_eq!(snapshot.kbps, 30 * 10_000 * 8 / 1_000);
        assert_eq!(snapshot.queue_ms, Some(0));
        assert_eq!(snapshot.queue_max_ms, Some(0));
        assert_eq!(snapshot.keyframes, 1);
    }

    /// The failure this overlay exists to show: a link asked for more than it
    /// carries, so every frame waits a little longer than the one before.
    #[test]
    fn a_growing_backlog_shows_up_as_queue_delay() {
        let start = Instant::now();
        let mut stats = ArrivalStats::new(start);
        let mut now = start;
        for i in 0..30u64 {
            // Captured every 33 ms, but delivered every 43 ms.
            now = start + ms(40) + ms(i * 43);
            stats.record(now, i * 33_000, 20_000, false);
        }
        let snapshot = stats.snapshot(now);
        // 29 frames later, the newest has fallen 29 * 10 ms behind.
        assert_eq!(snapshot.queue_ms, Some(290));
        assert_eq!(snapshot.queue_max_ms, Some(290));
    }

    /// A still screen sends nothing, and the per-second figures must say
    /// "nothing arrived", not repeat the last busy second.
    #[test]
    fn a_quiet_second_reports_no_frames_and_no_queue() {
        let start = Instant::now();
        let mut stats = ArrivalStats::new(start);
        stats.record(start, 0, 5_000, true);
        let snapshot = stats.snapshot(start + ms(1_500));
        assert_eq!(snapshot.fps, 0);
        assert_eq!(snapshot.kbps, 0);
        assert_eq!(snapshot.queue_ms, None);
        assert_eq!(snapshot.frames, 1, "the totals survive the quiet");
    }

    /// Capture reopened on the host and its clock started again from zero:
    /// measured against the old floor, every later frame would look minutes
    /// late.
    #[test]
    fn a_capture_clock_that_restarts_does_not_read_as_a_queue() {
        let start = Instant::now();
        let mut stats = ArrivalStats::new(start);
        stats.record(start + ms(40), 0, 1_000, true);
        stats.record(start + ms(5_040), 5_000_000, 1_000, false);
        // Capture reopened: the host's timestamps start again near zero.
        stats.record(start + ms(5_100), 20_000, 1_000, true);
        let snapshot = stats.snapshot(start + ms(5_100));
        assert_eq!(snapshot.queue_ms, Some(0));
    }

    /// A lucky early frame must not set the floor for the whole session: two
    /// buckets on, the floor is whatever the link has been doing since.
    #[test]
    fn the_delay_floor_ages_out() {
        let start = Instant::now();
        let mut stats = ArrivalStats::new(start);
        // One frame with a 10 ms delay, then a link that settles at 50 ms.
        stats.record(start + ms(10), 0, 1_000, true);
        let mut now = start;
        for i in 1..=25u64 {
            now = start + ms(i * 1_000) + ms(50);
            stats.record(now, i * 1_000_000, 1_000, false);
        }
        assert_eq!(stats.snapshot(now).queue_ms, Some(0));
    }

    #[test]
    fn the_path_rides_the_snapshot() {
        let start = Instant::now();
        let mut stats = ArrivalStats::new(start);
        assert_eq!(stats.snapshot(start).rtt_ms, None, "unmeasured is None");
        stats.set_path(Some(PathSnapshot {
            rtt: ms(87),
            cwnd: 64_000,
            lost_packets: 3,
            congestion_events: 1,
            relay: true,
        }));
        let snapshot = stats.snapshot(start);
        assert_eq!(snapshot.rtt_ms, Some(87));
        assert_eq!(snapshot.cwnd_bytes, Some(64_000));
        assert_eq!(snapshot.lost_packets, Some(3));
        assert_eq!(snapshot.relay, Some(true));
    }

    #[test]
    fn the_encode_summary_waits_for_its_period_and_then_starts_over() {
        let start = Instant::now();
        let mut stats = EncodeStats::new(start);
        for i in 0..150u32 {
            stats.captured(ms(20));
            stats.encoded(
                ms(0),
                ms(8 + u64::from(i % 3)),
                (1920, 1080),
                25_000,
                i == 0,
            );
        }
        stats.skipped();
        assert!(stats.due(start + ms(4_999)).is_none());

        let report = stats.due(start + ENCODE_STATS_PERIOD).unwrap();
        assert_eq!(report.fps, 30);
        assert_eq!(report.kbps, 150 * 25_000 * 8 / 5_000);
        assert_eq!(report.keyframes, 1);
        assert_eq!(report.skipped, 1);
        assert_eq!((report.width, report.height), (1920, 1080));
        assert!((report.encode_ms_avg - 9.0).abs() < 0.01);
        assert!((report.encode_ms_max - 10.0).abs() < 0.01);

        let next = stats.due(start + ENCODE_STATS_PERIOD * 2).unwrap();
        assert_eq!(next.frames, 0, "a new period starts empty");
    }
}
