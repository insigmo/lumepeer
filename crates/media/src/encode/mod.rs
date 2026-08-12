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

/// Selects an encoder: hardware if available, otherwise the software fallback
/// when it passes the resource gate (§18).
///
/// # Errors
/// [`MediaError::EncoderUnavailable`] when neither path is usable; the UI must
/// explain why instead of silently degrading.
pub fn select_encoder(_config: EncoderConfig) -> Result<Box<dyn VideoEncoder>> {
    Err(MediaError::EncoderUnavailable(
        "phase 2: hardware encoders and openh264 fallback per §11".to_owned(),
    ))
}
