//! Where a physical key sits on a macOS keyboard, by evdev code (ADR 0104).
//!
//! `InputEventPayload::scancode` has carried an evdev code since ADR 0065:
//! one physical encoding crosses the wire and each host platform translates
//! from it. Windows does that in `capture::windows::physical_key` and X11 in
//! `X11Injector::keycode`; this is the third translation, and until ADR 0104
//! it did not exist, so a macOS host had no way to press a key *by position*
//! at all. Everything arrived as a character through
//! `CGEventKeyboardSetUnicodeString`, which carries no virtual key — so
//! `Cmd+C` put a stray 'c' into the focused application and no copy command
//! ever fired, which is exactly the failure ADR 0065 fixed on the other two
//! platforms (`docs/bugs/17-remote-hotkeys.md`, `docs/bugs/20-hotkeys-and-vmware-grab.md`).
//!
//! macOS-only values, but the table and the lookup that reads it are plain
//! integers, so both compile — and are tested — on every platform, the same
//! way `lumepeer-guestkeys` keeps its Windows tables testable everywhere.
//! That matters more here than it does there: the machine this workspace is
//! usually built on is not a Mac, and a table nobody can run is a table
//! nobody can review.
//!
//! The codes are `HIToolbox`'s `kVK_*` constants (`Events.h`). Apple's
//! numbering follows the original Macintosh keyboard rather than any modern
//! row order, so every entry is written out next to the key it names instead
//! of being derived by arithmetic.

