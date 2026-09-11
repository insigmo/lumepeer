# ADR 0082 — Hole punching without a signalling channel

Status: accepted
Date: 2026-09-10

Finishes increment 2 of the roadmap in
[ADR 0052](0052-serverless-transport-obfuscated-quic-with-stun-discovery.md),
whose second half — "hole-punch coordination" — was answered by
[ADR 0053](0053-invite-ticket-carries-a-stun-address-and-pinned-cert-fingerprint.md)
with a NAT-mapping keep-alive and deferred by
[ADR 0080](0080-one-connection-type-over-two-transports.md) with one line:
"hole punching is still not attempted." This is that increment.

## Context

gap-tasks/22 puts the choice plainly: either the keep-alive stays the whole
mechanism, or the punch is coordinated over a live iroh connection used as a
signalling channel. It also says which order to answer in — measure first,
choose second — because the two differ only where the NATs make them differ.

### What was measured

`crates/net/examples/obfuscated_wan_probe.rs` grew a `nat` role: it asks every
reflector of `STUN_SERVERS` for the same socket's reflexive address and
compares the answers, then measures how long a mapping survives with nothing
sent through it. One address from every reflector is an endpoint-independent
mapping, whose address an invite can carry; disagreeing addresses are a
symmetric NAT, whose address is worth nothing to anybody but the reflector
that reported it. Reflectors resolving to one address are deduplicated first,
or the test compares a mapping with itself and calls every NAT a cone.

- **This machine** (Windows, Spain, provider NAT): Cloudflare
  (`162.159.207.0:3478`) and Google (`74.125.250.129:19302`) both saw
  `176.85.151.178:55434` from one socket — endpoint-independent, and
  port-preserving besides. Its mapping was still alive after **240 s** of
  silence, the longest period measured; no expiry was observed at all.
- **`pi.local`** (aarch64, Debian trixie) sits on this machine's own LAN,
  behind the same NAT, and measured the same. Two hosts behind one NAT are one
  measurement, not two.
- **`beta`** — the double-NAT machine, and the only known hard case — has been
  offline for over a day and could not be measured. What is on record for it
  is Fase 0 of ADR 0051: `MappingVariesByDestIP: false`, i.e. endpoint-
  independent mapping behind a double NAT, and a relay link it cannot hold.
- **Filtering behaviour was not measured, and cannot be from one machine.** It
  takes an unsolicited packet from a host outside the NAT; `crate::stun` sends
  a plain Binding request with no RFC 5780 `CHANGE-REQUEST`, so no reflector
  will answer from a second address either. The probe reports it as unmeasured
  rather than letting it be assumed.

That last point corrects something ADR 0053 states too strongly. It says an
endpoint-independent NAT "accepts inbound from *any* source for as long as the
mapping stays alive." That is mapping behaviour, and it is what the reflector
comparison measures. Filtering is a separate property: an address- or
port-restricted NAT keeps one mapping for every destination — so every
reflector reports one address and the test above rightly calls it a cone — and
still drops a packet from a source it has never sent to. On such a host the
keep-alive holds a mapping open that the guest's packets cannot enter, and no
unsynchronized punch can change that, because the fix is for the *host* to
send first and the host has no address to send to.

## Decision

**Variant 1: the keep-alive stays the host's whole half of the punch. No
signalling channel is borrowed, and the iroh path is not used to coordinate
anything.** Three reasons, in the order that decided it:

1. **The signalling channel is missing exactly where the punch is needed.**
   Variant 2 needs a live iroh connection before it can say "send now". On
   `beta` — our one double-NAT machine, and the reason this transport exists —
   the n0 relay link does not hold, which is what Fase 0 of ADR 0051 recorded
   and what has been true of it since. Where iroh *is* up, a session already
   exists: a coordinated punch would then buy a change of transport, not
   reachability, and changing transport mid-session is gap-tasks/23's subject,
   deliberately excluded by ADR 0080.

