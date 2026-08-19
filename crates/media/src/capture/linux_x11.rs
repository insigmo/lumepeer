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

/// `NoSymbol`: what an unbound entry of the keyboard mapping reads as.
const NO_SYMBOL: u32 = 0;

/// Start of the Unicode keysym range: a code point outside Latin-1 is carried
/// as `0x0100_0000 | code point` (X11 keysym convention, `keysymdef.h`).
const UNICODE_KEYSYM_BASE: u32 = 0x0100_0000;

/// Highest Latin-1 code point, which doubles as its own keysym.
const LATIN1_MAX: u32 = 0xff;

/// Keysym of `F1`; `F2..F24` follow it consecutively (`keysymdef.h`).
const KEYSYM_F1: u32 = 0xffbe;

/// How many F-keys the guest's `0xe100 + N` logical encoding covers.
const F_KEY_COUNT: u32 = 24;

/// Logical identifier of `F1` in that encoding.
const F_KEY_LOGICAL_FIRST: u32 = 0xe101;

/// Logical identifier of the last F-key it can name.
const F_KEY_LOGICAL_LAST: u32 = F_KEY_LOGICAL_FIRST + F_KEY_COUNT - 1;

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
    /// The host's own keyboard layout, as `keysym -> (keycode, shift level)`.
    /// Read once at connect: this is what lets a guest's character be typed on
    /// whatever layout the host happens to have loaded.
    layout: std::collections::HashMap<u32, (u8, u8)>,
    /// A keycode bound to nothing in the host's layout, borrowed to type
    /// characters the layout cannot reach. `None` when the layout is full.
    scratch: Option<u8>,
    /// Keysym currently bound to `scratch`, so a repeated character does not
    /// rebind the keyboard on every keystroke.
    scratch_keysym: Option<u32>,
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

        let (layout, scratch) = read_layout(&connection);
        Ok(Self {
            root: screen.root,
            width: screen.width_in_pixels,
            height: screen.height_in_pixels,
            connection,
            layout,
            scratch,
            scratch_keysym: None,
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

    /// Presses or releases the key the guest asked for (§11).
    ///
    /// A guest running in a webview has no physical scancode to send — the DOM
    /// reports `KeyboardEvent.key`, so `scancode` arrives as 0 and `logical`
    /// carries the meaning. Keying off the scancode therefore aimed every
    /// keystroke at X11 keycode 8, which is bound to nothing: injection
    /// succeeded, the server accepted it, and not one character was ever
    /// typed. A scancode that *is* set still takes the direct evdev path, for
    /// a future non-webview guest that has one.
    fn key(&mut self, logical: u32, scancode: u32, modifiers: u32, pressed: bool) -> Result<()> {
        if scancode != 0 {
            let keycode = Self::keycode(scancode)?;
            return self.fake_key(keycode, pressed);
        }
        let keysym = keysym_of(logical).ok_or_else(|| {
            MediaError::InputUnavailable(format!("logical key {logical} has no X11 keysym"))
        })?;
        let keycode = self.keycode_for(keysym, modifiers)?;
        self.fake_key(keycode, pressed)
    }

    fn fake_key(&self, keycode: u8, pressed: bool) -> Result<()> {
        self.fake(if pressed { KEY_PRESS } else { KEY_RELEASE }, keycode, 0, 0)
    }

    /// Finds a keycode that produces `keysym` under the modifier state the
    /// guest reports, borrowing the scratch keycode when the host's layout
    /// cannot reach it at all.
    ///
    /// The shift level matters because the guest sends the modifier keys
    /// themselves as ordinary events: by the time the character arrives, the
    /// host really does have Shift down, so `A` has to be looked up at level 1
    /// of whatever key carries it — not typed as a bare `a`.
    fn keycode_for(&mut self, keysym: u32, modifiers: u32) -> Result<u8> {
        let level = u8::from(modifiers & lumepeer_core::protocol::MODIFIER_SHIFT != 0);
        if let Some(&(keycode, at)) = self.layout.get(&keysym)
            && at == level
        {
            return Ok(keycode);
        }
        self.bind_scratch(keysym)
    }

    /// Binds `keysym` to the spare keycode and returns it.
    ///
    /// Both shift levels get the same keysym on purpose: the borrowed key then
    /// produces the character the guest asked for whether or not Shift happens
    /// to be down, which is the whole point of using it for symbols the host's
    /// layout has no key for.
    fn bind_scratch(&mut self, keysym: u32) -> Result<u8> {
        use x11rb::protocol::xproto::ConnectionExt as _;

        let keycode = self.scratch.ok_or_else(|| {
            MediaError::InputUnavailable(
                "this keyboard layout has no spare keycode to type through".to_owned(),
            )
        })?;
        if self.scratch_keysym == Some(keysym) {
            return Ok(keycode);
        }
        self.connection
            .change_keyboard_mapping(1, keycode, 2, &[keysym, keysym])
            .map_err(|e| MediaError::InputUnavailable(e.to_string()))?
            .check()
            .map_err(|e| MediaError::InputUnavailable(e.to_string()))?;
        self.scratch_keysym = Some(keysym);
        Ok(keycode)
    }
}

/// Hands the borrowed keycode back to the layout it came from.
///
/// The binding is a side effect on the whole X session, not just on this
/// process, so a session that ends must not leave a stray character bound to
/// one of the host's own keys. Best effort by design: if the server is already
/// gone there is nothing left to restore, and failing here would only turn a
/// finished session into an error.
impl Drop for X11Injector {
    fn drop(&mut self) {
        use x11rb::protocol::xproto::ConnectionExt as _;

        let (Some(keycode), Some(_)) = (self.scratch, self.scratch_keysym) else {
            return;
        };
        if let Ok(cookie) =
            self.connection
                .change_keyboard_mapping(1, keycode, 2, &[NO_SYMBOL, NO_SYMBOL])
        {
            let _ = cookie.check();
        }
    }
}

/// X11 keysym for one of the guest's logical key identifiers, or `None` when
/// there is nothing sensible to type.
///
/// The named half mirrors `NAMED_KEYS` in `apps/desktop/src/view-window.ts`
/// one for one, including its `0xe100 + N` F-key encoding; everything else is
/// a Unicode code point, which X11 carries as itself up to Latin-1 and as
/// `0x0100_0000 | code point` above that (`keysymdef.h`).
fn keysym_of(logical: u32) -> Option<u32> {
    let named = match logical {
        0x08 => 0xff08,   // BackSpace
        0x09 => 0xff09,   // Tab
        0x0d => 0xff0d,   // Return
        0x1b => 0xff1b,   // Escape
        0x7f => 0xffff,   // Delete
        0xe000 => 0xff51, // Left
        0xe001 => 0xff52, // Up
        0xe002 => 0xff53, // Right
        0xe003 => 0xff54, // Down
        0xe004 => 0xff50, // Home
        0xe005 => 0xff57, // End
        0xe006 => 0xff55, // Prior (Page Up)
        0xe007 => 0xff56, // Next (Page Down)
        0xe008 => 0xff63, // Insert
        0xe010 => 0xffe1, // Shift_L
        0xe011 => 0xffe3, // Control_L
        0xe012 => 0xffe9, // Alt_L
        0xe013 => 0xffeb, // Super_L
        0xe014 => 0xffe5, // Caps_Lock
        F_KEY_LOGICAL_FIRST..=F_KEY_LOGICAL_LAST => KEYSYM_F1 + (logical - F_KEY_LOGICAL_FIRST),
        _ => 0,
    };
    if named != 0 {
        return Some(named);
    }
    // A lone surrogate or anything else that is not a scalar value is not a
    // character the host could type.
    let ch = char::from_u32(logical)?;
    let code = u32::from(ch);
    Some(if code <= LATIN1_MAX {
        code
    } else {
        UNICODE_KEYSYM_BASE | code
    })
}

/// Reads the host's keyboard layout as `keysym -> (keycode, shift level)`, plus
/// a keycode bound to nothing that can be borrowed for characters the layout
/// has no key for.
///
/// Only the first two levels are indexed: those are the ones a `Shift` state
/// reaches, and the group/AltGr levels above them need a modifier the guest
/// protocol does not carry — the scratch keycode covers those characters
/// instead. A layout that cannot be read at all yields an empty map, which
/// simply means every character goes through the scratch key.
fn read_layout(
    connection: &RustConnection,
) -> (std::collections::HashMap<u32, (u8, u8)>, Option<u8>) {
    use x11rb::protocol::xproto::ConnectionExt as _;

    let setup = connection.setup();
    let first = setup.min_keycode;
    let count = setup.max_keycode.saturating_sub(first).saturating_add(1);
    let reply = match connection
        .get_keyboard_mapping(first, count)
        .map_err(|e| e.to_string())
        .and_then(|cookie| cookie.reply().map_err(|e| e.to_string()))
    {
        Ok(reply) => reply,
        Err(error) => {
            tracing::warn!(
                %error,
                "cannot read the X11 keyboard mapping; typing will use a spare keycode"
            );
            return (std::collections::HashMap::new(), None);
        }
    };

    let per_keycode = usize::from(reply.keysyms_per_keycode);
    let mut layout = std::collections::HashMap::new();
    let mut scratch = None;
    for (index, syms) in reply.keysyms.chunks(per_keycode.max(1)).enumerate() {
        let Ok(offset) = u8::try_from(index) else {
            break;
        };
        let Some(keycode) = first.checked_add(offset) else {
            break;
        };
        if syms.iter().all(|&sym| sym == NO_SYMBOL) {
            // Keep the last free one rather than the first: the low end of the
            // range is likelier to be reserved by something that simply has no
            // symbol bound today.
            scratch = Some(keycode);
            continue;
        }
        for (level, &sym) in syms.iter().take(2).enumerate() {
            if sym == NO_SYMBOL {
                continue;
            }
            let Ok(level) = u8::try_from(level) else {
                continue;
            };
            // First key wins: a layout that repeats a keysym is ambiguous, and
            // the earlier keycode is the one a user would press.
            layout.entry(sym).or_insert((keycode, level));
        }
    }
    if scratch.is_none() {
        tracing::warn!(
            "no unbound X11 keycode is available; characters outside the host's \
             layout cannot be typed"
        );
    }
    (layout, scratch)
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
                    self.key(event.logical, event.scancode, event.modifiers, pressed)
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
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "a failed assumption must fail the test"
    )]

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

    /// The regression this backend actually had: a webview guest sends
    /// `scancode: 0` and puts the meaning in `logical`, so every keystroke has
    /// to be resolved through a keysym. The named half must line up with
    /// `view-window.ts`'s `NAMED_KEYS` table one for one.
    #[test]
    fn named_keys_map_to_the_matching_x11_keysym() {
        assert_eq!(keysym_of(0x08), Some(0xff08)); // BackSpace
        assert_eq!(keysym_of(0x0d), Some(0xff0d)); // Return
        assert_eq!(keysym_of(0x1b), Some(0xff1b)); // Escape
        assert_eq!(keysym_of(0x7f), Some(0xffff)); // Delete
        assert_eq!(keysym_of(0xe000), Some(0xff51)); // Left
        assert_eq!(keysym_of(0xe003), Some(0xff54)); // Down
        assert_eq!(keysym_of(0xe010), Some(0xffe1)); // Shift_L
        assert_eq!(keysym_of(0xe014), Some(0xffe5)); // Caps_Lock
        // F1 and F24, the ends of the `0xe100 + N` encoding.
        assert_eq!(keysym_of(0xe101), Some(KEYSYM_F1));
        assert_eq!(keysym_of(0xe118), Some(KEYSYM_F1 + F_KEY_COUNT - 1));
    }

    /// Latin-1 code points are their own keysym; everything above them is
    /// carried in the Unicode range (`keysymdef.h`).
    #[test]
    fn characters_map_to_their_code_point_or_the_unicode_keysym_range() {
        assert_eq!(keysym_of(u32::from(b'a')), Some(0x61));
        assert_eq!(keysym_of(u32::from(b'A')), Some(0x41));
        assert_eq!(keysym_of(u32::from(b' ')), Some(0x20));
        // ä: still Latin-1, so still itself.
        assert_eq!(keysym_of(0xe4), Some(0xe4));
        // € and 😀: outside Latin-1.
        assert_eq!(keysym_of(0x20ac), Some(UNICODE_KEYSYM_BASE | 0x20ac));
        assert_eq!(keysym_of(0x1_f600), Some(UNICODE_KEYSYM_BASE | 0x1_f600));
        // A lone surrogate is not a character anyone can type.
        assert_eq!(keysym_of(0xd800), None);
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

    /// The keyboard fix end to end against a real server: a guest keystroke
    /// with `scancode: 0` has to come out the other side as a key the X server
    /// reports as physically down.
    ///
    /// `Shift` is the one key this can safely use on a live desktop: it is in
    /// every layout, and pressing and releasing it on its own types nothing
    /// and activates nothing in whatever window happens to have focus. Shares
    /// `LUMEPEER_TEST_XTEST`'s opt-in for the same reason the pointer test
    /// does.
    #[test]
    fn a_webview_keystroke_reaches_the_server_as_a_real_key() {
        use x11rb::protocol::xproto::ConnectionExt as _;

        if std::env::var("LUMEPEER_TEST_XTEST").as_deref() != Ok("1") {
            return;
        }
        let Ok(mut injector) = X11Injector::connect() else {
            return;
        };
        // 0xe010 is `Shift` in view-window.ts's NAMED_KEYS, and a webview
        // guest has no scancode to send with it.
        let shift = keysym_of(0xe010).expect("Shift is a named key");
        let keycode = injector.keycode_for(shift, 0).expect("Shift is mappable");
        let down = |injector: &mut X11Injector| {
            let keys = injector.connection.query_keymap().unwrap().reply().unwrap();
            keys.keys[usize::from(keycode) / 8] & (1 << (keycode % 8)) != 0
        };

        let press = |pressed| InputEventPayload {
            logical: 0xe010,
            scancode: 0,
            modifiers: 0,
            detail: if pressed {
                InputDetail::Press
            } else {
                InputDetail::Release
            },
        };

        assert!(
            !down(&mut injector),
            "Shift was already held before the test"
        );
        injector.inject(&press(true)).unwrap();
        assert!(
            down(&mut injector),
            "a keystroke carrying only a logical id never reached the server"
        );
        injector.inject(&press(false)).unwrap();
        assert!(!down(&mut injector), "the release never reached the server");
    }

    /// The other half of the keyboard fix, against a real server: resolving a
    /// character has to land on a keycode the X server agrees carries that
    /// keysym. Nothing is injected — this only reads the mapping back — but it
    /// still needs a display, so it shares `LUMEPEER_TEST_XTEST`'s opt-in.
    #[test]
    fn a_character_resolves_to_a_keycode_the_server_agrees_with() {
        use x11rb::protocol::xproto::ConnectionExt as _;

        if std::env::var("LUMEPEER_TEST_XTEST").as_deref() != Ok("1") {
            return;
        }
        let Ok(mut injector) = X11Injector::connect() else {
            return;
        };

        for (logical, modifiers) in [
            (u32::from(b'a'), 0),
            (u32::from(b'A'), lumepeer_core::protocol::MODIFIER_SHIFT),
            (0x0d, 0),   // Return
            (0x20ac, 0), // €, which most layouts cannot reach directly
        ] {
            let keysym = keysym_of(logical).expect("every case here is typeable");
            let keycode = injector
                .keycode_for(keysym, modifiers)
                .unwrap_or_else(|e| panic!("no keycode for logical {logical:#x}: {e}"));
            let mapping = injector
                .connection
                .get_keyboard_mapping(keycode, 1)
                .unwrap()
                .reply()
                .unwrap();
            assert!(
                mapping.keysyms.contains(&keysym),
                "keycode {keycode} does not carry keysym {keysym:#x} for logical {logical:#x}"
            );
        }
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
