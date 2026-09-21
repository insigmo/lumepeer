# ADR 0100 — A connect is retried unless the host gave a verdict

Status: accepted
Date: 2026-09-21

Completes [ADR 0096](0096-a-connect-ends-on-an-answer-not-on-a-budget.md),
which made a connect keep dialing while the user has it open. ADR 0096 built
the loop; this fixes what was allowed into it.

## Context

ADR 0050 introduced one rule, read in three places — the retry inside one
transport, the fallback to the next transport, and (from ADR 0096) the round
the whole connect starts again:

```rust
const fn is_retryable(error: &NetError) -> bool {
    matches!(*error, NetError::Dial(_) | NetError::Io(_))
}
```

It is an allow-list, and that is the bug. `NetError` has twenty-odd variants.
Two of them were retried; every other failure — including every one nobody
had thought about — fell through to "give up", and `classify_net`'s catch-all
then reported it to the user as **"the host refused the connection"**.

None of these is a host refusing anything:

| failure | what it actually is |
| --- | --- |
| `Endpoint(_)` | this machine could not set up its own endpoint |
| `Offline` | this machine has not reached a relay yet |
| `Keystore(_)` | this machine's keystore was busy or locked |
| `Obfuscation` | a datagram failed its own authentication — one packet |
| `TruncatedStream(_)` | the peer's stream ended mid-frame |
| `Framing(Malformed)` | a frame did not decode |

Every one of them ended the user's connect and sent them looking on the wrong
machine, which is exactly what ADR 0026 wrote the §18 classification to
prevent. An allow-list of two, in a codebase where new transports and new
error variants keep arriving, was always going to drift this way.

## Decision

Invert it. The list is of **verdicts** — things the far side decided about
this guest, which asking again can only collect a second time — and everything
not on it is retried:

```rust
const fn is_verdict(error: &NetError) -> bool {
    matches!(
        *error,
        NetError::InvalidTicket
            | NetError::MalformedTicket
            | NetError::AlreadyConnected
            | NetError::ReconnectRejected
            | NetError::ConsentUnavailable
            | NetError::Framing(CoreError::IncompatibleVersion { .. })
    )
}

const fn is_retryable(error: &NetError) -> bool {
    !is_verdict(error)
}
```

Five of the six are the far side speaking: the invite does not verify, the
protocol majors differ, a resume was refused, or the host said it has no way
to admit this guest at all — nobody is at it and it has no device password to
check (ADR 0085 §2), or something else on that machine holds the host role.
`AlreadyConnected` is this node's own bookkeeping and is not a failure to
retry either.

The test that holds this is written as the exhaustive table rather than a spot
check, because the defect was a *missing* case. A `NetError` variant added
later lands on the retry side by default, and the table is what says that is
the intended direction.

Consent that the person at the host actually refuses does not come through
here at all: it arrives as `ConsentRevoke` on an open connection and settles
the connect in `ConnectPhase::Denied`. That is the one ending a user asked
for, and it is untouched.

## Consequences

- A connect ends in a failure the user sees only when the far side said
  something, or when the invite itself is wrong. Everything else keeps trying,
  on ADR 0096's 5 s → 30 s backoff, for as long as the user leaves the connect
  open.
- A transport fallback (ADR 0083) now also happens for those cases — a
  `Framing` error or a lost stream on the obfuscated transport moves the dial
  to iroh instead of ending it.
- "It just said the host refused and the host was sitting right there" stops
  being a possible report for this class of failure.
- A future `NetError` variant that genuinely *is* a verdict has to be added to
  the list, or a user will wait on a spinner for an answer that has already
  arrived. That is the trade an allow-list-of-verdicts makes, and it is the
  safer direction: the failure mode is a wait the user can cancel, not a wrong
  accusation about somebody else's machine.

## Alternatives considered

**Add the missing variants to the old allow-list.** It fixes today's six and
leaves the next one to be found the same way — in a user's report about a
machine that was not at fault.

**Retry everything, including verdicts.** Then a wrong invite code is a
spinner that never stops, which is worse than an error message. The user's own
statement of the requirement carved this out: a connect may fail when the far
side turned it down.

**A separate, wider rule for the outer round** — so that a host whose consent
queue is momentarily full is waited out rather than reported. Drafted and
dropped: `CONSENT_UNAVAILABLE` is one close code covering both "I am busy" and
"I can never admit you", so widening it would make a host that admits nobody
into a connect that never ends. Splitting the code is a wire change, and it is
not worth one for a case this rare.
