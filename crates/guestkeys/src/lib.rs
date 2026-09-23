//! The guest's own keyboard, for the keys its window can never see (ADR 0090).
//!
//! A view window forwards what the operator types to another machine, and it
//! does that from `KeyboardEvent`s in a webview. Two whole classes of
//! keystroke never arrive there, so for two different reasons the same thing
//! happened: the chord did something on the operator's own machine and nothing
//! on the remote one.
//!
//! - **The webview eats some of them.** `WebView2` claims a set of
//!   "browser accelerator keys" — reload, print, find, view-source, save,
//!   zoom, the developer tools, back and forward — and acts on them itself.
//!   `Ctrl+R` reloaded the remote *picture* instead of reloading anything on
//!   the host. [`keep_accelerators_for_the_remote_machine`] turns that off for
//!   one webview.
//! - **The OS eats the rest.** `Win+D`, `Alt+Tab`, `Ctrl+Esc`,
//!   `Ctrl+Shift+Esc`, `Alt+F4`, `PrintScreen`: the shell claims these before
//!   any window sees them, so no amount of `preventDefault` reaches them — and
//!   so does any other program on this machine that registered a global
//!   hotkey. [`grab`] installs a low-level keyboard hook that takes every
//!   chord ([`route`], ADR 0107) and hands it over instead.
//!
//! **Nothing here decides anything about a session.** A grabbed keystroke
//! goes to the same `SessionManager::authorize_input` on the host as one that
//! came out of the webview, over the same `InputEventPayload`; this crate's
//! whole job is to notice the keystroke at all. It is a crate of its own
//! because the desktop app is `#![forbid(unsafe_code)]` and neither
//! `SetWindowsHookExW` nor a COM property setter has a safe binding — the same
//! reason `lumepeer-terminal` is a crate (ADR 0079).
//!
//! Off Windows every entry point here is a documented no-op: X11 and Wayland
//! do not hand a client the compositor's own chords, and a `WebKit` view claims
//! far less than `WebView2` does. Both are their own tasks and neither is
//! pretended to be solved here (§18: say so rather than fail quietly).

#[cfg(target_os = "windows")]
mod windows_hook;
#[cfg(target_os = "windows")]
mod windows_webview;

#[cfg(target_os = "windows")]
pub use windows_webview::keep_accelerators_for_the_remote_machine;

/// One keystroke the operator's own machine was about to act on, in the terms
/// the wire already uses (§9.1).
///
/// `logical` is deliberately absent: it is always 0 here, which
/// [`lumepeer_core::protocol::names_a_key`] reads as "this key has no
/// character meaning", so the host presses it **by position** — the only
/// correct answer for a chord, and the one that also avoids the
/// generic-modifier trap a released `Ctrl` walks into (ADR 0065).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrabbedKey {
    /// Physical key, as the evdev code `InputEventPayload::scancode` carries.
    pub scancode: u32,
    /// Modifier bitmask, as `lumepeer_core::protocol`'s `MODIFIER_*` bits.
    pub modifiers: u32,
    /// `true` for a press, `false` for a release.
    pub pressed: bool,
}

/// Where a grabbed keystroke goes. The desktop app wires this to the actor.
pub type Sink = std::sync::Arc<dyn Fn(GrabbedKey) + Send + Sync>;

/// Modifier keys held at the moment a keystroke happened.
///
/// Its own type rather than the packed bitmask because the decision in
/// [`route`] reads the modifiers one at a time, and a bitmask at that call
/// site is how the wrong bit gets tested.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "a keyboard has exactly these four modifiers; a bitfield here is               what this type exists to replace"
)]
pub struct Held {
    /// Either `Shift`.
    pub shift: bool,
    /// Either `Ctrl`.
    pub ctrl: bool,
    /// Either `Alt`.
    pub alt: bool,
    /// Either `Win`.
    pub meta: bool,
}

impl Held {
    /// The modifiers among `positions`, which are the evdev codes a grab has
    /// sent a press for and not yet a release.
    ///
    /// Read from what the grab sent rather than asked of the OS:
    /// `GetAsyncKeyState` inside a `WH_KEYBOARD_LL` callback does not yet
    /// reflect the event being handled, and never reflects one the hook
    /// swallowed. `Win` is swallowed, so a grab that asked the OS never saw
    /// it held — `Win+D` left as `Win` and a stray 'd'.
    #[must_use]
    pub fn of_positions(positions: &std::collections::BTreeSet<u32>) -> Self {
        let any = |codes: [u32; 2]| codes.iter().any(|code| positions.contains(code));
        Self {
            shift: any([EVDEV_LEFT_SHIFT, EVDEV_RIGHT_SHIFT]),
            ctrl: any([EVDEV_LEFT_CTRL, EVDEV_RIGHT_CTRL]),
            alt: any([EVDEV_LEFT_ALT, EVDEV_RIGHT_ALT]),
            meta: any([EVDEV_LEFT_META, EVDEV_RIGHT_META]),
        }
    }

    /// These modifiers as the wire's `MODIFIER_*` bitmask.
    #[must_use]
    pub const fn bits(self) -> u32 {
        use lumepeer_core::protocol::{MODIFIER_ALT, MODIFIER_CTRL, MODIFIER_META, MODIFIER_SHIFT};
        let mut bits = 0;
        if self.shift {
            bits |= MODIFIER_SHIFT;
        }
        if self.ctrl {
            bits |= MODIFIER_CTRL;
        }
        if self.alt {
            bits |= MODIFIER_ALT;
        }
        if self.meta {
            bits |= MODIFIER_META;
        }
        bits
    }
}

