# ADR 0045 — Forgetting the consent-rate counter after a clean session

Status: accepted
Date: 2026-08-29

## Context

`crates/core::consent::ConsentRateLimiter` bounds `on_handshaked` to
`CONSENT_RATE_PER_MINUTE` (= 5) consent requests per peer per minute (§9.2)
— a defence against a peer that floods a host with dialogs. Nothing ever
called `ConsentRateLimiter::forget`, even though the method already existed:
the counter only ever expired by falling out of the sliding window.

That made a legitimate, well-behaved reconnect indistinguishable from an
attack once it happened six times inside a minute. `docs/bugs/03-connection-
list.md` task 1 reproduces this deterministically
(`network::tests::h1_reconnecting_past_the_rate_limit_keeps_working_after_a_clean_session`,
before the fix): five ordinary connect → grant → close-view-window cycles
succeed, and the sixth is refused with no §18 code at all, which
`invite-view.ts` renders as "The connection ended before it was accepted" —
the reported symptom, with no crash, no malformed frame and no protocol
violation anywhere in the exchange.

## Decision

`network.rs::on_closed` (the actor's single "a peer's control connection
just closed" handler) now calls the new
`SessionManager::forget_consent_rate`, but **only** in the branch where
`SessionManager::on_disconnect` returned `Ok` — which happens if and only if
that peer had an entry in `SessionManager`'s own session table, i.e. it was
actually **granted** a session at some point, not merely queued. The `Err`
branch (a peer that disconnected while still only pending, never decided on)
is unchanged: its queued request is dropped, and the rate-limit history is
left alone.

This is deliberately narrower than "forget on every disconnect." A peer that
was never granted anything and disconnects repeatedly gains nothing from
this change — its five-per-minute budget still runs out and stays out,
exactly as before. Only a peer the host actually chose to admit, and who
then left normally, gets its counter reset. That is the same peer the
5-per-minute rule was never meant to slow down in the first place: the limit
exists to bound *unanswered* requests from a peer with no established
relationship to this host, not to punish a device the host has already said
yes to, for reconnecting.

`SessionManager::forget_consent_rate` is a thin wrapper that calls
`ConsentRateLimiter::forget` internally. The limiter stays a private field
of `SessionManager`, which stays the sole place inside `crates/core` that
decides anything about consent — `network.rs` asks a yes/no question
(`on_disconnect`'s `Result`) and calls a named intent (`forget_consent_rate`);
it never reaches into the limiter directly.

`CONSENT_RATE_PER_MINUTE` itself is unchanged. This is not a loosening of
the limit — it is a correction to when the limit's memory resets, so that
the number is spent only against genuinely unanswered, unrelated attempts.

## Consequences

- Closing the view window and reconnecting to the same host works
  repeatedly within a minute, without limit, as long as each attempt is
  actually granted and ended cleanly.
- A peer that dials and is never granted anything still exhausts its
  five-per-minute budget and stays exhausted until the sliding window
  clears on its own — the flood protection this limiter exists for is
  unchanged.
- `network.rs::close_connection_normal` (docs/bugs/03-connection-list.md,
  task 3) and this fix address two different symptoms of the same user
  report and are easy to conflate: task 3 stops an ordinary exit from being
  logged as a protocol fault, this ADR stops it from being rate-limited on
  the next attempt. Both were needed; neither substitutes for the other.