2. **It would cost the promise, and the promise is the point.** ADR 0052 draws
   its line at a stateless reflector: one packet in, one packet back, nothing
   carried. Signalling over iroh crosses that line, because iroh's rendezvous
   *is* n0's relay fleet. The honest wording, if variant 2 were taken, is
   this: no server would carry session data, but a session could no longer
   *start* without one reachable. "Serverless" would shrink from "no server is
   needed" to "no server sees the traffic" — a real property, and a smaller
   one than the transport was built to claim. That sentence is the price, and
   nothing measured here is worth paying it.

3. **It could not be validated.** A coordinated punch is exercised only by two
   machines behind two different NATs, at least one of them awkward. There is
   no such pair available: `beta` and `debian` are offline, and the Pi shares
   this machine's NAT. Variant 2 would have shipped as untested code on the
   one path that exercises it.

**What the guest does instead is punch with the dial itself, on a bounded
cadence** (`punch` in `crates/net/src/obfuscated_endpoint.rs`):
`OBFUSCATED_CONNECT_ATTEMPTS` attempts, each bounded by the new
`OBFUSCATED_PUNCH_ATTEMPT_TIMEOUT_MS` and spaced by the existing
`OBFUSCATED_CONNECT_RETRY_BACKOFF_MS`, stopping at the first that connects.

- **The punch packets are the dial's own.** Each attempt's QUIC Initial goes
  out through the same `ObfuscatedSocket`, sealed with the same
  invite-derived key, so a punch and a session are indistinguishable on the
  wire. A bespoke punch packet would have been a second shape for an observer
  to learn — the opposite of what this transport is for.
- **Success is an established connection**, never a sent packet. A packet
  proves nothing about a mapping it never reached.
- **The bound is what makes it a cadence.** The dial was already a retry loop,
  but an unbounded QUIC attempt against a mapping that is not open sits there
  until the idle timeout, so five attempts were minutes of near-silence rather
  than a train of packets, and no caller could give up in time to do anything
  else. Bounded, a failed punch costs `OBFUSCATED_CONNECT_ATTEMPTS` ×
  (attempt timeout + backoff) — twelve seconds — and then returns the ordinary
  dial error it always returned.

**Nothing about the failure is shown to a user, and no fallback is added
here.** Which transport a session takes is still one decision taken before a
packet is sent (ADR 0080); trying one and falling back to the other is
gap-tasks/23. This ADR only makes the obfuscated dial fail quickly and quietly
instead of hanging.

### What does not punch, and what happens to it

Recorded plainly, because each is a configuration a user can be behind and
none of them is a defect:

- **A host behind a symmetric NAT.** The address in the invite names a mapping
  the NAT made for the reflector; the port a guest would need is a different
  one the host cannot learn and the invite cannot carry. Predicting it by
  spraying a port range is scanning, and gap-tasks/22 forbids it.
- **A host behind an address- or port-restricted NAT that has never sent to
  this guest.** The mapping is there and open, and the filter drops the
  guest's packets anyway. Only the host sending first would fix it, which is
  the coordination variant 1 does not have.
- **Both sides symmetric.** The above, on both ends.
- **A host whose mapping lapsed before the guest dialed.** A lapsed mapping is
  a new port, so the invite names an address nobody answers on. The keep-alive
  exists to prevent this and, on the NAT measured here, has 240 s of headroom
  to work with, against an interval of 25 s.
- **A Windows host whose firewall has no inbound rule for the program.**
  Windows Firewall passes UDP from an address a socket has sent to and drops
  everything else unless a rule admits the program — a port-restricted filter
  on the host itself, whatever the router in front of it does. The app gets
  its rule only from the "allow access" prompt Windows shows the first time
  it listens: a user who declined it, or a process started where no prompt can
  appear (an SSH session), is filtered this way. Not measured here — the
  router in front of this machine does not hairpin, which hid it — but a
  property of the platform rather than of any network.
- **Two hosts behind one NAT that does not hairpin.** The invite names the
  public address, and a packet sent to it from inside the same NAT never comes
  back in. Measured here: the probe's host and guest on this machine, and
  again both on `pi.local`, 0 of 3 each, with no handshake reaching the host.

