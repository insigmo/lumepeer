//! Screen capture: one trait, one backend per platform (design doc §11, §11.1).
//!
//! Capture never starts without an active viewer and stops as soon as the last
//! viewer leaves (§8.1, §11).

use crate::error::{MediaError, Result};

#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub mod macos;

#[cfg(all(target_os = "linux", not(target_os = "android")))]
pub mod linux_x11;

#[cfg(all(target_os = "linux", not(target_os = "android")))]
pub mod linux_wayland;

/// What the platform allows besides pixels (§11.1, §18).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputCapability {
    /// Full keyboard and pointer injection.
    Full,
    /// Injection only through the desktop portal's `RemoteDesktop` interface.
    PortalRemoteDesktop,
    /// No injection at all: the session degrades to view-only and the UI says
    /// so explicitly (§18).
    None,
}

/// What to capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureTarget {
    /// The primary display.
    PrimaryDisplay,
    /// A specific display, as indexed by the platform backend.
    Display(u32),
}

/// Pixel format of a captured frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 8 bits per channel, BGRA order.
    Bgra8,
    /// Planar 4:2:0, 8 bits per sample.
    Nv12,
}

/// One captured frame.
#[derive(Debug, Clone)]
pub struct Frame {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Pixel format of `data`.
    pub format: PixelFormat,
    /// Monotonic capture timestamp in microseconds.
    pub timestamp_us: u64,
    /// Raw pixels.
    pub data: Vec<u8>,
}

/// Platform screen capture backend (§11.1).
pub trait ScreenCapturer: Send {
    /// Starts capturing `target`.
    ///
    /// # Errors
    /// [`MediaError::PermissionDenied`] if the user declines the system
    /// prompt, [`MediaError::CaptureUnavailable`] if the backend cannot run.
    fn start(&mut self, target: CaptureTarget) -> Result<()>;

    /// Returns the next frame, or `None` when it is identical to the previous
    /// one (§11.1).
    ///
    /// # Errors
    /// [`MediaError::CaptureInterrupted`] on screen lock, user switch or
    /// desktop change (§18).
    fn next_frame(&mut self) -> Result<Option<Frame>>;

    /// Stops capturing. Idempotent.
    fn stop(&mut self);

    /// What this backend allows on the input side.
    fn input_capability(&self) -> InputCapability;
}

/// Capturer that produces nothing. Used in phase 0/1, where the consent and
/// transport paths are exercised without any platform SDK.
#[derive(Debug, Default)]
pub struct StubCapturer {
    running: bool,
}

impl ScreenCapturer for StubCapturer {
    fn start(&mut self, _target: CaptureTarget) -> Result<()> {
        self.running = true;
        Ok(())
    }

    fn next_frame(&mut self) -> Result<Option<Frame>> {
        if self.running {
            Ok(None)
        } else {
            Err(MediaError::CaptureUnavailable(
                "capturer not started".to_owned(),
            ))
        }
    }

    fn stop(&mut self) {
        self.running = false;
    }

    fn input_capability(&self) -> InputCapability {
        InputCapability::None
    }
}

/// Opens the capture backend of the current platform.
///
/// # Errors
/// [`MediaError::CaptureUnavailable`] when no backend is compiled in for this
/// target.
pub fn platform_capturer() -> Result<Box<dyn ScreenCapturer>> {
    Err(MediaError::CaptureUnavailable(
        "phase 2: platform capture backends per §11".to_owned(),
    ))
}
