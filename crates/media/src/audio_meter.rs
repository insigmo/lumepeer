//! What one direction of session audio actually carried (§11, §18; ADR 0147).
//!
//! Every stage of an audio path can be fine on its own and the person at the
//! end still hear nothing: a capture that only ever reads silence, a stream
//! that never arrives, a speaker that refuses to open. A log line per stage
//! says *that* something ran; this says what went through it — how many
//! chunks, whether any of them was more than digital silence, how loud the
//! loudest was and what tone it held, and whether the device at the far end
//! took them. The statistics overlay and the e2e matrix read it through
//! `connection_stats`; nothing decides anything from it.
//!
//! Lock-free on the audio path: every counter is an atomic, and the one
//! string (the device's last error) changes only when a device opens or
//! fails.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Peak at or above which a chunk counts as audible: −30 dBFS of `i16`.
///
/// Well above a microphone's noise floor and Opus's own concealment, well
/// below anything a person would call sound.
pub const LOUD_PEAK: u16 = 1_036;

/// Whether the device a meter's stream plays on, or captures from, is open.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum DeviceState {
    /// Nothing has tried to open it yet.
    #[default]
    Unopened,
    /// It opened and is taking (or giving) audio.
    Open,
    /// The last attempt failed, and why.
    Failed(String),
}

/// One reading of an [`AudioMeter`]: every counter since the meter was made.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioMeterSnapshot {
    /// Audio streams opened or accepted.
    pub streams: u32,
    /// Chunks captured, or received and decoded.
    pub chunks: u64,
    /// Chunks with at least one sample that is not zero: a muted or blocked
    /// microphone, and a silent desktop, deliver exact zeros.
    pub nonzero: u64,
    /// Chunks whose peak reached [`LOUD_PEAK`].
    pub loud: u64,
    /// Chunks the playback device accepted (receiving side only).
    pub played: u64,
    /// Chunks dropped before the device, because it was behind.
    pub dropped: u64,
    /// Loudest sample magnitude seen.
    pub peak: u16,
    /// Tone of the latest loud chunk, when it held a steady one.
    pub loud_hz: Option<u32>,
    /// The device at this end.
    pub device: DeviceState,
}

/// Counters for one direction of one session's audio.
#[derive(Debug, Default)]
pub struct AudioMeter {
    streams: AtomicU32,
    chunks: AtomicU64,
    nonzero: AtomicU64,
    loud: AtomicU64,
    played: AtomicU64,
    dropped: AtomicU64,
    peak: AtomicU32,
    loud_hz: AtomicU32,
    device: Mutex<DeviceState>,
}

impl AudioMeter {
    /// A stream was opened (sending side) or accepted (receiving side).
    pub fn stream_opened(&self) {
        self.streams.fetch_add(1, Ordering::Relaxed);
    }

    /// One wire-format chunk (interleaved, `channels` per frame, at
    /// `rate`): what was captured, or what was decoded.
    pub fn observe(&self, samples: &[i16], channels: usize, rate: u32) {
        self.chunks.fetch_add(1, Ordering::Relaxed);
        let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        if peak == 0 {
            return;
        }
        self.nonzero.fetch_add(1, Ordering::Relaxed);
        self.peak.fetch_max(u32::from(peak), Ordering::Relaxed);
        if peak >= LOUD_PEAK {
            self.loud.fetch_add(1, Ordering::Relaxed);
            self.loud_hz.store(
                tone_hz(samples, channels, rate).unwrap_or(0),
                Ordering::Relaxed,
            );
        }
    }

    /// The playback device accepted one chunk.
    pub fn played(&self) {
        self.played.fetch_add(1, Ordering::Relaxed);
    }

