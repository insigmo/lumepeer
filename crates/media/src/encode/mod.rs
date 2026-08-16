//! Encoder selection: hardware first, software fallback second (design doc
//! §11, §18).
//!
//! H.264 baseline/main is the mandatory desktop baseline. AV1 is optional and
//! only when both sides have hardware support; there is no software AV1
//! fallback in v1. Opus is the only audio codec.

use lumepeer_core::constants::{ENCODE_DEFAULT_BITRATE_KBPS, ENCODE_DEFAULT_FPS};

use crate::capture::Frame;
use crate::error::{MediaError, Result};

/// Video codec on the wire (§11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    /// Mandatory desktop baseline.
    H264,
    /// Optional, only with mutual hardware support.
    Av1,
}

/// Where the encoding happens (§18).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderKind {
    /// Platform hardware encoder.
    Hardware,
    /// `openh264` software fallback, allowed only past the resource gate.
    SoftwareOpenH264,
}

/// Encoder settings; defaults come from §14, not from magic numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderConfig {
    /// Target frame rate.
    pub fps: u8,
    /// Target bitrate.
    pub bitrate_kbps: u32,
    /// Codec to use.
    pub codec: VideoCodec,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            fps: ENCODE_DEFAULT_FPS,
            bitrate_kbps: ENCODE_DEFAULT_BITRATE_KBPS,
            codec: VideoCodec::H264,
        }
    }
}

/// One encoded video frame.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    /// Whether this frame can be decoded without previous frames.
    pub keyframe: bool,
    /// Monotonic timestamp in microseconds, copied from the capture frame.
    pub timestamp_us: u64,
    /// Bitstream bytes.
    pub data: Vec<u8>,
}

/// Video encoder.
pub trait VideoEncoder: Send {
    /// Encodes one captured frame.
    ///
    /// # Errors
    /// [`MediaError::Encode`] if the backend rejects the frame.
    fn encode(&mut self, frame: &Frame) -> Result<EncodedFrame>;

    /// Applies a new bitrate target, rate-limited by the caller to
    /// `ABR_ADJUST_MAX_RATE_PER_SEC` (§11).
    ///
    /// # Errors
    /// [`MediaError::Encode`] if the backend refuses the change.
    fn set_bitrate(&mut self, bitrate_kbps: u32) -> Result<()>;

    /// Which backend is in use, for telemetry and the UI.
    fn kind(&self) -> EncoderKind;
}

/// Whether a platform hardware encoder can be used right now.
///
/// Media Foundation, `VideoToolbox`, `MediaCodec` and VA-API bindings are
/// phase 4 work (§19), so this is `None` everywhere for the moment and the
/// software fallback of §18 carries the pipeline.
#[must_use]
pub const fn probe_hardware(_config: EncoderConfig) -> Option<EncoderKind> {
    None
}

/// Selects an encoder: hardware if available, otherwise the software fallback
/// when it passes the resource gate (§18).
///
/// # Errors
/// [`MediaError::EncoderUnavailable`] when neither path is usable; the UI must
/// explain why instead of silently degrading.
pub fn select_encoder(config: EncoderConfig) -> Result<Box<dyn VideoEncoder>> {
    if config.codec == VideoCodec::Av1 && probe_hardware(config) != Some(EncoderKind::Hardware) {
        // §11: AV1 only with mutual hardware support, and there is no software
        // AV1 fallback in v1.
        return Err(MediaError::EncoderUnavailable(
            "AV1 needs hardware support on both sides and has no software fallback".to_owned(),
        ));
    }

    if let Some(EncoderKind::Hardware) = probe_hardware(config) {
        return Err(MediaError::EncoderUnavailable(
            "hardware encoder probing reported a backend that is not implemented yet".to_owned(),
        ));
    }

    #[cfg(feature = "encode-openh264")]
    {
        tracing::info!("no hardware encoder available, falling back to openh264 (§18)");
        software::OpenH264Encoder::new(config).map(|e| Box::new(e) as Box<dyn VideoEncoder>)
    }
    #[cfg(not(feature = "encode-openh264"))]
    {
        Err(MediaError::EncoderUnavailable(
            "no hardware encoder and the openh264 fallback is not built in".to_owned(),
        ))
    }
}

/// `openh264` software fallback (§5.1, §18).
///
/// Kept as an inline module so the file list of §6 stays exact.
#[cfg(feature = "encode-openh264")]
pub mod software {
    use openh264::encoder::{BitRate, Encoder, EncoderConfig as H264Config, FrameRate, FrameType};
    use openh264::formats::{BgraSliceU8, YUVBuffer};

    use super::{EncodedFrame, EncoderConfig, EncoderKind, VideoEncoder};
    use crate::capture::{Frame, PixelFormat};
    use crate::error::{MediaError, Result};

    /// Bits per kilobit, for the kbps of §14 against the bps of the codec API.
    const BITS_PER_KBIT: u32 = 1000;

    /// H.264 encoder backed by Cisco's `openh264`.
    pub struct OpenH264Encoder {
        inner: Encoder,
        config: EncoderConfig,
    }

