# ADR 0134 — A guest follows its host when the host moves, and knocks when it goes quiet

Status: accepted
Date: 2026-09-30

The user's report: "Why does lumepeer keep losing the connection? It has to
work over the internet with no breaks and no problems. Ask for the new
address every second and switch to it if you have to."

Amends [ADR 0052](0052-serverless-transport-obfuscated-quic-with-stun-discovery.md)
(the guest side of the obfuscated transport) and builds on the push
rendezvous of [ADR 0116](0116-the-rendezvous-pushes-through-nostr-relays.md).

## Context

This machine's client log from 2026-09-29, a guest with two sessions open on
the obfuscated transport to two hosts behind one public address
(`94.25.111.14`):

- The drops come every two to four minutes: 19:34, 19:37, 19:40, 19:52 and
  19:53. Each time **both** sessions go within 15 s of each other, so the
  cause is on the network the two hosts share, not in either session.
- Each drop has the same shape as the one ADR 0123 measured. The picture
  stops (`media stopped`), the control connection times out after that
  (`waiting for its session to come back`), the resume three seconds later
  dials and connects within a second, and the next drop follows minutes
  later.
- Each resume's knock is answered through a relay from a *different* public
  address than the one the host's UDP endpoint is at (`85.173.133.45` and
  `.255` against `94.25.111.14`). The hosts' network leaves the internet
  through more than one address. `beta` has already been seen on two
  uplinks (`85.173.126.211` and `176.208.57.14`,
  project memory of 2026-09-19).
- ADR 0123 measured the same pair and found the path "died every three to
  twenty-five minutes, **one direction first**".

The transport could not survive any of this, for a reason in the QUIC
library rather than the network. `noq` drops every packet that reaches a
**client** from an address other than the one it dialed
(`noq-proto` `early_discard_packet`, "discarding packet from unrecognized
peer"). RFC 9000 lets only the client migrate; a server that moves has no
standard way to say so. On this transport the guest is the client and the
host is the server. A host whose NAT rebinds, or whose traffic moves to its
other uplink, keeps sending, and its packets now come from a new address.
The guest's endpoint discards every one of them, sends its own packets to
the old address, which is gone, and the session dies of silence after
`QUIC_MAX_IDLE_TIMEOUT_SECS` with both machines online.

The opposite direction was already covered. A guest that moves is a client
migrating, and `noq`'s server follows it (`ServerConfig::migration`, on by
default), as long as the host's NAT lets the guest's new address in.

## Decision

### 1. The guest's endpoint never sees the host's real address

A guest's `ObfuscatedSocket` is given a `HostRoute`. Every connection dials
`HostRoute::ALIAS` (`192.0.2.1:9`, TEST-NET-1, so a datagram that leaked to
it would reach nobody). The socket does two things with that address:

- It sends what is addressed to the alias to wherever the route says the
  host is now.
- It hands every datagram that opens under the invite's key to the endpoint
  as coming from the alias, whatever its source.

`noq` therefore sees one peer at one fixed address for the life of the
endpoint, while the socket moves underneath it. Every channel of the session
(control, media, file, tunnel) goes through the same socket and moves with
it.

### 2. The route moves on two kinds of evidence

- **The host's signed word.** When a rendezvous record moves the dial target
  forward (ADR 0113 and 0116, including the host's answer to a knock and its
  announcement of a new address), the route moves to that address at once.
  Before this ADR such a record reached only the *next* dial attempt, and a
  live connection never heard of it.
- **The host's packets.** A datagram from a different address moves the
  route there, but only once the current address has delivered nothing for
  `OBFUSCATED_HOST_MOVE_QUIET_MS` (500 ms). Without that condition, a copy
  of an old datagram replayed from elsewhere could pull a live path away
  from a host that is still talking on it. A host streaming video sends
  every few milliseconds, so the condition costs nothing on a real move.

### 3. A live session that goes quiet knocks again

The route cannot repair a path that died because the *guest's* NAT moved and
the host's NAT only admits addresses it has sent to. The host is the only
side that can open its own NAT, and a knock is how a guest asks it to (ADR
0113/0116). Until now a guest knocked only while dialing.

Each guest endpoint with a push channel now runs `knock_on_silence`. It
checks four times a second, and if the endpoint has a connection open and
has heard nothing from the host for `OBFUSCATED_SILENCE_REKNOCK_MS`
(2.5 s), it:

- knocks at once through the relays with this socket's last known public
  address;
- asks the reflectors again, and knocks a second time if the socket's own
  address has moved.

The host answers as it answers any knock. It punches towards the guest and
says, signed, where it is, and that answer moves the route (§2).

This is the "ask for the new address every second" of the report, done only
when something is wrong. A healthy session is never silent that long,
because both ends send a keep-alive every `QUIC_KEEPALIVE_SECS` (2 s),
asserted to be below the threshold. The knock never repeats sooner than the
host would punch again for one address anyway (`RENDEZVOUS_REPUNCH_SECS`,
3 s), so it adds nothing to the host's punch budget (ADR 0123). Two knocks
fit before QUIC gives up (asserted: 2.5 s + 3 s < 8 s).

## Consequences

- A host that changes public address or uplink under a session no longer
  drops it. The guest follows within one packet after 500 ms of silence from
  the old address, and the session carries on without a reconnect, a
  consent round or a key frame.
- A guest that changes address was already covered by QUIC migration. It is
  now also covered when the host's NAT filters by sender, through the knock.
- The idle timeout, the resume path and ADR 0123's numbers are unchanged.
  They remain the fallback for a network that is really down, which no
  software on either end can carry a session through (ADR 0116, 0123).
- A holder of the invite secret, or someone who recorded the session's
  datagrams, can still point a guest's packets at an address of their
  choice, but only while the real host is already silent. What the guest
  then sends is its keep-alives and retransmissions to a peer that is not
  answering, not a stream. Before this ADR the same people could make a
  host punch at any address (ADR 0123 budgets that), so the exposure is no
  wider.
- The iroh transport is untouched. It has its own paths and relays.

## Not done

- **The cause on the hosts' network** (two uplinks, or a NAT that rebinds
  every few minutes) is not identified. The fix does not depend on which
  one it is.
- **Live verification** between `win` and `beta` over the internet, with the
  tailnet blocked for lumepeer (project rule: lumepeer must never rely on
  it). See `docs/release-checklist.md`, "A host that moves".

## Verification

Unit and loopback tests in `lumepeer-net`:

- `a_session_follows_a_host_whose_public_address_moved`: a simulated NAT
  moves the host's public address mid-session and drops everything at the
  old one; an echo stream keeps working, and the route names the new address.
  With the move disabled the same test fails after 7 s.
- `a_session_survives_the_guests_own_address_moving`: the mirror, carried
  by QUIC migration on the host.
- `a_live_session_that_goes_silent_knocks_again`: an idle session exchanging
  keep-alives does not knock; a blacked-out one knocks once its silence
  passes the threshold, and the session is the same one after the path comes
  back.
- `the_route_moves_to_a_new_address_only_once_the_old_one_is_quiet`,
  `the_host_pointing_elsewhere_moves_the_route_at_once`.
