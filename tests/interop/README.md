# Interop tests and golden vectors

Design doc §17.2. Holds the protocol golden vectors: valid and invalid
`MessageEnvelope` encodings. They do not change without bumping
`PROTOCOL_MINOR`. Also covers `minor`-version compatibility between builds.

## Golden vectors

`golden_vectors.txt` holds frozen wire encodings of `MessageEnvelope` for
`PROTOCOL_MAJOR` 1 / `PROTOCOL_MINOR` 2, plus inputs that must stay rejected.
Vectors from earlier minors are still there byte for byte: each minor appended
its new kinds as the last enum variants, so no existing discriminant moved.
`tests/integration/tests/protocol_golden.rs` checks that every valid vector
still parses and re-encodes to the same bytes, and that every invalid one is
still refused.

Changing a line means bumping `PROTOCOL_MINOR` (§17.2). The `trailing_garbage`
vector is the reason `MessageEnvelope::decode` rejects leftover bytes: postcard
itself stops at the end of the value, which made the encoding non-canonical.
