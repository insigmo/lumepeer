# ADR 0098 — The nearest relay is the only one in the map

Status: accepted
Date: 2026-09-21

Supersedes the "keep two" half of
[ADR 0097](0097-the-relay-fleet-is-narrowed-to-the-ones-this-machine-can-reach.md)
and keeps everything else in it. Answers the same question one step further:
not "prefer a near relay" but "never have a far one to drift to". Leaves
[ADR 0026](0026-direct-paths-by-default-and-a-diagnosable-release.md)
untouched — the relay is still the fallback, never the path.

## Context

ADR 0097 cut the public fleet to the two relays a machine reaches fastest,
because iroh's home-relay selection drifts: a partial net report that
re-measured one relay hands the home slot to that relay, and the hysteresis
meant to stop the switch is skipped exactly then. Keeping two bounded the
drift — but it did not remove it. For a machine in Spain the two nearest are
Frankfurt and Virginia, so the drift still crosses an ocean, and for a session
to a machine in the Caucasus there is no version of "Virginia" that is on the
way.

Three more things ADR 0097 left open, all of which showed up in use:

1. **The measurement is spent once, at bind.** A laptop carried to another
   country, or a VPN switched on, keeps the relays it measured where it was.
2. **A machine that starts before its network is up measures nothing** —
   which ADR 0097 correctly treats as "no evidence", and so keeps the *whole*
   global fleet for the rest of that run. A service host starting with the
   machine hits this every boot.
3. **A TCP connect to port 443 is not proof the relay protocol works.** A
   transparent proxy accepts the connection and then fails the WebSocket
   upgrade. Narrowing to one relay on that evidence alone would strand the
   node.

## Decision

The endpoint binds with **one** relay — the nearest — and the rest of the
measured fleet is held in reserve, off the map. A relay that is not in the map
is a relay iroh's ranking cannot drift onto, which is the whole mechanism.

A background task then keeps that honest, for as long as the endpoint lives:

- **Prove it.** The narrowing is not trusted until `Endpoint::online()` says a
  relay was actually reached, within 10 s. If it is not, the next-nearest
  reserve relay is put back.
- **Keep proving it.** Every 30 s, if no relay of the map is connected, the
  next reserve relay is put back.
- **Widening happens at most once, and is permanent.** Once, because "no relay
  is connected" is not evidence about a *relay*: a machine whose uplink is
  down reports exactly that, and a fleet that widened on every health check
  would walk itself back to the whole global map over a few minutes of no
  network. Two is where it stops — which is the fleet ADR 0097 shipped and
  knew to be safe. Permanent, because a network that needed a second relay
  once will need it again, and re-narrowing to the same unreachable relay
  every half hour would take the node off the air on a schedule.
- **Re-measure.** Every 30 minutes the fleet is measured again. A nearer relay
  is inserted; relays that are no longer nearest are removed — insert first,
  remove second, so at no point does the map hold nothing. A run that has
  widened reorders but never shrinks.
- **A bind that narrowed nothing** — no cache and no relay answered — is on
  the whole fleet, and measures again as soon as the grace period is over
  rather than waiting half an hour.

`Endpoint::insert_relay` and `Endpoint::remove_relay` do all of this on a live
endpoint, so none of it costs a rebind or a session.

The measurement is also **written to disk** (`relays.json`, beside the
connection history) and read back at the next bind while it is under 30
minutes old. That makes a cold start free rather than costing up to one probe
timeout, and — the case it was actually written for — it gives a machine whose
network is not up yet the relays it had last time instead of the whole world.
A stale file is still used when a fresh measurement finds nothing: it is the
only evidence about this machine that exists.

An operator who names a relay — `LUMEPEER_RELAY_URL`, or `[network].relay_url`
for a self-hosted one — still gets exactly that relay, no measurement, no
watcher.

## Consequences

- The home relay is the nearest one, and stays it. The five-minute oscillation
  ADR 0097 bounded is now impossible rather than merely cheaper.
- A machine that moves stops talking to the country it left, within half an
  hour and without being restarted.
- A cold start no longer pays for the measurement, and a boot-time start no
  longer loses the narrowing entirely.
- **A fleet of one is a single point of failure, and this accepts that on
  purpose** — but only for as long as it is working. The point of the grace
  check, the health check and the one-shot widening is that the fleet is one
  relay exactly while one relay is enough, and ADR 0097's two the moment it is
  not. The worst case is one health interval, 30 s, without a relay — during
  which direct paths, the DHT and the obfuscated transport are all still
  there.
- One more file on disk, rewritten every half hour, holding four hostnames and
  a timestamp. It carries nothing private: which public relay is nearest is a
  fact about geography.

## Alternatives considered

**Keep two, as ADR 0097 did.** It is the safer default and it is what this
replaces. What decided against it is that the second relay is not a spare — it
is in the map, so it is somewhere the drift can go, and for every machine
outside North America it is on another continent. A reserve that is *off* the
map gives the same redundancy without that.

**Re-rank and pin the home relay ourselves.** Re-implementing `net_report`
from outside, with worse information. The map is the one lever that does not
require it.

**Rebind the endpoint when the measurement changes.** It would apply the new
fleet, and it would drop every live session to do it.

**Widen back down once the primary recovers.** Tempting, and it is what the
sticky flag deliberately does not do: a relay that failed the protocol once on
this network will fail it again, and the flapping would be worse than the
extra relay.
