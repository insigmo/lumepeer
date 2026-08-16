//! Fuzzes the control-message parser (design doc §17.2, §19 phase 3).
//!
//! The parser is the first thing an unauthenticated peer reaches, so it must
//! never panic, never allocate on an announced length it has not checked, and
//! never accept a frame outside the bounds of §9.1.

#![no_main]

use libfuzzer_sys::fuzz_target;
use lumepeer_core::constants::MAX_CONTROL_FRAME_BYTES;
use lumepeer_core::protocol::MessageEnvelope;

fuzz_target!(|data: &[u8]| {
    match MessageEnvelope::decode(data) {
        Ok(envelope) => {
            // Anything the parser accepts must survive a round trip, otherwise
            // the two sides of an interop pair can disagree about a frame both
            // consider valid.
            let encoded = envelope.encode().expect("an accepted envelope must re-encode");
            assert!(encoded.len() <= MAX_CONTROL_FRAME_BYTES);
            let again = MessageEnvelope::decode(&encoded).expect("a re-encoded envelope must parse");
            assert_eq!(envelope, again);
        }
        Err(_) => {
            // Rejection is a normal outcome; what matters is that it is a
            // rejection and not a panic (§2.4).
        }
    }
});
