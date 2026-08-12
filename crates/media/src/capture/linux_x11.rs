//! X11 capture via XShm (design doc §11).
//!
//! Lower-trust path: X11 gives any client access to the whole screen, so this
//! backend requires a visible on-screen indicator for the duration of the
//! session.

use crate::capture::{CaptureTarget, Frame, InputCapability, ScreenCapturer};
use crate::error::{MediaError, Result};

/// XShm-based capturer.
#[derive(Debug, Default)]
pub struct X11Capturer {
    _private: (),
}

impl ScreenCapturer for X11Capturer {
    fn start(&mut self, _target: CaptureTarget) -> Result<()> {
        Err(MediaError::CaptureUnavailable(
            "phase 2: XShm capture not implemented yet".to_owned(),
        ))
    }

    fn next_frame(&mut self) -> Result<Option<Frame>> {
        Err(MediaError::CaptureUnavailable(
            "phase 2: XShm capture not implemented yet".to_owned(),
        ))
    }

    fn stop(&mut self) {}

    fn input_capability(&self) -> InputCapability {
        InputCapability::Full
    }
}