/// evdev code of the left `Shift`.
const EVDEV_LEFT_SHIFT: u32 = 42;
/// evdev code of the right `Shift`.
const EVDEV_RIGHT_SHIFT: u32 = 54;
/// evdev code of the left `Ctrl`.
const EVDEV_LEFT_CTRL: u32 = 29;
/// evdev code of the right `Ctrl`.
const EVDEV_RIGHT_CTRL: u32 = 97;
/// evdev code of the left `Alt`.
const EVDEV_LEFT_ALT: u32 = 56;
/// evdev code of the right `Alt` (`AltGr` on many layouts).
const EVDEV_RIGHT_ALT: u32 = 100;
/// evdev code of the left `Win`.
const EVDEV_LEFT_META: u32 = 125;
/// evdev code of the right `Win`.
const EVDEV_RIGHT_META: u32 = 126;

/// Virtual-key codes this crate names. Windows-only values, but the table and
/// the decision that reads it are plain integers, so both compile — and are
/// tested — everywhere.
mod vk {
    /// `VK_BACK`.
    pub(crate) const BACK: u16 = 0x08;
    /// `VK_TAB`.
    pub(crate) const TAB: u16 = 0x09;
    /// `VK_CLEAR`, which is the numpad 5 with `NumLock` off.
    pub(crate) const CLEAR: u16 = 0x0C;
    /// `VK_RETURN`.
    pub(crate) const RETURN: u16 = 0x0D;
    /// `VK_PAUSE`.
    pub(crate) const PAUSE: u16 = 0x13;
    /// `VK_CAPITAL`.
    pub(crate) const CAPITAL: u16 = 0x14;
    /// `VK_ESCAPE`.
    pub(crate) const ESCAPE: u16 = 0x1B;
    /// `VK_SPACE`.
    pub(crate) const SPACE: u16 = 0x20;
    /// `VK_PRIOR` (Page Up).
    pub(crate) const PRIOR: u16 = 0x21;
    /// `VK_NEXT` (Page Down).
    pub(crate) const NEXT: u16 = 0x22;
    /// `VK_END`.
    pub(crate) const END: u16 = 0x23;
    /// `VK_HOME`.
    pub(crate) const HOME: u16 = 0x24;
    /// `VK_LEFT`.
    pub(crate) const LEFT: u16 = 0x25;
    /// `VK_UP`.
    pub(crate) const UP: u16 = 0x26;
    /// `VK_RIGHT`.
    pub(crate) const RIGHT: u16 = 0x27;
    /// `VK_DOWN`.
    pub(crate) const DOWN: u16 = 0x28;
    /// `VK_SNAPSHOT` (Print Screen).
    pub(crate) const SNAPSHOT: u16 = 0x2C;
    /// `VK_INSERT`.
    pub(crate) const INSERT: u16 = 0x2D;
    /// `VK_DELETE`.
    pub(crate) const DELETE: u16 = 0x2E;
    /// `VK_0`.
    pub(crate) const DIGIT0: u16 = 0x30;
    /// `VK_1`.
    pub(crate) const DIGIT1: u16 = 0x31;
    /// `VK_9`.
    pub(crate) const DIGIT9: u16 = 0x39;
    /// `VK_A`.
    pub(crate) const LETTER_A: u16 = 0x41;
    /// `VK_B`.
    pub(crate) const LETTER_B: u16 = 0x42;
    /// `VK_C`.
    pub(crate) const LETTER_C: u16 = 0x43;
    /// `VK_D`.
    pub(crate) const LETTER_D: u16 = 0x44;
    /// `VK_E`.
    pub(crate) const LETTER_E: u16 = 0x45;
    /// `VK_F`.
    pub(crate) const LETTER_F: u16 = 0x46;
    /// `VK_G`.
    pub(crate) const LETTER_G: u16 = 0x47;
    /// `VK_H`.
    pub(crate) const LETTER_H: u16 = 0x48;
    /// `VK_I`.
    pub(crate) const LETTER_I: u16 = 0x49;
    /// `VK_J`.
    pub(crate) const LETTER_J: u16 = 0x4A;
    /// `VK_K`.
    pub(crate) const LETTER_K: u16 = 0x4B;
    /// `VK_L`.
    pub(crate) const LETTER_L: u16 = 0x4C;
    /// `VK_M`.
    pub(crate) const LETTER_M: u16 = 0x4D;
    /// `VK_N`.
    pub(crate) const LETTER_N: u16 = 0x4E;
    /// `VK_O`.
    pub(crate) const LETTER_O: u16 = 0x4F;
    /// `VK_P`.
    pub(crate) const LETTER_P: u16 = 0x50;
    /// `VK_Q`.
    pub(crate) const LETTER_Q: u16 = 0x51;
    /// `VK_R`.
    pub(crate) const LETTER_R: u16 = 0x52;
    /// `VK_S`.
    pub(crate) const LETTER_S: u16 = 0x53;
    /// `VK_T`.
    pub(crate) const LETTER_T: u16 = 0x54;
    /// `VK_U`.
    pub(crate) const LETTER_U: u16 = 0x55;
    /// `VK_V`.
    pub(crate) const LETTER_V: u16 = 0x56;
    /// `VK_W`.
    pub(crate) const LETTER_W: u16 = 0x57;
    /// `VK_X`.
    pub(crate) const LETTER_X: u16 = 0x58;
    /// `VK_Y`.
    pub(crate) const LETTER_Y: u16 = 0x59;
    /// `VK_Z`.
    pub(crate) const LETTER_Z: u16 = 0x5A;
    /// `VK_LWIN`.
    pub(crate) const LWIN: u16 = 0x5B;
    /// `VK_RWIN`.
    pub(crate) const RWIN: u16 = 0x5C;
    /// `VK_APPS` (the context-menu key).
    pub(crate) const APPS: u16 = 0x5D;
    /// `VK_NUMPAD0`.
    pub(crate) const NUMPAD0: u16 = 0x60;
    /// `VK_NUMPAD1`.
    pub(crate) const NUMPAD1: u16 = 0x61;
    /// `VK_NUMPAD2`.
    pub(crate) const NUMPAD2: u16 = 0x62;
    /// `VK_NUMPAD3`.
    pub(crate) const NUMPAD3: u16 = 0x63;
    /// `VK_NUMPAD4`.
    pub(crate) const NUMPAD4: u16 = 0x64;
    /// `VK_NUMPAD5`.
    pub(crate) const NUMPAD5: u16 = 0x65;
    /// `VK_NUMPAD6`.
    pub(crate) const NUMPAD6: u16 = 0x66;
    /// `VK_NUMPAD7`.
    pub(crate) const NUMPAD7: u16 = 0x67;
    /// `VK_NUMPAD8`.
    pub(crate) const NUMPAD8: u16 = 0x68;
    /// `VK_NUMPAD9`.
    pub(crate) const NUMPAD9: u16 = 0x69;
    /// `VK_MULTIPLY`.
    pub(crate) const MULTIPLY: u16 = 0x6A;
    /// `VK_ADD`.
    pub(crate) const ADD: u16 = 0x6B;
    /// `VK_SUBTRACT`.
    pub(crate) const SUBTRACT: u16 = 0x6D;
    /// `VK_DECIMAL`.
    pub(crate) const DECIMAL: u16 = 0x6E;
    /// `VK_DIVIDE`.
    pub(crate) const DIVIDE: u16 = 0x6F;
    /// `VK_F1`.
    pub(crate) const F1: u16 = 0x70;
    /// `VK_F10`.
    pub(crate) const F10: u16 = 0x79;
    /// `VK_F11`.
    pub(crate) const F11: u16 = 0x7A;
    /// `VK_F12`.
    pub(crate) const F12: u16 = 0x7B;
    /// `VK_F13`.
    pub(crate) const F13: u16 = 0x7C;
    /// `VK_F24`.
    pub(crate) const F24: u16 = 0x87;
    /// `VK_NUMLOCK`.
    pub(crate) const NUMLOCK: u16 = 0x90;
    /// `VK_SCROLL`.
    pub(crate) const SCROLL: u16 = 0x91;
    /// `VK_LSHIFT`.
    pub(crate) const LSHIFT: u16 = 0xA0;
    /// `VK_RSHIFT`.
    pub(crate) const RSHIFT: u16 = 0xA1;
    /// `VK_LCONTROL`.
    pub(crate) const LCONTROL: u16 = 0xA2;
    /// `VK_RCONTROL`.
    pub(crate) const RCONTROL: u16 = 0xA3;
    /// `VK_LMENU` (left Alt).
    pub(crate) const LMENU: u16 = 0xA4;
    /// `VK_RMENU` (right Alt, which is `AltGr` on many layouts).
    pub(crate) const RMENU: u16 = 0xA5;
    /// `VK_VOLUME_MUTE`.
    pub(crate) const VOLUME_MUTE: u16 = 0xAD;
    /// `VK_VOLUME_DOWN`.
    pub(crate) const VOLUME_DOWN: u16 = 0xAE;
    /// `VK_VOLUME_UP`.
    pub(crate) const VOLUME_UP: u16 = 0xAF;
    /// `VK_OEM_1` (`;` on a US layout).
    pub(crate) const OEM_1: u16 = 0xBA;
    /// `VK_OEM_PLUS`.
    pub(crate) const OEM_PLUS: u16 = 0xBB;
    /// `VK_OEM_COMMA`.
    pub(crate) const OEM_COMMA: u16 = 0xBC;
    /// `VK_OEM_MINUS`.
    pub(crate) const OEM_MINUS: u16 = 0xBD;
    /// `VK_OEM_PERIOD`.
    pub(crate) const OEM_PERIOD: u16 = 0xBE;
    /// `VK_OEM_2` (`/`).
    pub(crate) const OEM_2: u16 = 0xBF;
    /// `VK_OEM_3` (`` ` ``).
    pub(crate) const OEM_3: u16 = 0xC0;
    /// `VK_OEM_4` (`[`).
    pub(crate) const OEM_4: u16 = 0xDB;
    /// `VK_OEM_5` (`\`).
    pub(crate) const OEM_5: u16 = 0xDC;
    /// `VK_OEM_6` (`]`).
    pub(crate) const OEM_6: u16 = 0xDD;
    /// `VK_OEM_7` (`'`).
    pub(crate) const OEM_7: u16 = 0xDE;
    /// `VK_OEM_8`, the last of the layout-defined punctuation keys.
    pub(crate) const OEM_8: u16 = 0xDF;
    /// `VK_OEM_102`, the extra key on a 102-key keyboard.
    pub(crate) const OEM_102: u16 = 0xE2;
}

