# ADR 0097 — The relay fleet is narrowed to the ones this machine can reach

Status: accepted
Date: 2026-09-21

Answers "do not go to a farther server to reach a nearer machine". Follows
[ADR 0095](0095-a-machine-without-ipv6-must-not-be-told-to-dial-it.md), which
made the relay *reachable* on this host; this is about which relay it reaches.
Leaves [ADR 0026](0026-direct-paths-are-preferred-and-the-relay-is-the-fallback.md)
untouched: the relay is still the fallback, never the path.

## Context

With ADR 0095 in, the host in that transcript could finally hold a relay
connection — five minutes with no reconnect at all, where before it had been
failing every few seconds. It still put itself in Singapore:

```text
08:43:00  home is now relay euc1-1, was None
08:43:21  home is now relay use1-1, was Some(euc1-1)
08:43:45  home is now relay aps1-1, was Some(use1-1)
          (…and there it stayed)
```

Forty-five seconds, Frankfurt to Virginia to Singapore, on a machine in
Europe connecting to another machine in Europe. Its own application log had
been doing this on every start for days.

iroh's ranking is not wrong. Its *full* report is right:

```text
relay_latency ipv4: {aps1-1: 224ms, euc1-1: 81ms, use1-1: 157ms}
preferred_relay: euc1-1
```

What follows it is the problem. Between full reports — every five minutes —
iroh runs partial ones, and a partial report that re-measured a single relay
carries a single latency:

```text
relay_latency ipv4: {aps1-1: 207ms}
preferred_relay: aps1-1
```

The selection walks the latencies *in that report*, so with one entry the one
entry wins. The hysteresis that exists to stop exactly this — "if the old
relay is still reachable and the new one is not much better, stay" — is
guarded on the old relay having a latency in the same report, and it does not,
because it was not probed. So the guard is skipped precisely in the case it
was written for, and the home relay is whichever one the last partial report
happened to touch. Every five minutes the full report moved that host back to
Frankfurt, and the next partial one moved it away again.

This costs more than round-trip time. The relay is where two peers exchange
what they need to hole-punch a direct path, so a far relay is a slower and
less reliable way to reach the fast path — and when the link dies, as it did
in ADR 0095's transcript, a grant that was already sent is simply lost.

## Decision

When nobody has named a relay, the endpoint binds with the public fleet
narrowed to the two relays this machine reaches fastest, measured once at bind.

The measurement is a TCP connect to each relay's TLS port, all four at the
same time, bounded at 1.5 s for the whole thing. It only has to *rank*, and a
connect ranks the same way iroh's own QAD probes do — on the host above both
put Frankfurt first, Virginia second, Singapore last. Nothing is sent.

Two are kept, not one: a fleet of one is a single point of failure, and this
exists to make connecting more reliable, not less. What the drift can then
cost is bounded at "the second-nearest relay" instead of "the other side of
the world", and iroh goes on ranking what is left exactly as it does now.

Narrowing loses no configuration. `RelayMode::Default` is four hostnames, each
turned into a bare `RelayConfig` with nothing else on it, so a subset of those
same URLs is the same map with entries removed.

When fewer than two relays answer, the fleet is left whole. An offline machine
and a network that blocks TLS to all of them look alike from here, and
narrowing on no evidence would turn a momentary failure into a permanently
smaller set of ways to be reached (§18).

An operator who names a relay — `LUMEPEER_RELAY_URL`, or `[network].relay_url`
for a self-hosted one (`docs/relay-deployment.md`) — still gets exactly that
relay and none of this.

## Consequences

- A session starts on a near relay and hole-punches from there, instead of
  starting on one a quarter of a second away.
- The five-minute oscillation stops, which also removes a recurring source of
  the flap ADR 0050 had to widen the dial budget to ride out.
- Bind costs one bounded, parallel measurement — at most 1.5 s, typically the
  round-trip to the farthest relay.
- Two relays instead of four is less redundancy. It is deliberate, it is the
  smallest narrowing that removes a far relay from a four-relay fleet, and
  nothing about it touches the direct path a session actually wants.
- If iroh fixes the partial-report selection, this becomes a latency
  optimisation rather than a repair, and can be dropped without anything else
  changing.

## Alternatives considered

**Re-rank the relays ourselves and pin one.** That is re-implementing
`net_report` from outside, with worse information, and it throws away the
fallback.

**Force a full net report more often.** `NetReportConfig` has no such knob —
only HTTPS probes and the captive-portal check — and the full interval is a
private constant.

**Leave it: the relay is only a fallback.** It is also the bootstrap and the
hole-punch rendezvous. The session in ADR 0095 died at the credential
exchange, before any direct path existed, because that bootstrap was a link
the host could not hold to a relay it should never have chosen.
