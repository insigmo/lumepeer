# ADR 0096 — A connect ends on an answer, not on a budget

Status: accepted
Date: 2026-09-21

Answers "it must connect every time the program is running". Follows
[ADR 0050](0050-a-dial-is-widened-until-it-outlasts-a-flapping-relay.md), which
widened one round of dialing, and
[ADR 0093](0093-the-address-that-answered-is-worth-more-than-the-one-that-was-published.md),
which gave that round better addresses to try. Both made the round longer and
better aimed. Neither changed what happens when it runs out.

## Context

Same session as
[ADR 0095](0095-a-machine-without-ipv6-must-not-be-told-to-dial-it.md). After
the host's grant failed to arrive and the guest's idle timeout closed the
connection, the guest dialed again and spent its whole budget:

```text
guest  07:36:15  peer disconnected
guest  07:36:46  connect attempt failed  error="dial failed: no answer within 20s"  attempt=1 of 5
guest  07:37:07  connect attempt failed  …                                          attempt=2 of 5
guest  07:37:29  connect attempt failed  …                                          attempt=3 of 5
guest  07:37:52  connect attempt failed  …                                          attempt=4 of 5
guest  07:38:12  connect attempt failed  …                                          attempt=5 of 5
guest  07:38:12  invite connect failed
```

`DIAL_TOTAL_BUDGET_SECS` is about a hundred and ten seconds. The host's relay
link was down for longer than that, so the connect ended in a failure whose
only meaning was "this node stopped asking". The one thing that would have
worked — asking again — was left to the user, who has no way to tell a host
that is briefly unreachable from one that is off.

Two separate places produced that dead end, and only one of them was the dial:

1. A round of `DIAL_ATTEMPTS` that never reached the host at all.
2. A connection that came up, got as far as the credential exchange, and then
   died before the host said anything. The phase was pending, the link was
   gone, and `settle_connect(peer, Failed)` ended it — which is exactly the
   first half of the transcript above.

Both are this side's own silence. Neither is an answer.

## Decision

A connect the user has open ends on an answer from the host. Nothing else ends
it but the user.

While a connect is in flight the node remembers the peer and the invite code
behind it. When a round of dialing comes back with nothing, or a connection
dies before the host decides, the phase stays `Dialing` and the node dials
again, pausing `CONNECT_RETRY_BACKOFF_SECS` and doubling to
`CONNECT_RETRY_BACKOFF_CEILING_SECS`. To the user it is still the one connect
they started, its spinner still turning and its Cancel button still the way
out — the same button that already called off ADR 0084's wait.

What counts as an answer is not guessed:

- A dial that failed on anything but `NetError::Dial` or `NetError::Io` —
  `is_retryable`, the single rule one round's own retry already reads. A bad
  ticket, a version mismatch, a refusal: asking again only collects it twice.
- A connection **the far side closed**, whichever code it used. A peer that
  closed said something; a link that timed out, was reset, or failed at the
  transport said nothing at all. That distinction is `closed_by_peer` on the
  connection, and it is what keeps ADR 0085's "a host with nobody at it admits
  nobody" a refusal the guest is shown rather than something to ask again.
- A verdict already recorded in `connect_failure` — a refused device password
  leaves the phase pending so the user can retype, and dialing past it would
  throw the message away and ask again with a password known to be wrong.

This is not ADR 0084's wait. That one dials a host **unasked** after a session
ended, and is gated on the host being trusted with a remembered password,
because an automatic retry ending at a password prompt nobody is there to type
is a loop rather than a reconnection. Nothing here is unasked: the user
pressed Connect and is still waiting. The only thing this decides is whether
one round of attempts is the end of that wait.

## Consequences

- A host that is unreachable for minutes — a relay link down, a machine still
  coming up, a record not yet republished — is reached when it comes back,
  without anyone clicking anything.
- "Connect failed" now means the host answered. That is the only thing it ever
  usefully meant, and it is what makes the message worth showing.
- A connect left open holds one dial's worth of work every half minute at the
  ceiling. Bounded, and cheaper than the round of dialing it follows.
- A user who wants it to stop presses Cancel, which was always the way out of
  a pending connect and now also ends the rounds.
- A host that refuses is still a refusal, immediately, with no retry — the
  behaviour ADR 0085 §2 turns on, and the existing test for it is what holds
  this decision to that line.

## Alternatives considered

**Raise `DIAL_ATTEMPTS` again.** ADR 0050 already went from three to five for
this same class of failure. Any fixed budget is a guess at how long a far
machine's network stays broken, and the guess is wrong in one direction only.

**Show the failure and offer a Retry button.** That is what the connect form
already was: the user cannot tell a host that is briefly unreachable from a
dead one, so the button is a request to guess.

**Retry only for hosts marked trusted.** That is ADR 0084's gate, and it is
there because *that* dial is unasked. Applying it here would refuse to finish
a connect the user is watching.
