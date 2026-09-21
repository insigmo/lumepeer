# ADR 0095 — A machine without IPv6 must not be told to dial it

Status: accepted
Date: 2026-09-21

Answers "it will not connect, and it used to". Sits under
[ADR 0026](0026-direct-paths-are-preferred-and-the-relay-is-the-fallback.md),
which made the relay a fallback rather than the path, and under
[ADR 0093](0093-the-address-that-answered-is-worth-more-than-the-one-that-was-published.md),
which repaired the *address* half of the same failure.

## Context

A session between two machines in Europe stopped connecting. The guest reached
the host, was offered the unattended challenge, answered it, and then heard
nothing at all until its QUIC idle timeout — thirty seconds later — closed a
connection the host believed was live:

```text
guest  07:35:45.729  connected to a host, awaiting consent
guest  07:35:45.729  the host asked for device credentials  code_required=false
host   07:35:45.838  unattended login accepted               role=FullControl
host   07:35:46.398  consent granted                         role=FullControl
host   07:35:46.667  host session bar opened
guest  07:36:15.739  peer disconnected
```

The grant never arrived. Fifteen seconds after it was sent, the host's own log
said why:

```text
host   07:36:01  Lost connection to relay server: Ping timeout
host   07:36:03  Failed to connect to relay server: unable to connect: deadline has elapsed
host   07:36:21  Failed to connect to relay server: unable to connect: A socket operation
                 was attempted to an unreachable network. (os error 10051)
host   07:37:47  Failed to connect to relay server: unable to connect: Resolve failed,
                 IPv4: Request timed out, IPv6: Request timed out
host   07:37:01  dropping an incoming connection that did not finish its QUIC handshake in time
```

`os error 10051` is `WSAENETUNREACH`. The host had no IPv6 default route and no
global IPv6 address — only a link-local address per adapter and a unique-local
one from a VPN — while every relay hostname carries an `AAAA` record. Each
relay connect spent part of its budget on an address the routing table had no
way to reach, and each name lookup waited on a family that was never going to
answer.

The dropped relay link is the visible cost. The expensive one is quieter.
iroh ranks relays by measured latency and drops the ones that do not answer,
so a machine whose probes are being spent on an unreachable family ranks on
noise. This host had walked `euc1-1 → use1-1 → aps1-1` on *every* start since
the machine came up, and stayed on `aps1-1` — Singapore — for a session
between two machines in Europe. Measured from that host, over IPv4:

| relay    | TCP 443 |
| -------- | ------- |
| `euc1-1` | ~95 ms  |
| `use1-1` | ~200 ms |
| `aps1-1` | ~250 ms |

The relay it could reach fastest was the one it had abandoned. Every session
was being carried half-way around the world and back, on the one link it was
least able to hold, until it collapsed — and then new dials arrived and could
not be answered, because the coordination that answers them goes over that
same link.

## Decision

An endpoint binds with a resolver of this project's own: the system resolver,
wrapped so that `AAAA` is answered *empty* while this machine has no route to
the IPv6 internet.

Routability is asked of the routing table, not of the interface list. A host
can hold several IPv6 addresses that reach nothing, and counting them would
call that machine dual-stacked; binding a UDP socket and connecting it to a
documentation-range address is the one question whose answer is "is there a
route", and it sends no packet. The probe is made once, lazily, and a network
change — which hands the resolver back through `Resolver::reset` — forgets it
so the next lookup asks again.

Empty rather than an error, because iroh only swallows one family's failure
while the other succeeded: an error here would turn a host with no `A` record
into a reported DNS failure instead of "this machine has no IPv6".

A machine that *does* have IPv6 is untouched. Every lookup is the one iroh
would have made, and this decision is invisible to it.

## Consequences

- A single-stack host stops burning relay-connect budget on unreachable
  addresses, so it holds its home relay instead of walking the fleet.
- Relay latency is measured over the family the machine actually has, so the
  *nearest* relay wins — which is what "fewest hops between the two machines"
  means once a relay is in the path at all.
- The relay being holdable is also what lets a host answer incoming dials:
  the handshake that "did not finish in time" above was waiting on
  coordination that had nowhere to go.
- This does not make the relay the path. Direct paths are still preferred
  (ADR 0026) and a remembered address is still tried first (ADR 0093); this is
  about the fallback being a fallback rather than a trap.
- A host that gains IPv6 while running picks it up on the next network change,
  not instantly. That is the same latency every other network fact here has.

## Alternatives considered

**Pin `relay_url` in the config.** Fixes one machine by hand and nothing else,
and it is the operator's override for a self-hosted relay
(`docs/relay-deployment.md`) rather than a repair for a broken measurement.

**Restrict the relay map to a region.** There is no way to know a user's
region that is not this same measurement, done worse.

**Leave it and widen the dial further.** ADR 0050 already widened it and ADR
0093 already gave it better addresses. Neither helps when the far side cannot
answer, which is what this fixes.
