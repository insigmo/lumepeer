//! Host-side delivery of the Secure Attention Sequence (§11; ADR 0028).
//!
//! Ctrl+Alt+Del is not a key an application may synthesize: `SendInput` with
//! the SAS combo is filtered by UIPI, and the sequence belongs to Winlogon.
//! The supported path is `sas.dll`'s `SendSAS` — the exact entry point the
//! Remote Desktop client uses, exposed by the `windows` crate under
//! `Win32::Security::Authentication::Identity`. It works when the calling
//! process runs in session 0 as a service (the `SoftwareSASGeneration`
//! policy grants services the right) or when the user launches it elevated:
//! `SendSAS(FALSE)` from an elevated process in the user's own session
//! synthesizes the sequence on the secure desktop. An unelevated process gets
//! a silent no-op from the OS itself, which is why the wire answer is an ack
//! the guest can show, not a log line.
//!
//! Both shapes ship. `crates/service` is a `LocalSystem` helper whose only
//! operation is this one, and the desktop client asks it first (ADR 0043);
//! this function is what runs when that service is not installed or not
//! running, which is also the only shape that existed before it. Nothing here
//! knows which case it is in — the caller does, and reports it.
//!
//! A platform with no SAS mechanism at all refuses from [`send_sas`], and
//! the actor turns that into a `SasAck(false)` on the wire — never a silent
//! success. There is deliberately no "can this host do it?" question to ask
//! ahead of time: whether `SendSAS` is *permitted* is not observable without
//! calling it, so attempting and reporting is the only honest answer.

/// Asks the host OS to deliver the Secure Attention Sequence.
///
/// # Errors
/// A platform without a SAS mechanism returns an error carrying the reason.
/// Windows itself never errors here: `SendSAS` synthesizes the sequence or
/// fails silently inside the OS, so success means "the call was made", and
/// the caller reports exactly that to the guest.
pub fn send_sas() -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        // `SendSAS(FALSE)`: the `asUser` argument names whose session gets
        // the sequence — `FALSE` is the calling process's own session, the
        // one the host user is sitting at.
        // SAFETY: documented Win32 entry point with no invariants beyond the
        // argument; it either synthesizes the sequence or fails silently.
        // Raw FFI with no safe binding, the justification standard ADR 0012
        // set for `SendInput`.
        #[allow(
            unsafe_code,
            reason = "SendSAS is a raw FFI entry point of sas.dll with no safe binding; \
                      same justification standard as SendInput (ADR 0012)"
        )]
        unsafe {
            windows::Win32::Security::Authentication::Identity::SendSAS(false);
        }
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err("this platform has no SAS mechanism".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real path runs against the actual system library on Windows.
    /// Whether `SendSAS` is *permitted* depends on elevation and policy,
    /// which a test run does not control, so the assertion is only that the
    /// mechanism resolves and the call is made — or, off Windows, that the
    /// refusal carries a reason. Never a panic.
    #[test]
    fn send_sas_resolves_or_refuses_with_a_reason() {
        match send_sas() {
            Ok(()) => {}
            Err(reason) => assert!(!reason.is_empty()),
        }
    }
}
