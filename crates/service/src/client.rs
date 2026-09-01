//! Talking to the privileged helper service (ADR 0043, ADR 0046).
//!
//! Opening the pipe and sending two bytes needs no `unsafe` and no Win32
//! bindings — an ordinary `CreateFileW`, which the standard library already
//! does — which matters because the caller is `apps/desktop/src-tauri`,
//! which is `#![forbid(unsafe_code)]`. [`capture_secure_desktop_frame`] is
//! the one function here that reaches further, into [`crate::frame`], to
//! read the shared-memory side channel `OP_CAPTURE_SECURE_DESKTOP`'s answer
//! travels over — a memory mapping has no safe standard-library wrapper, the
//! way opening a named pipe does. That `unsafe` stays inside this crate,
//! which already carries it for the service's own side (ADR 0043); the
//! caller in `apps/desktop/src-tauri` gains none.
//!
//! Every failure — no service installed, no permission, a garbled answer — is
//! the same `false`/`None`. The caller falls back to doing the work
//! in-process (`OP_DELIVER_SAS`) or to the honest degradation of
//! `docs/bugs/11-uac-degradation.md` (`OP_CAPTURE_SECURE_DESKTOP`), so a
//! missing service degrades the privilege level or the picture, never leaves
//! the caller unable to tell what happened (§18).

use crate::protocol::{OP_CAPTURE_SECURE_DESKTOP, OP_DELIVER_SAS};

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

/// One frame of the secure desktop, as [`capture_secure_desktop_frame`] hands
/// it back (ADR 0046).
#[derive(Debug, Clone)]
pub struct SecureDesktopFrame {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Tightly packed BGRA8 pixels, `width * height * 4` bytes.
    pub data: Vec<u8>,
}

/// Asks the service for one frame of the secure desktop and reads it back.
///
/// `None` covers every reason at once, same as [`deliver_sas`]: not
/// installed, not reachable, the caller's session is not the one attached to
/// the active console (ADR 0046's session-binding check), the secure desktop
/// is not actually showing anything capturable right now, or the mapping
/// could not be read. The caller's answer to all of them is the same —
/// `docs/bugs/11-uac-degradation.md`'s honest message — so there is nothing
/// for a more detailed result to buy here that would not also be a
/// description of this machine's state handed to whatever asked.
#[must_use]
pub fn capture_secure_desktop_frame() -> Option<SecureDesktopFrame> {
    #[cfg(target_os = "windows")]
    {
        if !round_trip(OP_CAPTURE_SECURE_DESKTOP) {
            return None;
        }
        let (width, height, data) = crate::frame::Reader::open()?.read()?;
        Some(SecureDesktopFrame {
            width,
            height,
            data,
        })
    }
    #[cfg(not(target_os = "windows"))]
    {
        // No secure desktop concept off Windows, so no service, so nothing
        // to ask.
        let _ = OP_CAPTURE_SECURE_DESKTOP;
        None
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

    /// Same property for the secure-desktop capture: an unreachable service
    /// cannot have produced a frame, and — unlike `deliver_sas`, which has a
    /// real side effect this suite must never trigger — a *reachable* one is
    /// safe to actually call here too. Whether it is an older service that
    /// refuses the unknown opcode, or a real one with no secure desktop
    /// currently showing, or one whose caller fails ADR 0046's session
    /// check, every outcome is `None` and none of them has a visible effect
    /// on the machine — capturing is read-only, unlike `SendSAS`.
    #[test]
    fn capture_secure_desktop_frame_never_panics() {
        if is_reachable() {
            let _ = capture_secure_desktop_frame();
        } else {
            assert!(capture_secure_desktop_frame().is_none());
        }
    }
}
