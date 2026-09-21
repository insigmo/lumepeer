# ADR 0099 — A saved host is located before anybody presses Connect

Status: accepted
Date: 2026-09-21

Builds on [ADR 0093](0093-the-address-that-answered-is-worth-more-than-the-one-that-was-published.md),
which made a dial remember where a host answered, and
[ADR 0062](0062-an-invite-code-that-does-not-change.md), which made a
host findable by key alone. This is about *when* that finding happens.

## Context

A dial to a saved host starts from three things: the addresses baked into the
invite code, the addresses the last session was actually carried over
(ADR 0093), and whatever DNS and the Mainline DHT answer *now*. The first two
are as old as the last session. The third is the only fresh one, and it is
looked up inside the dial — while a user watches a spinner.

That makes the common case slower than it needs to be, and the interesting
case worse than that: a host that rebooted onto a new public address, changed
network, or had its NAT binding recycled has *nothing* true in the first two,
so the whole first attempt is spent dialing a machine that is not there, and
the retry of ADR 0050 is what eventually reaches it.

Separately: nothing in the log ever said whether a session went direct or
through a relay. `path_of` computes it for the diagnostics panel, which
answers the question for whoever is looking at that screen at that moment. It
does not answer "it was slow yesterday", which is the form the question
actually arrives in.

## Decision

Two things, both background, both guest-side.

**1. Saved hosts are located on a schedule.** Twenty seconds after the actor
starts — long enough for this node's own endpoint to have come online, since a
lookup from one that has not mostly answers nothing — and then every 30
minutes, the actor takes the eight most recent rows of the remembered-hosts
list, resolves each one's `EndpointId` through the address-lookup services
(DNS and the DHT), and writes the direct addresses it gets back onto that row.

- **Nothing is dialed and nothing reaches those machines.** This is the
  discovery half of a connect and none of the connecting half. No host sees a
  connection, no consent is queued, no invite is claimed.
- One host at a time, not eight at once: nobody is waiting for this, and eight
  DHT queries in one breath from a client that is not connecting to anything
  is not a good citizen.
- Eight seconds per host, after which the sweep moves on. Up to four addresses
  are kept per host, which is also a cap on the next dial's own fan-out.
- A host with a live session is skipped — its connection is the better answer,
  and ADR 0093 already writes it.
- A lookup that finds nothing writes nothing. It has learnt nothing, and it
  must not be able to erase addresses that were working.
- A lookup for a host the user has since removed writes nothing either.
  Nothing here may put a row back into a list somebody cleared.

`with_remembered_addrs` already feeds those addresses into the dial beside the
ticket's, so the next Connect starts from an address that is minutes old.

**2. The route goes in the log.** On every ping tick (20 s), each live
connection's route is recomputed — direct, relay (and which region), or both,
plus whether the path currently carrying packets is a direct one and its round
trip — and logged when it *changes*. The first line for a connection says how
the session came up; the next one says the hole punch landed, or the relay was
lost. The round trip is on the line but is not part of the change test, or the
same line would repeat every 20 seconds forever.

Addresses stay out of it. The route is a kind and a region, which is what §15
allows a screen to say and what a reader of a log actually needs.

## Consequences

- Connecting to a saved host is faster, and much faster in the case where the
  host moved: the address that works is already in hand.
- "Did this go direct or through a relay" is answerable after the fact, on the
  machine that had the problem, which is where reports come from.
- A client that is running but idle now makes a handful of DNS and DHT queries
  every half hour. That is the cost, it is bounded by the constants above, and
  it is the same traffic a dial would have made anyway — moved off the path
  the user waits on.
- The remembered-hosts file is rewritten when a host moves rather than only
  when a session ends.

## Alternatives considered

**Keep a warm connection to each saved host.** It would be faster still, and
it would mean a host showing a consent request, or holding a session, for a
guest that is not there. The whole design says a session is decided each time
(§2.3).

**Look up only at Connect time, but earlier in the dial.** That is where it
already is. The point is to be finished before the click, not to reorder the
work after it.

**Log the route on every tick.** It is the same line a hundred and eighty
times an hour per session. A log nobody can read is not a log.
