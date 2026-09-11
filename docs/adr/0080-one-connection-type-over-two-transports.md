# ADR 0080 — One connection type over two transports

Status: accepted
Date: 2026-09-10

Completes increment 3 of the roadmap in
[ADR 0052](0052-serverless-transport-obfuscated-quic-with-stun-discovery.md)
and builds on
[ADR 0053](0053-invite-ticket-carries-a-stun-address-and-pinned-cert-fingerprint.md),
which put the address and the pinned fingerprint into the invite. Keeps the
grant model of [ADR 0029](0029-independent-grants-are-issuable.md) and the
"authenticate here, authorize in core" rule of §2.3 untouched, which is the
whole point.

## Context

The obfuscated transport of ADR 0052 existed as a socket, a codec, a STUN
probe and a two-role probe binary. Nothing in the app used it. `bind_host`
returned an endpoint whose NAT-mapping keep-alive ran until the process ended,
and `connect_guest` returned a bare `noq::Connection` that no part of the
session logic could accept, because everything above the transport —
the handshake of §9.1, consent, grants, revoke, the lazy `rd/file/1` and
`rd/tunnel/1` — is written against `iroh::endpoint::Connection`.

That leaves exactly one interesting question, and it is not about QUIC. A
session is not a connection: it is four connections on four ALPNs (§4.1),
opened at different moments — control at the start, media after a grant, file
and tunnel lazily, sometimes many minutes later. Whatever carries the first
one has to carry the rest, or a guest that reached a host through the
obfuscated path would silently drop back onto iroh the moment a picture was
granted, which is the one thing that path exists to avoid.

The tempting way to get there is a second copy: an obfuscated accept loop with
its own classify step, an obfuscated dial with its own handshake, an
obfuscated media task. Every one of those copies is a place where an
authorization decision can drift, and §2.3 allows exactly one place for those.

## Decision

**One connection type, `lumepeer_net::PeerConnection`, and nothing above the
transport knows which one it holds.**

- It is a `match` over a private `enum Transport { Iroh(..), Obfuscated(..) }`
  and nothing else: `open_bi`/`accept_bi`/`open_uni`/`accept_uni`/`close`/
  `closed`/`close_reason` delegate straight through. Both arms are the same
  QUIC implementation — iroh is built on `noq` and re-exports its stream types
  unchanged — so there are no wrapper streams, no copies and no second framing
  path. This is why the pinned `noq` version must stay exactly the one iroh
  resolves to (ADR 0052): two `noq` crates would be two incompatible
  `AsyncUdpSocket` traits and this type could not exist.
- It carries two facts the transport itself does not agree on: the peer's
  `NodeId` and the negotiated ALPN. On iroh both are properties of the
  connection. `noq` has no notion of endpoint identity at all, so on the
  obfuscated side both are read once out of the TLS handshake by
  `obfuscated_endpoint` and carried along.
- `PeerEndpoint::connect`/`accept` and `ControlConnection` now speak this type.
  `crates/core` was not touched, and could not have been: nothing there ever
  sees a connection.

**Both certificates are the node's own ed25519 endpoint key.** iroh's TLS binds
a session to the endpoint key, which is what makes `NodeId` mean something. The
obfuscated transport had a throwaway self-signed certificate, so a host learned
nothing about who dialed it. Now each side generates its certificate *from* its
endpoint key, client authentication is mandatory, and the key is read back out
of the peer's certificate by walking the DER to `subjectPublicKeyInfo` — never
by searching the encoding for a byte pattern, which would let a peer plant a
decoy key earlier in the certificate and be recognised as a node whose private
key it does not hold. A node therefore has one identity however it was reached,
and `SessionManager`, which is keyed by `NodeId` throughout, needed no second
notion of a peer.

That certificate authenticates and authorizes nothing, exactly like an
endpoint key on the iroh path: it is the peer naming itself, with TLS proving
it holds the matching private key. Whether that peer may do anything is still
decided afterwards, from the invite ticket and the host's own consent, in
`lumepeer-core`.

