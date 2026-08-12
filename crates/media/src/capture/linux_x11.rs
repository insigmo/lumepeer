//! X11 capture (design doc §11).
//!
//! Lower-trust path: X11 gives any client access to the whole screen, so this
//! backend requires a visible on-screen indicator for the duration of the
//! session (§11, ADR 0003). Wayland via xdg-desktop-portal is the trusted path
//! and lands later.
//!
//! Frames are read with the core `GetImage` request through `x11rb`, which is
//! pure safe Rust. The MIT-SHM path of §6 is a later optimization: it needs a
//! shared segment and therefore `unsafe`, which is not worth it before the
//! resource gates of §15 actually measure the difference.

use lumepeer_core::constants::ENCODE_DEFAULT_FPS;
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat, Screen};
use x11rb::rust_connection::RustConnection;

use crate::capture::{CaptureTarget, Frame, InputCapability, PixelFormat, ScreenCapturer};
use crate::error::{MediaError, Result};

/// All planes of the image.
const ALL_PLANES: u32 = !0;

/// Live X11 connection and the geometry it captures.
#[derive(Debug)]
struct Active {
    connection: RustConnection,
    root: u32,
    width: u16,
    height: u16,
    /// Hash of the last frame handed out, so an unchanged screen yields `None`
    /// instead of a duplicate frame (§11.1).
    last_hash: Option<[u8; 32]>,
    started_at: std::time::Instant,
}

/// X11 screen capturer.
#[derive(Debug, Default)]
pub struct X11Capturer {
    active: Option<Active>,
}

impl X11Capturer {
    /// Creates a capturer that connects on [`ScreenCapturer::start`].
    #[must_use]
    pub const fn new() -> Self {
        Self { active: None }
    }

    /// Frames per second this backend is polled at; the caller paces itself,
    /// X11 has no frame clock of its own.
    #[must_use]
    pub const fn suggested_fps() -> u8 {
        ENCODE_DEFAULT_FPS
    }

    fn screen(connection: &RustConnection, screen_num: usize, target: CaptureTarget) -> Screen {
        let setup = connection.setup();
        let index = match target {
            CaptureTarget::PrimaryDisplay => screen_num,
            CaptureTarget::Display(n) => n as usize,
        };
        setup
            .roots
            .get(index)
            .unwrap_or(&setup.roots[screen_num])
            .clone()
    }
}

impl ScreenCapturer for X11Capturer {
    fn start(&mut self, target: CaptureTarget) -> Result<()> {
        let (connection, screen_num) = x11rb::connect(None).map_err(|e| {
            // A missing or refused display is the X11 equivalent of the user
            // declining the system prompt (§18).
            MediaError::CaptureUnavailable(format!("cannot connect to the X server: {e}"))
        })?;
        let screen = Self::screen(&connection, screen_num, target);

        self.active = Some(Active {
            root: screen.root,
            width: screen.width_in_pixels,
            height: screen.height_in_pixels,
            connection,
            last_hash: None,
            started_at: std::time::Instant::now(),
        });
        Ok(())
    }

    fn next_frame(&mut self) -> Result<Option<Frame>> {
        let active = self
            .active
            .as_mut()
            .ok_or_else(|| MediaError::CaptureUnavailable("capturer not started".to_owned()))?;

        let reply = active
            .connection
            .get_image(
                ImageFormat::Z_PIXMAP,
                active.root,
                0,
                0,
                active.width,
                active.height,
                ALL_PLANES,
            )
            .map_err(|e| MediaError::CaptureInterrupted(e.to_string()))?
            .reply()
            .map_err(|e| MediaError::CaptureInterrupted(e.to_string()))?;

        let hash = *blake3::hash(&reply.data).as_bytes();
        if active.last_hash == Some(hash) {
            return Ok(None);
        }
        active.last_hash = Some(hash);

        let timestamp_us =
            u64::try_from(active.started_at.elapsed().as_micros()).unwrap_or(u64::MAX);
        Ok(Some(Frame {
            width: u32::from(active.width),
            height: u32::from(active.height),
            // X11 TrueColor visuals hand back little-endian BGRX in Z_PIXMAP.
            format: PixelFormat::Bgra8,
            timestamp_us,
            data: reply.data,
        }))
    }

    fn stop(&mut self) {
        self.active = None;
    }

    fn input_capability(&self) -> InputCapability {
        // XTEST can inject into any X11 client; this is exactly why X11 is the
        // lower-trust path and needs the visible indicator (§11).
        InputCapability::Full
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without a display the backend refuses; with one it must produce a frame
    /// of the screen's own size. Skipped rather than failed on a headless CI
    /// runner, where there is no X server to talk to.
    #[test]
    fn capture_produces_a_frame_when_a_display_is_available() {
        let mut capturer = X11Capturer::new();
        if capturer.start(CaptureTarget::PrimaryDisplay).is_err() {
            return;
        }

        let frame = match capturer.next_frame() {
            Ok(Some(frame)) => frame,
            Ok(None) => panic!("the first frame after start cannot be a duplicate"),
            Err(e) => panic!("capture failed on a live display: {e}"),
        };
        assert!(frame.width > 0 && frame.height > 0);
        assert_eq!(frame.format, PixelFormat::Bgra8);
        // 4 bytes per pixel for a 24/32 bit TrueColor visual.
        assert_eq!(
            frame.data.len(),
            (frame.width as usize) * (frame.height as usize) * 4
        );

        capturer.stop();
        assert!(capturer.next_frame().is_err());
    }
}