    /// One chunk was dropped before reaching the device.
    pub fn dropped(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// The device opened.
    pub fn device_open(&self) {
        *self
            .device
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = DeviceState::Open;
    }

    /// The device failed to open, or stopped, with `error`.
    pub fn device_failed(&self, error: &str) {
        *self
            .device
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            DeviceState::Failed(error.to_owned());
    }

    /// Every counter, as of now.
    #[must_use]
    pub fn snapshot(&self) -> AudioMeterSnapshot {
        let hz = self.loud_hz.load(Ordering::Relaxed);
        AudioMeterSnapshot {
            streams: self.streams.load(Ordering::Relaxed),
            chunks: self.chunks.load(Ordering::Relaxed),
            nonzero: self.nonzero.load(Ordering::Relaxed),
            loud: self.loud.load(Ordering::Relaxed),
            played: self.played.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            peak: u16::try_from(self.peak.load(Ordering::Relaxed)).unwrap_or(u16::MAX),
            loud_hz: (hz > 0).then_some(hz),
            device: self
                .device
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        }
    }
}

/// The steady tone in one chunk's first channel, by the spacing of its
/// upward zero crossings; `None` when fewer than two full cycles fit.
///
/// Crossings are counted with hysteresis at a quarter of [`LOUD_PEAK`], so a
/// noise floor riding on a tone does not add crossings of its own. Measured
/// between the first and the last crossing rather than over the whole chunk,
/// which keeps a 20 ms chunk accurate to well under a percent.
#[must_use]
pub fn tone_hz(samples: &[i16], channels: usize, rate: u32) -> Option<u32> {
    let channels = channels.max(1);
    let hysteresis = i32::from(LOUD_PEAK / 4);
    let mut below = false;
    let mut first = None;
    let mut last = 0usize;
    let mut cycles = 0u32;
    for (frame, sample) in samples.iter().step_by(channels).enumerate() {
        let sample = i32::from(*sample);
        if sample < -hysteresis {
            below = true;
        } else if sample > hysteresis && below {
            below = false;
            match first {
                None => first = Some(frame),
                Some(_) => cycles += 1,
            }
            last = frame;
        }
    }
    let span = last.checked_sub(first?)?;
    if cycles < 2 || span == 0 {
        return None;
    }
    let span = u64::try_from(span).ok()?;
    u32::try_from(u64::from(cycles) * u64::from(rate) / span).ok()
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        reason = "a failed assumption must fail the test; test signals are tiny and bounded"
    )]

    use super::*;

    const RATE: u32 = 48_000;

    fn tone(hz: f32, amplitude: f32, frames: usize) -> Vec<i16> {
        (0..frames)
            .flat_map(|i| {
                let s = (amplitude
                    * (2.0 * std::f32::consts::PI * hz * i as f32 / RATE as f32).sin())
                    as i16;
                [s, s]
            })
            .collect()
    }

    #[test]
    fn a_steady_tone_is_measured_to_within_a_percent() {
        for hz in [440.0, 880.0, 1_000.0, 2_500.0] {
            let measured = tone_hz(&tone(hz, 8_000.0, 960), 2, RATE).unwrap();
            let error = (measured as f32 - hz).abs() / hz;
            assert!(error < 0.01, "{hz} Hz measured as {measured}");
        }
    }

    #[test]
    fn silence_and_noise_floor_hold_no_tone() {
        assert_eq!(tone_hz(&vec![0; 1920], 2, RATE), None);
        // A microphone's floor: a few LSBs either way.
        let floor: Vec<i16> = (0..1920).map(|i| [3, -2, 4, -5][i % 4]).collect();
        assert_eq!(tone_hz(&floor, 2, RATE), None);
    }

    #[test]
    fn the_meter_tells_digital_silence_from_a_floor_and_a_floor_from_sound() {
        let meter = AudioMeter::default();
        meter.observe(&vec![0; 1920], 2, RATE);
        meter.observe(&[3, -2, 4, -5].repeat(480), 2, RATE);
        meter.observe(&tone(880.0, 8_000.0, 960), 2, RATE);
        let seen = meter.snapshot();
        assert_eq!(seen.chunks, 3);
        assert_eq!(
            seen.nonzero, 2,
            "only the all-zero chunk is digital silence"
        );
        assert_eq!(seen.loud, 1, "only the tone is audible");
        let hz = seen.loud_hz.unwrap();
        assert!((870..=890).contains(&hz), "tone measured as {hz}");
        assert!(seen.peak >= 7_900);
    }

    #[test]
    fn the_device_reports_its_last_word() {
        let meter = AudioMeter::default();
        assert_eq!(meter.snapshot().device, DeviceState::Unopened);
        meter.device_failed("no device");
        assert_eq!(
            meter.snapshot().device,
            DeviceState::Failed("no device".to_owned())
        );
        meter.device_open();
        assert_eq!(meter.snapshot().device, DeviceState::Open);
    }
}
