# Fuzzing

Design doc §5.1, §17.2, §19 phase 3. `cargo-fuzz` (libFuzzer) against the
parsers an unauthenticated peer can reach. The corpus is part of the release
checklist (§21).

This directory is its own crate and deliberately not a workspace member:
`cargo fuzz` builds it on nightly with sanitizer flags, and `libfuzzer-sys`
would otherwise put a nightly-only dependency into every stable build.

## Targets

| Target | Parser | Why it is reachable |
|---|---|---|
| `control_envelope` | `MessageEnvelope::decode` (§9.1) | First thing an unauthenticated peer sends. |
| `license_token` | `LicenseToken::parse_and_verify` (§12.1) | A token is attacker-supplied until `verify_strict` says otherwise. |
| `invite_ticket` | `InviteTicket::from_qr_string` (§7) | Comes from a QR code or a short link. |

## Running

```sh
cargo install cargo-fuzz
cargo +nightly fuzz run --fuzz-dir tests/fuzz control_envelope
```

The targets assert more than "no panic":

- `control_envelope` requires anything accepted to re-encode to the same bytes.
  Non-canonical input, such as a frame with trailing padding, must be rejected
  rather than accepted and silently normalized.
- `license_token` requires that no fuzzer-generated input ever verifies against
  a key whose private half the fuzzer does not have.
- `invite_ticket` requires the same round-trip property as the control frames.

`tests/integration/tests/protocol_golden.rs` replays the checked-in corpus with
the same assertions on stable, so a CI run without nightly still covers it.
