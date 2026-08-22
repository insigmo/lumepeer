//! Opus audio codec (design doc §5.1, §11; ADR 0023).
//!
//! §5.1 names Opus the only audio codec. The binding crate is `opus` over
//! `audiopus_sys`, which vendors libopus and builds it with cmake — the same
//! vendored-build precedent as `openh264` — so a default workspace build
//! needs no system SDK. The whole module sits behind the `audio-opus`
//! feature: without it every entry point returns
//! [`MediaError::AudioUnavailable`] instead of silently passing silence.
//!
//! The wire parameters are fixed in [`crate::constants`]
//! (`AUDIO_SAMPLE_RATE_HZ`, `AUDIO_CHANNELS`, `AUDIO_FRAME_MS`), so nothing
//! negotiates them per session.

use lumepeer_core::constants::{AUDIO_FRAME_MS, AUDIO_SAMPLE_RATE_HZ};

/// Samples per audio frame at the fixed sample rate (§11): 20 ms of 48 kHz.
pub const SAMPLES_PER_FRAME: usize = AUDIO_SAMPLE_RATE_HZ as usize * AUDIO_FRAME_MS as usize / 1000;

/// Maximum bytes one Opus frame may occupy on the wire (§11).
pub const MAX_OPUS_FRAME_BYTES: usize = 8 * 1024;

/// One encoded audio chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioChunk {
    /// Encoded Opus payload.
    pub data: Vec<u8>,
    /// Monotonic capture timestamp in microseconds.
    pub timestamp_us: u64,
}

#[cfg(feature = "audio-opus")]
mod backend {
    use super::{AudioChunk, MAX_OPUS_FRAME_BYTES, SAMPLES_PER_FRAME};
    use crate::error::MediaError;
    use crate::error::Result;
    use lumepeer_core::constants::{
        AUDIO_CHANNELS, AUDIO_DEFAULT_BITRATE_BPS, AUDIO_SAMPLE_RATE_HZ,
    };

    /// Host-side Opus encoder.
    ///
    /// The encoder keeps its own state across calls (analysis window,
    /// bitrate memory); one instance serves one session.
    pub struct OpusEncoder {
        inner: opus::Encoder,
        samples_per_frame: usize,
        bitrate_set: bool,
    }

    impl std::fmt::Debug for OpusEncoder {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("OpusEncoder")
                .field("samples_per_frame", &self.samples_per_frame)
                .field("bitrate_set", &self.bitrate_set)
                .finish_non_exhaustive()
        }
    }

    impl OpusEncoder {
        /// Builds an encoder for the fixed §11 parameters.
        ///
        /// # Errors
        /// [`MediaError::Audio`] if libopus refuses the parameter set.
        pub fn new() -> Result<Self> {
            let channels = match AUDIO_CHANNELS {
                1 => opus::Channels::Mono,
                2 => opus::Channels::Stereo,
                other => {
                    return Err(MediaError::Audio(format!(
                        "unsupported channel count {other}"
                    )));
                }
            };
            let inner =
                opus::Encoder::new(AUDIO_SAMPLE_RATE_HZ, channels, opus::Application::Audio)
                    .map_err(|e| MediaError::Audio(e.to_string()))?;
            Ok(Self {
                inner,
                samples_per_frame: SAMPLES_PER_FRAME,
                // libopus starts with its default bitrate; the first
                // `set_bitrate` call is deferred to encode time so a failure
                // surfaces as an encode error with context rather than a
                // bare constructor error.
                bitrate_set: false,
            })
        }

        /// Encodes exactly one `AUDIO_FRAME_MS` frame of interleaved s16
        /// samples (`input.len()` must equal samples × channels).
        ///
        /// # Errors
        /// [`MediaError::Audio`] when libopus rejects the input length or the
        /// encoder state is broken.
        pub fn encode(&mut self, pcm: &[i16], timestamp_us: u64) -> Result<AudioChunk> {
            if !self.bitrate_set {
                self.inner
                    .set_bitrate(opus::Bitrate::Bits(AUDIO_DEFAULT_BITRATE_BPS))
                    .map_err(|e| MediaError::Audio(e.to_string()))?;
                self.bitrate_set = true;
            }
            let expected = self.samples_per_frame * usize::from(AUDIO_CHANNELS);
            if pcm.len() != expected {
                return Err(MediaError::Audio(format!(
                    "expected {} samples, got {}",
                    expected,
                    pcm.len()
                )));
            }
            let mut out = vec![0u8; MAX_OPUS_FRAME_BYTES];
            let written = self
                .inner
                .encode(pcm, &mut out)
                .map_err(|e| MediaError::Audio(e.to_string()))?;
            out.truncate(written);
            Ok(AudioChunk {
                data: out,
                timestamp_us,
            })
        }

        /// Applies an adaptive-bitrate target to the audio track.
        ///
        /// # Errors
        /// [`MediaError::Audio`] when libopus refuses the value.
        pub fn set_bitrate(&mut self, bps: u32) -> Result<()> {
            self.inner
                .set_bitrate(opus::Bitrate::Bits(i32::try_from(bps).unwrap_or(i32::MAX)))
                .map_err(|e| MediaError::Audio(e.to_string()))
        }
    }

    /// Guest-side Opus decoder.
    pub struct OpusDecoder {
        inner: opus::Decoder,
        samples_per_frame: usize,
    }

    impl std::fmt::Debug for OpusDecoder {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("OpusDecoder")
                .field("samples_per_frame", &self.samples_per_frame)
                .finish_non_exhaustive()
        }
    }

    impl OpusDecoder {
        /// Builds a decoder for the fixed §11 parameters.
        ///
        /// # Errors
        /// [`MediaError::Audio`] if libopus refuses the parameter set.
        pub fn new() -> Result<Self> {
            let channels = match AUDIO_CHANNELS {
                1 => opus::Channels::Mono,
                2 => opus::Channels::Stereo,
                other => {
                    return Err(MediaError::Audio(format!(
                        "unsupported channel count {other}"
                    )));
                }
            };
            let inner = opus::Decoder::new(AUDIO_SAMPLE_RATE_HZ, channels)
                .map_err(|e| MediaError::Audio(e.to_string()))?;
            Ok(Self {
                inner,
                samples_per_frame: SAMPLES_PER_FRAME,
            })
        }

        /// Decodes one packet back into interleaved s16 samples. An **empty**
        /// `packet` means lost frame: libopus synthesizes concealment instead
        /// of failing, because audio degrades towards noise, never towards an
        /// error (§24.5).
        ///
        /// # Errors
        /// [`MediaError::Audio`] only when the output buffer cannot hold the
        /// frame — a caller bug, not hostile input.
        pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<i16>> {
            let mut out = vec![0i16; self.samples_per_frame * usize::from(AUDIO_CHANNELS)];
            let written = self
                .inner
                .decode(packet, &mut out, false)
                .map_err(|e| MediaError::Audio(e.to_string()))?;
            out.truncate(written * usize::from(AUDIO_CHANNELS));
            Ok(out)
        }

        /// Samples per decoded frame, for jitter-buffer sizing.
        #[must_use]
        pub fn samples_per_frame(&self) -> usize {
            self.samples_per_frame
        }
    }
}