/// Where a live grab sends one keystroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Left to this machine. The webview sees it, and `ViewInput` forwards it
    /// if it is typing — as a character, in the operator's own layout
    /// (ADR 0065).
    Here,
    /// Sent to the remote machine and hidden from this one.
    There,
    /// Sent to the remote machine *and* let through here: `Ctrl`, `Alt` and
    /// `Shift` themselves. This machine has to go on seeing them, or the view
    /// window could not recognise its own `Ctrl+Alt+Shift` chords, and a
    /// `Ctrl` that one side believes held and the other does not is how keys
    /// get stuck. The view window must therefore not forward them a second
    /// time; see [`shared_with_this_machine`].
    Both,
}

/// Where a live grab sends this keystroke (ADR 0107).
///
/// `sent` is whether the grab has already sent this key's press and not yet
/// its release; `held` is the modifiers it has sent the same way
/// ([`Held::of_positions`]).
///
/// **Every chord is the remote machine's.** ADR 0090 took only the chords no
/// window can receive and left the rest to the webview, and that was not
/// enough, for two reasons. Whatever this machine claims first — the shell, a
/// global hotkey another program registered, the webview — never reached the
/// webview at all. And what did reach it travelled a different path from the
/// grab's, so `Alt` could arrive at the host after the `Tab` it was held for.
/// So a grab now takes, in one ordered stream:
///
/// - **`Ctrl`, `Alt` and `Shift` themselves**, as [`Route::Both`]. `VMware`'s
///   `Ctrl+Alt` is nothing but these.
/// - **Either `Win` key**, hidden from this machine.
/// - **Any key held under `Ctrl`, `Alt` or `Win`**: `Ctrl+G`, `Ctrl+C`,
///   `Alt+Tab`, `Alt+F4`, `Win+D`, `Ctrl+Shift+Esc`, `Alt+Space`.
/// - **Any key under `Shift` that does not type a character**: `Shift+Tab`,
///   `Shift+Del`, `Shift+F10`, `Shift+arrow`. `Shift` travels here, so the
///   key it selects with has to travel with it.
/// - **`PrintScreen`**, which opens the local screen-clipping tool.
///
/// Plain typing is not a chord and stays [`Route::Here`]: a character typed in
/// the operator's layout is still sent as that character, whatever the host's
/// layout is.
///
/// Also left here:
///
/// - **The view window's own chords**, everything under exactly
///   `Ctrl+Alt+Shift`: they include the one that releases this grab.
/// - **`Ctrl+Alt+Del`.** The Secure Attention Sequence is not hookable by
///   design, which is what makes it secure; this machine acts on it whatever a
///   hook answers, and the view window sends the host its own as a request
///   (ADR 0028).
/// - **`Win+L`**, which locks this machine the same way.
/// - **A release whose press the grab did not send.** The key went down before
///   the grab did, so this machine has it down and has to see it come up.
///
/// A key the grab sent down stays the remote machine's until it comes up,
/// whatever was let go of in between, so a press and its release always
/// travel the same way.
#[must_use]
pub const fn route(virtual_key: u16, pressed: bool, sent: bool, held: Held) -> Route {
    if matches!(
        virtual_key,
        vk::LSHIFT | vk::RSHIFT | vk::LCONTROL | vk::RCONTROL | vk::LMENU | vk::RMENU
    ) {
        return Route::Both;
    }
    if sent {
        return Route::There;
    }
    if !pressed {
        return Route::Here;
    }
    // `Win` is the one modifier that is itself unreachable: a window that let
    // the press through has already lost the chord, whatever it does with the
    // key that follows.
    if matches!(virtual_key, vk::LWIN | vk::RWIN) {
        return Route::There;
    }
    if held.ctrl && held.alt && held.shift && !held.meta {
        return Route::Here;
    }
    if held.ctrl && held.alt && matches!(virtual_key, vk::DELETE | vk::DECIMAL) {
        return Route::Here;
    }
    if held.meta && virtual_key == vk::LETTER_L {
        return Route::Here;
    }
    if held.ctrl || held.alt || held.meta {
        return Route::There;
    }
    if held.shift && !types_a_character(virtual_key) {
        return Route::There;
    }
    if virtual_key == vk::SNAPSHOT {
        return Route::There;
    }
    Route::Here
}