Every one of these falls back to the existing iroh path, unchanged, which is
what the guest uses today whenever the flag is off or the ticket carries no
obfuscated address. `beta` may well remain in this set; ADR 0051 said so
before this increment started, and nothing here was expected to move it.

## Consequences

- One new constant, `OBFUSCATED_PUNCH_ATTEMPT_TIMEOUT_MS`. No new dependency,
  no protocol change, no ticket-format change, no grant, no UI string.
  `crates/core` gains a number and nothing else.
- The punch's schedule is testable without a network: `punch` takes the dial
  as a closure, so tokio's paused clock drives three tests that assert the
  attempt count and the exact spacing when attempts are refused, when they go
  unanswered, and when one connects.
- The keep-alive is untouched, as gap-tasks/22 requires: it remains sufficient
  for a full-cone host and is the only half of the punch a one-way invite can
  coordinate.
- The claim in ADR 0053 about inbound from "any source" now has its filtering
  caveat written down. That ADR's decision does not change — the keep-alive is
  still the only mechanism available — but the limit it understated is on the
  record here.
- Whether a restricted-cone host punches is still unknown, and stays unknown
  until two machines behind two NATs can be measured at once. gap-tasks/22's
  definition of done asks for both machines' NAT types and a success rate over
  ten attempts; neither can be answered from one NAT, and both are left open
  rather than filled in with the half that was reachable.
- So that closing them is a run and not another change, the probe's `host`
  and `guest` roles take a count: one invite serves a series, each guest
  punch goes out from a socket of its own — a reused socket would leave the
  first attempt's mapping open, and every later attempt would count a punch
  the first had already made — and the guest ends on `PUNCH landed=k/n`.

## Alternatives considered

- **Variant 2, the punch signalled over iroh.** Rejected above: absent where
  it is needed, redundant where it is available, costly to the one property
  the transport exists for, and unverifiable today. It is not refuted as an
  idea — if a rig ever shows restricted-cone hosts to be the common case and a
  relay to be reachable on them, this is the ADR to revisit.
- **A dedicated punch packet.** A datagram sent to open the mapping before
  QUIC starts. It buys nothing the Initial does not, and costs a second thing
  on the wire that is not a session — either a distinguishable shape, or a
  second format to keep indistinguishable.
- **Port prediction.** Guessing the next port a symmetric NAT will allocate,
  and sending to a range around it. Forbidden by gap-tasks/22, and rightly:
  it looks like a scan because it is one.
- **Raising `NAT_MAPPING_KEEPALIVE_SECS` headroom, or lowering it.** The
  measurement gives 240 s of observed survival against a 25 s interval, which
  is an order of magnitude of margin. Tuning it without a NAT that actually
  expires a mapping would be tuning against nothing.

## Verification

- `cargo test -p lumepeer-net` — 72 passed, 0 failed (69 before this
  increment; the three new ones are the punch's timing).
- `cargo clippy -p lumepeer-core -p lumepeer-net --all-targets -- -D warnings`
  — clean.
- `cargo run -p lumepeer-net --example obfuscated_wan_probe -- nat`, on this
  machine and on `pi.local`: the measurements above.
- `obfuscated_wan_probe -- host 3` and `-- guest <invite> 3`, both on this
  machine and both on `pi.local`: every punch failed after ~15 s — the 12 s
  train plus the endpoint's shutdown — and the host saw no handshake, because
  the router does not hairpin (see above). That exercises the series' failure
  path and its cadence on a real socket; its success path was **not**
  exercised.
- The two-machine items — both NAT types, and the share of successful punches
  over ten attempts — are **not** verified. There is no second NAT to measure
  from; see the consequences above. On the day they can be, it is `nat` on
  both machines, then `host 10` on one and `guest <invite> 10` on the other.
  If the host is a Windows machine driven over SSH, its probe executable needs
  an inbound firewall rule first, or the run measures the firewall and not
  the NAT.
