//! CPU time of the whole process and of the calling thread.

use std::time::Duration;

#[cfg(windows)]
fn filetime(ft: windows_sys::Win32::Foundation::FILETIME) -> Duration {
    let ticks = (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime);
    Duration::from_nanos(ticks * 100)
}

/// User + kernel time of every thread this process has run so far.
#[cfg(windows)]
pub fn process() -> Duration {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut c, mut e, mut k, mut u) = (zero, zero, zero, zero);
    // SAFETY: the pseudo-handle is always valid and the four out-pointers are locals.
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) };
    filetime(k) + filetime(u)
}

/// User + kernel time of the calling thread.
#[cfg(windows)]
pub fn thread() -> Duration {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut c, mut e, mut k, mut u) = (zero, zero, zero, zero);
    // SAFETY: as above.
    unsafe { GetThreadTimes(GetCurrentThread(), &mut c, &mut e, &mut k, &mut u) };
    filetime(k) + filetime(u)
}

#[cfg(unix)]
fn clock(id: libc::clockid_t) -> Duration {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a local the call only writes.
    unsafe { libc::clock_gettime(id, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

#[cfg(unix)]
pub fn process() -> Duration {
    clock(libc::CLOCK_PROCESS_CPUTIME_ID)
}

#[cfg(unix)]
pub fn thread() -> Duration {
    clock(libc::CLOCK_THREAD_CPUTIME_ID)
}
