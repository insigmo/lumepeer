//! The whole protocol between the client and the privileged service
//! (ADR 0043, ADR 0049).
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
//! ADR 0049 adds a second operation, [`OP_CAPTURE_SECURE_DESKTOP`], whose
//! answer cannot fit two bytes: a screen's worth of pixels. Rather than teach
//! the pipe a length field, the pixels travel over a second, fixed-capacity
//! channel this file also names — a single named shared-memory mapping,
//! sized once at compile time and never resized at runtime, so a caller
//! still cannot make the service allocate, expose or listen on anything it
//! did not already have before the request arrived. See `crates/service/src/
//! frame.rs` for the mechanics and ADR 0049 for why a mapping was chosen
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

/// Perform one input event on the secure desktop (`Winsta0\Winlogon`) — a
/// pointer move, or a key/button press or release (ADR 0057).
///
/// The mirror of [`OP_CAPTURE_SECURE_DESKTOP`]: where that reads one frame off
/// the secure desktop, this drives one event onto it, through the same
/// short-lived `LocalSystem` worker (`secure_desktop_launch`). Unlike the
/// other two operations this one is *not* parameterless — it cannot be, an
/// event has a where and a which — so an `OP_INJECT_SECURE_DESKTOP` request
/// frame is followed on the wire by exactly [`INJECT_PAYLOAD_LEN`] more bytes,
/// a size fixed here at compile time and never named by the caller. There is
/// still no length field, no string and no peer-driven allocation: the wire
/// now expresses one more *fixed-shape* thing, not an open-ended one
/// (ADR 0057 §3).
pub const OP_INJECT_SECURE_DESKTOP: u8 = 0x03;

/// Bytes of the fixed input descriptor that follow an
/// [`OP_INJECT_SECURE_DESKTOP`] request frame: `kind:u8 | logical:u32 |
/// x:u16 | y:u16`, little-endian, read and written as a plain byte array the
/// same way the frame-mapping header is, so there is no struct layout to get
/// wrong across the two sides.
pub const INJECT_PAYLOAD_LEN: usize = 9;

/// What an [`OP_INJECT_SECURE_DESKTOP`] descriptor asks the worker to do.
///
/// `logical` is the guest's own logical key/button code — the exact encoding
/// `lumepeer_core::protocol::InputEventPayload::logical` carries, so the
/// worker's `SendInput` reproduces what the ordinary in-session injector would
/// have done, only on `Winlogon`. `x`/`y` are normalized `0..=65535` over the
/// captured surface, which is `MOUSEEVENTF_ABSOLUTE`'s own coordinate space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectAction {
    /// Move the pointer to an absolute, normalized point.
    Move {
        /// Horizontal position, `0..=65535`.
        x: u16,
        /// Vertical position, `0..=65535`.
        y: u16,
    },
    /// Press the key or pointer button named by `logical`.
    Press {
        /// The guest's logical key/button code.
        logical: u32,
    },
    /// Release the key or pointer button named by `logical`.
    Release {
        /// The guest's logical key/button code.
        logical: u32,
    },
}

/// Byte-0 discriminant of a [`InjectAction::Move`] descriptor.
const INJECT_KIND_MOVE: u8 = 0;
/// Byte-0 discriminant of a [`InjectAction::Press`] descriptor.
const INJECT_KIND_PRESS: u8 = 1;
/// Byte-0 discriminant of a [`InjectAction::Release`] descriptor.
const INJECT_KIND_RELEASE: u8 = 2;

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
/// frames into (ADR 0049).
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
/// pipeline's existing size ceiling (ADR 0049).
///
/// This is the same `1920 * 1080` bound `lumepeer_core::constants::
/// MAX_PICTURE_PIXELS` already puts on every other frame this codebase
/// moves, kept as a literal here rather than imported: `crates/service` does
/// not depend on `lumepeer-core` (ADR 0043's dependency-minimalism
/// argument, restated for this crate in ADR 0049), so raising one bound
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

/// Serializes an [`InjectAction`] into the fixed descriptor that follows an
/// [`OP_INJECT_SECURE_DESKTOP`] request on the wire.
///
/// Every field has a slot whether or not this action uses it, so the encoding
/// is a fixed [`INJECT_PAYLOAD_LEN`] bytes with no branch on length. Unused
/// slots are zero, and [`parse_inject`] ignores them for the kinds that do not
/// read them.
#[must_use]
pub fn encode_inject(action: InjectAction) -> [u8; INJECT_PAYLOAD_LEN] {
    let mut out = [0u8; INJECT_PAYLOAD_LEN];
    match action {
        InjectAction::Move { x, y } => {
            out[0] = INJECT_KIND_MOVE;
            out[5..7].copy_from_slice(&x.to_le_bytes());
            out[7..9].copy_from_slice(&y.to_le_bytes());
        }
        InjectAction::Press { logical } => {
            out[0] = INJECT_KIND_PRESS;
            out[1..5].copy_from_slice(&logical.to_le_bytes());
        }
        InjectAction::Release { logical } => {
            out[0] = INJECT_KIND_RELEASE;
            out[1..5].copy_from_slice(&logical.to_le_bytes());
        }
    }
    out
}

