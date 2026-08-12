# ADR 0006 — Broker decisions left open by §12

Status: accepted
Date: 2026-08-13

## Context

§12 fixes the token format, the endpoints, the idempotency rule, the timestamp
window and the replay protection. Four things it leaves to implementation had
to be decided to make phase 3 (§19) testable, and one parser rule turned out to
be looser than §17.2 assumes.

## Decisions

**Webhook signature scheme.** §12.2 requires "provider signature" without
naming an algorithm. The broker verifies a keyed BLAKE3 MAC over
`timestamp || "." || body`, sent hex-encoded in `x-lumepeer-signature` with the
timestamp in `x-lumepeer-timestamp`. Binding the timestamp into the MAC is what
makes the 5 minute window meaningful: without it, an attacker could replay a
valid body under a fresh timestamp. `blake3::Hash` compares in constant time,
so no separate `subtle` dependency is needed, consistent with §20 rejecting
hand-rolled comparisons. A real provider integration will bring its own scheme;
this one is the contract the tests and any first-party payment shim speak.

**Device seats and conflict resolution.** §19 requires the broker to "resolve a
conflict between two devices by heartbeat" without defining how many devices a
license has. A license has as many device seats as the plan has concurrent
guest slots (§8.2, §14): one for Trial and Pro, five for Team. When one more
device asks for a token, the device with the oldest heartbeat is displaced. Its
row is kept and marked, not deleted, so its next heartbeat answers
`ok: false` with a reason instead of `UNKNOWN_DEVICE`, which would look like a
broker fault to a client that did nothing wrong.

**Token lifetime.** Tokens are valid for 24 hours, capped by the license's own
`expires_at`. That is shorter than every offline grace of §12.3, so a device
must refresh at least once before the grace window can carry it, and a revoked
license stops mattering within a day even for a client that never heartbeats.

**Token identifier fields.** `license_id` and `device_id` in the token are the
first 16 bytes of the BLAKE3 of the textual ids. The layout of §12.1 is fixed at
16 bytes each, client-supplied ids are arbitrary strings, and hashing keeps an
account-controlled string out of the token entirely (§15).

**Trailing bytes in a control frame.** `postcard::from_bytes` stops at the end
of the value and ignores what follows, so a frame with padding was accepted and
then re-encoded to different bytes. §9.1 frames are length-prefixed, so trailing
bytes mean the two sides disagree about the frame's content, and §17.2 freezes
the encoding as canonical. `MessageEnvelope::decode` now uses `take_from_bytes`
and rejects any leftover as `Malformed`. This was found by the golden vector for
`trailing_garbage`, which is exactly what those vectors are for.

## Consequences

The phase 3 criteria hold: the trial limit works offline, the broker resolves a
two-device conflict by heartbeat, an invalid webhook signature is rejected by a
test, and clock rollback, duplicate webhooks and device conflicts each have an
integration test.

The webhook scheme is ours, not a provider's; integrating a real provider means
implementing its verification behind the same `AppState::verify_webhook` seam
and updating this ADR. Tightening `decode` is a wire-visible change, but it only
rejects frames that were never canonical, so `PROTOCOL_MINOR` stays 0.
