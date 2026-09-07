# ADR 0062 — An invite code that does not change

Status: accepted
Date: 2026-09-07

Amends ADR 0016 (a ticket is reusable, and issuing a replacement is how a host
revokes) and §7's ten-minute TTL. Extends ADR 0052 (serverless transport) with
the address-discovery half it left open. Answers a host's report: a device
saved in the connection list stops being reachable, with
"Could not reach that device. It may be offline, or its invite code may be out
of date — ask for a fresh one."

## Context

A saved connection is a button the user expects to work forever. Three separate
things stopped it, and each one on its own is enough to produce that message.

1. **The address inside the code goes stale.** An invite ticket carries a
   serialized `EndpointAddr` — the host's addresses as of the moment the code
   was issued. Reboot the host onto a new public IP and every one of them is
   wrong. iroh *does* have address lookup, but `Endpoint::connect` only falls
   back to it when the addresses it was handed fail **and no relay URL came
   with them** — and a ticket always carries one. So all five dial attempts of
   ADR 0050 were spent, patiently, on addresses that no longer exist.

2. **The ticket expires in ten minutes.** §7 specified that TTL for a one-shot
   invite. ADR 0016 already made a ticket reusable; what actually bounds one is
   the host retiring it. Ten minutes did not add a bound so much as guarantee
   that any code older than a coffee break was refused.

3. **The registry does not survive a restart.** `TicketRegistry` is a
   `HashMap` in memory. `claim` refuses an invite id it has never seen, so
   every restart of the host silently revoked every code it had ever read out
   — and the sidebar's "create invite" button reissued (and therefore retired)
   on every press, so looking at your own code was enough to break the one the
   guest was holding.

The identity half was never the problem: the host's `EndpointId` comes from the
OS keystore and is the same key for the life of the install. What a permanent
code needs is a way to turn that key back into an address, and a host that
still recognises the code tomorrow.

Two ways to resolve a key to a current address were considered. A rendezvous
server of our own (the existing `services/broker` could have grown the
endpoint) is the fastest and the most private — only our server sees the
address — but it is one machine that has to stay up and one IP that can be
blocked, which is the dependency ADR 0052 was written to get out of. The public
Mainline DHT needs nothing of ours.

## Decision

**Address lookup over the Mainline DHT.** `PeerEndpoint::bind_with_lan` and
`bind_relay_only` add `iroh_mainline_address_lookup::DhtAddressLookup`, signed
by the endpoint's own secret key, beside the n0 pkarr/DNS lookup `presets::N0`
already installs. Two independent ways to be found; a network that blocks one
may still allow the other. A DHT node that cannot be built is a warning, not a
failed bind (§18).

The publisher is `AddrFilter::unfiltered`. The default publishes relay
addresses only, which is exactly nothing for a host that cannot hold a relay —
the case this whole ADR is about.

**Dial attempts alternate between the ticket's addresses and the key alone.**
Odd attempts use the `EndpointAddr` from the ticket, which is the fast path
while it is still true; even attempts strip it to the bare `EndpointId`, which
is what makes iroh run the lookup. ADR 0050 widened this loop to five attempts
precisely because one try each is not enough to ride out a flapping link, so
both routes get several.

**A year-long TTL** (`INVITE_TICKET_TTL_SECS`), and **the live invite is
persisted** to `invite.json` beside the address book. At start-up the stored
code is parsed, checked against this host's own signing key, and re-registered,
so a code written down last week still claims.

**Asking for the code and asking for a new code become different calls.**
`invite_create` takes `renew`. Without it the host hands back the invite it is
already living with, issuing only if there is none; the sidebar and the new
read-only `invite_current` use that. With it, every code handed out so far is
retired and a fresh one issued — the settings window's reissue control, which
is now the only thing that revokes.

## Consequences

- A saved connection survives a reboot, a new public IP and a restart of both
  ends. The code is the same string it was yesterday, and the sidebar shows it
  without a click.
- **Anyone holding a host's invite code can read that host's current IP
  addresses out of the public DHT.** This is the real cost, and it is stated
  plainly in `with_dht_lookup`. It is close to the trade the code already made
  by carrying addresses in the first place; what is new is that the addresses
  stay current.
- Revocation is now a deliberate act rather than a side effect of pressing the
  wrong button, which is what ADR 0016 wanted and is a stronger position than
  before — but a leaked code stays useful for a year unless reissued. The host
  still decides every connection (§2.3), and a trusted device still has to pass
  the unattended password (§8), so a code alone remains an introduction, not a
  key.
- One new dependency, `iroh-mainline-address-lookup` (three crates with its
  own transitive `n0-mainline` and `serde_bencode`).
- Not addressed here: publishing the obfuscated transport's address (ADR 0053)
  to the DHT. That record still carries only the iroh addressing.
