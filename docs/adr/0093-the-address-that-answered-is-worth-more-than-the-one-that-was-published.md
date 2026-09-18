# ADR 0093 — The address that answered is worth more than the one that was published

Status: accepted
Date: 2026-09-19

Answers "no matter how often I try to reconnect, it fails with a different
error each time". Extends
[ADR 0050](0050-a-dial-is-widened-until-it-outlasts-a-flapping-relay.md), which
widened the retry loop, and [ADR 0062](0062-an-invite-outlives-a-restart.md),
which added the lookup half of a dial.

## Context

One reconnect, from this pair of machines, in full (guest log, UTC):

```text
21:37:10  discovered endpoint info  addrs=[Relay(euc1-1), Ip(85.173.126.211:21966), Ip(…:63508)…]
21:37:10  connected to a host, awaiting consent
21:37:44  peer disconnected
21:37:53  discovered endpoint info  addrs=[Relay(aps1-1), Ip(176.208.57.14:9415),  Ip(…:53589)…]
21:38:10  connect attempt failed  error="dial failed: no answer within 20s"  attempt=1 of 5
21:38:32  connect attempt failed  …                                          attempt=2 of 5
21:38:54  connect attempt failed  …                                          attempt=3 of 5
21:39:15  connect attempt failed  …                                          attempt=4 of 5
21:39:37  connect attempt failed  …                                          attempt=5 of 5
21:39:37  invite connect failed
```

The first lookup found the host. Forty seconds later the same lookup, for the
same host, answered with a *different run's* addresses — a public address on
another uplink and a local port the host has not held since its last restart —
and every attempt after that dialed a machine that was not there. The host's
own log explains why: its publish had just failed
(`pkarr publish error: Failed to find any nodes close to store value`) and its
relay link was collapsing at the same moment
(`Failed to connect to relay server: Resolve failed, IPv4: Request timed out`),
so what the DHT still had to answer with was the record a previous run had
managed to store.

Everything the dial had to go on was a claim. The invite ticket carries where
the host said it was when the code was issued, which can be days old. Discovery
carries where the host last managed to say it was, which — exactly when the
network is bad enough to need it — can be a whole run old. Both were stale
together, and ADR 0050's five attempts only asked the same two liars five times
each.

Nothing in the system remembered the one address that was not a claim: the
address the connection forty seconds earlier had actually been carried over.

## Decision

The guest remembers the direct addresses each session was reached at, and
offers them to the next dial beside the ticket's.

- `ConnectionHistory` gains `addrs` on each row, filled from the live
  connection's own open paths (`Connection::paths()`, IP paths only) at both
  moments a row is written — when consent is granted and when the view closes.
  A write with nothing to report keeps what is already there, the same rule
  `transport` already follows: the disconnect write happens after the
  connection is gone, and letting it clear the field would throw away what the
  connect-time write learned.
- `Actor::dial_plan` adds them to the `EndpointAddr` it builds for the iroh
  dialer. Added, never substituted: the ticket's addresses stay, the lookup on
  even attempts stays (ADR 0062), and this is a third thing to try rather than
  a replacement for either.
- They are not sent to the webview. `HistoryEntryDto` keeps its five fields;
  §15's rule is that another network's addresses never reach a surface the
  host does not control, not that this process may not know where it just
  connected. They are, however, written to the history file, which already
  holds the invite code — and the code contains the same addresses.

## Consequences

- A host whose published record has gone stale is still reachable for as long
  as its NAT binding lasts, which is the case this was written for: the same
  socket that carried a session a minute ago is almost always still open.
- A remembered address that has gone stale costs one probe in a set iroh
  races. It cannot mislead: QUIC authenticates the endpoint key, so a stranger
  now holding that address fails the handshake instead of answering for the
  host.
- A host that moved networks between two sessions is found by the lookup half
  of the loop, unchanged. The memory only ever adds candidates.
- Only the iroh transport uses it. The obfuscated transport has one address
  the ticket pinned and no lookup at all (ADR 0083), so there is nothing here
  it does not already know.
