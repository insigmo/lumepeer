//! The whole protocol between the client and the privileged service
//! (ADR 0043).
//!
//! Two bytes in, two bytes out, one operation. That is not minimalism for its
//! own sake: this is an unprivileged process talking to a process running as
//! `LocalSystem`, which is the classic shape of a local privilege escalation.
//! Everything the wire cannot express is something an attacker cannot ask for,
//! so the wire expresses almost nothing — no paths, no lengths, no strings, no
//! allocation driven by the peer.
//!
//! A frame is fixed-size on both directions, so there is no length field to
//! lie about and no partial read to reassemble.

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
