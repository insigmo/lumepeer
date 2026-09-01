# ADR 0053 — Invite ticket carries a STUN address and a pinned cert fingerprint

Status: accepted
Date: 2026-09-01

## Context

Task 17 increment 2 (`docs/tasks/17-serverless-obfuscated-quic.md`, ADR 0052
item 2): embed the host's STUN-discovered reflexive address in the invite so
a guest can dial it directly over the obfuscated transport increment 1
already proved (`crates/net/src/obfuscate.rs`). Two questions increment 1
left open had to be settled to build this: how the two sides find each
other's live address without a rendezvous server, and what a QUIC/TLS
handshake between two ad-hoc peers with no CA is actually protected by.

## Decision

### 1. NAT-mapping keepalive, not synchronized simultaneous punch

The task doc's Fase 1 describes "simultaneous hole punch by both sides." That
assumes a live signalling channel each side can use to learn the other's
current reflexive address and time a send — which is exactly what Fase 0
(`[[project-lumepeer-serverless-quic-task17]]`) found this app does not have
and, by design (no server), will not have: the invite is a one-shot, one-way
message from host to guest.

For an endpoint-independent ("non-symmetric") NAT — measured on both rig
machines in Fase 0 — that channel turns out not to be necessary. Such a NAT's
mapping is opened by the *first* outbound packet through a local port and then
accepts inbound from *any* source for as long as the mapping stays alive, not
only from the address it was first opened towards. The host's own STUN query
already sends that first outbound packet. So the only thing that has to
happen afterward is keeping that one mapping alive — by resending a STUN
request through the same socket, on a timer well under a NAT's usual UDP
binding timeout — until the guest dials in, any time up to the ticket's
`INVITE_TICKET_TTL_SECS`. No synchronization with the guest's send is needed;
the guest's dial is an ordinary outbound-then-inbound packet on its own side,
which opens its own mapping the same way any outbound UDP does.

This does not help a **symmetric** NAT or a **double NAT** (`beta`'s case):
symmetric mapping varies per destination, so a mapping opened towards the
STUN server does not admit the guest; double NAT means the reflexive address
STUN reports is not even the host's own edge. Both are the same honest limit
Fase 0 already recorded, not something this increment changes.

### 2. TLS certificate pinned by a fingerprint carried in the signed ticket, not CA-validated

QUIC requires a TLS handshake to exist. There is no real CA for a cert an
ephemeral desktop process generates for itself, so the two live options are
"accept any cert" (no defense in depth) or "pin the exact cert the host
generated." Both need the same custom
`rustls::client::danger::ServerCertVerifier` (rustls dropped a one-line
"skip verification" toggle some releases ago) — writing one is unavoidable
either way, so accept-any buys nothing over pinning except one less struct
field.

Pinning wins: the actual authentication boundary here is already the
obfuscation layer from increment 1 (`Obfuscator`, `crates/net/src/
obfuscate.rs`) — every datagram is sealed with a key derived from the
invite's `invite_id` via blake3, so an observer or an active attacker without
the invite cannot produce a datagram `noq` will even decode, let alone reach
the TLS layer underneath. TLS here is a protocol requirement, not the
security boundary. But pinning the host's cert fingerprint — computed once
when the cert is generated, signed into the ticket alongside everything
else — costs one extra `[u8; 32]` field and blocks an attacker who somehow
does reach the QUIC layer (a future bug in the AEAD wrapper, say) from
impersonating the host with a different cert. Defense in depth for the price
of a hash comparison.

## Consequences

- `InviteTicket` (`crates/net/src/ticket.rs`) gains two new signed fields,
  `obfuscated_addr: Option<SocketAddr>` and `host_cert_fingerprint:
  Option<[u8; 32]>`, both `None` when STUN discovery found no usable address
  (double NAT, no reflector reachable) — the ticket still carries the
  existing iroh `node_addr` unchanged, so the fallback path is untouched.
  This is a breaking change to the ticket's wire format; tickets are
  ephemeral (10-minute TTL, never persisted across a version boundary), so no
  migration or back-compat shim is needed, only the same-build assumption the
  ticket format already relied on.
- `noq = "=1.2.0"` (already a direct dependency since increment 1) and
  `rcgen = "0.14"` (previously dev-only, used only by increment 1's test)
  both back a real, non-test code path now:
  `crates/net/src/obfuscated_endpoint.rs`'s host side generates its cert with
  `rcgen` at bind time, not only inside `#[cfg(test)]`.
- New constants in `crates/core/src/constants.rs`:
  `NAT_MAPPING_KEEPALIVE_SECS` (the resend interval that holds the mapping
  open) and `OBFUSCATED_CONNECT_ATTEMPTS`/`OBFUSCATED_CONNECT_RETRY_BACKOFF_MS`
  (the guest's dial-retry budget on this transport — kept separate from the
  existing `DIAL_ATTEMPTS`/`DIAL_RETRY_BACKOFF_MS` of ADR 0050 since that
  budget was tuned for iroh's relay-flap symptom, a different failure mode).
- Still not done by this increment (ADR 0052's own roadmap, items 3-4): the
  live desktop app does not call any of this yet — `on_invite_create`/
  `spawn_dial` in `apps/desktop/src-tauri/src/network.rs` are untouched, and
  the new address/fingerprint fields are `None` on every ticket the app
  issues until that wiring lands. Validated so far only via a new example,
  `crates/net/examples/obfuscated_wan_probe.rs`, mirroring the existing
  `wan_probe.rs`.
