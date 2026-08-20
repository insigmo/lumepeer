# ADR 0020 — Relay-only transport while the WAN path is being verified

Status: accepted (temporary)
Date: 2026-08-20

## Context

Two machines on one network — a Windows host and a Debian guest in a VMware
VM — connected reliably, and the same build was reported not to work "over the
internet". That report could not be checked with the setup available, because
the setup is exactly the one that hides the answer: iroh prefers a direct path,
the two machines can always reach each other directly, and so every successful
session proved only that the LAN works. `config/default.toml` says as much —
`prefer_direct = true` — and §5 makes relay a fallback, not the normal route.

The two machines also share one public IP (81.38.20.57), so "connect over the
public IP" is a NAT-hairpin test, not a WAN test, and most consumer routers do
not do hairpin at all. The only honest internet path between them is the one
that leaves the network and comes back: the relay.

## Decision

`PeerEndpoint::bind` chooses its transports at runtime:

- **relay only** by default — `Endpoint::builder(presets::N0).clear_ip_transports()`,
  so there is no IP transport to hole-punch onto and no LAN shortcut to fall
  into. Every packet of every session goes out to a relay and back.
- **relay + direct** when `LUMEPEER_LAN_DIRECT` is set to `1`/`true`/`yes`/`on`.

Neither path is new code that replaces the old one. `bind_with_lan` is the
previous body of `bind`, verbatim; `bind_relay_only` is the same builder with
one call added; `bind_local` (`presets::Minimal`, used by the integration
tests) is untouched. Reverting is one env var, and making relay-only opt-in
again is one `if` in `endpoint.rs`.

A second, permanent change came out of the same work: **the host refuses to
issue an invite while its endpoint has no dialable address** (`NetError::Offline`,
IPC code `OFFLINE`). Relay-only makes the failure sharp — before a relay is
reached the `EndpointAddr` is empty, so a ticket issued then is undialable by
anyone — but the bug predates it and is worse with direct paths on: a ticket
issued in that window carries LAN addresses and no relay URL, works perfectly
across the room, and cannot be dialed from outside. That is precisely the
"works locally, not over the internet" report, and it fails on the guest's
machine, minutes later, with an error that blames the wrong side.

## Consequences

- Sessions are slower and cost relay bandwidth: measured RTT between the two
  test machines was ~150 ms via `euc1-1.relay.n0.iroh.link` (Frankfurt), against
  a sub-millisecond LAN path. This is the price of the test, not a target.
- `docs`/§5's `prefer_direct = true` is not honoured while this is in force.
  The setting is not read by any Rust code today (nothing loads
  `config/default.toml`), so nothing silently disagrees — but the file now
  describes an intent the binary does not implement, and that has to go back in
  step when this ADR is reverted.
- Direct-path and hole-punching behaviour is untested by anything run under the
  default build until then.

## Verification

`crates/net/examples/wan_probe.rs` is a two-role probe over the real stack —
`PeerEndpoint`, a signed `InviteTicket`, `guest_handshake`/`host_handshake`, a
`ConsentGrant` — that reports which path carries the session:

```sh
cargo run -p lumepeer-net --example wan_probe -- host          # prints INVITE …
cargo run -p lumepeer-net --example wan_probe -- guest <code>  # on the far machine
```

Run between the Windows host and the Debian VM on 2026-08-20 it reported
`path kind=relay selected=true rtt=164ms`, `RESULT ok` on both sides, with the
invite carrying `{Relay(https://euc1-1.relay.n0.iroh.link./)}` and nothing else.
The full desktop app was then driven through the same path — invite, consent
grant, media connection, decoded picture on the guest's canvas — with no direct
transport available to either side.

## Reverting

Delete the `lan_direct_enabled()` branch in `PeerEndpoint::bind` so it calls
`bind_with_lan` unconditionally, and drop `LAN_DIRECT_ENV`. Keep
`bind_relay_only` and `wan_probe`: a relay-only mode that can be turned on is
how this question gets answered the next time it is asked. Keep the
`NetError::Offline` guard on invite issuance — it is not part of this
experiment.
