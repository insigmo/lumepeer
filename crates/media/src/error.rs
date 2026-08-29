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

    /// Capture stopped because a secure desktop (UAC prompt, lock screen or
    /// fast user switch) took the foreground on Windows
    /// (`docs/bugs/11-uac-degradation.md`). Distinct from the general
    /// [`Self::CaptureInterrupted`]: this is expected to resolve on its own,
    /// so the caller keeps retrying instead of revoking the session.
    #[error("capture interrupted by the secure desktop: {0}")]
    SecureDesktopActive(String),

    /// The platform refuses input injection: no adapter, no permission, or a
    /// permission withdrawn mid-session (§18). The session degrades to
    /// view-only and the UI says so; it never silently keeps "control".
    #[error("input injection unavailable: {0}")]
    InputUnavailable(String),

    /// No hardware encoder and the software fallback did not pass the
    /// resource gate (§18).
    #[error("no usable encoder: {0}")]
    EncoderUnavailable(String),

    /// Encoding a frame failed.
    #[error("encode failed: {0}")]
    Encode(String),

    /// The Opus audio codec refused an operation (§5.1, ADR 0023). The
    /// session keeps running without audio; the UI reports it (§18).
    #[error("audio codec error: {0}")]
    Audio(String),

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