    // `openh264::encoder::Encoder` is not `Debug`, and the settings are what
    // matters for logs anyway; the codec state must never be printed.
    impl std::fmt::Debug for OpenH264Encoder {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("OpenH264Encoder")
                .field("config", &self.config)
                .finish_non_exhaustive()
        }
    }

    impl OpenH264Encoder {
        /// Builds an encoder for `config`.
        ///
        /// # Errors
        /// [`MediaError::EncoderUnavailable`] if the codec refuses the settings.
        pub fn new(config: EncoderConfig) -> Result<Self> {
            let inner = Self::build(config)?;
            Ok(Self { inner, config })
        }

        fn build(config: EncoderConfig) -> Result<Encoder> {
            let h264 = H264Config::new()
                .bitrate(BitRate::from_bps(
                    config.bitrate_kbps.saturating_mul(BITS_PER_KBIT),
                ))
                .max_frame_rate(FrameRate::from_hz(f32::from(config.fps)));
            Encoder::with_api_config(openh264::OpenH264API::from_source(), h264)
                .map_err(|e| MediaError::EncoderUnavailable(e.to_string()))
        }

        /// Crops to even dimensions: 4:2:0 subsampling has no odd rows or
        /// columns, and the conversion asserts on them.
        fn even_bgra(frame: &Frame) -> Result<(Vec<u8>, usize, usize)> {
            if frame.format != PixelFormat::Bgra8 {
                return Err(MediaError::Encode(format!(
                    "openh264 fallback expects BGRA8 input, got {:?}",
                    frame.format
                )));
            }
            let width = (frame.width as usize) & !1;
            let height = (frame.height as usize) & !1;
            if width == 0 || height == 0 {
                return Err(MediaError::Encode("frame is smaller than 2x2".to_owned()));
            }

            let src_stride = frame.width as usize * 4;
            let dst_stride = width * 4;
            if frame.data.len() < src_stride * height {
                return Err(MediaError::Encode("frame buffer is short".to_owned()));
            }
            let mut cropped = Vec::with_capacity(dst_stride * height);
            for row in 0..height {
                let start = row * src_stride;
                cropped.extend_from_slice(&frame.data[start..start + dst_stride]);
            }
            Ok((cropped, width, height))
        }
    }

    impl VideoEncoder for OpenH264Encoder {
        fn encode(&mut self, frame: &Frame) -> Result<EncodedFrame> {
            let (bgra, width, height) = Self::even_bgra(frame)?;
            let yuv = YUVBuffer::from_bgra8_source(BgraSliceU8::new(&bgra, (width, height)));
            let bitstream = self
                .inner
                .encode(&yuv)
                .map_err(|e| MediaError::Encode(e.to_string()))?;
            Ok(EncodedFrame {
                keyframe: matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I),
                timestamp_us: frame.timestamp_us,
                data: bitstream.to_vec(),
            })
        }

        fn set_bitrate(&mut self, bitrate_kbps: u32) -> Result<()> {
            if bitrate_kbps == self.config.bitrate_kbps {
                return Ok(());
            }
            // `openh264` takes the bitrate at construction, so a change rebuilds
            // the encoder. The next frame is therefore a keyframe. That is
            // acceptable at the one adjustment per second of
            // `ABR_ADJUST_MAX_RATE_PER_SEC` (§11, §14).
            let config = EncoderConfig {
                bitrate_kbps,
                ..self.config
            };
            self.inner = Self::build(config)?;
            self.config = config;
            Ok(())
        }

        fn kind(&self) -> EncoderKind {
            EncoderKind::SoftwareOpenH264
        }
    }

    #[cfg(test)]
    mod tests {
        #![allow(clippy::unwrap_used)]

        use super::*;
        use crate::capture::PixelFormat;

        fn frame(width: u32, height: u32, fill: u8) -> Frame {
            Frame {
                width,
                height,
                format: PixelFormat::Bgra8,
                timestamp_us: 0,
                data: vec![fill; (width as usize) * (height as usize) * 4],
            }
        }

        #[test]
        fn encodes_a_frame_and_starts_with_a_keyframe() {
            let mut encoder = OpenH264Encoder::new(EncoderConfig::default()).unwrap();
            let first = encoder.encode(&frame(64, 64, 0x20)).unwrap();
            assert!(first.keyframe, "the first frame must be decodable alone");
            assert!(!first.data.is_empty());
            assert_eq!(encoder.kind(), EncoderKind::SoftwareOpenH264);
        }

        #[test]
        fn odd_dimensions_are_cropped_rather_than_panicking() {
            let mut encoder = OpenH264Encoder::new(EncoderConfig::default()).unwrap();
            assert!(encoder.encode(&frame(65, 33, 0x40)).is_ok());
        }

        #[test]
        fn a_bitrate_change_is_accepted_and_encoding_continues() {
            let mut encoder = OpenH264Encoder::new(EncoderConfig::default()).unwrap();
            encoder.encode(&frame(64, 64, 0x10)).unwrap();
            encoder.set_bitrate(1_500).unwrap();
            assert_eq!(encoder.config.bitrate_kbps, 1_500);
            assert!(
                !encoder
                    .encode(&frame(64, 64, 0x80))
                    .unwrap()
                    .data
                    .is_empty()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_come_from_the_constants_of_14() {
        let config = EncoderConfig::default();
        assert_eq!(config.fps, ENCODE_DEFAULT_FPS);
        assert_eq!(config.bitrate_kbps, ENCODE_DEFAULT_BITRATE_KBPS);
        assert_eq!(config.codec, VideoCodec::H264);
    }

    #[test]
    fn av1_is_refused_without_hardware_support() {
        let config = EncoderConfig {
            codec: VideoCodec::Av1,
            ..EncoderConfig::default()
        };
        assert!(matches!(
            select_encoder(config),
            Err(MediaError::EncoderUnavailable(_))
        ));
    }
}
