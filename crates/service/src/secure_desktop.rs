//! One GDI snapshot of `Winsta0\Winlogon`, the secure desktop (ADR 0049).
//!
//! A service's thread does not start out able to see this: by default a
//! Windows service runs on a *non-interactive* window station
//! (session-0 isolation), and even a thread that is on the interactive one
//! (`Winsta0`) is not thereby on its `Winlogon` desktop — a process is
//! associated with one window station at a time, and a thread with one
//! desktop inside that station at a time. Reaching the secure desktop is
//! three explicit switches, all reversed before this function returns:
//!
//! 1. [`OpenWindowStationW`]/[`SetProcessWindowStation`] onto `WinSta0`,
//!    the one interactive window station.
//! 2. [`OpenDesktopW`]/[`SetThreadDesktop`] onto `Winlogon`, the secure
//!    desktop inside it.
//! 3. An ordinary GDI screen capture (`CreateDCW`/`BitBlt`/
//!    `CreateDIBSection`) — the same technique `crates/media`'s Windows
//!    backend already uses for its own first-frame snapshot, reimplemented
//!    here rather than shared, because `crates/service` does not depend on
//!    `crates/media` (ADR 0043's dependency-minimalism argument, ADR 0049).
//!
//! `SetProcessWindowStation` changes the *whole process*, not just this
//! thread. That is safe here because the service serves one pipe connection
//! at a time on a single thread (`windows_service.rs::serve_until_stopped`)
//! and named-pipe I/O needs no window station of its own — but it is also
//! why the original window station and desktop are restored before this
//! function returns, rather than left switched for the rest of the
//! process's life: minimizing how long a `LocalSystem` process holds a live
//! handle onto the secure desktop is part of limiting what its compromise
//! would be worth (ADR 0049).
//!
//! [`OpenWindowStationW`]: windows::Win32::System::StationsAndDesktops::OpenWindowStationW
//! [`SetProcessWindowStation`]: windows::Win32::System::StationsAndDesktops::SetProcessWindowStation
//! [`OpenDesktopW`]: windows::Win32::System::StationsAndDesktops::OpenDesktopW
//! [`SetThreadDesktop`]: windows::Win32::System::StationsAndDesktops::SetThreadDesktop

#![allow(
    unsafe_code,
    reason = "the window-station/desktop switch and GDI capture have no safe bindings; same justification standard as SendInput (ADR 0012) and the rest of this crate's Win32 surface (ADR 0043)"
)]

use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleDC, CreateDCW, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, SRCCOPY, SelectObject,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, CloseWindowStation, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS,
    DESKTOP_WRITEOBJECTS, GetProcessWindowStation, GetThreadDesktop, OpenDesktopW,
    OpenWindowStationW, SetProcessWindowStation, SetThreadDesktop,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
use windows::core::PCWSTR;

/// Bytes per pixel of the BGRA8 this module always produces.
const BYTES_PER_PIXEL: usize = 4;

/// `WINSTA_ALL_ACCESS` (`winuser.h`): full rights on a window station. Not
/// exposed as a named constant by this version of the `windows` crate's
/// `StationsAndDesktops` bindings, so it is reproduced here as the
/// documented literal rather than reduced to only the rights this module
/// actually uses — `SetThreadDesktop` requires the desktop handle's window
/// station to be the process's current one, and this process's only use for
/// the handle is the one immediate switch below, so there is no narrower
/// request worth constructing by hand from the individual `WINSTA_*` bits.
const WINSTA_ALL_ACCESS: u32 = 0x37F;

/// A null-terminated UTF-16 copy of `text`.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Captures one frame of `Winsta0\Winlogon`.
///
/// `None` on any failure — a window station or desktop this process cannot
/// open, a GDI step that fails, or a zero-sized result — collapsing every
/// reason to the same "no frame this poll", exactly as every other failure
/// in this crate's client-facing surface does. This is read-only: nothing
/// here changes what desktop is active or draws anything, so calling it
/// when the secure desktop is not actually showing is harmless — it only
/// ever produces a picture of whatever `Winlogon` currently holds, which is
/// nothing in particular outside a real secure-desktop transition.
#[must_use]
pub fn capture() -> Option<(u32, u32, Vec<u8>)> {
    // SAFETY: `GetProcessWindowStation`/`GetThreadDesktop` read this
    // process's/thread's own current handles and return them borrowed —
    // they are not closed here, only remembered so they can be restored.
    // `GetCurrentThreadId` names this call's own thread, which is the one
    // `SetThreadDesktop` below actually moves.
    let original_winsta = unsafe { GetProcessWindowStation() }.ok();
    let original_desktop = unsafe { GetThreadDesktop(GetCurrentThreadId()) }.ok();

    let result = capture_on_winlogon();

    // Restore both, regardless of whether the capture above succeeded: a
    // `LocalSystem` process should hold a live handle onto the interactive
    // window station and the secure desktop for no longer than the one
    // capture actually needs (ADR 0049).
    if let Some(winsta) = original_winsta
        && !winsta.is_invalid()
    {
        // SAFETY: `winsta` is the process's own handle from before this
        // function touched anything; setting it back is always valid.
        unsafe {
            let _ = SetProcessWindowStation(winsta);
        }
    }
    if let Some(desktop) = original_desktop
        && !desktop.is_invalid()
    {
        // SAFETY: as above, for the thread's desktop.
        unsafe {
            let _ = SetThreadDesktop(desktop);
        }
    }

    result
}

