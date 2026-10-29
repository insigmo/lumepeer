//! How long the console has gone without input (ADR 0169 §8).
//!
//! The session host installs an update nobody asked for only while nobody is
//! at the machine. `GetLastInputInfo` answers for the session of the process
//! that asks, and the session host runs in the console session, so it reads
//! the person's own keyboard and mouse — and a guest's injected input too,
//! which is input to that session all the same.

#![allow(
    unsafe_code,
    reason = "GetLastInputInfo and GetTickCount have no safe bindings; same \
              justification standard as the rest of this crate's Win32 surface \
              (ADR 0043)"
)]

use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

/// Milliseconds since the last input in this process's session, or `None`
/// when the session will not say.
#[must_use]
pub fn idle_ms() -> Option<u64> {
    let mut last = LASTINPUTINFO {
        cbSize: u32::try_from(size_of::<LASTINPUTINFO>()).unwrap_or(0),
        dwTime: 0,
    };
    // SAFETY: `last` is a live `LASTINPUTINFO` with `cbSize` set, which is all
    // `GetLastInputInfo` requires; it keeps no pointer into it.
    if !unsafe { GetLastInputInfo(&raw mut last) }.as_bool() {
        return None;
    }
    // Both are the same 32-bit millisecond tick, so the difference is right
    // across its 49-day wrap as long as it is taken wrapping.
    // SAFETY: no arguments, no preconditions.
    let now = unsafe { GetTickCount() };
    Some(u64::from(now.wrapping_sub(last.dwTime)))
}

#[cfg(test)]
mod tests {
    /// This test runs in a session with a desktop, which has a last input
    /// time; it cannot be in the future.
    #[test]
    fn the_console_has_an_idle_time() {
        if let Some(ms) = super::idle_ms() {
            assert!(ms < u64::from(u32::MAX));
        }
    }
}
