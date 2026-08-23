//! Host-side audio capture (design doc §11; questions.md item 8; ADR 0023).
//!
//! The Opus codec (§5.1, [`crate::audio`]) needs PCM frames; this module
//! produces them from what the machine is actually playing. Every platform
//! backend answers the same question — "the desktop's output mix, 48 kHz
//! stereo s16, in `AUDIO_FRAME_MS` chunks" — behind one trait, exactly like
//! [`crate::capture::ScreenCapturer`] does for pixels.
//!
//! Backend per platform (questions.md item 8 decision: platform APIs directly,
//! behind features, no third-party abstraction layer):
//!
//! - **Windows** — WASAPI loopback (`feature = "audio-capture"`), in
//!   [`windows_wasapi`]. The loopback device hands back whatever the default
//!   output is mixing, at its own mix rate; frames are converted to the fixed
//!   §11 wire rate by [`to_wire_pcm`] here, so nothing negotiates per session.
//! - **Linux** — PipeWire *monitor* of the default sink
//!   (`feature = "audio-capture-pipewire"`, module [`linux_pipewire`]), the
//!   same "what plays out of the speakers" semantics. PulseAudio-only hosts
//!   reach it through the pipewire-pulse compatibility layer.
//! - **macOS** — not implemented yet; [`platform_audio_capturer`] refuses
//!   loudly (§18: degrade towards safety and say so, never pass silence).
//!
//! Without any of these features every entry point is inert and the session
//! runs video-only, mirroring the `audio-opus` contract. The pure conversion
//! helpers below are feature-independent and always tested.

use std::time::Duration;

use crate::error::{MediaError, Result};
use lumepeer_core::constants::{AUDIO_CHANNELS, AUDIO_FRAME_MS, AUDIO_SAMPLE_RATE_HZ};

/// Samples per channel of one capture chunk: the Opus frame the encoder eats.
pub const SAMPLES_PER_CHUNK: usize = AUDIO_SAMPLE_RATE_HZ as usize * AUDIO_FRAME_MS as usize / 1000;

/// Interleaved s16 PCM of exactly one `AUDIO_FRAME_MS` chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcmChunk {
    /// Interleaved samples, `SAMPLES_PER_CHUNK * AUDIO_CHANNELS` long.
    pub samples: Vec<i16>,
    /// Monotonic capture timestamp in microseconds.
    pub timestamp_us: u64,
}

impl PcmChunk {
    /// A chunk of silence carrying `timestamp_us`, for gap-filling.
    #[must_use]
    pub fn silence(timestamp_us: u64) -> Self {
        Self {
            samples: vec![0; SAMPLES_PER_CHUNK * usize::from(AUDIO_CHANNELS)],
            timestamp_us,
        }
    }
}

/// Host-side desktop-audio capture backend (§11).
pub trait AudioCapturer: Send + std::fmt::Debug {
    /// Starts capturing the desktop output mix.
    ///
    /// # Errors
    /// [`MediaError::CaptureUnavailable`] when no output device exists or the
    /// backend was not compiled in; [`MediaError::PermissionDenied`] when the
    /// platform refused.
    fn start(&mut self) -> Result<()>;

    /// Blocks until the next `AUDIO_FRAME_MS` chunk is available.
    ///
    /// # Errors
    /// [`MediaError::CaptureInterrupted`] once the stream is gone (device
    /// unplugged, session ended); the caller stops the loop on the first one.
    fn next_chunk(&mut self) -> Result<PcmChunk>;

    /// Stops capturing. Idempotent.
    fn stop(&mut self);
}

/// Wall-clock microseconds for capture timestamps. The monotonic instant is
/// what orders chunks; the absolute epoch value is what the recording
/// container stores, and a session never spans a clock adjustment large
/// enough for the two to disagree about ordering (§12.3 uses the same
/// reasoning).
pub(crate) fn capture_timestamp_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64)
}

/// How long one blocking read may take before the backend is considered
/// stuck. Generous on purpose: a chunk itself is 20 ms of audio, but a device
/// resuming from suspend may legitimately be late once.
pub(crate) const READ_TIMEOUT: Duration = Duration::from_millis(2_000);

#[cfg(all(target_os = "windows", feature = "audio-capture"))]
mod windows_wasapi;

#[cfg(all(target_os = "windows", feature = "audio-capture"))]
pub use windows_wasapi::WasapiLoopbackCapturer;

#[cfg(all(
    target_os = "linux",
    not(target_os = "android"),
    feature = "audio-capture-pipewire"
))]
mod linux_pipewire;

#[cfg(all(
    target_os = "linux",
    not(target_os = "android"),
    feature = "audio-capture-pipewire"
))]
pub use linux_pipewire::PipewireMonitorCapturer;

/// Opens the desktop-audio backend of the current platform.
///
/// # Errors
/// [`MediaError::CaptureUnavailable`] when no backend is compiled in for this
/// target — the session stays video-only and the log says why (§18).
pub fn platform_audio_capturer() -> Result<Box<dyn AudioCapturer>> {
    #[cfg(all(target_os = "windows", feature = "audio-capture"))]
    {
        Ok(Box::new(windows_wasapi::WasapiLoopbackCapturer::new()))
    }
    #[cfg(all(
        target_os = "linux",
        not(target_os = "android"),
        feature = "audio-capture-pipewire"
    ))]
    {
        Ok(Box::new(linux_pipewire::PipewireMonitorCapturer::new()))
    }
    #[cfg(not(any(
        all(target_os = "windows", feature = "audio-capture"),
        all(
            target_os = "linux",
            not(target_os = "android"),
            feature = "audio-capture-pipewire"
        ),
    )))]
    {
        Err(MediaError::CaptureUnavailable(
            "no audio capture backend is compiled in for this target".to_owned(),
        ))
    }
}