/// Whether this key types a character rather than naming an action: the
/// letters, the digit row, the layout's punctuation, space, and the numpad
/// with `NumLock` on.
///
/// With `NumLock` off the numpad reports `VK_HOME` and friends instead, which
/// is exactly right: those keys then move, and `Shift` selects with them.
const fn types_a_character(virtual_key: u16) -> bool {
    matches!(
        virtual_key,
        vk::SPACE
            | vk::DIGIT0..=vk::DIGIT9
            | vk::LETTER_A..=vk::LETTER_Z
            | vk::NUMPAD0..=vk::DIVIDE
            | vk::OEM_1..=vk::OEM_3
            | vk::OEM_4..=vk::OEM_8
            | vk::OEM_102
    )
}

/// Whether a live grab sends this physical key to the remote machine *and*
/// lets it through here — `Ctrl`, `Alt` and `Shift` ([`Route::Both`]) — so a
/// view window sees it too and must not forward it a second time.
///
/// `scancode` is an evdev code, as a view window sends it.
#[must_use]
pub const fn shared_with_this_machine(scancode: u32) -> bool {
    matches!(
        scancode,
        EVDEV_LEFT_SHIFT
            | EVDEV_RIGHT_SHIFT
            | EVDEV_LEFT_CTRL
            | EVDEV_RIGHT_CTRL
            | EVDEV_LEFT_ALT
            | EVDEV_RIGHT_ALT
    )
}