**The same four ALPNs, set in `ClientConfig`/`ServerConfig`.** No new ALPN, no
renaming, no "obfuscated" variant. The host offers all four; the guest asks for
exactly one per connection, which is what makes `Channel::from_alpn` answer
that connection's channel and no other. The host's accept path, its classify
step, its handshake deadline and its inflight-handshake budget are one code
path for both transports.

**The host endpoint's life is the invite's.** It owns its keep-alive and closes
explicitly: issuing an invite binds one, issuing a replacement closes the one
before it, and the accept loop ends when the endpoint does. Before this, the
keep-alive ran until the process exited, so a host that renewed its code a few
times kept sending a STUN request every `NAT_MAPPING_KEEPALIVE_SECS` from every
socket it had ever bound, for codes nobody could claim any more. Because the
transport derives its datagram keys from the invite id, the endpoint has to
exist before the ticket advertising it can be signed — so the id is chosen on
the actor's thread and the bind runs off it, with the caller's reply travelling
along and answered when it lands (ADR 0027).

**No reflector, or an unusable mapping, is an ordinary outcome.** `public_addr`
is `None`, the ticket says nothing about this transport, and every guest uses
the iroh path. It is not an error and it is not a refusal to issue an invite —
behind a double NAT it is the expected answer.

**The guest keeps one endpoint per host, not one per channel.** The datagram
keys, the NAT mapping and the socket belong to the invite, not to `rd/media/1`.
Each channel is its own QUIC connection on that endpoint, so a busy media or
file channel still cannot delay a revoke on the control one. The actor
remembers a `HostDialer` per peer instead of a bare address, and every later
channel is opened through it.

**Off by default: `[network] obfuscated = false`.** The transport is used when
the flag is on *and* the ticket carries both an address and a fingerprint;
otherwise the iroh path, unchanged. One decision, taken once, before a packet
is sent. Trying one transport and falling back to the other, with the
diagnostics that needs, is deliberately not here.

## Consequences

- With the flag off — every shipping build — nothing is bound, nothing is
  dialed and no STUN request is sent. The only difference from before is the
  type the connection is carried in, which is the same connection.
- The obfuscated path has no relay and no address lookup. A host that moves to
  a new address cannot be found again by endpoint key the way iroh finds it
  (ADR 0062); the invite's address is all there is. The dial retry still
  alternates, because the alternation costs nothing when there is one target.
- The diagnostics panel reports an obfuscated session as a direct path with no
  relay region, because that is what it is: one direct UDP path, never relayed.
  Paths are iroh's own notion and there is nothing else to report.
- Hole punching is still not attempted. The keep-alive of ADR 0053 is what
  holds the mapping open, and a symmetric NAT on both sides will still fail —
  that is the next increment, not this one.
- Two local endpoints can now run the whole handshake and consent exchange over
  this transport inside `cargo test`, with no reflector and no internet, which
  is what makes the wire testable at all.

## Alternatives considered

- **A trait, `trait Connection`, with two implementations.** Every method
  returns a different concrete future type; making that a trait means boxing
  every stream open on the media path, or generics threaded through the actor,
  the view task and the file transfer. A two-arm `enum` has neither cost and
  fewer places to be wrong.
- **Wrap iroh's own socket instead.** Rejected in ADR 0052 and still true:
  iroh 1.0.2 exposes no hook onto its UDP socket, which is why this transport
  is a second endpoint rather than a filter on the first.
- **A second actor, or a second set of actor events, for the obfuscated
  transport.** The copy this ADR exists to refuse.
- **A fifth ALPN, or a variant of each, to mark the transport.** The ALPN names
  a channel, not a route. A guest that had to ask for a different protocol
  string depending on how it got there would make the transport visible to
  every peer, and to the wire, for no gain.
- **Bind the guest endpoint per channel.** Four sockets, four NAT mappings and
  four dials where one suffices, and each one a fresh mapping the host has
  never seen.
- **Ship it on.** The transport is new, it is beside the iroh path rather than
  in place of it, and turning it on costs a host a STUN round trip to a public
  reflector on every invite. A flag that defaults to off is what makes "with
  the flag off, nothing changed" a property of the build rather than of a code
  path nobody took.