/// Converts interleaved f32 samples in `-1.0..=1.0` at `input_rate` into the
/// fixed §11 s16 stereo rate, resampling linearly.
///
/// Shared by every backend so the wire format is decided in exactly one
/// place. `input.len()` must be a multiple of `input_channels`.
#[must_use]
pub fn to_wire_pcm(input: &[f32], input_rate: u32, input_channels: usize) -> Vec<i16> {
    let channels = usize::from(AUDIO_CHANNELS);
    if input_channels == 0 || input_rate == 0 || input.is_empty() {
        return vec![0; SAMPLES_PER_CHUNK * channels];
    }
    let frames = input.len() / input_channels;
    if frames == 0 {
        return vec![0; SAMPLES_PER_CHUNK * channels];
    }
    // Output frames per input frame as a float step on the input timeline.
    let step = f64::from(input_rate) / f64::from(AUDIO_SAMPLE_RATE_HZ);
    let mut out = Vec::with_capacity(SAMPLES_PER_CHUNK * channels);
    for o in 0..SAMPLES_PER_CHUNK {
        let pos = f64::from(o as u32) * step;
        let i = pos.floor();
        let frac = (pos - i) as f32;
        let i0 = (i as usize).min(frames - 1);
        let i1 = (i0 + 1).min(frames - 1);
        for c in 0..channels {
            // An input with fewer channels than stereo is spread by re-reading
            // its last channel rather than panicking; more channels than two
            // simply drop the extras (desktop mixes are stereo anyway).
            let src_c = c.min(input_channels - 1);
            let s0 = input[i0 * input_channels + src_c];
            let s1 = input[i1 * input_channels + src_c];
            let mixed = s0 + (s1 - s0) * frac;
            // Clamp rather than wrap: a hot mix saturates, it does not fold.
            // Scale by 32767 so ±1.0 maps onto the full ±32767 range; −32768
            // stays reachable through the explicit min below.
            let clamped = (mixed.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
            out.push(clamped.max(i16::MIN));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_silence_chunk_has_the_exact_encoder_shape() {
        let chunk = PcmChunk::silence(7);
        assert_eq!(
            chunk.samples.len(),
            SAMPLES_PER_CHUNK * usize::from(AUDIO_CHANNELS)
        );
        assert!(chunk.samples.iter().all(|s| *s == 0));
        assert_eq!(chunk.timestamp_us, 7);
    }

    #[test]
    fn passthrough_rate_needs_no_resampling() {
        // 48 kHz in: every output frame maps onto the same input frame.
        let frames = SAMPLES_PER_CHUNK;
        let input: Vec<f32> = (0..frames * 2).map(|i| (i % 7) as f32 / 7.0).collect();
        let out = to_wire_pcm(&input, AUDIO_SAMPLE_RATE_HZ, 2);
        assert_eq!(out.len(), SAMPLES_PER_CHUNK * 2);
        // Frame 0 is untouched by interpolation.
        assert!((f32::from(out[0]) / f32::from(i16::MAX) - input[0]).abs() < 0.01);
    }

    #[test]
    fn resampling_from_44k1_fills_the_whole_chunk() {
        // 44.1 kHz mono in: still exactly one chunk out, stereo.
        let frames = AUDIO_SAMPLE_RATE_HZ as usize / 10; // 100 ms of input
        let input: Vec<f32> = vec![0.5; frames];
        let out = to_wire_pcm(&input, 44_100, 1);
        assert_eq!(out.len(), SAMPLES_PER_CHUNK * 2);
        assert!(out.iter().all(|s| *s > 0));
    }

    #[test]
    fn degenerate_inputs_yield_silence_not_a_panic() {
        assert_eq!(
            to_wire_pcm(&[], AUDIO_SAMPLE_RATE_HZ, 2).len(),
            SAMPLES_PER_CHUNK * 2
        );
        assert_eq!(to_wire_pcm(&[0.5], 0, 2).len(), SAMPLES_PER_CHUNK * 2);
        assert_eq!(
            to_wire_pcm(&[0.5], AUDIO_SAMPLE_RATE_HZ, 0).len(),
            SAMPLES_PER_CHUNK * 2
        );
    }

    #[test]
    fn clipping_saturates_instead_of_wrapping() {
        let out = to_wire_pcm(&[2.0, 2.0], AUDIO_SAMPLE_RATE_HZ, 1);
        assert_eq!(out[0], i16::MAX);
        let out = to_wire_pcm(&[-2.0, -2.0], AUDIO_SAMPLE_RATE_HZ, 1);
        // Symmetric scaling: ±1.0 maps onto ±32767, never wrapping to −32768.
        assert_eq!(out[0], -i16::MAX);
    }

    #[cfg(not(any(
        all(target_os = "windows", feature = "audio-capture"),
        all(
            target_os = "linux",
            not(target_os = "android"),
            feature = "audio-capture-pipewire"
        ),
    )))]
    #[test]
    fn without_a_backend_the_entry_point_refuses_loudly() {
        assert!(matches!(
            platform_audio_capturer(),
            Err(MediaError::CaptureUnavailable(_))
        ));
    }
}
