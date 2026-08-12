//! Windows capture (design doc §11, §5.1).
//!
//! MVP goes through the `scap` crate; the hardening step replaces it with
//! direct DXGI/Windows.Graphics.Capture bindings from the `windows` crate, so
//! that a security-critical path does not depend on a beta crate.

use crate::capture::{CaptureTarget, Frame, InputCapability, ScreenCapturer};
use crate::error::{MediaError, Result};

/// DXGI/WGC capturer.
#[derive(Debug, Default)]
pub struct WindowsCapturer {
    _private: (),
}

impl ScreenCapturer for WindowsCapturer {
    fn start(&mut self, _target: CaptureTarget) -> Result<()> {
        Err(MediaError::CaptureUnavailable(
            "phase 2: windows capture not implemented yet".to_owned(),
        ))
    }

    fn next_frame(&mut self) -> Result<Option<Frame>> {
        Err(MediaError::CaptureUnavailable(
            "phase 2: windows capture not implemented yet".to_owned(),
        ))
    }

    fn stop(&mut self) {}

    fn input_capability(&self) -> InputCapability {
        InputCapability::Full
    }
}