/// The [`InjectAction`] a descriptor names, or `None` if byte 0 is not a kind
/// this protocol knows.
///
/// An unknown kind is the descriptor's only failure, and it reads as `None`
/// exactly the way an unknown opcode reads as [`STATUS_REFUSED`]: the caller
/// learns it did not work and nothing about why.
#[must_use]
pub fn parse_inject(payload: &[u8; INJECT_PAYLOAD_LEN]) -> Option<InjectAction> {
    let logical = u32::from_le_bytes([payload[1], payload[2], payload[3], payload[4]]);
    let x = u16::from_le_bytes([payload[5], payload[6]]);
    let y = u16::from_le_bytes([payload[7], payload[8]]);
    match payload[0] {
        INJECT_KIND_MOVE => Some(InjectAction::Move { x, y }),
        INJECT_KIND_PRESS => Some(InjectAction::Press { logical }),
        INJECT_KIND_RELEASE => Some(InjectAction::Release { logical }),
        _ => None,
    }
}

/// The four integers the service passes an inject worker on its command line
/// (`kind logical x y`, ADR 0057 §3), and the sole encoder for them.
///
/// The worker crosses the session boundary as a *process*, not over the pipe,
/// so its parameters are argv, not a frame — but they are still the same fixed
/// set of bounded integers, built here and nowhere else so the launcher and
/// [`inject_from_args`] cannot drift.
#[must_use]
pub fn inject_to_args(action: InjectAction) -> [u32; 4] {
    match action {
        InjectAction::Move { x, y } => [u32::from(INJECT_KIND_MOVE), 0, u32::from(x), u32::from(y)],
        InjectAction::Press { logical } => [u32::from(INJECT_KIND_PRESS), logical, 0, 0],
        InjectAction::Release { logical } => [u32::from(INJECT_KIND_RELEASE), logical, 0, 0],
    }
}

/// Rebuilds an [`InjectAction`] from the four command-line integers, refusing
/// an unknown kind or an `x`/`y` that does not fit the normalized `u16` range.
///
/// The inverse of [`inject_to_args`]. A worker re-parsing its own arguments is
/// the last place the descriptor is validated before it becomes a `SendInput`,
/// so it fails closed rather than clamping silently.
#[must_use]
pub fn inject_from_args(kind: u32, logical: u32, x: u32, y: u32) -> Option<InjectAction> {
    let x = u16::try_from(x).ok()?;
    let y = u16::try_from(y).ok()?;
    match u8::try_from(kind).ok()? {
        INJECT_KIND_MOVE => Some(InjectAction::Move { x, y }),
        INJECT_KIND_PRESS => Some(InjectAction::Press { logical }),
        INJECT_KIND_RELEASE => Some(InjectAction::Release { logical }),
        _ => None,
    }
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

    /// A wire that could confuse the operations would let a caller ask for
    /// one and have the service perform another.
    #[test]
    fn the_operations_are_distinct() {
        assert_ne!(OP_DELIVER_SAS, OP_CAPTURE_SECURE_DESKTOP);
        assert_ne!(OP_DELIVER_SAS, OP_INJECT_SECURE_DESKTOP);
        assert_ne!(OP_CAPTURE_SECURE_DESKTOP, OP_INJECT_SECURE_DESKTOP);
    }

    /// Every inject action survives the fixed descriptor unchanged, including
    /// the coordinate extremes `MOUSEEVENTF_ABSOLUTE` runs to.
    #[test]
    fn inject_actions_round_trip_through_the_fixed_descriptor() {
        for action in [
            InjectAction::Move { x: 0, y: 0 },
            InjectAction::Move {
                x: u16::MAX,
                y: u16::MAX,
            },
            InjectAction::Move {
                x: 12_345,
                y: 54_321,
            },
            InjectAction::Press { logical: 0x0d },
            InjectAction::Release {
                logical: 0xF000_0000,
            },
            InjectAction::Press { logical: u32::MAX },
        ] {
            assert_eq!(parse_inject(&encode_inject(action)), Some(action));
        }
    }

    /// A descriptor whose kind byte names nothing is refused, not guessed —
    /// the same one-bit "no" an unknown opcode gets.
    #[test]
    fn an_unknown_inject_kind_is_refused() {
        let mut payload = encode_inject(InjectAction::Move { x: 1, y: 2 });
        payload[0] = 0xff;
        assert_eq!(parse_inject(&payload), None);
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
