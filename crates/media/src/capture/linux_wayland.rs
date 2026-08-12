//! Wayland capture via xdg-desktop-portal and PipeWire (design doc §11).
//!
//! Deferred: the first supported Linux path is X11
//! ([`crate::capture::linux_x11`]); Wayland lands later.
//!
//! The portal call order is normative and must not be reordered "to simplify
//! the code": `CreateSession`, then `SelectDevices`, then `SelectSources`,
//! then `Start`. A zero input-device mask returned by `CreateSession`/`Start`
//! is not an error — it is the user declining input in the system dialog, and
//! it falls back to [`InputCapability::None`] (§18).

use crate::capture::{CaptureTarget, Frame, InputCapability, ScreenCapturer};
use crate::error::{MediaError, Result};

/// Portal/PipeWire capturer.
#[derive(Debug, Default)]
pub struct WaylandPortalCapturer {
    _private: (),
}

impl ScreenCapturer for WaylandPortalCapturer {
    fn start(&mut self, _target: CaptureTarget) -> Result<()> {
        Err(MediaError::CaptureUnavailable(
            "wayland portal capture is not part of the current milestone".to_owned(),
        ))
    }

    fn next_frame(&mut self) -> Result<Option<Frame>> {
        Err(MediaError::CaptureUnavailable(
            "wayland portal capture is not part of the current milestone".to_owned(),
        ))
    }

    fn stop(&mut self) {}

    fn input_capability(&self) -> InputCapability {
        InputCapability::PortalRemoteDesktop
    }
}