/// The evdev code for a physical key named by its Windows virtual key, or
/// `None` for one this table does not place.
///
/// evdev because that is what `InputEventPayload::scancode` has meant since
/// ADR 0065: one physical encoding crosses the wire and each host platform
/// translates from it. This is the exact inverse of `physical_key` in
/// `crates/media/src/capture/windows.rs`, which turns the same codes back into
/// virtual keys on the host — the two tables are each other's mirror and a
/// change to one is a change to both.
///
/// `extended` is the `E0` prefix the hook reports, and it is not decoration:
/// it is the only thing that tells the navigation cluster from the numpad
/// (which reports `VK_HOME` and friends while `NumLock` is off) and the
/// numpad's `Enter` from the main one.
///
/// `None` is not an error, it is "this build does not know where that key
/// sits" — the caller passes the keystroke on to the ordinary path rather than
/// forwarding a position it made up.
///
/// A `Shift` with the `E0` prefix is `None` for that reason. Neither `Shift`
/// carries the prefix; one that does is a *fake* shift the keyboard or the OS
/// wraps around a numpad or navigation key to undo `NumLock`, and the host's
/// own OS makes the same ones for the same key. Forwarded, it would hold
/// `Shift` down over there around an arrow nobody meant to select with.
#[must_use]
pub fn evdev_of_virtual_key(virtual_key: u16, extended: bool) -> Option<u32> {
    // The keys that exist in two places and are told apart only by the
    // prefix. Without it these virtual keys come from the numpad rather than
    // the navigation cluster: `NumLock` off relabels the numpad's keys and
    // leaves their scan codes exactly where they were.
    if extended {
        if virtual_key == vk::RETURN {
            return Some(96);
        }
        if matches!(virtual_key, vk::LSHIFT | vk::RSHIFT) {
            return None;
        }
    } else if let Some(position) = numpad_position(virtual_key) {
        return Some(position);
    }
    if let Some(position) = letter_position(virtual_key) {
        return Some(position);
    }
    Some(match virtual_key {
        vk::ESCAPE => 1,
        // The digit row: '1'..'9' run consecutively, and '0' sits after them
        // on the keyboard but before them in the virtual-key numbering.
        vk::DIGIT1..=vk::DIGIT9 => 2 + u32::from(virtual_key - vk::DIGIT1),
        vk::DIGIT0 => 11,
        vk::OEM_MINUS => 12,
        vk::OEM_PLUS => 13,
        vk::BACK => 14,
        vk::TAB => 15,
        vk::OEM_4 => 26,
        vk::OEM_6 => 27,
        vk::RETURN => 28,
        vk::LCONTROL => EVDEV_LEFT_CTRL,
        vk::OEM_1 => 39,
        vk::OEM_7 => 40,
        vk::OEM_3 => 41,
        vk::LSHIFT => EVDEV_LEFT_SHIFT,
        vk::OEM_5 => 43,
        vk::OEM_COMMA => 51,
        vk::OEM_PERIOD => 52,
        vk::OEM_2 => 53,
        vk::RSHIFT => EVDEV_RIGHT_SHIFT,
        vk::MULTIPLY => 55,
        vk::LMENU => EVDEV_LEFT_ALT,
        vk::SPACE => 57,
        vk::CAPITAL => 58,
        vk::F1..=vk::F10 => 59 + u32::from(virtual_key - vk::F1),
        vk::NUMLOCK => 69,
        vk::SCROLL => 70,
        vk::SUBTRACT => 74,
        vk::ADD => 78,
        vk::DECIMAL => 83,
        vk::OEM_102 => 86,
        vk::F11 => 87,
        vk::F12 => 88,
        vk::RCONTROL => EVDEV_RIGHT_CTRL,
        vk::DIVIDE => 98,
        vk::SNAPSHOT => 99,
        vk::RMENU => EVDEV_RIGHT_ALT,
        vk::HOME => 102,
        vk::UP => 103,
        vk::PRIOR => 104,
        vk::LEFT => 105,
        vk::RIGHT => 106,
        vk::END => 107,
        vk::DOWN => 108,
        vk::NEXT => 109,
        vk::INSERT => 110,
        vk::DELETE => 111,
        vk::VOLUME_MUTE => 113,
        vk::VOLUME_DOWN => 114,
        vk::VOLUME_UP => 115,
        vk::PAUSE => 119,
        vk::LWIN => EVDEV_LEFT_META,
        vk::RWIN => EVDEV_RIGHT_META,
        vk::APPS => 127,
        vk::F13..=vk::F24 => 183 + u32::from(virtual_key - vk::F13),
        _ => return None,
    })
}

