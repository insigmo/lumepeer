# ADR 0004 — Phase 1 dependencies and the keystore fallback

Status: accepted
Date: 2026-08-12

## Context

§5 lists the dependency set, but phase 1 (§19) needs three things it does not
name: a CSPRNG for session and invite ids (§7, §9.1), a text encoding for the QR
ticket (§7), and an authenticated cipher for the encrypted-file keystore
fallback (§11.2). §24.4 requires that any deviation is recorded rather than
made silently.

§11.2 also names four native keystore backends, while §19 puts "keystore on all
platforms" in phase 4.

## Decision

Three dependencies are added:

- `rand = "0.10"` — `rand::rng()` is a CSPRNG that reseeds from the OS. It is
  already in the tree as an iroh dependency at the same major, so it adds no
  new supply-chain surface. Used for `session_id` (§9.1), `invite_id` and the
  short-link id (§7), the keystore nonce, and the per-run audit salt.
- `data-encoding = "2"` — base32 without padding for the QR payload. Also
  already transitively present via iroh.
- `chacha20poly1305 = "0.10"` — XChaCha20-Poly1305 for the encrypted-file
  keystore. AEAD, so a tampered file fails authentication instead of decoding
  into a wrong key.

The invite signing key is `ed25519-dalek` 2, the version §5 pins. iroh 1.0 uses
`ed25519-dalek` 3.0.0-rc.0 internally for endpoint identity, so both are in the
tree. They never mix: endpoint identity is iroh's, the invite signature is ours.

Keystore scope for phase 1:

- `Keystore` trait, `MemoryKeystore`, and `FileKeystore` (the §11.2 fallback).
- `keystore::open()` returns an error on every platform, including the ones with
  a native store, because none of the four backends is implemented yet. It does
  **not** silently fall back to the file: that would downgrade §11.2 storage
  without the user knowing. A caller that accepts the fallback constructs
  `FileKeystore` explicitly.

## Consequences

The phase 1 acceptance criterion is reachable without any platform SDK, and
`cargo build --workspace` still needs nothing beyond the Tauri system libraries.

`FileKeystore` is only as strong as the "OS user-specific secret" handed to it;
choosing that material per platform is part of the phase 4 keystore work, and
until then callers must not treat it as equivalent to a native store.
