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
//!   any window sees them, so no amount of `preventDefault` reaches them.
//!   [`grab`] installs a low-level keyboard hook that takes exactly those and
//!   hands them over instead.
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
/// [`claimed_by_the_remote_machine`] reads the modifiers one at a time, and a
/// bitmask at that call site is how the wrong bit gets tested.
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
    /// `VK_F4`.
    pub(crate) const F4: u16 = 0x73;
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
    /// `VK_OEM_102`, the extra key on a 102-key keyboard.
    pub(crate) const OEM_102: u16 = 0xE2;
}

/// Whether this keystroke belongs to the remote machine, so the local OS must
/// not be allowed to act on it.
///
/// **This is the whole of what a grab claims, and it is deliberately narrow.**
/// Everything else keeps travelling the ordinary way — the webview sees it,
/// `ViewInput` forwards it, and the operator's own machine is untouched — so a
/// bug here cannot cost anyone their keyboard beyond this list. What is on the
/// list is exactly the set no window can receive:
///
/// - **Either `Win` key, and anything held with one.** `Win+D`, `Win+E`,
///   `Win+L`, `Win+Tab`, `Win+arrow`, `Win+Shift+S`: the shell takes all of
///   them, and the key itself as well.
/// - **`Alt+Tab`, `Alt+Shift+Tab` and `Alt+Esc`** — window switching.
/// - **`Ctrl+Esc`** (the Start menu) and **`Ctrl+Shift+Esc`** (Task Manager).
/// - **`Alt+F4`.** Claimed on purpose, the way every remote-control tool
///   claims it: closing the *remote* window is what the operator means. The
///   view window's own chords all start `Ctrl+Alt+Shift`, which is not on this
///   list, so releasing the grab from the keyboard still works.
/// - **`PrintScreen`**, which opens the local screen-clipping tool.
///
/// Not on the list, and unreachable for a different reason: `Ctrl+Alt+Del`.
/// The Secure Attention Sequence is not hookable by design, which is what
/// makes it secure; the view window sends it as a request instead (ADR 0028).
#[must_use]
pub const fn claimed_by_the_remote_machine(virtual_key: u16, held: Held) -> bool {
    // The Win keys and everything under them. Tested before the modifier
    // combinations below because `Win` is the one modifier that is itself
    // unreachable: a window that let the press through has already lost the
    // chord, whatever it does with the key that follows.
    if held.meta || matches!(virtual_key, vk::LWIN | vk::RWIN) {
        return true;
    }
    match virtual_key {
        // Alt+Tab / Alt+Shift+Tab (window switching) and Alt+F4 (close).
        vk::TAB | vk::F4 => held.alt,
        // Alt+Esc, Ctrl+Esc, Ctrl+Shift+Esc. Plain Escape is emphatically not
        // claimed: it is one of the most-used keys on a remote machine and it
        // already reaches the webview perfectly well.
        vk::ESCAPE => held.alt || held.ctrl,
        // The local screen-clipping tool, with or without a modifier.
        vk::SNAPSHOT => true,
        _ => false,
    }
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
        vk::LCONTROL => 29,
        vk::OEM_1 => 39,
        vk::OEM_7 => 40,
        vk::OEM_3 => 41,
        vk::LSHIFT => 42,
        vk::OEM_5 => 43,
        vk::OEM_COMMA => 51,
        vk::OEM_PERIOD => 52,
        vk::OEM_2 => 53,
        vk::RSHIFT => 54,
        vk::MULTIPLY => 55,
        vk::LMENU => 56,
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
        vk::RCONTROL => 97,
        vk::DIVIDE => 98,
        vk::SNAPSHOT => 99,
        vk::RMENU => 100,
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
        vk::LWIN => 125,
        vk::RWIN => 126,
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

/// Takes the system chords of [`claimed_by_the_remote_machine`] away from this
/// machine and sends them to `sink` instead, until the returned guard drops.
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

    /// The chords no window can receive are claimed, and the ones that reach
    /// the webview perfectly well are left alone. The second half matters more
    /// than the first: every key claimed here is a key the operator's own
    /// machine stops responding to.
    #[test]
    fn a_grab_claims_the_system_chords_and_nothing_else() {
        let meta = Held { meta: true, ..NONE };
        let alt = Held { alt: true, ..NONE };
        let ctrl = Held { ctrl: true, ..NONE };

        // Either Win key, and anything under one.
        assert!(claimed_by_the_remote_machine(vk::LWIN, NONE));
        assert!(claimed_by_the_remote_machine(vk::RWIN, NONE));
        assert!(claimed_by_the_remote_machine(0x44, meta));
        assert!(claimed_by_the_remote_machine(vk::TAB, meta));
        assert!(claimed_by_the_remote_machine(vk::LEFT, meta));

        // Window switching, the Start menu, Task Manager, Alt+F4.
        assert!(claimed_by_the_remote_machine(vk::TAB, alt));
        assert!(claimed_by_the_remote_machine(
            vk::TAB,
            Held { shift: true, ..alt }
        ));
        assert!(claimed_by_the_remote_machine(vk::ESCAPE, alt));
        assert!(claimed_by_the_remote_machine(vk::ESCAPE, ctrl));
        assert!(claimed_by_the_remote_machine(
            vk::ESCAPE,
            Held {
                shift: true,
                ..ctrl
            }
        ));
        assert!(claimed_by_the_remote_machine(vk::F4, alt));
        assert!(claimed_by_the_remote_machine(vk::SNAPSHOT, NONE));

        // Ordinary typing, ordinary chords, and the keys a remote operator
        // needs most.
        assert!(!claimed_by_the_remote_machine(0x41, NONE));
        assert!(!claimed_by_the_remote_machine(0x43, ctrl));
        assert!(!claimed_by_the_remote_machine(0x56, ctrl));
        assert!(!claimed_by_the_remote_machine(vk::ESCAPE, NONE));
        assert!(!claimed_by_the_remote_machine(vk::TAB, NONE));
        assert!(!claimed_by_the_remote_machine(vk::F4, NONE));
        assert!(!claimed_by_the_remote_machine(vk::DELETE, ctrl));
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
        // KeyF, KeyM, Digit0, KeyC, KeyD, KeyT, and the K this adds.
        for key in [0x46, 0x4D, 0x30, 0x43, 0x44, 0x54, 0x4B] {
            assert!(
                !claimed_by_the_remote_machine(key, prefix),
                "the grab claimed one of the window's own chords: {key:#04x}"
            );
        }
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
