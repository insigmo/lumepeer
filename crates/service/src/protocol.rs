//! The whole protocol between the client and the privileged service
//! (ADR 0043, ADR 0046).
//!
//! Two bytes in, two bytes out, one operation *per pipe round trip*. That is
//! not minimalism for its own sake: this is an unprivileged process talking
//! to a process running as `LocalSystem`, which is the classic shape of a
//! local privilege escalation. Everything the wire cannot express is
//! something an attacker cannot ask for, so the wire expresses almost
//! nothing — no paths, no lengths, no strings, no allocation driven by the
//! peer.
//!
//! A frame is fixed-size on both directions, so there is no length field to
//! lie about and no partial read to reassemble.
//!
//! ADR 0046 adds a second operation, [`OP_CAPTURE_SECURE_DESKTOP`], whose
//! answer cannot fit two bytes: a screen's worth of pixels. Rather than teach
//! the pipe a length field, the pixels travel over a second, fixed-capacity
//! channel this file also names — a single named shared-memory mapping,
//! sized once at compile time and never resized at runtime, so a caller
//! still cannot make the service allocate, expose or listen on anything it
//! did not already have before the request arrived. See `crates/service/src/
//! frame.rs` for the mechanics and ADR 0046 for why a mapping was chosen
//! over extending `crates/media`'s decoder ring buffer (§11.3).

/// First byte of every frame, in both directions.
///
/// Not a security measure — anything that can open the pipe can send it. It is
/// here so that something else connecting to the pipe by accident is rejected
/// as garbage instead of being parsed as an operation.
pub const MAGIC: u8 = b'L';

/// Bytes in a request, and in a response.
pub const FRAME_LEN: usize = 2;

/// Deliver the Secure Attention Sequence to the interactive session (§11).
///
/// The only operation this service has. A service exists at all because
/// `SendSAS` needs either session 0 or an elevated process (`sas.rs`), and
/// asking a user to run a remote-access client elevated all the time is worse
/// than giving one narrow capability to a service that can do nothing else.
pub const OP_DELIVER_SAS: u8 = 0x01;

/// Capture one frame of the secure desktop (`Winsta0\Winlogon`) and publish
/// it to the mapping named by [`SECURE_DESKTOP_MAPPING_NAME`] (§11, ADR
/// 0046).
///
/// Narrow on purpose, the same way [`OP_DELIVER_SAS`] is: this is not "give
/// me any desktop" or "capture the desktop named X" — there is exactly one
/// desktop this operation ever means, and the request carries no parameter
/// that could name a different one. `STATUS_OK` means a fresh frame is now
/// in the mapping; it carries no more information than that, same as every
/// other reply this protocol gives.
pub const OP_CAPTURE_SECURE_DESKTOP: u8 = 0x02;

/// The operation was carried out.
pub const STATUS_OK: u8 = 0x00;
/// The operation was refused, or is not one this service knows.
///
/// Deliberately the only failure: a caller learns whether it worked and
/// nothing about why, exactly as `UnattendedError` refuses to be an oracle.
pub const STATUS_REFUSED: u8 = 0x01;

/// Name of the endpoint the service listens on.
///
/// Windows: a named pipe whose DACL admits `LocalSystem`, administrators and
/// interactive users, and nobody else — notably not network logons and not
/// service accounts.
#[cfg(target_os = "windows")]
pub const ENDPOINT: &str = r"\\.\pipe\lumepeer-service";

/// Name of the shared-memory mapping [`OP_CAPTURE_SECURE_DESKTOP`] publishes
/// frames into (ADR 0046).
///
/// `Global\` is required, not stylistic: the service runs in session 0 and
/// the client runs in an interactive session, and a name without that prefix
/// would be created in the caller's own session-private `BaseNamedObjects`,
/// invisible across the session boundary this mapping exists to cross.
/// `LocalSystem` holds the privilege needed to *create* an object in the
/// `Global` namespace; opening an existing one, which is all the client ever
/// does, needs no such privilege — only what the mapping's own DACL admits
/// (`crates/service/src/frame.rs`).
#[cfg(target_os = "windows")]
pub const SECURE_DESKTOP_MAPPING_NAME: &str = r"Global\lumepeer-secure-desktop-frame";

