# Integration tests

Design doc §17.1 and §17.2. Every row of the error matrix (§18) needs its own
test here, not just the happy path, on each supported OS (phase 4 criterion).

Phase 1 starts with: two local instances completing
`Hello`/`HelloAck` -> `ConsentRequest` -> `ConsentGrant` -> `ConsentRevoke`,
the concurrent-guest limits of §8.2, and the consent-queue overflow of §8.1.
