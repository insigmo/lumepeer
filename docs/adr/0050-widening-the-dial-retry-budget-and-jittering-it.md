# ADR 0050 — Widening the dial retry budget, and jittering it

Status: accepted
Date: 2026-09-01

## Context

ADR 0027 gave a guest connect `DIAL_ATTEMPTS` (3) tries, `DIAL_RETRY_BACKOFF_MS`
(750 ms) apart, each capped at `CONNECT_ATTEMPT_TIMEOUT_SECS` (20 s), against
exactly this failure: a host whose relay link comes up, moves, and drops on
its own, independent of anything the guest does. `[[project-lumepeer-connect-
and-media-limits]]` already recorded that one of this pair's two machines —
`beta` — cannot hold a relay session to n0's fleet at all: it loses its
active relay to `Ping timeout` roughly every 20 s and its home relay walks
between `euc1-1`, `use1-1` and `aps1-1` within minutes. Sessions still work
because a *direct* path exists on the shared tailnet, but a fresh dial has to
survive whatever the relay is doing at the moment it lands, before a direct
path has been punched.

`beta`'s own release log from today shows ADR 0027's retry doing exactly what
it was built to do, and still losing:

```
11:40:49  connect attempt failed  error="dial failed: no answer within 20s"        attempt=1 of=3
11:41:07  connect attempt failed  error="stream i/o failed: connection lost"       attempt=2 of=3
11:41:25  connect attempt failed  error="stream i/o failed: connection lost"       attempt=3 of=3
11:41:25  invite connect failed   error="stream i/o failed: connection lost"
```

Three attempts, all inside a 36-second window, all lost — reported to the
user as "The connection ended before it was accepted" (`invite.failed`,
`i18n.ts`), the same string `docs/bugs/03-connection-list.md` investigated,
but not that bug: v0.0.45, already running on `beta`, has ADR 0045's
consent-rate `forget` and ADR 0027's retry both in place, and the host's log
for this window shows no consent-rate warning at all — the connection never
got that far. A manual retry about seven minutes later (`11:48:39`) succeeded
on its first attempt, with no code change and nothing about the network that
a human did anything to fix. The only thing that changed was when the retry
ran relative to `beta`'s relay cycle.

That is the tell. `beta`'s relay drops recur on a period close to
`CONNECT_ATTEMPT_TIMEOUT_SECS` itself. Three fixed-interval attempts, each
bounded to roughly that same 20 s, do not sample three independent moments in
that cycle — they can land in the same phase of it three times in a row, and
on this pair's evidence, sometimes do. ADR 0027 was right that retrying
fixes this; it did not anticipate the retry itself falling into step with
the thing it was retrying past.

## Decision

1. **`DIAL_ATTEMPTS` goes from 3 to 5.** More tries widen the total window an
   automatic reconnect covers, raising the odds that at least one lands
   outside a bad stretch, without changing what a single attempt is allowed
   to cost (`CONNECT_ATTEMPT_TIMEOUT_SECS` is untouched).
2. **A new `DIAL_RETRY_BACKOFF_JITTER_MS` (1500) is added on top of the fixed
   `DIAL_RETRY_BACKOFF_MS`.** Each retry in `dial_with_retries` now sleeps
   `DIAL_RETRY_BACKOFF_MS` plus a fresh random amount up to the jitter bound,
   drawn from `rand::rng()` — the same source `network.rs` already uses for
   nonces and secrets, no new dependency. This is what actually breaks the
   lockstep: a fixed backoff keeps every attempt the same distance from the
   last, so a host with a roughly periodic failure looks the same to every
   attempt in a run. Jitter spreads the attempts across the cycle instead.
3. What is **not** changed: `CONNECT_ATTEMPT_TIMEOUT_SECS`, the "an answer is
   never retried" rule (a bad ticket or a version mismatch still fails on the
   first response), and `INCOMING_ACCEPT_TIMEOUT_SECS` /
   `CONTROL_HANDSHAKE_TIMEOUT_SECS` on the host's accept side. Those bound a
   different thing — one connection's own handshake, not how many times the
   guest is willing to dial — and today's evidence does not implicate them:
   the host's accept-side log for this same window shows no dropped
   handshake, only the relay itself going away before a connection reached
   the host at all.

This does not fix `beta`'s relay instability — that is the environment, not
this code, exactly as `[[project-lumepeer-connect-and-media-limits]]`
concluded for the direct-path measurement. It widens the odds an ordinary
reconnect survives it without the user having to notice a failure and try
again by hand.

## Consequences

- A cold connect against a host with a flapping relay link gets up to five
  tries spread pseudo-randomly over roughly 750 ms – 2250 ms apart, instead
  of three tries locked to a fixed 750 ms cadence — a larger and less
  predictable sample of the host's relay cycle, at the cost of a slower worst
  case (five failed attempts at the full 20 s budget each, plus jitter,
  before the guest sees a final failure — up to roughly 108 s versus ADR
  0027's roughly 62 s).
- No change to the failure the user sees when it does still happen: still
  `invite.failed`, still no §18 code, because nothing here changes what kind
  of error a dial failure carries — only how many times it is retried and how
  the retries are spaced.
- `docs/bugs/03-connection-list.md` and ADR 0045 remain the fix for the
  rate-limiter symptom that shares this same error string. This ADR is the
  fix for a different cause behind the same string: a relay link the host
  cannot hold, not a consent request the host silently refused.