/// Where a key of the numeric keypad sits, for the virtual keys the keypad
/// reports — its own `VK_NUMPAD*` when `NumLock` is on, and the navigation
/// names when it is off. Callers must have ruled out the `E0` prefix first,
/// which is the only thing that tells these from the real navigation cluster.
fn numpad_position(virtual_key: u16) -> Option<u32> {
    Some(match virtual_key {
        vk::HOME | vk::NUMPAD7 => 71,
        vk::UP | vk::NUMPAD8 => 72,
        vk::PRIOR | vk::NUMPAD9 => 73,
        vk::LEFT | vk::NUMPAD4 => 75,
        vk::CLEAR | vk::NUMPAD5 => 76,
        vk::RIGHT | vk::NUMPAD6 => 77,
        vk::END | vk::NUMPAD1 => 79,
        vk::DOWN | vk::NUMPAD2 => 80,
        vk::NEXT | vk::NUMPAD3 => 81,
        vk::INSERT | vk::NUMPAD0 => 82,
        vk::DELETE => 83,
        _ => return None,
    })
}

/// Where a letter key sits. A table rather than arithmetic because the
/// alphabet's virtual keys are in alphabetical order and the keyboard's rows
/// are not.
fn letter_position(virtual_key: u16) -> Option<u32> {
    Some(match virtual_key {
        vk::LETTER_Q => 16,
        vk::LETTER_W => 17,
        vk::LETTER_E => 18,
        vk::LETTER_R => 19,
        vk::LETTER_T => 20,
        vk::LETTER_Y => 21,
        vk::LETTER_U => 22,
        vk::LETTER_I => 23,
        vk::LETTER_O => 24,
        vk::LETTER_P => 25,
        vk::LETTER_A => 30,
        vk::LETTER_S => 31,
        vk::LETTER_D => 32,
        vk::LETTER_F => 33,
        vk::LETTER_G => 34,
        vk::LETTER_H => 35,
        vk::LETTER_J => 36,
        vk::LETTER_K => 37,
        vk::LETTER_L => 38,
        vk::LETTER_Z => 44,
        vk::LETTER_X => 45,
        vk::LETTER_C => 46,
        vk::LETTER_V => 47,
        vk::LETTER_B => 48,
        vk::LETTER_N => 49,
        vk::LETTER_M => 50,
        _ => return None,
    })
}

/// Takes the chords of [`route`] away from this machine and sends them to
/// `sink` instead, until the returned guard drops.
///
/// Returns `None` where there is nothing to install — every platform but
/// Windows — and on Windows when the hook cannot be installed, which is a
/// warning and not a failure: the ordinary webview path still carries every
/// key it can see.
///
/// The guard releases, over there, every key the grab is still holding down
/// (§11's rule that a press this side sent must be followed by its release).
/// That is what keeps `Win` from being left down on the host when the operator
/// clicks away mid-chord.
#[must_use]
pub fn grab(sink: Sink) -> Option<Grab> {
    #[cfg(target_os = "windows")]
    {
        windows_hook::install(sink).map(Grab)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = sink;
        tracing::info!(
            "no keyboard grab on this platform: the compositor's own chords stay with it"
        );
        None
    }
}

/// A live keyboard grab. Dropping it gives the chords back to this machine.
#[cfg(target_os = "windows")]
#[derive(Debug)]
pub struct Grab(
    #[allow(
        dead_code,
        reason = "held for its Drop, which is what unhooks and releases every                   key the grab was holding down"
    )]
    windows_hook::Installed,
);

/// A live keyboard grab. Never constructed off Windows, where [`grab`] always
/// answers `None`.
#[cfg(not(target_os = "windows"))]
#[derive(Debug)]
pub struct Grab(());

#[cfg(test)]
mod tests {
    use lumepeer_core::protocol::{MODIFIER_ALT, MODIFIER_CTRL, MODIFIER_META, MODIFIER_SHIFT};

    use super::*;

    const NONE: Held = Held {
        shift: false,
        ctrl: false,
        alt: false,
        meta: false,
    };

    const SHIFT: Held = Held {
        shift: true,
        ..NONE
    };
    const CTRL: Held = Held { ctrl: true, ..NONE };
    const ALT: Held = Held { alt: true, ..NONE };
    const META: Held = Held { meta: true, ..NONE };

    /// `VK_F4`, named once for the `Alt+F4` cases.
    const F4: u16 = vk::F1 + 3;

    /// Where a fresh press goes: nothing about it sent yet.
    const fn press(virtual_key: u16, held: Held) -> Route {
        route(virtual_key, true, false, held)
    }

    /// Every chord goes to the remote machine, including the ones this report
    /// was about — `VMware`'s `Ctrl+G`, and `Ctrl+Alt`, which is nothing but
    /// its two modifiers — and the ones ADR 0090 already took.
    #[test]
    fn a_grab_sends_every_chord() {
        let ctrl_shift = Held {
            shift: true,
            ..CTRL
        };
        let ctrl_alt = Held { alt: true, ..CTRL };
        for (virtual_key, held, chord) in [
            (vk::LETTER_G, CTRL, "Ctrl+G"),
            (vk::LETTER_C, CTRL, "Ctrl+C"),
            (vk::LETTER_V, CTRL, "Ctrl+V"),
            (vk::LETTER_T, ctrl_shift, "Ctrl+Shift+T"),
            (vk::ESCAPE, CTRL, "Ctrl+Esc"),
            (vk::ESCAPE, ctrl_shift, "Ctrl+Shift+Esc"),
            (vk::LEFT, ctrl_alt, "Ctrl+Alt+Left"),
            (vk::INSERT, ctrl_alt, "Ctrl+Alt+Ins"),
            (vk::TAB, ALT, "Alt+Tab"),
            (vk::TAB, Held { shift: true, ..ALT }, "Alt+Shift+Tab"),
            (F4, ALT, "Alt+F4"),
            (vk::SPACE, ALT, "Alt+Space"),
            (vk::LETTER_D, META, "Win+D"),
            (vk::LEFT, Held { ctrl: true, ..META }, "Win+Ctrl+Left"),
            (vk::TAB, SHIFT, "Shift+Tab"),
            (vk::DELETE, SHIFT, "Shift+Del"),
            (vk::F1 + 9, SHIFT, "Shift+F10"),
            (vk::RIGHT, SHIFT, "Shift+Right"),
            (vk::SNAPSHOT, NONE, "PrintScreen"),
            (vk::LWIN, NONE, "Win"),
            (vk::RWIN, NONE, "right Win"),
        ] {
            assert_eq!(press(virtual_key, held), Route::There, "{chord}");
        }
    }

