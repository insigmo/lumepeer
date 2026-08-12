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

use lumepeer_core::protocol::{InputDetail, InputEventPayload, POINTER_BUTTON_LOGICAL_BASE};

use crate::capture::{
    CaptureTarget, Frame, InputCapability, InputInjector, PixelFormat, ScreenCapturer,
};
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

/// X11 event type codes used by the XTEST extension.
const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;
const BUTTON_PRESS: u8 = 4;
const BUTTON_RELEASE: u8 = 5;
const MOTION_NOTIFY: u8 = 6;

/// X11 button numbers for the scroll wheel.
const BUTTON_WHEEL_UP: u8 = 4;
const BUTTON_WHEEL_DOWN: u8 = 5;
const BUTTON_WHEEL_LEFT: u8 = 6;
const BUTTON_WHEEL_RIGHT: u8 = 7;

/// Offset between an evdev scancode and an X11 keycode.
const EVDEV_KEYCODE_OFFSET: u32 = 8;

/// Full range of a normalized pointer coordinate (§9.1).
const POINTER_RANGE: u32 = 65_535;

/// Input injection through the XTEST extension.
///
/// Lower trust, like the capture path: XTEST reaches every client on the
/// display. `lumepeer-core` has already authorized each event by the time it
/// arrives here; this type never looks at grants (§2.3, §11).
#[derive(Debug)]
pub struct X11Injector {
    connection: RustConnection,
    root: u32,
    width: u16,
    height: u16,
}

impl X11Injector {
    /// Connects to the X server and checks that XTEST is present.
    ///
    /// # Errors
    /// [`MediaError::InputUnavailable`] if there is no display or the server
    /// has no XTEST extension; the session then continues view-only (§18).
    pub fn connect() -> Result<Self> {
        let (connection, screen_num) = x11rb::connect(None).map_err(|e| {
            MediaError::InputUnavailable(format!("cannot connect to the X server: {e}"))
        })?;

        // Refusing here is what keeps a session from believing it has control
        // it cannot exercise (§18).
        let present = x11rb::connection::RequestConnection::extension_information(
            &connection,
            x11rb::protocol::xtest::X11_EXTENSION_NAME,
        )
        .map_err(|e| MediaError::InputUnavailable(e.to_string()))?;
        if present.is_none() {
            return Err(MediaError::InputUnavailable(
                "the X server has no XTEST extension".to_owned(),
            ));
        }

        let screen = {
            let setup = connection.setup();
            setup
                .roots
                .get(screen_num)
                .ok_or_else(|| MediaError::InputUnavailable("no such screen".to_owned()))?
                .clone()
        };

        Ok(Self {
            root: screen.root,
            width: screen.width_in_pixels,
            height: screen.height_in_pixels,
            connection,
        })
    }

    fn fake(&self, type_: u8, detail: u8, x: i16, y: i16) -> Result<()> {
        use x11rb::protocol::xtest::ConnectionExt as _;

        self.connection
            .xtest_fake_input(type_, detail, 0, self.root, x, y, 0)
            .map_err(|e| MediaError::InputUnavailable(e.to_string()))?
            .check()
            .map_err(|e| MediaError::InputUnavailable(e.to_string()))?;
        Ok(())
    }

    /// Maps a normalized 0..=65535 coordinate onto the screen.
    fn to_screen(value: u16, extent: u16) -> i16 {
        let scaled = u32::from(value) * u32::from(extent) / POINTER_RANGE;
        i16::try_from(scaled).unwrap_or(i16::MAX)
    }

    /// X11 keycode for a guest scancode. Guests send physical scancodes, never
    /// raw OS handles (§11), and X11 keycodes are evdev codes plus 8.
    fn keycode(scancode: u32) -> Result<u8> {
        u8::try_from(scancode.saturating_add(EVDEV_KEYCODE_OFFSET))
            .map_err(|_| MediaError::InputUnavailable("scancode outside the X11 range".to_owned()))
    }

    /// X11 button number for a pointer button carried as a logical id.
    fn button(logical: u32) -> Result<u8> {
        let index = logical.saturating_sub(POINTER_BUTTON_LOGICAL_BASE);
        u8::try_from(index.saturating_add(1))
            .map_err(|_| MediaError::InputUnavailable("button outside the X11 range".to_owned()))
    }
}

