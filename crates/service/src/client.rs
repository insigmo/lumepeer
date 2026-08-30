//! Talking to the privileged helper service (ADR 0043).
//!
//! Deliberately tiny and deliberately safe: opening a Windows named pipe is an
//! ordinary `CreateFileW`, which the standard library already does, so a
//! client needs no `unsafe` and no Win32 bindings. That matters because the
//! caller is `apps/desktop/src-tauri`, which is `#![forbid(unsafe_code)]`.
//!
//! Every failure — no service installed, no permission, a garbled answer — is
//! the same `false`. The caller falls back to doing the work in-process, which
//! is what it did before the service existed, so a missing service degrades
//! the privilege level and never the feature (§18).

use crate::protocol::OP_DELIVER_SAS;

/// Asks the service to deliver the Secure Attention Sequence.
///
/// Returns whether the service confirmed it. `false` covers every reason at
/// once, on purpose: the caller's next move is the same for all of them, and a
/// detailed answer here would be a description of this machine's service
/// configuration handed to whatever asked.
#[must_use]
pub fn deliver_sas() -> bool {
    #[cfg(target_os = "windows")]
    {
        round_trip(OP_DELIVER_SAS)
    }
    #[cfg(not(target_os = "windows"))]
    {
        // No SAS mechanism, so no service, so nothing to ask.
        let _ = OP_DELIVER_SAS;
        false
    }
}

/// Whether the service is reachable right now.
///
/// Distinct from "installed": a service that is installed but stopped is not
/// reachable, and it is reachability the client actually depends on.
#[must_use]
pub fn is_reachable() -> bool {
    #[cfg(target_os = "windows")]
    {
        open().is_some()
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

#[cfg(target_os = "windows")]
fn open() -> Option<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(crate::protocol::ENDPOINT)
        .ok()
}

#[cfg(target_os = "windows")]
fn round_trip(op: u8) -> bool {
    use crate::protocol::{FRAME_LEN, request, succeeded};
    use std::io::{Read as _, Write as _};

    let Some(mut pipe) = open() else {
        return false;
    };
    if pipe.write_all(&request(op)).is_err() || pipe.flush().is_err() {
        return false;
    }
    let mut reply = [0u8; FRAME_LEN];
    // `read_exact`, not `read`: a short answer is not an answer. The service
    // always writes the whole frame, so anything less means the connection
    // died mid-reply and the operation's outcome is unknown — which is `false`.
    if pipe.read_exact(&mut reply).is_err() {
        return false;
    }
    succeeded(&reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With no service installed the client says so rather than hanging or
    /// panicking. This is the state every developer machine is in.
    #[test]
    fn an_absent_service_is_simply_unreachable() {
        // Whatever this machine's state, neither call may panic and both must
        // agree: unreachable means nothing was delivered.
        if !is_reachable() {
            assert!(!deliver_sas());
        }
    }
}
