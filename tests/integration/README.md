# Integration tests

Design doc §17.1 and §17.2. Every row of the error matrix (§18) needs its own
test here, not just the happy path, on each supported OS (phase 4 criterion).

These tests live in their own workspace member so that the `/tests/integration`
path of §6 is preserved; `cargo test --workspace` picks them up.

Phase 1 covers:

| File | Rows it covers |
|---|---|
| `consent_cycle.rs` | The phase 1 acceptance criterion: two local instances complete `Hello`/`HelloAck` -> `ConsentRequest` -> `ConsentGrant` -> `ConsentRevoke`, and each session gets its own CSPRNG id. |
| `limits.rs` | §8.2 concurrent-guest ceilings for Trial/Pro and Team, the single-controller rule, and the §8.1 consent-queue overflow. |
| `protocol_negative.rs` | §18: protocol major mismatch refused before consent, oversized frame refused on its length prefix. |

The endpoints bind with iroh's `Minimal` preset, so no relay and no address
lookup service is involved and the tests need no network beyond loopback.