    /// `Ctrl`, `Alt` and `Shift` go to the remote machine and stay visible
    /// here, pressed or released, whatever else is held — including the
    /// half of `Ctrl+Alt+Shift` the window's own chords start with.
    #[test]
    fn ctrl_alt_and_shift_are_sent_and_still_seen_here() {
        let everything = Held {
            shift: true,
            ctrl: true,
            alt: true,
            meta: true,
        };
        for virtual_key in [
            vk::LSHIFT,
            vk::RSHIFT,
            vk::LCONTROL,
            vk::RCONTROL,
            vk::LMENU,
            vk::RMENU,
        ] {
            for held in [NONE, CTRL, everything] {
                for (pressed, sent) in [(true, false), (true, true), (false, true), (false, false)]
                {
                    assert_eq!(
                        route(virtual_key, pressed, sent, held),
                        Route::Both,
                        "{virtual_key:#04x}"
                    );
                }
            }
            assert_eq!(
                evdev_of_virtual_key(virtual_key, false).map(shared_with_this_machine),
                Some(true),
                "a view window would forward {virtual_key:#04x} a second time"
            );
        }
        // The Win keys are hidden from this machine, so a view window never
        // sees them to forward in the first place.
        assert!(!shared_with_this_machine(EVDEV_LEFT_META));
        assert!(!shared_with_this_machine(EVDEV_RIGHT_META));
        // Nor is anything else shared: a pointer button travels with 0.
        assert!(!shared_with_this_machine(0));
        assert!(!shared_with_this_machine(30));
    }

    /// Typing is not a chord. Plain and shifted characters, and the keys a
    /// remote operator types between them, stay on the webview's path, which
    /// sends the character the operator's own layout makes (ADR 0065).
    #[test]
    fn typing_is_left_to_this_machine() {
        for (virtual_key, held) in [
            (vk::LETTER_A, NONE),
            (vk::LETTER_A, SHIFT),
            (vk::DIGIT1, SHIFT),
            (vk::SPACE, SHIFT),
            (vk::OEM_1, SHIFT),
            (vk::OEM_8, SHIFT),
            (vk::OEM_102, SHIFT),
            (vk::NUMPAD0 + 5, SHIFT),
            (vk::DIVIDE, SHIFT),
            (vk::ESCAPE, NONE),
            (vk::TAB, NONE),
            (vk::RETURN, NONE),
            (vk::BACK, NONE),
            (vk::LEFT, NONE),
            (vk::F1 + 4, NONE),
            (vk::DELETE, NONE),
        ] {
            assert_eq!(
                press(virtual_key, held),
                Route::Here,
                "{virtual_key:#04x} under {held:?}"
            );
        }
    }

    /// The view window's own chords all start `Ctrl+Alt+Shift`, and a grab
    /// must not take any of them: they are how the operator releases it
    /// without a mouse.
    #[test]
    fn a_grab_never_takes_the_view_windows_own_chords() {
        let prefix = Held {
            shift: true,
            ctrl: true,
            alt: true,
            meta: false,
        };
        // KeyF, KeyM, Digit0, KeyC, KeyD, KeyT, KeyK.
        for key in [0x46, 0x4D, 0x30, 0x43, 0x44, 0x54, 0x4B] {
            assert_eq!(
                press(key, prefix),
                Route::Here,
                "the grab took one of the window's own chords: {key:#04x}"
            );
        }
        // With `Win` held too it is not the window's chord, and it goes over.
        assert_eq!(
            press(
                0x4B,
                Held {
                    meta: true,
                    ..prefix
                }
            ),
            Route::There
        );
    }

    /// What this machine acts on whatever a hook answers is not sent: the
    /// operator would get it twice, here and there.
    #[test]
    fn the_protected_chords_stay_here() {
        let ctrl_alt = Held { alt: true, ..CTRL };
        assert_eq!(press(vk::DELETE, ctrl_alt), Route::Here);
        // The numpad's Del, with `NumLock` on and off.
        assert_eq!(press(vk::DECIMAL, ctrl_alt), Route::Here);
        assert_eq!(press(vk::LETTER_L, META), Route::Here);
    }

    /// A key the remote machine was sent down stays its own until it comes
    /// up, even once the chord that sent it is over: `Ctrl` let go of before
    /// the `C` must not strand `C` held down over there.
    #[test]
    fn a_key_sent_down_is_sent_up() {
        // The repeat of a key held after its chord ended.
        assert_eq!(route(vk::LETTER_C, true, true, NONE), Route::There);
        // Its release.
        assert_eq!(route(vk::LETTER_C, false, true, NONE), Route::There);
        assert_eq!(route(vk::LWIN, false, true, NONE), Route::There);
        // And a release the grab never sent the press for is this machine's:
        // the key went down before the grab did.
        assert_eq!(route(vk::LETTER_C, false, false, CTRL), Route::Here);
        assert_eq!(route(vk::LWIN, false, false, NONE), Route::Here);
    }

