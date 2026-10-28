//! The medium-integrity poke (`--medium-poke <title>`), launched by `--measure`
//! with the console user's own (limited) token. It tries to press the probe's
//! System-integrity TaskDialog with `SendInput` and `PostMessage(BM_CLICK)`.
//! UIPI should block both; the human's mouse should not. Its findings go to
//! `poke_path` for `--measure` to read back.

#![allow(unsafe_code)]

use std::thread::sleep;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, RECT, TRUE, WPARAM};
use windows::core::BOOL;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEINPUT, SendInput,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, FindWindowW, GetClassNameW, GetWindowRect, PostMessageW, SetCursorPos,
    SetForegroundWindow,
};
use windows::core::PCWSTR;

use crate::win;

const BM_CLICK: u32 = 0x00F5;

pub fn run(title: &str) -> u32 {
    let path = win::poke_path();
    win::append(
        &path,
        &format!(
            "poke: running as SID={} (expected: the user, not S-1-5-18)",
            win::current_user_sid().unwrap_or_else(|| "?".into())
        ),
    );

    let wide_title = win::wide(title);
    // Wait up to 15 s for the dialog to appear.
    let dialog = {
        let start = Instant::now();
        loop {
            // SAFETY: `wide_title` is a live null-terminated wide string.
            let h = unsafe { FindWindowW(PCWSTR::null(), PCWSTR(wide_title.as_ptr())) }.ok();
            if let Some(h) = h {
                break Some(h);
            }
            if start.elapsed() > Duration::from_secs(15) {
                break None;
            }
            sleep(Duration::from_millis(200));
        }
    };
    let Some(dialog) = dialog else {
        win::append(&path, "poke: dialog NOT found within 15s");
        return 2;
    };
    win::append(&path, "poke: dialog found");

    // Attempt 1 — SendInput a left click at the dialog's OK button area.
    let mut rect = RECT::default();
    // SAFETY: `dialog` is a live HWND; `rect` outlives the call.
    if unsafe { GetWindowRect(dialog, &raw mut rect) }.is_ok() {
        // OK button sits near the bottom-right of a TaskDialog.
        let x = rect.right - 60;
        let y = rect.bottom - 30;
        // SAFETY: `dialog` live; bringing it to the foreground is harmless.
        unsafe {
            let _ = SetForegroundWindow(dialog);
        }
        // SAFETY: plain cursor move.
        let _ = unsafe { SetCursorPos(x, y) };
        let inputs = [
            mouse(MOUSEEVENTF_LEFTDOWN.0),
            mouse(MOUSEEVENTF_LEFTUP.0),
        ];
        // SAFETY: `inputs` is a live slice of the exact count passed.
        let sent = unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
        win::append(
            &path,
            &format!("poke: SendInput left-click at ({x},{y}) returned {sent} (events queued; UIPI decides if they land)"),
        );
    } else {
        win::append(&path, "poke: could not read the dialog rect");
    }

    // Attempt 2 — PostMessage(BM_CLICK) to each Button child.
    let mut buttons: Vec<HWND> = Vec::new();
    // SAFETY: the callback writes through the &mut Vec passed as LPARAM.
    unsafe {
        let _ = EnumChildWindows(
            Some(dialog),
            Some(collect_buttons),
            LPARAM(&raw mut buttons as isize),
        );
    }
    let mut posted = 0;
    for b in &buttons {
        // SAFETY: `*b` is a live child HWND from the enumeration.
        if unsafe { PostMessageW(Some(*b), BM_CLICK, WPARAM(0), LPARAM(0)) }.is_ok() {
            posted += 1;
        }
    }
    win::append(
        &path,
        &format!("poke: PostMessage(BM_CLICK) sent to {posted}/{} Button child(ren)", buttons.len()),
    );
    win::append(&path, "poke: done");
    0
}

fn mouse(flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS(flags),
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

extern "system" fn collect_buttons(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let mut class = [0u16; 64];
    // SAFETY: `class` is a live buffer; the call writes at most its length.
    let n = unsafe { GetClassNameW(hwnd, &mut class) };
    if n > 0 {
        let name = String::from_utf16_lossy(&class[..n as usize]);
        if name.eq_ignore_ascii_case("Button") {
            // SAFETY: `lparam` carries the &mut Vec<HWND> from `run`.
            let v = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
            v.push(hwnd);
        }
    }
    TRUE
}