impl InputInjector for X11Injector {
    fn inject(&mut self, event: &InputEventPayload) -> Result<()> {
        match event.detail {
            InputDetail::PointerMove { x, y } => self.fake(
                MOTION_NOTIFY,
                0,
                Self::to_screen(x, self.width),
                Self::to_screen(y, self.height),
            ),
            InputDetail::Wheel { dx, dy } => {
                // X11 has no wheel axis: a scroll is a button click per notch.
                for (delta, negative, positive) in [
                    (dy, BUTTON_WHEEL_DOWN, BUTTON_WHEEL_UP),
                    (dx, BUTTON_WHEEL_LEFT, BUTTON_WHEEL_RIGHT),
                ] {
                    let button = if delta < 0 { negative } else { positive };
                    for _ in 0..delta.unsigned_abs() {
                        self.fake(BUTTON_PRESS, button, 0, 0)?;
                        self.fake(BUTTON_RELEASE, button, 0, 0)?;
                    }
                }
                Ok(())
            }
            InputDetail::Press | InputDetail::Release => {
                let pressed = matches!(event.detail, InputDetail::Press);
                if event.logical >= POINTER_BUTTON_LOGICAL_BASE {
                    let button = Self::button(event.logical)?;
                    self.fake(
                        if pressed {
                            BUTTON_PRESS
                        } else {
                            BUTTON_RELEASE
                        },
                        button,
                        0,
                        0,
                    )
                } else {
                    let keycode = Self::keycode(event.scancode)?;
                    self.fake(if pressed { KEY_PRESS } else { KEY_RELEASE }, keycode, 0, 0)
                }
            }
        }
    }

    fn capability(&self) -> InputCapability {
        InputCapability::Full
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn normalized_coordinates_map_onto_the_screen() {
        assert_eq!(X11Injector::to_screen(0, 1920), 0);
        assert_eq!(X11Injector::to_screen(u16::MAX, 1920), 1920);
        assert_eq!(X11Injector::to_screen(32_767, 1920), 959);
    }

    #[test]
    fn scancodes_and_buttons_map_into_the_x11_ranges() {
        // evdev KEY_A is 30, X11 keycode 38.
        assert_eq!(X11Injector::keycode(30).unwrap(), 38);
        assert!(X11Injector::keycode(1_000).is_err());
        // The first pointer button is X11 button 1.
        assert_eq!(X11Injector::button(POINTER_BUTTON_LOGICAL_BASE).unwrap(), 1);
        assert_eq!(
            X11Injector::button(POINTER_BUTTON_LOGICAL_BASE + 2).unwrap(),
            3
        );
    }

    /// Injection is opt-in through `LUMEPEER_TEST_XTEST=1`: it drives whatever
    /// display the suite runs against, and a developer running the tests on
    /// their own desktop should not have their session touched. Even when
    /// enabled the test only injects a move to the position the pointer already
    /// has, so nothing visible happens.
    #[test]
    fn xtest_injection_works_when_explicitly_enabled() {
        use x11rb::protocol::xproto::ConnectionExt as _;

        if std::env::var("LUMEPEER_TEST_XTEST").as_deref() != Ok("1") {
            return;
        }
        let Ok(mut injector) = X11Injector::connect() else {
            return;
        };
        assert_eq!(injector.capability(), InputCapability::Full);

        let before = injector
            .connection
            .query_pointer(injector.root)
            .unwrap()
            .reply()
            .unwrap();
        let normalize = |value: i16, extent: u16| -> u16 {
            u16::try_from(u32::from(value.unsigned_abs()) * POINTER_RANGE / u32::from(extent))
                .unwrap_or(u16::MAX)
        };
        injector
            .inject(&InputEventPayload {
                logical: 0,
                scancode: 0,
                modifiers: 0,
                detail: InputDetail::PointerMove {
                    x: normalize(before.root_x, injector.width),
                    y: normalize(before.root_y, injector.height),
                },
            })
            .unwrap();

        let after = injector
            .connection
            .query_pointer(injector.root)
            .unwrap()
            .reply()
            .unwrap();
        // Rounding through the normalized range costs at most one pixel.
        assert!((after.root_x - before.root_x).abs() <= 1);
        assert!((after.root_y - before.root_y).abs() <= 1);
    }

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
