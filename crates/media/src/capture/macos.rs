//! macOS capture via `ScreenCaptureKit` (design doc §11, §5.1).
//!
//! Losing the Accessibility permission mid-session is a normal, handled event:
//! the next `CGEvent` fails, which revokes the session and notifies both sides
//! (§18). iOS is viewer-only in v1, so no capture backend exists there (§1.2).

use crate::capture::{CaptureTarget, Frame, InputCapability, ScreenCapturer};
use crate::error::{MediaError, Result};

/// `ScreenCaptureKit` capturer.
#[derive(Debug, Default)]
pub struct MacosCapturer {
    _private: (),
}

impl ScreenCapturer for MacosCapturer {
    fn start(&mut self, _target: CaptureTarget) -> Result<()> {
        Err(MediaError::CaptureUnavailable(
            "phase 2: macOS capture not implemented yet".to_owned(),
        ))
    }

    fn next_frame(&mut self) -> Result<Option<Frame>> {
        Err(MediaError::CaptureUnavailable(
            "phase 2: macOS capture not implemented yet".to_owned(),
        ))
    }

    fn stop(&mut self) {}

    fn input_capability(&self) -> InputCapability {
        InputCapability::Full
    }
}
