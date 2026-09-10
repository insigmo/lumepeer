# ADR 0078 — A tunnel is a grant plus an address the host named

Status: accepted
Date: 2026-09-10

Follows [ADR 0048](0048-a-fifth-independent-grant-for-the-hosts-own-screen.md)
in shape — a capability nothing already granted implies gets a flag of its
own — and [ADR 0032](0032-file-transfer-start-and-the-lazy-file-connection.md)
in mechanism: a fourth ALPN, opened lazily, so a busy channel can never delay
a revoke on another one. Extends
[ADR 0054](0054-full-control-carries-every-independent-grant.md), which
settled that `Role::FullControl` carries every independent grant.

## Context

Backlog item 21 is port forwarding, and it is the most far-reaching thing in
the whole list. Every other capability in this app acts on the host's own
machine: its screen, its keyboard, its clipboard, its files. A tunnel does not
— it makes the guest a node inside the network the host is sitting in. The
router's web interface, the database that only listens on loopback, the
internal service behind the VPN: all of them were reachable from the host and
by nothing else, and a tunnel is the guest borrowing that position.

So the question is not "how do bytes get forwarded" — that part is a socket
and a frame header. It is "what exactly did the host agree to", and the
tempting answer is the wrong one: a single `tunnel` switch, after which the
guest names any address it likes. That is not a permission, it is a network
handover, and the person clicking it has no way to know what it covers.

## Decision

**Two decisions, not one.** A tunnel needs a grant *and* an address, and
neither implies the other.

- **`Grants::tunnel` / `IndependentGrant::Tunnel`**, the ninth independent
  grant, carried by `Role::FullControl` alone (ADR 0054) and separately
  revocable. It says a tunnel may exist. It opens no address.
- **A per-session address list**, `ActiveSession::allowed_targets`,
  **snapshotted at the moment of the grant and empty by default.** Every
  entry is a deliberate act by the host about *this* session
  (`allow_tunnel_target`) — which is what separates it from editing a policy
  file, exactly as `set_control_policy` must not widen a running
  `ControlLimited` session (§8.2). `TunnelTarget::loopback` is the default
  policy in code: `127.0.0.1`, `::1` and `localhost` are what a port forward
  is *for*, and anything else is the host's own subnet, nameable one address
  at a time and never by default.
- **Both are re-read for every connection**, not when the tunnel opened.
  `SessionManager::tunnel_allows(peer, host, port)` is asked once per
  `TunnelOpenRequest` and once again after the TCP connect returns, because a
  connect takes real time and a grant withdrawn during it must not be honoured
  by the socket it produced. A tunnel is many connections; a revoke that
  landed only on the first one would be a revoke that waits for a socket to
  close.

**A fourth ALPN.** `rd/tunnel/1`, `Channel::Tunnel`, opened lazily by the same
side that dialed the control connection (ADR 0026), and only once something
has actually been forwarded. Not a second use of `rd/file/1`: the whole reason
these are separate connections is that traffic on one cannot delay a decision
on another, and a tunnel is the busiest thing a guest can hold open. Every
forwarded TCP connection is one stream inside one bidirectional QUIC stream,
told apart by `u32_be stream_id || u32_be len || bytes` — the same
self-describing shape the file chunk header uses, so many short connections do
not serialize behind one slow one, and a length is refused against
`TUNNEL_BUFFER_BYTES` before anything allocates it (§9.1). A `len` of zero is
the end of a stream, which is what turns one end's `FIN` into the other's.

**Three control messages, `PROTOCOL_MINOR` 15**, behind `FEATURE_TUNNEL`:
`TunnelOpenRequest { host, port, stream_id }`,
`TunnelOpenResponse { stream_id, refused }` and `TunnelClose { stream_id }`.
None of them carries payload. The refusal is a closed set (`TunnelRefusal`)
and deliberately coarse in its first variant: `NotGranted` covers both "no
grant" and "not on the list", because which of the two the host is missing is
a fact about the host's own consent screen and the guest's next move — ask the
operator — is the same either way.

**Bounds, all constants.** `MAX_TUNNEL_STREAMS_PER_SESSION` (32) is what one
session may hold open at once; `TUNNEL_BUFFER_BYTES` (64 KiB) is one stream's
read buffer in each direction, so a full tunnel's buffers are 4 MiB inside the
§15 budget; `TUNNEL_IDLE_TIMEOUT_SECS` (300) closes a stream that has carried
nothing, because an idle socket is still a socket held open on somebody else's
machine; `TUNNEL_HOST_MAX_BYTES` (253) bounds a host string before it becomes
a resolver call; `MAX_TUNNEL_TARGETS_PER_SESSION` (16) bounds the list itself.

**The guest binds the loopback and nothing else.** A forwarded port that other
machines on the guest's network could reach would make the guest's own machine
a second entrance to the host's network, which nobody consented to.

**What the host sees.** Every address on a session's list is a row on that
session's own line, with how many connections are open through it and how many
bytes the tunnel has carried; a "close all" ends every one of them at once,
and taking an address off the list closes the connections that were using it.
A tunnel is never silent on the host's screen (§2.2). Opening a tunnel, every
connection through it, every address named or taken back and every close is
audited, with the peer as a salted hash — and the *address* deliberately not
recorded, because §15 keeps the host's own network layout out of a log that
can leave the machine.

**Revoking closes everything immediately.** `IndependentGrant::Tunnel` turned
off runs `close_tunnel`, which ends every stream, aborts every local listener
and drops the QUIC connection — the same discipline withdrawing
`file_transfer` already applies to `rd/file/1`. The address list is left
alone: the host withdrew the permission, not its own opinion about which
addresses this guest could have.

## Consequences

- A guest with full control can reach a service on the host's loopback once
  the host names it, and nothing before that. The default state of a
  full-control session is a `tunnel` flag with an empty list, which reaches
  nowhere.
- The host has to name every address. That is friction, and it is the point:
  the alternative is a switch whose meaning nobody can state.
- A name is resolved on the host, so a target that resolves differently there
  than here is the host's answer. That is correct — it is the host's network
  — and it means a guest cannot use the tunnel to learn this machine's own
  resolver.
- Nothing here is a VPN. There is no TAP/TUN device, no subnet routing and no
  UDP: one TCP connection to one named address at a time, which is what
  backlog item 21 asks for and the boundary the batch drew.
- A tunnel's addresses are not remembered between sessions. A new session
  starts with an empty list even for a device in the address book.

## Alternatives considered

- **One `tunnel` switch and no address list.** Rejected above: it is the
  handover this ADR exists to avoid.
- **A policy file of allowed targets**, like `control_policy.toml`. It would
  make the decision editable when no guest is on screen, which is the wrong
  moment to make it — and §8.2's own rule then forbids the edit from reaching
  a running session, so the host would still need the per-session act. The
  snapshot at grant time keeps that rule and the per-session act is the whole
  interface.
- **Reuse `rd/file/1`.** One connection carrying both a 64 GiB transfer and a
  database session makes each one the other's problem, and it puts the tunnel
  behind the file grant.
- **Forward UDP as well.** A different lifetime model, no connection to close,
  and nothing in item 21 asks for it.
- **Let the guest name any address and ask the host to confirm each one.** A
  dialog per address is a dialog that gets clicked through, and unlike the
  file case (ADR 0076) there is no grant already in force that covers the
  answer — the addresses *are* the decision.