    /// The modifiers a grab holds are the ones it sent, either hand.
    #[test]
    fn the_held_modifiers_are_the_ones_sent() {
        use std::collections::BTreeSet;

        assert_eq!(Held::of_positions(&BTreeSet::new()), NONE);
        assert_eq!(Held::of_positions(&BTreeSet::from([30, 46])), NONE);
        assert_eq!(
            Held::of_positions(&BTreeSet::from([EVDEV_RIGHT_CTRL, 46])),
            CTRL
        );
        assert_eq!(
            Held::of_positions(&BTreeSet::from([
                EVDEV_LEFT_SHIFT,
                EVDEV_RIGHT_ALT,
                EVDEV_LEFT_META
            ])),
            Held {
                shift: true,
                alt: true,
                meta: true,
                ctrl: false
            }
        );
    }

    /// Every position this places must be the one `physical_key` in
    /// `crates/media` turns back into the same virtual key — the two tables
    /// are each other's inverse, and a keystroke that round-trips to a
    /// different key is a keystroke that does the wrong thing on the host.
    #[test]
    fn the_positions_are_the_ones_the_host_reads_them_back_as() {
        // A spot check of every part of the keyboard, written out rather than
        // generated, so the expected value is reviewable next to the key.
        for (virtual_key, extended, evdev) in [
            (vk::ESCAPE, false, 1),
            (vk::DIGIT1, false, 2),
            (vk::DIGIT9, false, 10),
            (vk::DIGIT0, false, 11),
            (0x51, false, 16),
            (0x41, false, 30),
            (0x5A, false, 44),
            (0x43, false, 46),
            (0x56, false, 47),
            (0x4D, false, 50),
            (vk::RETURN, false, 28),
            (vk::RETURN, true, 96),
            (vk::LCONTROL, false, 29),
            (vk::RCONTROL, true, 97),
            (vk::LSHIFT, false, 42),
            (vk::RSHIFT, false, 54),
            (vk::LMENU, false, 56),
            (vk::RMENU, true, 100),
            (vk::LWIN, true, 125),
            (vk::RWIN, true, 126),
            (vk::APPS, true, 127),
            (vk::SPACE, false, 57),
            (vk::F1, false, 59),
            (vk::F10, false, 68),
            (vk::F11, false, 87),
            (vk::F12, false, 88),
            (vk::F13, false, 183),
            (vk::F24, false, 194),
            (vk::SNAPSHOT, true, 99),
            (vk::PAUSE, false, 119),
            (vk::NUMLOCK, false, 69),
            (vk::SCROLL, false, 70),
            // The navigation cluster, and the same keys on the numpad with
            // `NumLock` off.
            (vk::HOME, true, 102),
            (vk::HOME, false, 71),
            (vk::UP, true, 103),
            (vk::UP, false, 72),
            (vk::DELETE, true, 111),
            (vk::DELETE, false, 83),
            // The numpad proper.
            (vk::NUMPAD0, false, 82),
            (vk::NUMPAD0 + 1, false, 79),
            (vk::NUMPAD0 + 5, false, 76),
            (vk::NUMPAD0 + 8, false, 72),
            (vk::NUMPAD9, false, 73),
            (vk::DIVIDE, true, 98),
            (vk::MULTIPLY, false, 55),
            (vk::SUBTRACT, false, 74),
            (vk::ADD, false, 78),
            (vk::DECIMAL, false, 83),
        ] {
            assert_eq!(
                evdev_of_virtual_key(virtual_key, extended),
                Some(evdev),
                "virtual key {virtual_key:#04x} (extended: {extended}) landed on the wrong position"
            );
        }
    }

    /// A key this build cannot place is passed on rather than forwarded at a
    /// position it invented.
    #[test]
    fn a_key_with_no_known_position_is_not_given_one() {
        // IME and browser-control keys, none of which this table places.
        for virtual_key in [0x15u16, 0x1C, 0xA6, 0xA7, 0xFA] {
            assert_eq!(evdev_of_virtual_key(virtual_key, false), None);
        }
        // A fake shift: a real `Shift` never carries the `E0` prefix.
        assert_eq!(evdev_of_virtual_key(vk::LSHIFT, true), None);
        assert_eq!(evdev_of_virtual_key(vk::RSHIFT, true), None);
    }

    /// The bitmask is the wire's, in the wire's order.
    #[test]
    fn the_modifier_bits_are_the_ones_the_host_reads() {
        assert_eq!(NONE.bits(), 0);
        assert_eq!(
            Held {
                shift: true,
                ..NONE
            }
            .bits(),
            MODIFIER_SHIFT
        );
        assert_eq!(Held { ctrl: true, ..NONE }.bits(), MODIFIER_CTRL);
        assert_eq!(Held { alt: true, ..NONE }.bits(), MODIFIER_ALT);
        assert_eq!(Held { meta: true, ..NONE }.bits(), MODIFIER_META);
        assert_eq!(
            Held {
                shift: true,
                ctrl: true,
                alt: true,
                meta: true
            }
            .bits(),
            MODIFIER_SHIFT | MODIFIER_CTRL | MODIFIER_ALT | MODIFIER_META
        );
    }
}