/// The macOS key code for the position an evdev code names, or `None` for a
/// position this keyboard has no key at.
///
/// `None` is not an error, it is "a Mac has no such key". The caller falls
/// back to typing the character, which is what every key did before this
/// existed — and for the keys a Mac genuinely lacks (`PrintScreen`,
/// `ScrollLock`, `Pause`, `NumLock`, the context-menu key) that is the only
/// honest answer there is. `Insert` is absent for the same reason it is
/// absent from `named_key_vk`: `kVK_Help` sits in that position on an old
/// Apple keyboard but is a different key, and pressing it would be a guess.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "a flat table of one arm per physical key: its length is the number of keys on a keyboard. Apple's numbering follows no row order, so there is no arithmetic to factor out, and splitting it by keyboard region would only hide which positions are mapped and which fall through to the `_` arm — the one thing a reviewer of this file has to be able to see"
)]
pub const fn key_code_at(scancode: u32) -> Option<u16> {
    Some(match scancode {
        1 => 0x35,  // Escape -> kVK_Escape
        2 => 0x12,  // Digit1 -> kVK_ANSI_1
        3 => 0x13,  // Digit2
        4 => 0x14,  // Digit3
        5 => 0x15,  // Digit4
        6 => 0x17,  // Digit5 — 5 and 6 are inverted in Apple's numbering
        7 => 0x16,  // Digit6
        8 => 0x1A,  // Digit7
        9 => 0x1C,  // Digit8
        10 => 0x19, // Digit9
        11 => 0x1D, // Digit0 -> kVK_ANSI_0
        12 => 0x1B, // Minus
        13 => 0x18, // Equal
        14 => 0x33, // Backspace -> kVK_Delete (the key left of Return)
        15 => 0x30, // Tab
        16 => 0x0C, // KeyQ
        17 => 0x0D, // KeyW
        18 => 0x0E, // KeyE
        19 => 0x0F, // KeyR
        20 => 0x11, // KeyT
        21 => 0x10, // KeyY
        22 => 0x20, // KeyU
        23 => 0x22, // KeyI
        24 => 0x1F, // KeyO
        25 => 0x23, // KeyP
        26 => 0x21, // BracketLeft
        27 => 0x1E, // BracketRight
        28 => 0x24, // Enter -> kVK_Return
        29 => 0x3B, // ControlLeft
        30 => 0x00, // KeyA
        31 => 0x01, // KeyS
        32 => 0x02, // KeyD
        33 => 0x03, // KeyF
        34 => 0x05, // KeyG
        35 => 0x04, // KeyH — G and H are inverted in Apple's numbering
        36 => 0x26, // KeyJ
        37 => 0x28, // KeyK
        38 => 0x25, // KeyL
        39 => 0x29, // Semicolon
        40 => 0x27, // Quote
        41 => 0x32, // Backquote -> kVK_ANSI_Grave
        42 => 0x38, // ShiftLeft
        43 => 0x2A, // Backslash
        44 => 0x06, // KeyZ
        45 => 0x07, // KeyX
        46 => 0x08, // KeyC
        47 => 0x09, // KeyV
        48 => 0x0B, // KeyB
        49 => 0x2D, // KeyN
        50 => 0x2E, // KeyM
        51 => 0x2B, // Comma
        52 => 0x2F, // Period
        53 => 0x2C, // Slash
        54 => 0x3C, // ShiftRight
        55 => 0x43, // NumpadMultiply -> kVK_ANSI_KeypadMultiply
        56 => 0x3A, // AltLeft -> kVK_Option
        57 => 0x31, // Space
        58 => 0x39, // CapsLock
        // The function row. Apple's numbering runs in no order at all here,
        // which is the whole reason this is a table.
        59 => 0x7A, // F1
        60 => 0x78, // F2
        61 => 0x63, // F3
        62 => 0x76, // F4
        63 => 0x60, // F5
        64 => 0x61, // F6
        65 => 0x62, // F7
        66 => 0x64, // F8
        67 => 0x65, // F9
        68 => 0x6D, // F10
        // The numeric keypad. A Mac has `Clear` where a PC has `NumLock`, and
        // it is not the same key: a Mac keypad has no lock to toggle. Left
        // unmapped rather than pressed by position (evdev 69, 70).
        71 => 0x59, // Numpad7
        72 => 0x5B, // Numpad8
        73 => 0x5C, // Numpad9
        74 => 0x4E, // NumpadSubtract
        75 => 0x56, // Numpad4
        76 => 0x57, // Numpad5
        77 => 0x58, // Numpad6
        78 => 0x45, // NumpadAdd
        79 => 0x53, // Numpad1
        80 => 0x54, // Numpad2
        81 => 0x55, // Numpad3
        82 => 0x52, // Numpad0
        83 => 0x41, // NumpadDecimal
        86 => 0x0A, // IntlBackslash -> kVK_ISO_Section
        87 => 0x67, // F11
        88 => 0x6F, // F12
        89 => 0x5E, // IntlRo -> kVK_JIS_Underscore
        96 => 0x4C, // NumpadEnter -> kVK_ANSI_KeypadEnter
        97 => 0x3E, // ControlRight
        98 => 0x4B, // NumpadDivide
        // 99 (PrintScreen) and 119 (Pause): no key on any Mac keyboard.
        100 => 0x3D, // AltRight -> kVK_RightOption
        102 => 0x73, // Home
        103 => 0x7E, // ArrowUp
        104 => 0x74, // PageUp
        105 => 0x7B, // ArrowLeft
        106 => 0x7C, // ArrowRight
        107 => 0x77, // End
        108 => 0x7D, // ArrowDown
        109 => 0x79, // PageDown
        // 110 (Insert): see the note on this function.
        111 => 0x75, // Delete -> kVK_ForwardDelete
        113 => 0x4A, // AudioVolumeMute -> kVK_Mute
        114 => 0x49, // AudioVolumeDown
        115 => 0x48, // AudioVolumeUp
        124 => 0x5D, // IntlYen -> kVK_JIS_Yen
        125 => 0x37, // MetaLeft -> kVK_Command
        126 => 0x36, // MetaRight -> kVK_RightCommand
        // 127 (ContextMenu): a Mac has no menu key.
        183 => 0x69, // F13
        184 => 0x6B, // F14
        185 => 0x71, // F15
        186 => 0x6A, // F16
        187 => 0x40, // F17
        188 => 0x4F, // F18
        189 => 0x50, // F19
        190 => 0x5A, // F20
        // F21..F24 (191..194) have no `kVK_` constant at all, the same gap
        // `named_key_vk` records for the logical side of those keys.
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The keys a chord is actually made of. Every one of these is a key the
    /// report in `docs/bugs/20-hotkeys-and-vmware-grab.md` named, and before
    /// ADR 0104 not one of them could be pressed by position on a Mac.
    #[test]
    fn the_keys_a_chord_is_made_of_all_have_a_position() {
        // Cmd/Ctrl/Alt/Shift, either hand.
        assert_eq!(key_code_at(125), Some(0x37)); // MetaLeft -> Command
        assert_eq!(key_code_at(126), Some(0x36)); // MetaRight
        assert_eq!(key_code_at(29), Some(0x3B)); // ControlLeft
        assert_eq!(key_code_at(97), Some(0x3E)); // ControlRight
        assert_eq!(key_code_at(56), Some(0x3A)); // AltLeft -> Option
        assert_eq!(key_code_at(100), Some(0x3D)); // AltRight
        assert_eq!(key_code_at(42), Some(0x38)); // ShiftLeft
        assert_eq!(key_code_at(54), Some(0x3C)); // ShiftRight

        // The letters copy, paste, cut, select-all and undo are addressed to.
        assert_eq!(key_code_at(46), Some(0x08)); // KeyC
        assert_eq!(key_code_at(47), Some(0x09)); // KeyV
        assert_eq!(key_code_at(45), Some(0x07)); // KeyX
        assert_eq!(key_code_at(30), Some(0x00)); // KeyA
        assert_eq!(key_code_at(44), Some(0x06)); // KeyZ

        // Tab, for the window switcher the keyboard grab forwards.
        assert_eq!(key_code_at(15), Some(0x30));
    }

    /// Apple's numbering inverts two pairs relative to every other keyboard
    /// encoding, and a table that "looked obvious" would get both wrong: 5/6
    /// on the digit row and G/H on the home row.
    #[test]
    fn the_pairs_apple_inverts_are_the_way_round_apple_has_them() {
        assert_eq!(key_code_at(6), Some(0x17)); // Digit5
        assert_eq!(key_code_at(7), Some(0x16)); // Digit6
        assert_eq!(key_code_at(34), Some(0x05)); // KeyG
        assert_eq!(key_code_at(35), Some(0x04)); // KeyH
    }

    /// A key a Mac does not have is reported as not having a position, rather
    /// than being given the code of whatever key happens to sit near it. The
    /// caller then types the character, which is the behaviour every key had
    /// before this table existed.
    #[test]
    fn a_key_no_mac_keyboard_has_is_not_given_one() {
        for scancode in [
            69,  // NumLock — a Mac keypad has Clear, which is a different key
            70,  // ScrollLock
            99,  // PrintScreen
            110, // Insert
            119, // Pause
            127, // ContextMenu
            191, // F21
            194, // F24
            0,   // "no physical key", which the wire has always carried
        ] {
            assert_eq!(
                key_code_at(scancode),
                None,
                "evdev {scancode} was given a macOS key code it has no key for"
            );
        }
    }

    /// No two positions may share a key code: the table is a map from
    /// position to key, and a duplicate means one of the two keys presses the
    /// other. This is the check that catches a transcription slip, which is
    /// the failure mode a hand-written table of 100 magic numbers actually
    /// has.
    #[test]
    fn every_position_names_a_key_of_its_own() {
        let mut seen = std::collections::BTreeMap::new();
        for scancode in 0..=255u32 {
            let Some(code) = key_code_at(scancode) else {
                continue;
            };
            if let Some(previous) = seen.insert(code, scancode) {
                panic!(
                    "macOS key code {code:#04x} is claimed by evdev {previous} and evdev {scancode}"
                );
            }
        }
        // A spot check that the loop above actually walked a full table
        // rather than an empty one.
        assert!(seen.len() > 90, "only {} positions are mapped", seen.len());
    }
}