/// Bytes of the fixed header at the start of the mapping: `width:u32 |
/// height:u32 | payload_len:u32`, little-endian, read and written as plain
/// byte slices rather than a `#[repr(C)]` cast — the same style
/// `apps/desktop/src-tauri/src/view.rs::decode_media_payload` already uses
/// for untrusted-shaped input, so there is no struct layout to get subtly
/// wrong across the two sides of the mapping.
pub const SECURE_DESKTOP_FRAME_HEADER_BYTES: usize = 12;

/// Capacity of the payload region after the header: one BGRA8 frame at this
/// pipeline's existing size ceiling (ADR 0046).
///
/// This is the same `1920 * 1080` bound `lumepeer_core::constants::
/// MAX_PICTURE_PIXELS` already puts on every other frame this codebase
/// moves, kept as a literal here rather than imported: `crates/service` does
/// not depend on `lumepeer-core` (ADR 0043's dependency-minimalism
/// argument, restated for this crate in ADR 0046), so raising one bound
/// without the other is caught by review rather than by the compiler. A
/// frame captured larger than this is refused by [`crate::frame`] rather
/// than published truncated — a partial BGRA8 image is a corrupt one, not a
/// smaller one — so the mapping never resizes at runtime and never carries
/// a picture nobody asked for at that size.
pub const SECURE_DESKTOP_FRAME_CAPACITY_BYTES: usize = 1920 * 1080 * 4;

/// Total size of the mapping: the fixed header plus the fixed payload
/// capacity. Neither side ever asks the other how big it is — both compute
/// this the same way from the same two constants.
pub const SECURE_DESKTOP_FRAME_MAPPING_BYTES: usize =
    SECURE_DESKTOP_FRAME_HEADER_BYTES + SECURE_DESKTOP_FRAME_CAPACITY_BYTES;

/// Builds a request frame.
#[must_use]
pub const fn request(op: u8) -> [u8; FRAME_LEN] {
    [MAGIC, op]
}

/// The operation a request frame names, if it is a request frame at all.
#[must_use]
pub const fn parse_request(frame: &[u8; FRAME_LEN]) -> Option<u8> {
    if frame[0] == MAGIC {
        Some(frame[1])
    } else {
        None
    }
}

/// Builds a response frame.
#[must_use]
pub const fn response(status: u8) -> [u8; FRAME_LEN] {
    [MAGIC, status]
}

/// Whether a response frame says the operation succeeded.
///
/// A frame that is not a response at all reads as failure: this is the answer
/// to "did Ctrl+Alt+Del reach the host", and the safe reading of an
/// unintelligible answer is "no".
#[must_use]
pub const fn succeeded(frame: &[u8; FRAME_LEN]) -> bool {
    frame[0] == MAGIC && frame[1] == STATUS_OK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_round_trips() {
        assert_eq!(
            parse_request(&request(OP_DELIVER_SAS)),
            Some(OP_DELIVER_SAS)
        );
    }

    #[test]
    fn the_secure_desktop_capture_request_round_trips() {
        assert_eq!(
            parse_request(&request(OP_CAPTURE_SECURE_DESKTOP)),
            Some(OP_CAPTURE_SECURE_DESKTOP)
        );
    }

    /// A wire that could confuse the two operations would let a caller ask
    /// for one and have the service perform the other.
    #[test]
    fn the_two_operations_are_distinct() {
        assert_ne!(OP_DELIVER_SAS, OP_CAPTURE_SECURE_DESKTOP);
    }

    /// The mapping's total size is computed the same way on both sides, from
    /// the same two constants — nobody ever sends the other one a size.
    #[test]
    fn the_mapping_size_is_header_plus_capacity() {
        assert_eq!(
            SECURE_DESKTOP_FRAME_MAPPING_BYTES,
            SECURE_DESKTOP_FRAME_HEADER_BYTES + SECURE_DESKTOP_FRAME_CAPACITY_BYTES
        );
    }

    #[test]
    fn a_frame_without_the_magic_is_not_a_request() {
        assert_eq!(parse_request(&[0, OP_DELIVER_SAS]), None);
    }

    #[test]
    fn only_an_explicit_ok_reads_as_success() {
        assert!(succeeded(&response(STATUS_OK)));
        assert!(!succeeded(&response(STATUS_REFUSED)));
        // Garbage, a truncated frame padded with zeroes, or a reply from
        // something else on the pipe: all of it means "no".
        assert!(!succeeded(&[0, 0]));
        assert!(!succeeded(&[MAGIC, 0xff]));
    }
}
