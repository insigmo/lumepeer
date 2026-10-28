//! 2.9 — a `Shell_NotifyIcon` tray icon from the `LocalSystem` measure process,
//! and whether a click from explorer (medium integrity) reaches it. Expected:
//! the icon is added, but the callback does not arrive without
//! `ChangeWindowMessageFilterEx` — which decides whether the host draws its own
//! tray or relies on the client.

#![allow(unsafe_code)]

use std::sync::atomic::{AtomicU32, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, IDI_APPLICATION, LoadIconW,
    MSG, PM_REMOVE, PeekMessageW, RegisterClassW, TranslateMessage, WINDOW_EX_STYLE, WNDCLASSW,
    WS_OVERLAPPED,
};
use windows::core::PCWSTR;

use crate::win;

const CALLBACK_MSG: u32 = 0x0400 + 1; // WM_APP + 1
static CALLBACKS: AtomicU32 = AtomicU32::new(0);

pub fn measure() -> Vec<String> {
    let mut out = Vec::new();
    CALLBACKS.store(0, Ordering::SeqCst);

    let class_name = win::wide("LumepeerProbeTrayWnd");
    // SAFETY: GetModuleHandleW(None) returns this module's handle.
    let hinstance = match unsafe { GetModuleHandleW(PCWSTR::null()) } {
        Ok(h) => h,
        Err(e) => {
            out.push(format!("GetModuleHandleW failed: {e:?}"));
            out.push("RESULT: NO".into());
            return out;
        }
    };
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: hinstance.into(),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };
    // SAFETY: `wc` and its borrowed class name outlive the call.
    let atom = unsafe { RegisterClassW(&wc) };
    if atom == 0 {
        out.push("RegisterClassW failed".into());
        out.push("RESULT: NO".into());
        return out;
    }
    let window_name = win::wide("probe-tray");
    // SAFETY: class/window names outlive the call; an invisible owner window.
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(window_name.as_ptr()),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance.into()),
            None,
        )
    };
    let Ok(hwnd) = hwnd else {
        out.push("CreateWindowExW failed".into());
        out.push("RESULT: NO".into());
        return out;
    };

    // SAFETY: a shared icon handle that needs no free.
    let hicon = unsafe { LoadIconW(None, IDI_APPLICATION) }.unwrap_or_default();
    let mut tip = [0u16; 128];
    for (i, c) in "Lumepeer probe".encode_utf16().enumerate().take(127) {
        tip[i] = c;
    }
    let mut nid = NOTIFYICONDATAW {
        cbSize: u32::try_from(size_of::<NOTIFYICONDATAW>()).unwrap_or(0),
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: CALLBACK_MSG,
        hIcon: hicon,
        szTip: tip,
        ..Default::default()
    };
    // SAFETY: `nid` is a fully initialised owned struct.
    let added = unsafe { Shell_NotifyIconW(NIM_ADD, &raw const nid) }.as_bool();
    out.push(format!(
        "Shell_NotifyIcon(NIM_ADD) = {} (look for the icon in the tray overflow)",
        if added { "OK" } else { "FAILED" }
    ));

    // Pump messages for 8 s, counting any tray callback.
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(8) {
        let mut msg = MSG::default();
        // SAFETY: `msg` outlives the call; PM_REMOVE dequeues.
        while unsafe { PeekMessageW(&raw mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            // SAFETY: `msg` is a dequeued message.
            unsafe {
                let _ = TranslateMessage(&raw const msg);
                DispatchMessageW(&raw const msg);
            }
        }
        sleep(Duration::from_millis(50));
    }
    let callbacks = CALLBACKS.load(Ordering::SeqCst);
    out.push(format!(
        "tray callbacks received = {callbacks} (expected 0 without ChangeWindowMessageFilterEx; explorer is medium, this window is System)"
    ));

    // SAFETY: remove the icon and destroy the window we own.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &raw mut nid);
        let _ = DestroyWindow(hwnd);
    }
    // The icon was added (the capability ADR 0169 §5 cares about); the callback
    // outcome is a note, not a pass/fail for the probe.
    out.push(format!("RESULT: {}", if added { "YES (icon added)" } else { "NO" }));
    out
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == CALLBACK_MSG {
        CALLBACKS.fetch_add(1, Ordering::SeqCst);
    }
    // SAFETY: a plain default window procedure call.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}
