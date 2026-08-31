//! Golden vectors and fuzz-corpus replay (design doc §17.2).
//!
//! The vectors in `tests/interop/golden_vectors.txt` are frozen per
//! `PROTOCOL_MINOR`: an interop partner that passes today must keep passing.
//! Changing one means bumping the minor version. Each minor so far has only
//! *added* vectors — minor 2 appended `MediaUnavailable` (docs/adr/0024),
//! minor 5 appended `FileTransferStart` (docs/adr/0032), minor 6 appended
//! `ReceiverReport` (docs/adr/0037), minor 7 appended `StreamScaleRequest`
//! (D7, docs/bugs/13-stream-resolution.md) — and every earlier vector is
//! still in the file unchanged, which is the compatibility claim this test
//! checks.
//!
//! The corpus replay runs the same assertions the `cargo fuzz` targets make,
//! so a stable toolchain still exercises them on every CI run; the nightly
//! fuzzer explores beyond the checked-in corpus.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a failed assumption must fail the test"
)]

use std::path::PathBuf;

use lumepeer_core::constants::MAX_CONTROL_FRAME_BYTES;
use lumepeer_core::protocol::{MessageEnvelope, PROTOCOL_MINOR};

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is tests/integration.
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path
}

fn unhex(text: &str) -> Vec<u8> {
    text.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).expect("bad hex in vectors")
        })
        .collect()
}

struct Vector {
    valid: bool,
    name: String,
    bytes: Vec<u8>,
}

fn vectors() -> Vec<Vector> {
    let path = repo_root().join("tests/interop/golden_vectors.txt");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));

    text.lines()
        .filter(|line| !line.trim_start().starts_with('#') && !line.trim().is_empty())
        .map(|line| {
            let mut parts = line.splitn(3, ' ');
            let kind = parts.next().unwrap();
            let name = parts.next().expect("a vector needs a name").to_owned();
            let hex = parts.next().unwrap_or("").trim();
            Vector {
                valid: kind == "valid",
                name,
                bytes: unhex(hex),
            }
        })
        .collect()
}

/// §17.2: every frozen vector still parses, or still fails, exactly as recorded.
#[test]
fn the_golden_vectors_still_hold_for_this_minor_version() {
    assert_eq!(
        PROTOCOL_MINOR, 7,
        "the vectors are frozen per minor; bump the file together with the version"
    );

    let vectors = vectors();
    assert!(vectors.len() >= 20, "the vector set shrank unexpectedly");

    for vector in vectors {
        let parsed = MessageEnvelope::decode(&vector.bytes);
        if vector.valid {
            let envelope = parsed
                .unwrap_or_else(|e| panic!("valid vector {} no longer parses: {e}", vector.name));
            let reencoded = envelope.encode().unwrap();
            assert_eq!(
                reencoded, vector.bytes,
                "valid vector {} does not re-encode to the same bytes",
                vector.name
            );
        } else {
            assert!(
                parsed.is_err(),
                "invalid vector {} is now accepted",
                vector.name
            );
        }
    }
}

/// §17.2: the checked-in fuzz corpus replays without a panic on stable.
#[test]
fn the_fuzz_corpus_replays_without_panicking() {
    let dir = repo_root().join("tests/fuzz/corpus/control_envelope");
    let entries =
        std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));

    let mut count = 0usize;
    for entry in entries {
        let path = entry.unwrap().path();
        if !path.is_file() {
            continue;
        }
        let bytes = std::fs::read(&path).unwrap();
        // The same assertions the `control_envelope` fuzz target makes.
        if let Ok(envelope) = MessageEnvelope::decode(&bytes) {
            let encoded = envelope
                .encode()
                .expect("an accepted envelope must re-encode");
            assert!(encoded.len() <= MAX_CONTROL_FRAME_BYTES);
            assert_eq!(MessageEnvelope::decode(&encoded).unwrap(), envelope);
        }
        count += 1;
    }
    assert!(count > 0, "the corpus is empty");
}

/// A frame at and just past the size bound of §9.1, checked on the exact edge.
#[test]
fn the_frame_size_bound_is_enforced_on_its_exact_edge() {
    assert!(MessageEnvelope::decode(&[]).is_err());
    // Well-formed bytes are not required: the size check runs first, before
    // postcard ever sees the input.
    let oversized = vec![0u8; MAX_CONTROL_FRAME_BYTES + 1];
    assert!(MessageEnvelope::decode(&oversized).is_err());
}
