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
//! 3. An ordinary GDI screen capture (`CreateDCW`/`StretchBlt`/
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
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDCW, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, HALFTONE, SRCCOPY, SelectObject, SetStretchBltMode,
    StretchBlt,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, CloseWindowStation, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS,
    DESKTOP_WRITEOBJECTS, GetProcessWindowStation, GetThreadDesktop, OpenDesktopW,
    OpenWindowStationW, SetProcessWindowStation, SetThreadDesktop,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
use windows::core::PCWSTR;

use lumepeer_service::protocol::SECURE_DESKTOP_FRAME_CAPACITY_BYTES;

/// Bytes per pixel of the BGRA8 this module always produces.
const BYTES_PER_PIXEL: usize = 4;

/// Denominator of the fraction [`fit_within_capacity`] scales by.
///
/// The search there is integer-only — no float casts on a path that decides a
/// buffer length — so it walks numerators over this fixed denominator. 1/64 is
/// finer than any difference visible on a lock screen and coarse enough that
/// trying every step is a loop of at most sixty-four comparisons.
const SCALE_STEPS: u32 = 64;

/// The largest size not wider than `width`/`height` and the same shape, whose
/// BGRA8 payload still fits [`SECURE_DESKTOP_FRAME_CAPACITY_BYTES`].
///
/// The mapping is fixed at one 1920×1080 frame and never resizes (ADR 0049),
/// and `crate::frame::Writer::write` refuses — rather than truncates — a
/// payload past that. A screen larger than 1080p is the ordinary case, not an
/// edge one, so capturing it at native size meant every single secure-desktop
/// capture on such a host was refused and the guest saw
/// `docs/bugs/11-uac-degradation.md`'s "can't see this" message for the whole
/// episode, with nothing on either side able to say why. Fitting the picture
/// to the channel it has to travel over is what makes the feature work on the
/// hosts people actually have.
fn fit_within_capacity(width: i32, height: i32) -> (i32, i32) {
    let max_pixels =
        u64::try_from(SECURE_DESKTOP_FRAME_CAPACITY_BYTES / BYTES_PER_PIXEL).unwrap_or(0);
    let (wide_px, high_px) = (
        u64::try_from(width).unwrap_or(0),
        u64::try_from(height).unwrap_or(0),
    );
    if wide_px * high_px <= max_pixels {
        return (width, height);
    }
    for step in (1..SCALE_STEPS).rev() {
        let scaled_width = wide_px * u64::from(step) / u64::from(SCALE_STEPS);
        let scaled_height = high_px * u64::from(step) / u64::from(SCALE_STEPS);
        if scaled_width > 0
            && scaled_height > 0
            && scaled_width * scaled_height <= max_pixels
            && let (Ok(scaled_width), Ok(scaled_height)) =
                (i32::try_from(scaled_width), i32::try_from(scaled_height))
        {
            return (scaled_width, scaled_height);
        }
    }
    // Only reachable if the capacity is smaller than a single pixel, which
    // the constants make impossible; a one-pixel frame is still a frame the
    // writer can refuse honestly rather than a panic here.
    (1, 1)
}

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
    // What the fixed mapping can actually carry. On a 1080p-or-smaller screen
    // this is the screen itself and the blit below is one-to-one.
    let (target_width, target_height) = fit_within_capacity(width, height);

    // The `"DISPLAY"` driver name is what gives a device context for the
    // whole screen of the calling thread's *current desktop*; the all-null
    // form this used before always returned `NULL` (confirmed on real
    // hardware — that was ADR 0049's silent failure). Since this runs in the
    // worker, whose thread is on `Winsta0\Winlogon`, that screen is the
    // secure desktop (ADR 0056).
    let driver = wide("DISPLAY");
    // SAFETY: every call below takes plain values or owns what it creates;
    // the DCs and bitmap are released on every exit path via
    // `DeleteDC`/`DeleteObject`. `driver` is a null-terminated wide string
    // that outlives the `CreateDCW` call.
    unsafe {
        let dc = CreateDCW(
            PCWSTR(driver.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            None,
        );
        if dc.is_invalid() {
            return None;
        }

        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: u32::try_from(size_of::<BITMAPINFOHEADER>()).unwrap_or(u32::MAX),
                biWidth: target_width,
                // Top-down rows, matching the wire's row order.
                biHeight: -target_height,
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

        // `StretchBlt` rather than `BitBlt` so a screen larger than the
        // mapping's capacity arrives scaled instead of being refused whole.
        // `HALFTONE` averages the pixels it drops, which is what keeps a UAC
        // prompt's text legible after a reduction; it costs nothing when the
        // sizes match, but asking for it then would be a mode set for no
        // reason.
        if (target_width, target_height) != (width, height) {
            SetStretchBltMode(memory_dc, HALFTONE);
        }
        let blitted = StretchBlt(
            memory_dc,
            0,
            0,
            target_width,
            target_height,
            Some(dc),
            0,
            0,
            width,
            height,
            SRCCOPY,
        )
        .as_bool();

        let bytes = usize::try_from(target_width).unwrap_or(0)
            * usize::try_from(target_height).unwrap_or(0)
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

        data.map(|data| {
            (
                target_width.cast_unsigned(),
                target_height.cast_unsigned(),
                data,
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A screen the mapping can already carry is captured at its own size;
    /// one larger comes back smaller, in the same shape, and small enough to
    /// publish. Before this, a 1440p or 4K host had every secure-desktop
    /// capture refused by the writer and never saw the prompt at all.
    #[test]
    fn a_screen_larger_than_the_mapping_is_fitted_to_it_and_keeps_its_shape() {
        let capacity_px = SECURE_DESKTOP_FRAME_CAPACITY_BYTES / BYTES_PER_PIXEL;
        assert_eq!(fit_within_capacity(1920, 1080), (1920, 1080));
        assert_eq!(fit_within_capacity(1280, 720), (1280, 720));

        for (width, height) in [(2560, 1440), (3840, 2160), (5120, 1440), (7680, 4320)] {
            let (fitted_width, fitted_height) = fit_within_capacity(width, height);
            let pixels = usize::try_from(fitted_width).unwrap_or(usize::MAX)
                * usize::try_from(fitted_height).unwrap_or(usize::MAX);
            assert!(
                pixels <= capacity_px,
                "{width}x{height} fitted to {fitted_width}x{fitted_height}, which still does not fit"
            );
            assert!(fitted_width > 0 && fitted_height > 0);
            assert!(fitted_width < width && fitted_height < height);
            // Same shape to within one step of the search's granularity: a
            // stretched lock screen would be worse than a smaller one.
            let source_ratio = f64::from(width) / f64::from(height);
            let fitted_ratio = f64::from(fitted_width) / f64::from(fitted_height);
            assert!(
                (source_ratio - fitted_ratio).abs() < 0.05,
                "{width}x{height} became {fitted_width}x{fitted_height}, a different shape"
            );
        }
    }

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
