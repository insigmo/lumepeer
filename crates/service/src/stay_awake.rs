//! Keeping the machine out of sleep while a guest is connected to it.
//!
//! A host whose power plan puts it to sleep after N idle minutes fell asleep
//! under a guest who was only watching, or whose input could not reset the
//! idle timer — on a lock screen a pointer move is cached rather than sent
//! (ADR 0057) — and the session ended with the machine. What the guest saw was
//! a picture that died.
//!
//! A system-required power request is exactly "do not sleep for idleness" and
//! nothing more: the display still turns off, and the screen still locks, on
//! whatever schedule the owner set. Unlike `SetThreadExecutionState` it belongs
//! to a handle rather than to a thread, so it stays in force whichever worker
//! thread the host's actor happens to be running on. Windows lists it by name
//! in `powercfg /requests`, so the owner can see what is keeping the machine
//! up.

#![allow(
    unsafe_code,
    reason = "PowerCreateRequest and its companions have no safe bindings; same \
              justification standard as the rest of this crate's Win32 surface \
              (ADR 0043, ADR 0049)"
)]

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Power::{
    PowerClearRequest, PowerCreateRequest, PowerRequestSystemRequired, PowerSetRequest,
};
use windows::Win32::System::Threading::{
    POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0,
};
use windows::core::PWSTR;

/// `POWER_REQUEST_CONTEXT_VERSION`, the only version there is. `windows-rs`
/// files it under `Win32_System_SystemServices`, a feature this crate has no
/// other use for.
const POWER_REQUEST_CONTEXT_VERSION: u32 = 0;

/// What `powercfg /requests` shows next to this process.
const REASON: &str = "Lumepeer: a remote session is connected to this computer";

/// A held request that keeps the machine from sleeping for idleness; released
/// when dropped.
#[derive(Debug)]
pub struct StayAwake {
    request: HANDLE,
}

// SAFETY: a power request handle is a kernel handle; any thread may set,
// clear or close it.
unsafe impl Send for StayAwake {}

impl StayAwake {
    /// Asks Windows not to sleep for idleness until the returned value is
    /// dropped; `None`, logged, if it refused.
    #[must_use]
    pub fn hold() -> Option<Self> {
        let mut reason: Vec<u16> = REASON.encode_utf16().chain(std::iter::once(0)).collect();
        let context = REASON_CONTEXT {
            Version: POWER_REQUEST_CONTEXT_VERSION,
            Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
            Reason: REASON_CONTEXT_0 {
                SimpleReasonString: PWSTR(reason.as_mut_ptr()),
            },
        };
        // SAFETY: `context` is a fully initialized REASON_CONTEXT whose string
        // is a null-terminated wide buffer that outlives the call; the kernel
        // copies it.
        let request = match unsafe { PowerCreateRequest(&raw const context) } {
            Ok(request) => request,
            Err(error) => {
                tracing::warn!(%error, "cannot create a power request; the host may sleep under its guest");
                return None;
            }
        };
        // SAFETY: `request` is the live handle just created.
        if let Err(error) = unsafe { PowerSetRequest(request, PowerRequestSystemRequired) } {
            tracing::warn!(%error, "Windows refused the power request; the host may sleep under its guest");
            // SAFETY: as above, and not used again.
            unsafe {
                let _ = CloseHandle(request);
            }
            return None;
        }
        Some(Self { request })
    }
}

impl Drop for StayAwake {
    fn drop(&mut self) {
        // SAFETY: `self.request` is the live handle `hold` set, owned by this
        // value and not used after this.
        unsafe {
            let _ = PowerClearRequest(self.request, PowerRequestSystemRequired);
            let _ = CloseHandle(self.request);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The request can be taken and given back, repeatedly, by an ordinary
    /// unelevated process — the one the client is when its tests run.
    #[test]
    fn a_power_request_is_held_and_released() {
        for _ in 0..3 {
            let held = StayAwake::hold();
            assert!(
                held.is_some(),
                "Windows refused a system-required power request"
            );
            drop(held);
        }
    }
}
