# ADR 0111 — The obfuscated transport is an automatic default

Status: accepted
Date: 2026-09-24

Extends [ADR 0052](0052-serverless-transport-obfuscated-quic-with-stun-discovery.md),
[ADR 0080](0080-one-connection-type-over-two-transports.md) and
[ADR 0083](0083-one-dial-plan-per-connect-and-a-fallback-before-consent.md).
Completes the last step of gap-tasks/23.

## Context

The obfuscated transport (ADR 0051/0052) wraps every datagram of a session in
an `XChaCha20-Poly1305` envelope keyed from the invite id, on a random
ephemeral UDP port, discovered through a stateless STUN reflector with no
server in the data path. On the wire it is uniform random noise: no QUIC
Initial, no TLS ClientHello, no SNI, no ALPN, no fixed header — nothing a
signature- or SNI-based packet inspector can key on. It was built beside the
iroh path, not in place of it.

ADR 0080 shipped it **off by default**, on the stated grounds that automatic
selection between the two transports, and the fallback when the obfuscated one
cannot be reached, were separate work (gap-tasks/23). That work is now done and
tested: `dial_order` prefers the obfuscated path and puts iroh last;
`attempt_shares` divides one dial budget (ADR 0050) across the transports so a
two-transport dial costs no more wall clock than the single-transport dial it
replaced; `dial_plan` builds the plan on the actor thread for both a first
connect and a reconnect; and `obfuscated_dialer` leaves the transport out of
the plan whenever a ticket carries no address for it. With the machinery in
place, the only thing keeping real sessions on the iroh-only path was the
default itself.

Meanwhile the iroh path's relay fallback is TCP + WebSocket + TLS to a fixed
public fleet, and a network that freezes TLS to those foreign hosts (observed
on a real client's ISP) leaves a session with no working path at all once a
direct hole punch is unavailable. The obfuscated transport is exactly the path
that carries no such fingerprint.

## Decision

- `[network] obfuscated` defaults to **`true`**. The obfuscated transport is
  preferred automatically, with iroh as the fallback, within one dial budget.
- It stays *beside* iroh, never in place of it. A host whose STUN discovery
  finds no usable address issues a ticket with no obfuscated address, and its
  guests dial over iroh exactly as before — an ordinary outcome, not a
  failure. The transport being on changes what a node *tries*, never what it
  *depends on*.
- `false` remains the escape hatch: a node on a network it knows is clean can
  turn the whole transport off with one key and skip the per-invite STUN round
  trip.
- The public STUN reflector list gains operator and port diversity (an
  operator that answers on 443, beside the usual UDP/3478 ones). Discovery
  returns on the first reflector that answers, so the extra entries cost
  nothing on the common path and only matter when a reflector — or the 3478
  port itself — is filtered.

## Consequences

- A host now does one STUN round trip when it issues an invite. It is off the
  session's data path and does not touch connection quality; the ADR 0080 cost
  note stands, but the cost is now judged worth paying by default.
- The obfuscated path never rides the debug tailnet: the address it advertises
  is the STUN-reflexive public address over the physical uplink, and it does
  not consult iroh's address set (where a `100.x` CGNAT address would come
  from). Preferring it is therefore also what keeps a real session off the
  tailnet without any per-process firewall rule.
- Whether a direct obfuscated path *establishes* still depends on the two NATs.
  Behind a symmetric or double NAT a hole punch may not land, and the session
  then falls back to iroh — including its relay, which is the path a
  TLS-freezing network can still break. This ADR removes the relay as the
  *only* thing tried; it does not promise a direct path where none can be
  punched. That is the remaining open edge of gap-tasks/23.
- The two config assertions that guarded "off by default" (ADR 0080) now assert
  the automatic default instead.
