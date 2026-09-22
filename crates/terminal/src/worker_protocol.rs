//! What the elevated client says to the unelevated shell worker (ADR 0102).
//!
//! One direction is framed and one is not, and the asymmetry is the whole
//! design. Back from the worker comes what the shell said and nothing else, so
//! raw bytes on its stdout are a complete language. Towards the worker travel
//! two different things — what somebody typed, and how big the window now is —
//! down one pipe, because the worker owns the pseudo-console and
//! `ResizePseudoConsole` is a call only it can make. Two kinds need a tag.
//!
//! Both sides of it live here so they cannot drift: `crates/terminal-worker`
//! reads exactly what `windows.rs` writes.
//!
//! Nothing in here is a security boundary. The worker runs as the person at
//! the machine and starts that person's own shell; anything that could send it
//! a frame could already run a shell as itself. What the boundary is remains
//! `terminal_allows(&peer)` on the actor loop, upstream of all of this
//! (ADR 0079).

/// What the worker writes once the shell is actually running.
///
/// The client blocks on this one byte before it reports a shell at all, the
/// way the decoder worker's readiness byte works (§11.3): a worker that could
/// not start a shell must become a refusal the person sees, not a terminal
/// that opens and closes again. Deliberately not a printable character, so a
/// diagnostic the worker wrote to its shared stderr can never be mistaken for
/// it.
pub const WORKER_READY: u8 = 0x01;

/// Bytes on their way to the shell.
pub const FRAME_INPUT: u8 = 0;
/// A new geometry: `cols:u16 | rows:u16`, little endian.
pub const FRAME_RESIZE: u8 = 1;

/// `kind:u8 | length:u32` before every payload, little endian.
pub const FRAME_HEADER_BYTES: usize = 1 + 4;

/// Payload length of a [`FRAME_RESIZE`], which is the only fixed-size frame.
pub const RESIZE_PAYLOAD_BYTES: usize = 4;

/// Longest payload either side will write or accept, in bytes.
///
/// A bound rather than a buffer size: the worker reads a length off a pipe and
/// is about to allocate for it, and "a number a peer sent decides an
/// allocation" is the shape §9.1 refuses everywhere else. Nothing legitimate
/// comes close — the actor's own `TERMINAL_OUTPUT_MAX_BYTES` is smaller, and a
/// keystroke is a handful of bytes.
pub const FRAME_PAYLOAD_MAX_BYTES: usize = 64 * 1024;

/// Frames `payload` as input for the shell.
#[must_use]
pub fn encode_input(payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(FRAME_HEADER_BYTES + payload.len());
    frame.push(FRAME_INPUT);
    frame.extend_from_slice(
        &u32::try_from(payload.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    frame.extend_from_slice(payload);
    frame
}

/// Frames a new geometry for the worker's pseudo-console.
#[must_use]
pub fn encode_resize(cols: u16, rows: u16) -> Vec<u8> {
    let mut frame = Vec::with_capacity(FRAME_HEADER_BYTES + RESIZE_PAYLOAD_BYTES);
    frame.push(FRAME_RESIZE);
    frame.extend_from_slice(
        &u32::try_from(RESIZE_PAYLOAD_BYTES)
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    frame.extend_from_slice(&cols.to_le_bytes());
    frame.extend_from_slice(&rows.to_le_bytes());
    frame
}

/// A geometry read back out of a [`FRAME_RESIZE`] payload.
///
/// `None` for a payload that is not exactly four bytes, which the worker
/// treats as a frame to skip rather than as a reason to stop: a shell somebody
/// is typing into should not end because one resize was malformed.
#[must_use]
pub fn decode_resize(payload: &[u8]) -> Option<(u16, u16)> {
    let cols: [u8; 2] = payload.get(0..2)?.try_into().ok()?;
    let rows: [u8; 2] = payload.get(2..4)?.try_into().ok()?;
    Some((u16::from_le_bytes(cols), u16::from_le_bytes(rows)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_geometry_survives_the_round_trip() {
        let frame = encode_resize(120, 40);
        assert_eq!(frame.len(), FRAME_HEADER_BYTES + RESIZE_PAYLOAD_BYTES);
        assert_eq!(frame[0], FRAME_RESIZE);
        assert_eq!(decode_resize(&frame[FRAME_HEADER_BYTES..]), Some((120, 40)));
    }

    #[test]
    fn input_carries_its_own_length_and_nothing_else() {
        let frame = encode_input(b"ls\r");
        assert_eq!(frame[0], FRAME_INPUT);
        assert_eq!(&frame[1..5], &3u32.to_le_bytes());
        assert_eq!(&frame[FRAME_HEADER_BYTES..], b"ls\r");
    }

    /// The worker skips one of these rather than ending the shell, so the
    /// decode has to say "no" instead of panicking on the slice.
    #[test]
    fn a_resize_payload_of_the_wrong_length_is_not_a_geometry() {
        assert_eq!(decode_resize(&[]), None);
        assert_eq!(decode_resize(&[1, 2, 3]), None);
        // Longer than four is still readable: the first four bytes are the
        // geometry and a later version may append to it.
        assert_eq!(decode_resize(&[80, 0, 24, 0, 9]), Some((80, 24)));
    }

    /// An empty keystroke buffer is a frame, not a zero-length write that the
    /// other side would read as the pipe closing.
    #[test]
    fn an_empty_input_is_still_a_frame() {
        assert_eq!(encode_input(b"").len(), FRAME_HEADER_BYTES);
    }
}