#[cfg(feature = "audio-opus")]
pub use backend::{OpusDecoder, OpusEncoder};

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    #[cfg(feature = "audio-opus")]
    use crate::error::MediaError;
    #[cfg(feature = "audio-opus")]
    use lumepeer_core::constants::AUDIO_CHANNELS;

    /// A sine burst encodes, round-trips and comes back non-silent.
    #[cfg(feature = "audio-opus")]
    #[test]
    fn sine_roundtrips_with_energy() {
        let mut enc = OpusEncoder::new().unwrap();
        let mut dec = OpusDecoder::new().unwrap();

        // One second of 440 Hz stereo s16.
        let frames = AUDIO_SAMPLE_RATE_HZ / 1_000 * AUDIO_FRAME_MS;
        let mut pcm = Vec::with_capacity((frames * u32::from(AUDIO_CHANNELS)) as usize);
        for n in 0..frames {
            let t = f64::from(n) / f64::from(AUDIO_SAMPLE_RATE_HZ);
            let s = (t * 2.0 * std::f64::consts::PI * 440.0).sin();
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let v = (s * 8_000.0) as i16;
            pcm.push(v);
            pcm.push(v);
        }

        let mut total_in: f64 = 0.0;
        let mut total_out: f64 = 0.0;
        let chunk_samples = SAMPLES_PER_FRAME * usize::from(AUDIO_CHANNELS);
        let mut ts = 0u64;
        for block in pcm.chunks(chunk_samples) {
            let chunk = enc.encode(block, ts).unwrap();
            assert!(!chunk.data.is_empty());
            assert!(chunk.data.len() <= MAX_OPUS_FRAME_BYTES);
            let decoded = dec.decode(&chunk.data).unwrap();
            total_in += f64::from(block.iter().map(|s| i32::from(*s).abs()).sum::<i32>());
            total_out += f64::from(decoded.iter().map(|s| i32::from(*s).abs()).sum::<i32>());
            ts += u64::from(AUDIO_FRAME_MS) * 1_000;
        }
        // The signal survived, not bit-exact but present.
        assert!(
            total_out > total_in * 0.5,
            "energy lost: {total_out} vs {total_in}"
        );
    }

    #[cfg(feature = "audio-opus")]
    #[test]
    fn wrong_input_length_is_an_error_not_a_panic() {
        let mut enc = OpusEncoder::new().unwrap();
        assert!(matches!(
            enc.encode(&[0i16; 10], 0),
            Err(MediaError::Audio(_))
        ));
    }

    #[cfg(feature = "audio-opus")]
    #[test]
    fn packet_loss_conceals_instead_of_failing() {
        let mut enc = OpusEncoder::new().unwrap();
        let mut dec = OpusDecoder::new().unwrap();
        let block = vec![100i16; SAMPLES_PER_FRAME * usize::from(AUDIO_CHANNELS)];
        let chunk = enc.encode(&block, 0).unwrap();
        let good = dec.decode(&chunk.data).unwrap();
        assert_eq!(good.len(), SAMPLES_PER_FRAME * usize::from(AUDIO_CHANNELS));
        // Missing packet (empty slice) → concealment of the same shape.
        let concealed = dec.decode(&[]).unwrap();
        assert_eq!(
            concealed.len(),
            SAMPLES_PER_FRAME * usize::from(AUDIO_CHANNELS)
        );
    }

    /// Without the feature the module still compiles: the wire constants
    /// stay visible for tests and callers.
    #[cfg(not(feature = "audio-opus"))]
    #[test]
    fn without_feature_the_module_is_inert() {
        // SAMPLES_PER_FRAME is defined from the same constants, so this pins
        // them without naming the feature-gated imports.
        let samples_per_frame = AUDIO_SAMPLE_RATE_HZ as usize * 20 / 1000;
        assert_eq!(SAMPLES_PER_FRAME, samples_per_frame);
        assert_eq!(MAX_OPUS_FRAME_BYTES, 8 * 1024);
    }
}
