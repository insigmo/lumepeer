//! One `SendInput` on the secure desktop (`Winsta0\Winlogon`), ADR 0057.
//!
//! The mirror of `secure_desktop.rs`: that module takes one GDI snapshot of
//! the desktop the worker was launched onto, this one performs one input
//! event on it. Both run in the worker process the service put on `Winlogon`
//! in the console session (`secure_desktop_launch.rs`), which is the only
//! place a thread can be attached to that desktop — no integrity level puts an
//! ordinary process's thread there, so `SendInput` from the elevated client
//! itself would land on the wrong desktop and vanish.
//!
//! The `SendInput` mapping here is deliberately the same one
//! `crates/media/src/capture/windows.rs`'s `WindowsInjector` uses — logical
//! codes at or above [`POINTER_BUTTON_LOGICAL_BASE`] are pointer buttons, the
//! rest are named virtual keys or Unicode code points — so a click or a
//! keystroke on the secure desktop does exactly what the same event does on
//! the ordinary one. It is reimplemented rather than shared because
//! `crates/service` does not depend on `crates/media` (ADR 0043's
//! dependency-minimalism argument, the same reason `secure_desktop.rs`
//! reimplements the GDI capture).

#![allow(
    unsafe_code,
    reason = "SendInput has no safe binding; same justification standard as the rest of this crate's Win32 surface (ADR 0012, ADR 0043, ADR 0057)"
)]

use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY,
    VK_BACK, VK_CAPITAL, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_F1, VK_HOME,
    VK_INSERT, VK_LEFT, VK_LWIN, VK_MENU, VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_TAB,
    VK_UP,
};

use lumepeer_service::protocol::InjectAction;

/// `mouseData` value for the first X button, as `WindowsInjector` names it —
/// not exposed as a constant by this `windows` build, so the documented
/// literal, the same as `crates/media`'s own copy.
const XBUTTON1: u32 = 0x0001;
/// `mouseData` value for the second X button.
const XBUTTON2: u32 = 0x0002;

/// Logical codes at or above this are pointer buttons; the value is the same
/// one `lumepeer_core::protocol::POINTER_BUTTON_LOGICAL_BASE` defines, kept as
/// a literal here for the dependency reason in the module header (the two are
/// checked to agree by review, the same way the frame-capacity bound is).
const POINTER_BUTTON_LOGICAL_BASE: u32 = 0xF000_0000;

/// Performs one event on the desktop this thread is attached to.
///
/// Returns whether `SendInput` accepted every synthesized event. The worker
/// turns this into its exit code, which is the only thing the service learns
/// (ADR 0057).
#[must_use]
pub fn perform(action: InjectAction) -> bool {
    match action {
        InjectAction::Move { x, y } => move_to(x, y),
        InjectAction::Press { logical } => press(logical, true),
        InjectAction::Release { logical } => press(logical, false),
    }
}

/// Sends a batch of inputs, reporting whether the OS accepted all of them.
fn send(inputs: &[INPUT]) -> bool {
    let size = i32::try_from(size_of::<INPUT>()).unwrap_or(0);
    // SAFETY: `inputs` is a fully initialized `&[INPUT]` for its own length,
    // exactly what `SendInput` requires; no pointer into it is retained.
    let sent = unsafe { SendInput(inputs, size) };
    sent as usize == inputs.len()
}

/// A pointer move, or a key/button press or release.
fn press(logical: u32, pressed: bool) -> bool {
    if logical >= POINTER_BUTTON_LOGICAL_BASE {
        button(logical, pressed)
    } else {
        key(logical, pressed)
    }
}

/// `x`/`y` are normalized `0..=65535` over the captured surface, which is
/// `MOUSEEVENTF_ABSOLUTE`'s coordinate space, so no scaling is needed — the
/// same reasoning `WindowsInjector::move_to` gives.
fn move_to(x: u16, y: u16) -> bool {
    send(&[INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: i32::from(x),
                dy: i32::from(y),
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }])
}

/// One pointer button, indexed off [`POINTER_BUTTON_LOGICAL_BASE`] the same
/// way `WindowsInjector::button` indexes it.
fn button(logical: u32, pressed: bool) -> bool {
    let index = logical.saturating_sub(POINTER_BUTTON_LOGICAL_BASE);
    let (flags, mouse_data) = match (index, pressed) {
        (0, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (0, false) => (MOUSEEVENTF_LEFTUP, 0),
        (1, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (1, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (2, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (2, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (3, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
        (3, false) => (MOUSEEVENTF_XUP, XBUTTON1),
        (4, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
        (4, false) => (MOUSEEVENTF_XUP, XBUTTON2),
        _ => return false,
    };
    send(&[INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: mouse_data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }])
}

/// A key: a named virtual key if `logical` is one, otherwise the Unicode code
/// point `logical` names, sent with `KEYEVENTF_UNICODE` — the same two-path
/// mapping `WindowsInjector::key` uses so any layout and language work.
fn key(logical: u32, pressed: bool) -> bool {
    if let Some(vk) = named_key_vk(logical) {
        return key_vk(vk, pressed);
    }
    let Some(ch) = char::from_u32(logical) else {
        return false;
    };
    let mut buf = [0u16; 2];
    for unit in ch.encode_utf16(&mut buf) {
        if !key_unicode(*unit, pressed) {
            return false;
        }
    }
    true
}

/// A press or release of a virtual key.
fn key_vk(vk: VIRTUAL_KEY, pressed: bool) -> bool {
    send(&[INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if pressed {
                    KEYBD_EVENT_FLAGS::default()
                } else {
                    KEYEVENTF_KEYUP
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }])
}

/// One UTF-16 code unit sent by value rather than by virtual key.
fn key_unicode(unit: u16, pressed: bool) -> bool {
    send(&[INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: unit,
                dwFlags: if pressed {
                    KEYEVENTF_UNICODE
                } else {
                    KEYEVENTF_UNICODE | KEYEVENTF_KEYUP
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }])
}

/// The named virtual key a logical code maps to, if any — the same table
/// `WindowsInjector::named_key_vk` carries, reproduced for the dependency
/// reason in the module header.
fn named_key_vk(logical: u32) -> Option<VIRTUAL_KEY> {
    Some(match logical {
        0x08 => VK_BACK,
        0x09 => VK_TAB,
        0x0d => VK_RETURN,
        0x1b => VK_ESCAPE,
        0x7f => VK_DELETE,
        0xe000 => VK_LEFT,
        0xe001 => VK_UP,
        0xe002 => VK_RIGHT,
        0xe003 => VK_DOWN,
        0xe004 => VK_HOME,
        0xe005 => VK_END,
        0xe006 => VK_PRIOR,
        0xe007 => VK_NEXT,
        0xe008 => VK_INSERT,
        0xe010 => VK_SHIFT,
        0xe011 => VK_CONTROL,
        0xe012 => VK_MENU,
        0xe013 => VK_LWIN,
        0xe014 => VK_CAPITAL,
        0xe101..=0xe118 => VIRTUAL_KEY(VK_F1.0 + u16::try_from(logical - 0xe101).unwrap_or(0)),
        _ => return None,
    })
}