/// The actual switch-and-capture, isolated so [`capture`] can restore the
/// original window station and desktop on every exit path with one `?`-free
/// block rather than duplicating the restore in each failure branch.
fn capture_on_winlogon() -> Option<(u32, u32, Vec<u8>)> {
    let winsta_name = wide("WinSta0");
    // SAFETY: `winsta_name` is a null-terminated wide string that outlives
    // the call; the returned handle is owned by this function and closed
    // below.
    let winsta =
        unsafe { OpenWindowStationW(PCWSTR(winsta_name.as_ptr()), false, WINSTA_ALL_ACCESS) }
            .inspect_err(|error| tracing::warn!(%error, "cannot open WinSta0"))
            .ok()?;

    // SAFETY: `winsta` was just opened above and is live for this call.
    let switched = unsafe { SetProcessWindowStation(winsta) };
    if switched.is_err() {
        tracing::warn!("cannot switch this process onto WinSta0");
        // SAFETY: `winsta` is live and owned here.
        unsafe {
            let _ = CloseWindowStation(winsta);
        }
        return None;
    }

    let desktop_name = wide("Winlogon");
    // SAFETY: `desktop_name` is a null-terminated wide string that outlives
    // the call; this only runs after the process is on `WinSta0`, the
    // window station `Winlogon` lives inside.
    let desktop = unsafe {
        OpenDesktopW(
            PCWSTR(desktop_name.as_ptr()),
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_READOBJECTS.0 | DESKTOP_WRITEOBJECTS.0,
        )
    }
    .inspect_err(|error| tracing::warn!(%error, "cannot open the secure desktop"))
    .ok();
    let Some(desktop) = desktop else {
        // SAFETY: `winsta` is live and owned here.
        unsafe {
            let _ = CloseWindowStation(winsta);
        }
        return None;
    };

    // SAFETY: `desktop` was just opened above and is live for this call.
    let on_desktop = unsafe { SetThreadDesktop(desktop) };
    let frame = if on_desktop.is_ok() {
        gdi_snapshot()
    } else {
        tracing::warn!("cannot switch this thread onto the secure desktop");
        None
    };

    // SAFETY: `desktop` and `winsta` are both live and owned here; nothing
    // below uses either again.
    unsafe {
        let _ = CloseDesktop(desktop);
        let _ = CloseWindowStation(winsta);
    }

    frame
}

/// A `BitBlt` of whatever desktop the calling thread is currently on, into a
/// tightly packed BGRA8 buffer — the same technique
/// `crates/media/src/capture/windows.rs::gdi_snapshot` uses, reimplemented
/// here at whole-screen scope rather than shared (see this module's own doc
/// comment for why).
fn gdi_snapshot() -> Option<(u32, u32, Vec<u8>)> {
    // SAFETY: `GetSystemMetrics` reads system state and returns a plain
    // integer.
    let (width, height) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    if width <= 0 || height <= 0 {
        return None;
    }

    // SAFETY: every call below takes plain values or owns what it creates;
    // the DCs and bitmap are released on every exit path via
    // `DeleteDC`/`DeleteObject`.
    unsafe {
        let dc = CreateDCW(PCWSTR::null(), PCWSTR::null(), PCWSTR::null(), None);
        if dc.is_invalid() {
            return None;
        }

        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: u32::try_from(size_of::<BITMAPINFOHEADER>()).unwrap_or(u32::MAX),
                biWidth: width,
                // Top-down rows, matching the wire's row order.
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let Ok(section) = CreateDIBSection(
            Some(dc),
            core::ptr::from_ref(&bi),
            DIB_RGB_COLORS,
            &raw mut bits,
            None,
            0,
        ) else {
            let _ = DeleteDC(dc);
            return None;
        };
        let memory_dc = CreateCompatibleDC(Some(dc));
        let old = SelectObject(memory_dc, section.into());

        let blitted = BitBlt(memory_dc, 0, 0, width, height, Some(dc), 0, 0, SRCCOPY).is_ok();

        let bytes = usize::try_from(width).unwrap_or(0)
            * usize::try_from(height).unwrap_or(0)
            * BYTES_PER_PIXEL;
        let data = if blitted && !bits.is_null() {
            Some(std::slice::from_raw_parts(bits.cast::<u8>(), bytes).to_vec())
        } else {
            None
        };

        SelectObject(memory_dc, old);
        let _ = DeleteObject(section.into());
        let _ = DeleteDC(memory_dc);
        let _ = DeleteDC(dc);

        data.map(|data| (width.cast_unsigned(), height.cast_unsigned(), data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `capture` never panics, on a machine with no active secure-desktop
    /// transition (the ordinary case in an automated test — this suite must
    /// not, and does not, try to trigger one) and regardless of whether this
    /// process happens to be privileged enough to open `Winlogon` at all.
    /// Either a clean `None` (the expected outcome, unelevated) or `Some`
    /// with a non-empty buffer (only if this happens to run as `LocalSystem`
    /// or an equivalently privileged account) is acceptable; a panic or a
    /// zero-sized `Some` is not.
    #[test]
    fn capture_never_panics_and_never_returns_an_empty_frame() {
        if let Some((width, height, data)) = capture() {
            assert!(width > 0 && height > 0);
            assert!(!data.is_empty());
        }
    }
}
