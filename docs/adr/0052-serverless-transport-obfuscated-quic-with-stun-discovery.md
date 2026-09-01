# ADR 0052 — Serverless transport: obfuscated direct QUIC with STUN discovery

Status: accepted
Date: 2026-09-01

## Context

Task 17 (`docs/tasks/17-serverless-obfuscated-quic.md`) asks for a fast, stable,
DPI-resistant host↔guest connection with no server carrying data, where the app
is both client and server. Fase 0 (ADR 0051) established the hard boundary: a
NAT'd host cannot learn its own public reflexive `ip:port` without an external
reflector, and iroh's relay does double duty today — reflector/rendezvous
**and** TCP fallback transport. When asked to choose the shape, the user
delegated it ("decide for me, just make it fast and stable") after being shown
the one unavoidable fact: two peers both behind NAT need *something* to help
them find each other; the only choice is whether that something is a relay
(which carries data — rejected) or a stateless reflector (which does not).

## Decision

The serverless transport is **direct QUIC that is always obfuscated and never
relayed**, with **stateless STUN-style discovery** for the one step that cannot
be done from the host alone:

1. **Data path: direct QUIC over UDP, obfuscated, no relay ever.** Speed comes
   from QUIC (congestion control, loss recovery, multiplexing) running over the
   real network path; DPI resistance comes from the Fase 2 codec
   (`crates/net/src/obfuscate.rs`, ADR 0051) wrapping every datagram so the wire
   is uniform noise on a non-standard port. No middlebox ever sits in the data
   path. The app stays a symmetric peer — it already both listens and dials; no
   separate server component is introduced.

2. **Discovery: the host's own STUN query, not n0's relay.**
   `crates/net/src/stun.rs` sends one stateless Binding request from the very
   socket that will carry the session and learns the public reflexive address to
   put in the invite. The servers are caller-supplied public reflectors, not
   n0's fleet — chosen precisely because a single stateless request/response is
   something `beta` can do reliably, unlike the persistent relay link it cannot
   hold (`[[project-lumepeer-connect-and-media-limits]]`). A reflector sees one
   packet and answers with its source address; it never carries data, so it is
   not the "intermediate server" the task forbids.

3. **Reachability: hole punch to the address in the invite.** With both peers'
   reflexive addresses known (each side's own via STUN, the other's from the
   invite), a session is established by simultaneous send, which works for the
   endpoint-independent (non-symmetric) NATs this pair has. No relay fallback:
   if the punch cannot succeed (a truly unreachable host — symmetric NAT with no
   forwarding, and no reflector reachable), the connection honestly fails rather
   than falling back to a server.

4. **The existing iroh path stays as-is, unobfuscated, as the fallback**
   (task 17 trap: do not remove existing paths). The obfuscated serverless
   transport is added beside it and proves itself before anything is removed.

### Why not the alternatives

- **Fork iroh to obfuscate its own IP transport** keeps iroh's mature NAT
  traversal but takes on vendoring and maintaining a fork of a published crate,
  and still leaves iroh's discovery leaning on n0's relay. Deferred: it remains
  a possible future if the hand-rolled punch proves to cover too few networks.
- **Rebuilding all of iroh's magicsock** (multi-path, relay, DHT) would be less
  stable than iroh, the opposite of the goal. The chosen design is deliberately
  narrower — targeted at a known peer whose address arrives out of band in the
  invite — which is simpler and more predictable for the pair this is for.

### Honest limits (stated, not hidden)

- A stateless reflector can itself be blocked (in Russia, public STUN hosts may
  be). Mitigation: a list of servers tried in turn, and the query can be
  obfuscated later; but if every reflector is blocked and the host is behind
  NAT, discovery fails. That is the residual of "no server," not a code gap.
- Against a strict whitelist DPI ("only known traffic allowed"), obfuscation
  that looks like unknown UDP has no guarantee (ADR 0051; task Fase 3).

## Status and roadmap

Built and tested so far (both reusable regardless of how the QUIC socket is
finally wired):

- Fase 2 obfuscation codec — `obfuscate.rs`, unit + real-UDP tests (ADR 0051).
- STUN discovery — `stun.rs`, unit tests on RFC 5769 vectors plus a live probe
  (`examples/stun_probe.rs`) that returned this machine's real public address
  through Cloudflare's reflector with no n0 relay.

Remaining increments (staged; not done):

1. The obfuscated QUIC endpoint: a `noq::AsyncUdpSocket` that applies the codec,
   driving a `noq::Endpoint`, with `keep_alive_interval` = `QUIC_KEEPALIVE_SECS`
   (mandatory — else the ~30 s idle timeout reads as a DPI drop) and a non-443
   default port. Proven by two local endpoints exchanging data over it.
2. Hole-punch coordination and the invite carrying the STUN-learned address.
3. Wiring the app's control/media/file channels onto the transport, behind the
   existing consent/grant path (which is untouched — obfuscation hides traffic,
   it never widens a grant).
4. Real-path validation on the rig (LAN and the provider path), and Fase 3's
   active countermeasures if the plain obfuscated path proves insufficient.

## Consequences

- The pieces that gate serverless connection for a double-NAT host — learning a
  dialable address without n0, and hiding the traffic from DPI — exist and are
  verified. A working end-to-end serverless session still needs increment 1–3
  above; this ADR records the direction so those are built against one decision.
- No new production dependency and no iroh fork were taken to get here; both the
  codec and STUN reuse the crate's existing crypto and std only.
