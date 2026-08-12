//! Media pipeline errors (design doc §6, §18).

/// Errors of capture, encode, decode and the adaptive bitrate controller.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MediaError {
    /// The platform capture backend is unavailable or was not built in.
    #[error("capture backend unavailable: {0}")]
    CaptureUnavailable(String),

    /// The user declined the system capture or input prompt. Not an error
    /// condition of ours: fall back to a narrower capability (§18).
    #[error("capture permission denied by the user")]
    PermissionDenied,

    /// Capture stopped: screen lock, user switch or desktop change (§18).
    #[error("capture interrupted: {0}")]
    CaptureInterrupted(String),

    /// No hardware encoder and the software fallback did not pass the
    /// resource gate (§18).
    #[error("no usable encoder: {0}")]
    EncoderUnavailable(String),

    /// Encoding a frame failed.
    #[error("encode failed: {0}")]
    Encode(String),

    /// The decoder sandbox could not be established, so decoding must not
    /// start at all: degrade towards safety, not convenience (§11.3).
    #[error("decoder sandbox unavailable on this platform: {0}")]
    SandboxUnavailable(String),

    /// The decoder worker process died or misbehaved.
    #[error("decoder worker failed: {0}")]
    DecoderWorker(String),

    /// Decoding a frame failed.
    #[error("decode failed: {0}")]
    Decode(String),
}

/// Convenience alias for media results.
pub type Result<T> = core::result::Result<T, MediaError>;
